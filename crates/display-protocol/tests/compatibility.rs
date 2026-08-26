//! A guest installed months ago still has to talk to today's host.
//!
//! There is no second version of this crate to link, so an older or newer peer
//! is played by hand: the hello it would send, and the answer this build gives
//! it.

use prost::Message;
use vmlord_display_protocol::{
    keys::{SESSION_ID_LEN, Secret},
    record::{Channel, Record},
    session::{Offer, Session, SessionError, Support},
    v1::{
        Capability, ClientHello, ControlRecord, DisplayTiming, Mode, ProtocolVersion, ServerHello,
        SetAvailableModes, SetDisplayMode,
    },
};

fn support() -> Support {
    Support {
        capabilities: vec![Capability::CursorStream, Capability::DynamicResolution],
        modes: vec![Mode::Desktop],
        tile_sizes: vec![32],
        width: 1920,
        height: 1080,
    }
}

fn hello_from(version: ProtocolVersion, capabilities: Vec<i32>) -> Record {
    Record::new(
        Channel::Control,
        ControlRecord::ClientHello as u16,
        0,
        0,
        0,
        ClientHello {
            version: Some(version),
            capabilities,
            session_id: vec![0x11; SESSION_ID_LEN],
            host_nonce: vec![0x22; 32],
            mode: Mode::Desktop as i32,
            width: 1920,
            height: 1080,
            tile_size: 32,
        }
        .encode_to_vec(),
    )
}

/// A build that has the file clipboard, to be answered by peers that may not.
fn file_clipboard_support() -> Support {
    Support {
        capabilities: vec![Capability::Clipboard, Capability::FileClipboard],
        modes: vec![Mode::Desktop],
        tile_sizes: vec![32],
        width: 1920,
        height: 1080,
    }
}

fn host_modes_support() -> Support {
    Support {
        capabilities: vec![Capability::HostDisplayModes],
        modes: vec![Mode::Desktop],
        tile_sizes: vec![32],
        width: 1920,
        height: 1080,
    }
}

#[test]
fn display_timings_and_mode_updates_survive_the_wire() {
    let timing = DisplayTiming {
        width: 2560,
        height: 1440,
        refresh_hz: 144,
    };
    let available = SetAvailableModes {
        modes: vec![timing.clone()],
        preferred: Some(timing.clone()),
    };
    let selected = SetDisplayMode { mode: Some(timing) };

    assert_eq!(
        SetAvailableModes::decode(available.encode_to_vec().as_slice()).unwrap(),
        available
    );
    assert_eq!(
        SetDisplayMode::decode(selected.encode_to_vec().as_slice()).unwrap(),
        selected
    );
}

#[test]
fn a_host_from_before_host_modes_never_settles_them() {
    let hello = hello_from(
        ProtocolVersion { major: 1, minor: 3 },
        vec![i32::from(Capability::HostDisplayModes)],
    );

    let answered = answer(host_modes_support(), &hello);

    assert_eq!(answered.capabilities, Vec::<i32>::new());
}

#[test]
fn this_revision_settles_host_modes() {
    let hello = hello_from(
        ProtocolVersion::current(),
        vec![i32::from(Capability::HostDisplayModes)],
    );

    let answered = answer(host_modes_support(), &hello);

    assert_eq!(
        answered.capabilities,
        vec![i32::from(Capability::HostDisplayModes)]
    );
}

/// What a guest answers a hello with.
fn answer(support: Support, hello: &Record) -> ServerHello {
    let mut guest = Session::guest(&Secret::generate(), support);
    let reply = guest
        .handle(&hello.header, &hello.payload)
        .expect("a well-formed client hello")
        .reply
        .expect("a server hello");

    ServerHello::decode(reply.payload.as_slice()).expect("a server hello")
}

#[test]
fn a_host_from_before_the_file_clipboard_never_settles_one() {
    let hello = hello_from(
        ProtocolVersion { major: 1, minor: 2 },
        vec![
            i32::from(Capability::Clipboard),
            i32::from(Capability::FileClipboard),
        ],
    );

    let answered = answer(file_clipboard_support(), &hello);

    assert_eq!(
        answered.version,
        Some(ProtocolVersion { major: 1, minor: 2 })
    );
    // The old host would not know record types 9 to 15, so nothing may send
    // them: the capability they hang from is gone from what was settled.
    assert_eq!(
        answered.capabilities,
        vec![i32::from(Capability::Clipboard)]
    );
}

#[test]
fn a_host_at_this_revision_settles_the_file_clipboard_beside_the_clipboard() {
    let hello = hello_from(
        ProtocolVersion::current(),
        vec![
            i32::from(Capability::Clipboard),
            i32::from(Capability::FileClipboard),
        ],
    );

    let answered = answer(file_clipboard_support(), &hello);

    assert_eq!(
        answered.capabilities,
        vec![
            i32::from(Capability::Clipboard),
            i32::from(Capability::FileClipboard),
        ]
    );
}

#[test]
fn a_file_clipboard_offered_without_the_clipboard_is_dropped_not_promoted() {
    let hello = hello_from(
        ProtocolVersion::current(),
        vec![i32::from(Capability::FileClipboard)],
    );

    let answered = answer(file_clipboard_support(), &hello);

    assert!(answered.capabilities.is_empty());
}

#[test]
fn a_newer_host_and_this_guest_settle_on_this_builds_minor() {
    let mut guest = Session::guest(&Secret::generate(), support());
    let newer = ProtocolVersion {
        major: ProtocolVersion::current().major,
        minor: ProtocolVersion::current().minor + 3,
    };
    let hello = hello_from(newer, Vec::new());

    let reply = guest
        .handle(&hello.header, &hello.payload)
        .expect("a hello from a newer minor")
        .reply
        .expect("a server hello");

    let answered = ServerHello::decode(reply.payload.as_slice()).expect("a server hello");

    assert_eq!(answered.version, Some(ProtocolVersion::current()));
}

#[test]
fn a_capability_from_a_newer_peer_is_dropped_rather_than_refused() {
    let mut guest = Session::guest(&Secret::generate(), support());
    let hello = hello_from(
        ProtocolVersion::current(),
        vec![i32::from(Capability::CursorStream), 4242],
    );

    let reply = guest
        .handle(&hello.header, &hello.payload)
        .expect("a hello naming a capability this build has never heard of")
        .reply
        .expect("a server hello");

    let answered = ServerHello::decode(reply.payload.as_slice()).expect("a server hello");

    assert_eq!(
        answered.capabilities,
        vec![i32::from(Capability::CursorStream)]
    );
}

#[test]
fn a_guest_that_agreed_on_something_the_host_never_offered_is_refused() {
    let secret = Secret::generate();
    let (mut host, _) = Session::host(
        &secret,
        Offer {
            capabilities: vec![Capability::CursorStream],
            mode: Mode::Desktop,
            width: 1920,
            height: 1080,
            tile_size: 32,
        },
    );

    let overreaching = Record::new(
        Channel::Control,
        ControlRecord::ServerHello as u16,
        0,
        0,
        0,
        ServerHello {
            version: Some(ProtocolVersion::current()),
            capabilities: vec![i32::from(Capability::DynamicResolution)],
            guest_nonce: vec![0x33; 32],
            modes: vec![Mode::Desktop as i32],
            tile_sizes: vec![32],
            width: 1920,
            height: 1080,
        }
        .encode_to_vec(),
    );

    assert!(matches!(
        host.handle(&overreaching.header, &overreaching.payload),
        Err(SessionError::Capability(_))
    ));
}

#[test]
fn a_record_type_from_a_newer_minor_is_refused_rather_than_guessed_at() {
    let mut guest = Session::guest(&Secret::generate(), support());
    let unknown = Record::new(Channel::Control, 4242, 0, 0, 0, Vec::new());

    assert!(matches!(
        guest.handle(&unknown.header, &unknown.payload),
        Err(SessionError::Unexpected {
            message_type: 4242,
            ..
        })
    ));
}
