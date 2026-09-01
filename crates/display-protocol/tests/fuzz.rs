//! Arbitrary bytes against the two things that face an untrusted peer.
//!
//! Deterministic rather than a `cargo-fuzz` target: this repository builds on
//! stable, and a fuzzer nobody runs finds nothing. The seed is fixed, so a
//! failure here reproduces exactly; the corpus is the golden vectors, so the
//! mutations start from bytes that mean something.
//!
//! Two invariants: nothing panics, and no session hands out a channel key
//! unless a real handshake put one there.

use prost::Message;
use vmlord_display_protocol::{
    keys::Secret,
    record::{self, Channel, Limits, Record},
    session::{Offer, Session, Support},
    v1::{
        Capability, ClipboardFileCancel, ClipboardFileChunk, ClipboardFileComplete,
        ClipboardFileEntry, ClipboardFileOffer, ClipboardFilePolicy, ClipboardFileRequest,
        ClipboardRecord, FileCancelReason, FileEntryKind, Mode,
    },
};

/// xorshift64*, so the corpus is the same on every machine and every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

fn corpus() -> Vec<Vec<u8>> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    vec![
        std::fs::read(dir.join("handshake.bin")).expect("the handshake vector"),
        std::fs::read(dir.join("records.bin")).expect("the records vector"),
    ]
}

/// One corpus entry with a handful of bytes flipped.
fn mutated(rng: &mut Rng, corpus: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = corpus[rng.below(corpus.len())].clone();
    for _ in 0..1 + rng.below(8) {
        let at = rng.below(bytes.len());
        bytes[at] ^= (rng.next() & 0xFF) as u8;
    }
    bytes
}

fn support() -> Support {
    Support {
        capabilities: vec![Capability::CursorStream],
        modes: vec![Mode::Desktop],
        tile_sizes: vec![32],
        width: 1920,
        height: 1080,
    }
}

fn offer() -> Offer {
    Offer {
        capabilities: vec![Capability::CursorStream],
        mode: Mode::Desktop,
        width: 1920,
        height: 1080,
        tile_size: 32,
    }
}

#[test]
fn the_record_reader_survives_mutated_input() {
    let mut rng = Rng(0x5EED_1234_5678_9ABC);
    let limits = Limits::new(1920, 1080);
    let corpus = corpus();

    for _ in 0..20_000 {
        let mut bytes = mutated(&mut rng, &corpus);
        let keep = rng.below(bytes.len() + 1);
        bytes.truncate(keep);

        let mut payload = Vec::new();
        let mut cursor = bytes.as_slice();
        // Whatever it returns, it must return: a reader that panics on a
        // hostile guest is a viewer that dies on one.
        while record::read(&mut cursor, &limits, &mut payload).is_ok() {}
    }
}

#[test]
fn a_session_never_yields_a_channel_key_to_input_that_did_not_authenticate() {
    let mut rng = Rng(0x1234_5EED_9ABC_5678);
    let corpus = corpus();
    let limits = Limits::new(1920, 1080);

    for _ in 0..5_000 {
        let secret = Secret::generate();
        let mut guest = Session::guest(&secret, support());
        let (mut host, _) = Session::host(&secret, offer());

        let bytes = mutated(&mut rng, &corpus);
        let mut cursor = bytes.as_slice();
        let mut payload = Vec::new();
        while let Ok(header) = record::read(&mut cursor, &limits, &mut payload) {
            let _ = guest.handle(&header, &payload);
            let _ = host.handle(&header, &payload);
        }

        for channel in [
            Channel::Frame,
            Channel::Input,
            Channel::Clipboard,
            Channel::Audio,
        ] {
            assert!(
                guest.channel_key(channel).is_none(),
                "a guest bound a channel to mutated input"
            );
            assert!(
                host.channel_key(channel).is_none(),
                "a host bound a channel to mutated input"
            );
        }
    }
}

/// One of every file clipboard record, written as a peer would send them.
fn file_clipboard_wire() -> Vec<u8> {
    let limits = Limits::new(1920, 1080);
    let mut wire = Vec::new();

    let records = [
        (
            ClipboardRecord::FilePolicy,
            ClipboardFilePolicy {
                max_file_bytes: 1 << 30,
                max_transfer_bytes: 4 << 30,
                retention_seconds: 86_400,
            }
            .encode_to_vec(),
        ),
        (
            ClipboardRecord::FileOffer,
            ClipboardFileOffer { serial: 7 }.encode_to_vec(),
        ),
        (
            ClipboardRecord::FileRequest,
            ClipboardFileRequest {
                serial: 7,
                transfer: 3,
            }
            .encode_to_vec(),
        ),
        (
            ClipboardRecord::FileEntry,
            ClipboardFileEntry {
                transfer: 3,
                path: "notes/todo.txt".into(),
                kind: FileEntryKind::File as i32,
                size: 4096,
            }
            .encode_to_vec(),
        ),
        (
            ClipboardRecord::FileChunk,
            ClipboardFileChunk {
                transfer: 3,
                chunk: vec![0x5A; 1024],
            }
            .encode_to_vec(),
        ),
        (
            ClipboardRecord::FileComplete,
            ClipboardFileComplete { transfer: 3 }.encode_to_vec(),
        ),
        (
            ClipboardRecord::FileCancel,
            ClipboardFileCancel {
                transfer: 3,
                reason: FileCancelReason::TooLarge as i32,
            }
            .encode_to_vec(),
        ),
    ];

    for (sequence, (message_type, payload)) in records.into_iter().enumerate() {
        let record = Record::new(
            Channel::Clipboard,
            message_type as u16,
            sequence as u32,
            0,
            0,
            payload,
        );
        record::write(&mut wire, &record, &limits).expect("a file clipboard record");
    }

    wire
}

#[test]
fn the_file_clipboard_records_survive_mutated_input() {
    let mut rng = Rng(0x0F11_E5EE_D123_4567);
    let limits = Limits::new(1920, 1080);
    let corpus = vec![file_clipboard_wire()];

    for _ in 0..20_000 {
        let mut bytes = mutated(&mut rng, &corpus);
        let keep = rng.below(bytes.len() + 1);
        bytes.truncate(keep);

        let mut payload = Vec::new();
        let mut cursor = bytes.as_slice();
        while let Ok(header) = record::read(&mut cursor, &limits, &mut payload) {
            // Whatever the header claims to be, every decoder it could reach
            // has to answer rather than panic.
            let _ = ClipboardRecord::try_from(i32::from(header.message_type));
            let _ = ClipboardFilePolicy::decode(payload.as_slice());
            let _ = ClipboardFileOffer::decode(payload.as_slice());
            let _ = ClipboardFileRequest::decode(payload.as_slice());
            let _ = ClipboardFileEntry::decode(payload.as_slice());
            let _ = ClipboardFileChunk::decode(payload.as_slice());
            let _ = ClipboardFileComplete::decode(payload.as_slice());
            let _ = ClipboardFileCancel::decode(payload.as_slice());
        }
    }
}
