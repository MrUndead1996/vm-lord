//! The guest's clipboard daemon.
//!
//! It lives in the user's graphical session, because a selection does. What it
//! owns is one vsock socket and one Mutter session; what it decides is nothing
//! -- every rule about what may cross, how large it may be and when a transfer
//! ends is [`vmlord_display_protocol::clipboard`], which the host runs too.
//!
//! It holds no secret. The broker does the control handshake and sends one
//! channel key over `/run/vmlord/display-clipboard.sock`, which is worth one
//! session's clipboard and nothing else -- not a picture, not a keyboard.
//!
//! Nothing it writes to the journal carries a byte of a selection: a mime type,
//! a byte count and an outcome are what a clipboard problem is diagnosed from,
//! and the contents are the one thing that must never be diagnosed from.

use std::{
    collections::VecDeque,
    env,
    io::{Read, Write},
    path::PathBuf,
    process::ExitCode,
    sync::mpsc::TryRecvError,
    time::{Duration, Instant},
};

use prost::Message as _;
use vmlord_display_protocol::{
    clipboard::{Exchange, Kind, Message as Outgoing, Op, Piece},
    keys::ChannelKey,
    record::{self, Channel, Header, Limits, Record, RecordError},
    v1::{
        CancelReason, ClipboardCancel, ClipboardData, ClipboardOffer, ClipboardRecord,
        ClipboardRequest,
    },
};

use crate::{
    channel::{self, BindError},
    ipc::Message,
    mutter::{self, Clipboard},
    unix::Connection,
    vsock::{self, CLIPBOARD_PORT},
};

/// Where the broker offers the clipboard channel.
const BROKER_SOCKET: &str = "/run/vmlord/display-clipboard.sock";

/// How long to wait before looking for the broker again.
///
/// The daemon starts with a graphical session, which is usually minutes after
/// the broker -- but it may also start before one, on a guest whose services
/// are being installed while somebody is already logged in.
const RETRY: Duration = Duration::from_secs(2);

/// How long a read waits before the loop does its other work.
const PATIENCE: Duration = Duration::from_millis(200);

/// What the daemon was started with.
pub struct Options {
    /// The socket the broker offers the clipboard channel on.
    pub broker_socket: PathBuf,
}

impl Options {
    /// The defaults, with the environment allowed to override each one.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            broker_socket: env::var("VMLORD_DISPLAY_CLIPBOARD_SOCKET")
                .unwrap_or_else(|_| BROKER_SOCKET.to_owned())
                .into(),
        }
    }
}

/// Runs the daemon until it cannot bind its socket.
///
/// Everything short of that is waited through rather than exited over: no
/// broker yet, nobody logged in, no session open. A daemon that exited on any
/// of those would spend its restart budget on the ordinary state of a guest
/// that is still booting.
#[must_use]
pub fn run(options: Options) -> ExitCode {
    let listener = match vsock::Listener::bind(CLIPBOARD_PORT) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("vmlord-display-clipboard: this guest has no vsock: {error}");

            return ExitCode::FAILURE;
        }
    };

    // The generation a channel was last bound at, and the session it belonged
    // to. Scoped to the session on purpose: the host counts generations inside
    // one session and starts a new one from zero, so a bare number carried
    // across sessions would refuse every channel of every session after the
    // first.
    let mut last_bound: Option<(Vec<u8>, u32)> = None;
    loop {
        let Some(broker) = wait_for_broker(&options.broker_socket) else {
            continue;
        };

        // One session's clipboard, then back for the next one. A viewer that
        // reconnects is a new session with a new key.
        match serve_session(&broker, &listener, &mut last_bound) {
            Ok(()) => {}
            Err(reason) => eprintln!("vmlord-display-clipboard: {reason}"),
        }
    }
}

/// Connects to the broker and waits for a session to be open.
fn wait_for_broker(path: &std::path::Path) -> Option<Connection> {
    let connection = match Connection::connect(path) {
        Ok(connection) => connection,
        Err(_) => {
            std::thread::sleep(RETRY);

            return None;
        }
    };
    if connection.send(&Message::Attach, &[]).is_err() {
        std::thread::sleep(RETRY);

        return None;
    }

    Some(connection)
}

/// Serves one session's clipboard, from the broker's key to a lost socket.
fn serve_session(
    broker: &Connection,
    listener: &vsock::Listener,
    last_bound: &mut Option<(Vec<u8>, u32)>,
) -> Result<(), String> {
    let (session_id, key) = loop {
        let (message, _) = broker
            .receive()
            .map_err(|error| format!("the broker went away: {error}"))?;

        match message {
            Message::ClipboardOpened {
                session_id,
                clipboard_key,
            } => {
                let key: [u8; 32] = clipboard_key
                    .as_slice()
                    .try_into()
                    .map_err(|_| "the broker sent a key of the wrong width".to_owned())?;

                break (session_id, ChannelKey::from_bytes(key));
            }
            // Everything else on this socket is about a session this daemon
            // has no part in.
            _ => continue,
        }
    };

    let mut stream = listener
        .accept()
        .map_err(|error| format!("the clipboard socket could not be accepted: {error}"))?;
    let generation = channel::bind(
        &mut stream,
        Channel::Clipboard,
        &key,
        &session_id,
        guard(last_bound.as_ref(), &session_id),
    )
    .map_err(|error: BindError| format!("the clipboard channel did not bind: {error}"))?;
    *last_bound = Some((session_id.clone(), generation));
    eprintln!("vmlord-display-clipboard: the clipboard channel bound at generation {generation}");

    stream
        .set_read_timeout(PATIENCE)
        .map_err(|error| format!("the clipboard socket refused a timeout: {error}"))?;

    pump(&mut stream, generation)
}

/// The generation a hello has to climb past, for this session.
///
/// `None` for a session this daemon has not bound a channel of yet -- which
/// includes every session after the first. The host counts generations inside
/// one session and starts the next from zero, so carrying a bare number across
/// sessions would refuse the first channel of every session but the first, for
/// as long as the daemon ran.
fn guard(last_bound: Option<&(Vec<u8>, u32)>, session_id: &[u8]) -> Option<u32> {
    last_bound
        .filter(|(bound, _)| bound == session_id)
        .map(|(_, generation)| *generation)
}

/// The loop: records in, compositor events in, records out.
fn pump<S: Read + Write>(stream: &mut S, generation: u32) -> Result<(), String> {
    let limits = Limits::new(0, 0);
    let mut exchange = Exchange::new();
    let mut sequence = 0u32;
    let mut payload = Vec::new();
    // What the host's last selection produced, to answer the compositor's
    // transfer requests from. Never logged.
    let mut held: Vec<Piece> = Vec::new();

    // The compositor may not be reachable yet -- this daemon can outlive a
    // logout -- so it is opened lazily and reopened when it closes.
    let mut clipboard: Option<(Clipboard, std::sync::mpsc::Receiver<mutter::Event>)> = None;
    let mut next_open = Instant::now();

    loop {
        let now = Instant::now();
        let mut ops = Vec::new();

        if clipboard.is_none() && now >= next_open {
            next_open = now + RETRY;
            match Clipboard::open().and_then(|(clipboard, events)| {
                // A listener, never an owner, until the host actually sends a
                // selection: Mutter refuses to read a selection its caller owns.
                clipboard.listen()?;
                Ok((clipboard, events))
            }) {
                Ok(opened) => {
                    eprintln!("vmlord-display-clipboard: attached to the desktop's clipboard");
                    clipboard = Some(opened);
                }
                Err(error) => {
                    // Ordinary while nobody is logged in, so it is not repeated
                    // every two seconds at a level anybody reads.
                    eprintln!("vmlord-display-clipboard: no desktop clipboard yet ({error})");
                }
            }
        }

        match record::read(stream, &limits, &mut payload) {
            Ok(header) => {
                if header.generation == generation {
                    ops.extend(handle(&mut exchange, &header, &payload, now));
                }
            }
            Err(RecordError::Idle) => {}
            Err(error) => return Err(format!("the clipboard channel ended: {error}")),
        }

        if let Some((mutter_clipboard, events)) = clipboard.as_ref() {
            loop {
                match events.try_recv() {
                    Ok(mutter::Event::PeerOffer { kinds }) => {
                        // The guest's selection is the local one from here.
                        ops.extend(exchange.local_offer(&kinds, now));
                    }
                    Ok(mutter::Event::Transfer { kind, serial }) => {
                        answer_transfer(mutter_clipboard, &held, kind, serial);
                    }
                    Ok(mutter::Event::Closed) | Err(TryRecvError::Disconnected) => {
                        eprintln!("vmlord-display-clipboard: the desktop's clipboard went away");
                        clipboard = None;
                        held.clear();
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                }
            }
        }

        ops.extend(exchange.tick(now));

        // A queue rather than a list: producing a selection appends the chunks
        // that carry it, and those have to follow what is already waiting.
        let mut queue: VecDeque<Op> = ops.into();
        while let Some(op) = queue.pop_front() {
            match op {
                Op::Send(message) => {
                    let record = record_of(&message, sequence, generation);
                    sequence = sequence.wrapping_add(1);
                    if let Err(error) = record::write(stream, &record, &limits) {
                        return Err(format!(
                            "the clipboard channel could not be written: {error}"
                        ));
                    }
                }
                Op::Produce { kind, transfer } => {
                    let produced = clipboard
                        .as_ref()
                        .ok_or(mutter::MutterError::Idle)
                        .and_then(|(clipboard, _)| clipboard.read(kind, kind.cap()));

                    match produced {
                        Ok(bytes) => {
                            eprintln!(
                                "vmlord-display-clipboard: sending {} bytes of {}",
                                bytes.len(),
                                kind.mime()
                            );
                            queue.extend(exchange.produced(transfer, bytes, Instant::now()));
                        }
                        Err(error) => {
                            eprintln!(
                                "vmlord-display-clipboard: the desktop would not produce {}: {error}",
                                kind.mime()
                            );
                            queue.extend(exchange.unavailable(transfer));
                        }
                    }
                }
                Op::Apply { pieces } => {
                    let kinds: Vec<Kind> = pieces.iter().map(|piece| piece.kind).collect();
                    eprintln!(
                        "vmlord-display-clipboard: taking the selection with {} format(s)",
                        kinds.len()
                    );
                    held = pieces;
                    if let Some((clipboard, _)) = clipboard.as_ref()
                        && let Err(error) = clipboard.own(&kinds)
                    {
                        eprintln!(
                            "vmlord-display-clipboard: the desktop refused the selection: {error}"
                        );
                        held.clear();
                    }
                }
            }
        }
    }
}

/// Answers the compositor's request for the selection this side owns.
fn answer_transfer(clipboard: &Clipboard, held: &[Piece], kind: Kind, serial: u32) {
    let Some(piece) = held.iter().find(|piece| piece.kind == kind) else {
        let _ = clipboard.refuse(serial);

        return;
    };

    if let Err(error) = clipboard.write(serial, &piece.bytes) {
        eprintln!(
            "vmlord-display-clipboard: {} could not be handed to the desktop: {error}",
            kind.mime()
        );
    }
}

/// What one record off the channel means to the exchange.
fn handle(exchange: &mut Exchange, header: &Header, payload: &[u8], now: Instant) -> Vec<Op> {
    match parse(header, payload) {
        Some(Incoming::Offer { serial, mime_types }) => {
            exchange.peer_offer(serial, &mime_types, now)
        }
        Some(Incoming::Request {
            serial,
            mime_type,
            transfer,
        }) => exchange.peer_request(serial, &mime_type, transfer, now),
        Some(Incoming::Data {
            transfer,
            chunk,
            last,
        }) => exchange.peer_data(transfer, &chunk, last, now),
        Some(Incoming::Cancel { transfer, reason }) => exchange.peer_cancel(transfer, reason),
        None => Vec::new(),
    }
}

/// One record off the clipboard channel, decoded.
#[derive(Debug, PartialEq, Eq)]
enum Incoming {
    Offer {
        serial: u32,
        mime_types: Vec<String>,
    },
    Request {
        serial: u32,
        mime_type: String,
        transfer: u32,
    },
    Data {
        transfer: u32,
        chunk: Vec<u8>,
        last: bool,
    },
    Cancel {
        transfer: u32,
        reason: CancelReason,
    },
}

/// Reads a record, or nothing at all for one this build has no name for.
fn parse(header: &Header, payload: &[u8]) -> Option<Incoming> {
    match ClipboardRecord::try_from(i32::from(header.message_type)).ok()? {
        ClipboardRecord::Offer => {
            let offer = ClipboardOffer::decode(payload).ok()?;

            Some(Incoming::Offer {
                serial: offer.serial,
                mime_types: offer.mime_types,
            })
        }
        ClipboardRecord::Request => {
            let request = ClipboardRequest::decode(payload).ok()?;

            Some(Incoming::Request {
                serial: request.serial,
                mime_type: request.mime_type,
                transfer: request.transfer,
            })
        }
        ClipboardRecord::Data => {
            let data = ClipboardData::decode(payload).ok()?;

            Some(Incoming::Data {
                transfer: data.transfer,
                chunk: data.chunk,
                last: data.last,
            })
        }
        ClipboardRecord::Cancel => {
            let cancel = ClipboardCancel::decode(payload).ok()?;

            Some(Incoming::Cancel {
                transfer: cancel.transfer,
                reason: CancelReason::try_from(cancel.reason).unwrap_or_default(),
            })
        }
        _ => None,
    }
}

/// Wraps one message as the record that carries it.
fn record_of(message: &Outgoing, sequence: u32, generation: u32) -> Record {
    let (message_type, payload) = match message {
        Outgoing::Offer { serial, mime_types } => (
            ClipboardRecord::Offer,
            ClipboardOffer {
                serial: *serial,
                mime_types: mime_types.iter().map(|mime| (*mime).to_owned()).collect(),
            }
            .encode_to_vec(),
        ),
        Outgoing::Request {
            serial,
            mime_type,
            transfer,
        } => (
            ClipboardRecord::Request,
            ClipboardRequest {
                serial: *serial,
                mime_type: (*mime_type).to_owned(),
                transfer: *transfer,
            }
            .encode_to_vec(),
        ),
        Outgoing::Data {
            transfer,
            chunk,
            last,
        } => (
            ClipboardRecord::Data,
            ClipboardData {
                transfer: *transfer,
                chunk: chunk.clone(),
                last: *last,
            }
            .encode_to_vec(),
        ),
        Outgoing::Cancel { transfer, reason } => (
            ClipboardRecord::Cancel,
            ClipboardCancel {
                transfer: *transfer,
                reason: i32::from(*reason),
            }
            .encode_to_vec(),
        ),
    };

    Record::new(
        Channel::Clipboard,
        message_type as u16,
        sequence,
        0,
        generation,
        payload,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generation_is_only_held_against_the_session_it_was_bound_in() {
        let first = vec![1u8; 16];
        let second = vec![2u8; 16];

        assert_eq!(guard(None, &first), None, "nothing bound yet holds nothing");
        assert_eq!(
            guard(Some(&(first.clone(), 3)), &first),
            Some(3),
            "a reconnect inside one session has to climb past what it used"
        );
        assert_eq!(
            guard(Some(&(first, 3)), &second),
            None,
            "a new session counts from zero, so the old number cannot refuse it"
        );
    }

    #[test]
    fn an_op_becomes_the_record_its_type_names() {
        let record = record_of(
            &Outgoing::Offer {
                serial: 3,
                mime_types: vec![Kind::Text.mime()],
            },
            0,
            7,
        );

        assert_eq!(record.header.channel, Channel::Clipboard);
        assert_eq!(record.header.message_type, ClipboardRecord::Offer as u16);
        assert_eq!(record.header.generation, 7);
    }

    #[test]
    fn a_record_becomes_the_call_its_type_names() {
        let record = record_of(
            &Outgoing::Offer {
                serial: 4,
                mime_types: vec![Kind::Text.mime()],
            },
            0,
            0,
        );

        let parsed = parse(&record.header, &record.payload).expect("a clipboard record");

        assert_eq!(
            parsed,
            Incoming::Offer {
                serial: 4,
                mime_types: vec![Kind::Text.mime().to_owned()],
            }
        );
    }

    #[test]
    fn every_message_survives_the_wire() {
        let messages = [
            Outgoing::Request {
                serial: 1,
                mime_type: Kind::Html.mime(),
                transfer: 2,
            },
            Outgoing::Data {
                transfer: 2,
                chunk: b"bytes".to_vec(),
                last: true,
            },
            Outgoing::Cancel {
                transfer: 2,
                reason: CancelReason::TooLarge,
            },
        ];

        for message in messages {
            let record = record_of(&message, 0, 0);
            assert!(
                parse(&record.header, &record.payload).is_some(),
                "{message:?} came back as nothing"
            );
        }
    }

    #[test]
    fn a_record_this_build_has_no_name_for_is_ignored() {
        let record = Record::new(Channel::Clipboard, 4242, 0, 0, 0, Vec::new());

        assert_eq!(parse(&record.header, &record.payload), None);
    }
}
