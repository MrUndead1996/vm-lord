//! What a peer that is broken, hostile, or from another protocol can send.
//!
//! Each case asserts the specific refusal, not merely that something failed:
//! a checksum mismatch reported as a decode error would hide a transport
//! problem behind a schema one.

use prost::Message;
use vmlord_display_protocol::{
    keys::{SESSION_ID_LEN, Secret, TAG_LEN},
    record::{self, CLIPBOARD_MAX_PAYLOAD, Channel, Limits, Record, RecordError},
    session::{Event, Offer, Session, SessionError, Support},
    v1::{
        Capability, ChannelHello, ClientHello, ClipboardFileCancel, ClipboardFileEntry,
        ClipboardRecord, ControlRecord, ErrorCode, FileCancelReason, FileEntryKind, FrameRecord,
        Mode, ProtocolVersion, ServerAuth,
    },
};

fn limits() -> Limits {
    Limits::new(1920, 1080)
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

fn support() -> Support {
    Support {
        capabilities: vec![Capability::CursorStream],
        modes: vec![Mode::Desktop],
        tile_sizes: vec![32],
        width: 1920,
        height: 1080,
    }
}

/// A host and a guest whose control handshake has finished.
fn established(secret: &Secret) -> (Session, Session) {
    let (mut host, client_hello) = Session::host(secret, offer());
    let mut guest = Session::guest(secret, support());

    let server_hello = guest
        .handle(&client_hello.header, &client_hello.payload)
        .expect("a well-formed client hello")
        .reply
        .expect("a server hello");
    let server_auth = guest.pending_auth().expect("the guest's proof");

    host.handle(&server_hello.header, &server_hello.payload)
        .expect("a well-formed server hello");
    let client_auth = host
        .handle(&server_auth.header, &server_auth.payload)
        .expect("a valid guest proof")
        .reply
        .expect("the host's proof");

    let outcome = guest
        .handle(&client_auth.header, &client_auth.payload)
        .expect("a valid host proof");
    assert_eq!(outcome.event, Event::ControlEstablished);

    (host, guest)
}

#[test]
fn a_header_from_no_protocol_at_all_is_refused() {
    let mut payload = Vec::new();
    let error = record::read(&mut [0u8; 24].as_slice(), &limits(), &mut payload)
        .expect_err("a header of zeroes");

    assert!(matches!(
        error,
        RecordError::MalformedHeader { header_len: 0 }
    ));
}

#[test]
fn a_frame_larger_than_the_agreed_geometry_is_refused_before_it_is_allocated() {
    let mut header = Record::new(
        Channel::Frame,
        FrameRecord::Keyframe as u16,
        0,
        0,
        0,
        Vec::new(),
    )
    .header;
    header.length = 1920 * 1080 * 4 + 65537;

    let mut payload = Vec::new();
    let error = record::read(&mut header.encode().as_slice(), &limits(), &mut payload)
        .expect_err("a frame over the geometry-derived cap");

    assert!(matches!(
        error,
        RecordError::TooLarge {
            channel: Channel::Frame,
            ..
        }
    ));
}

#[test]
fn a_control_record_over_its_fixed_cap_is_refused() {
    let mut header = Record::new(
        Channel::Control,
        ControlRecord::Ping as u16,
        0,
        0,
        0,
        Vec::new(),
    )
    .header;
    header.length = 65537;

    let mut payload = Vec::new();
    let error = record::read(&mut header.encode().as_slice(), &limits(), &mut payload)
        .expect_err("a control record over its cap");

    assert!(matches!(
        error,
        RecordError::TooLarge {
            channel: Channel::Control,
            cap: 65536,
            ..
        }
    ));
}

#[test]
fn a_truncated_payload_is_a_transport_fault_not_a_schema_one() {
    let record = Record::new(
        Channel::Control,
        ControlRecord::Ping as u16,
        0,
        0,
        0,
        vec![1, 2, 3, 4],
    );
    let mut wire = record.header.encode().to_vec();
    wire.extend_from_slice(&record.payload[..2]);

    let mut payload = Vec::new();
    let error =
        record::read(&mut wire.as_slice(), &limits(), &mut payload).expect_err("half a payload");

    assert!(matches!(error, RecordError::Io(_)));
}

#[test]
fn a_flipped_bit_in_a_frame_is_caught_by_the_checksum() {
    let record = Record::new(
        Channel::Frame,
        FrameRecord::Keyframe as u16,
        0,
        0,
        0,
        vec![0x5A; 512],
    );
    let mut wire = Vec::new();
    record::write(&mut wire, &record, &limits()).expect("a keyframe within the cap");
    wire[100] ^= 0x01;

    let mut payload = Vec::new();
    let error =
        record::read(&mut wire.as_slice(), &limits(), &mut payload).expect_err("a flipped bit");

    assert!(matches!(error, RecordError::ChecksumMismatch { .. }));
}

#[test]
fn a_hello_from_another_major_is_refused_with_the_version_code() {
    let mut guest = Session::guest(&Secret::generate(), support());

    let hello = Record::new(
        Channel::Control,
        ControlRecord::ClientHello as u16,
        0,
        0,
        0,
        ClientHello {
            version: Some(ProtocolVersion { major: 2, minor: 0 }),
            capabilities: Vec::new(),
            session_id: vec![0x11; SESSION_ID_LEN],
            host_nonce: vec![0x22; 32],
            mode: Mode::Desktop as i32,
            width: 1920,
            height: 1080,
            tile_size: 32,
        }
        .encode_to_vec(),
    );

    let error = guest
        .handle(&hello.header, &hello.payload)
        .expect_err("a major this build has not");

    assert!(matches!(error, SessionError::Version(_)));
    assert_eq!(error.code(), ErrorCode::UnsupportedVersion);
}

#[test]
fn a_nonce_of_the_wrong_width_is_refused_rather_than_padded() {
    let mut guest = Session::guest(&Secret::generate(), support());

    let hello = Record::new(
        Channel::Control,
        ControlRecord::ClientHello as u16,
        0,
        0,
        0,
        ClientHello {
            version: Some(ProtocolVersion::current()),
            capabilities: Vec::new(),
            session_id: vec![0x11; SESSION_ID_LEN],
            host_nonce: vec![0x22; 8],
            mode: Mode::Desktop as i32,
            width: 1920,
            height: 1080,
            tile_size: 32,
        }
        .encode_to_vec(),
    );

    assert!(matches!(
        guest.handle(&hello.header, &hello.payload),
        Err(SessionError::Field(_))
    ));
}

#[test]
fn a_channel_hello_whose_field_disagrees_with_its_header_is_refused() {
    let secret = Secret::generate();
    let (mut host, mut guest) = established(&secret);

    let hello = host
        .open_channel(Channel::Frame)
        .expect("an established session");
    let mut message = ChannelHello::decode(hello.payload.as_slice()).expect("what was built");
    // Says input in the message, frame in the header.
    message.channel = u32::from(Channel::Input.as_wire());
    let forged = Record::new(
        Channel::Frame,
        FrameRecord::ChannelHello as u16,
        0,
        0,
        0,
        message.encode_to_vec(),
    );

    assert!(matches!(
        guest.handle(&forged.header, &forged.payload),
        Err(SessionError::Unexpected { .. })
    ));
}

#[test]
fn a_tag_of_the_wrong_width_never_reaches_a_comparison() {
    let secret = Secret::generate();
    let (mut host, client_hello) = Session::host(&secret, offer());
    let mut guest = Session::guest(&secret, support());

    let server_hello = guest
        .handle(&client_hello.header, &client_hello.payload)
        .expect("a well-formed client hello")
        .reply
        .expect("a server hello");
    host.handle(&server_hello.header, &server_hello.payload)
        .expect("a server hello");

    let short = Record::new(
        Channel::Control,
        ControlRecord::ServerAuth as u16,
        2,
        0,
        0,
        ServerAuth {
            tag: vec![0u8; TAG_LEN - 1],
        }
        .encode_to_vec(),
    );

    assert!(matches!(
        host.handle(&short.header, &short.payload),
        Err(SessionError::Field(_))
    ));
}

#[test]
fn a_header_that_is_all_ones_allocates_nothing() {
    let mut payload = Vec::new();
    let error = record::read(&mut [0xFFu8; 24].as_slice(), &limits(), &mut payload)
        .expect_err("a header of ones");

    // The channel byte is checked before the length is trusted.
    assert!(matches!(error, RecordError::UnknownChannel { value: 0xFF }));
    assert!(payload.is_empty());
}

#[test]
fn a_file_chunk_over_the_clipboard_cap_is_refused_before_it_is_allocated() {
    let mut header = Record::new(
        Channel::Clipboard,
        ClipboardRecord::FileChunk as u16,
        0,
        0,
        0,
        Vec::new(),
    )
    .header;
    header.length = CLIPBOARD_MAX_PAYLOAD + 1;

    let mut payload = Vec::new();
    let error = record::read(&mut header.encode().as_slice(), &limits(), &mut payload)
        .expect_err("a file chunk over the clipboard cap");

    assert!(matches!(
        error,
        RecordError::TooLarge {
            channel: Channel::Clipboard,
            cap: CLIPBOARD_MAX_PAYLOAD,
            ..
        }
    ));
    assert!(payload.is_empty());
}

#[test]
fn an_entry_kind_from_a_newer_peer_is_not_mistaken_for_a_file() {
    let entry = ClipboardFileEntry {
        transfer: 1,
        path: "notes/todo.txt".into(),
        kind: 4242,
        size: 12,
    };

    let decoded =
        ClipboardFileEntry::decode(entry.encode_to_vec().as_slice()).expect("a well-formed entry");

    // The unknown value survives decoding rather than collapsing onto a
    // variant, so the reader refuses it instead of creating the wrong thing.
    assert_eq!(decoded.kind, 4242);
    assert!(FileEntryKind::try_from(decoded.kind).is_err());
}

#[test]
fn a_cancel_reason_from_a_newer_peer_is_not_mapped_onto_a_known_one() {
    let cancel = ClipboardFileCancel {
        transfer: 1,
        reason: 4242,
    };

    let decoded = ClipboardFileCancel::decode(cancel.encode_to_vec().as_slice())
        .expect("a well-formed cancel");

    assert_eq!(decoded.reason, 4242);
    assert!(FileCancelReason::try_from(decoded.reason).is_err());
}

#[test]
fn a_clipboard_record_type_past_the_file_ones_is_not_guessed_at() {
    assert!(ClipboardRecord::try_from(16).is_err());
    for value in 9..=15 {
        assert!(
            ClipboardRecord::try_from(value).is_ok(),
            "the file records are 9 through 15"
        );
    }
}
