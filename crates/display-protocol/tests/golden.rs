//! The bytes this build puts on the wire, held still.
//!
//! A golden vector is the only test that fails when a change to the format is
//! correct in Rust and wrong on the wire -- a renumbered field, a reordered
//! transcript, a header field that moved. The guest and the host of a VMLord
//! release are upgraded separately, so the wire is where compatibility lives.
//!
//! To refresh after an intentional format change -- which is a major or minor
//! version bump, never a silent edit:
//!
//! ```text
//! VMLORD_REFRESH_GOLDEN=1 cargo test -p vmlord-display-protocol --test golden
//! ```

use std::{
    env, fs,
    path::{Path, PathBuf},
};

use prost::Message;
use vmlord_display_protocol::{
    keys::{NONCE_LEN, SESSION_ID_LEN, Secret},
    record::{self, Channel, Limits, Record},
    session::{Offer, Session, Support},
    v1::{
        AudioFormat, AudioRecord, Capability, ControlRecord, DisplayTiming, FrameRecord,
        InputRecord, KeyEvent, Mode, PixelFormat, PointerMotion, SampleFormat, SetAvailableModes,
        SetDisplayMode, StreamConfig,
    },
};

/// A secret nobody holds, so that these bytes may live in a public tree.
const SECRET: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";
const SESSION_ID: [u8; SESSION_ID_LEN] = [0x11; SESSION_ID_LEN];
const HOST_NONCE: [u8; NONCE_LEN] = [0x22; NONCE_LEN];
const GUEST_NONCE: [u8; NONCE_LEN] = [0x33; NONCE_LEN];

fn golden(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name)
}

fn hold(name: &str, produced: &[u8]) {
    let path = golden(name);

    if env::var_os("VMLORD_REFRESH_GOLDEN").is_some() {
        fs::create_dir_all(path.parent().expect("a parent"))
            .expect("failed to create tests/golden");
        fs::write(&path, produced).expect("failed to refresh a golden vector");
        return;
    }

    let held = fs::read(&path).expect("failed to read a golden vector");
    assert_eq!(
        held, produced,
        "the wire format changed; if that was intended, bump the protocol version and refresh with \
         VMLORD_REFRESH_GOLDEN=1 cargo test -p vmlord-display-protocol --test golden"
    );
}

fn offer() -> Offer {
    Offer {
        capabilities: vec![Capability::CursorStream, Capability::DynamicResolution],
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
        tile_sizes: vec![16, 32, 64],
        width: 1920,
        height: 1080,
    }
}

#[test]
fn the_handshake_is_the_bytes_it_has_always_been() {
    let secret = Secret::from_base64(SECRET).expect("a fixed secret");
    let limits = Limits::new(1920, 1080);

    let (mut host, client_hello) =
        Session::host_with_randomness(&secret, offer(), SESSION_ID, HOST_NONCE);
    let mut guest = Session::guest_with_randomness(&secret, support(), GUEST_NONCE);

    let mut wire = Vec::new();
    record::write(&mut wire, &client_hello, &limits).expect("a client hello");

    let server_hello = guest
        .handle(&client_hello.header, &client_hello.payload)
        .expect("a well-formed client hello")
        .reply
        .expect("a server hello");
    record::write(&mut wire, &server_hello, &limits).expect("a server hello");

    let server_auth = guest.pending_auth().expect("the guest's proof");
    record::write(&mut wire, &server_auth, &limits).expect("a server auth");

    host.handle(&server_hello.header, &server_hello.payload)
        .expect("a well-formed server hello");
    let client_auth = host
        .handle(&server_auth.header, &server_auth.payload)
        .expect("a valid guest proof")
        .reply
        .expect("the host's proof");
    record::write(&mut wire, &client_auth, &limits).expect("a client auth");

    hold("handshake.bin", &wire);
}

#[test]
fn one_record_of_each_carrying_type_is_the_bytes_it_has_always_been() {
    let limits = Limits::new(1920, 1080);
    let mut wire = Vec::new();

    let timing = DisplayTiming {
        width: 2560,
        height: 1440,
        refresh_hz: 144,
    };
    let available = Record::new(
        Channel::Control,
        ControlRecord::SetAvailableModes as u16,
        0,
        0,
        0,
        SetAvailableModes {
            modes: vec![timing],
            preferred: Some(timing),
        }
        .encode_to_vec(),
    );
    record::write(&mut wire, &available, &limits).expect("available display modes");

    let selected = Record::new(
        Channel::Control,
        ControlRecord::SetDisplayMode as u16,
        1,
        0,
        0,
        SetDisplayMode { mode: Some(timing) }.encode_to_vec(),
    );
    record::write(&mut wire, &selected, &limits).expect("a selected display mode");

    let stream_config = Record::new(
        Channel::Frame,
        FrameRecord::StreamConfig as u16,
        0,
        0,
        0,
        StreamConfig {
            width: 1920,
            height: 1080,
            tile_size: 32,
            pixel_format: PixelFormat::Bgra8888 as i32,
        }
        .encode_to_vec(),
    );
    record::write(&mut wire, &stream_config, &limits).expect("a stream config");

    let keyframe = Record::new(
        Channel::Frame,
        FrameRecord::Keyframe as u16,
        1,
        0,
        0,
        vec![0xAB; 64],
    );
    record::write(&mut wire, &keyframe, &limits).expect("a keyframe");

    let delta = Record::new(
        Channel::Frame,
        FrameRecord::TileDelta as u16,
        2,
        1,
        0,
        vec![0xCD; 32],
    );
    record::write(&mut wire, &delta, &limits).expect("a tile delta");

    let key = Record::new(
        Channel::Input,
        InputRecord::KeyEvent as u16,
        0,
        0,
        0,
        KeyEvent {
            keycode: 30,
            pressed: true,
        }
        .encode_to_vec(),
    );
    record::write(&mut wire, &key, &limits).expect("a key event");

    let motion = Record::new(
        Channel::Input,
        InputRecord::PointerMotion as u16,
        1,
        0,
        0,
        PointerMotion { x: 640, y: 480 }.encode_to_vec(),
    );
    record::write(&mut wire, &motion, &limits).expect("a pointer motion");

    let format = Record::new(
        Channel::Audio,
        AudioRecord::Format as u16,
        0,
        0,
        0,
        AudioFormat {
            sample_rate: 48_000,
            channels: 2,
            sample_format: SampleFormat::S16Le as i32,
            frames_per_period: 480,
        }
        .encode_to_vec(),
    );
    record::write(&mut wire, &format, &limits).expect("an audio format");

    // `base` is the stream position: the frames captured before this period.
    // It is held here because a gap the host reads out of it is the whole of
    // how silence and dropped periods are reported.
    let period = Record::new(
        Channel::Audio,
        AudioRecord::Data as u16,
        1,
        480,
        0,
        vec![0xEF; 1920],
    );
    record::write(&mut wire, &period, &limits).expect("an audio period");

    hold("records.bin", &wire);
}
