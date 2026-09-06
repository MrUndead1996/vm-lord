//! The guest's clipboard daemon.
//!
//! It lives in the user's graphical session, because a selection does. What it
//! owns is one vsock socket and one compositor clipboard; what it decides is
//! nothing -- every rule about what may cross, how large it may be and when a
//! transfer ends is [`vmlord_display_protocol::clipboard`], which the host runs
//! too.
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
    clipboard::{
        Exchange, Kind, Message as Outgoing, Op, Piece,
        files::{self, EntryKind, Message as FileOutgoing, Op as FileOp, Policy},
    },
    keys::ChannelKey,
    record::{self, CLIPBOARD_MAX_PAYLOAD, Channel, Header, Limits, Record, RecordError},
    v1::{
        CancelReason, ClipboardCancel, ClipboardData, ClipboardFileCancel, ClipboardFileChunk,
        ClipboardFileComplete, ClipboardFileEntry, ClipboardFileOffer, ClipboardFilePolicy,
        ClipboardFileRequest, ClipboardOffer, ClipboardRecord, ClipboardRequest, FileCancelReason,
    },
};

use crate::{
    channel::{self, BindError},
    clipboard_files::{Produced, SourceTree, Staging, parse_uri_list, uri_lists},
    guest_clipboard::{
        ClipboardError, Desktop, Event, GNOME_COPIED_MIME, GuestClipboard, URI_LIST_MIME,
    },
    ipc::Message,
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
    let listener = match wait_for_port() {
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

/// Takes the clipboard port, waiting for whoever holds it to let go.
///
/// The unit is enabled for every user, so the greeter's own graphical session
/// runs one of these too, and for a few seconds after a login there are two.
/// Exiting on that would spend the restart budget on the ordinary shape of
/// logging in; waiting costs nothing, because a daemon that is not serving the
/// clipboard has nothing else to do. Anything other than a taken port is a
/// guest this cannot run on, and that is worth exiting over.
///
/// # Errors
///
/// The bind error, for every reason except the port already being held.
fn wait_for_port() -> std::io::Result<vsock::Listener> {
    loop {
        match vsock::Listener::bind(CLIPBOARD_PORT) {
            Ok(listener) => return Ok(listener),
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                std::thread::sleep(RETRY);
            }
            Err(error) => return Err(error),
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

    // Which implementation of the seam serves this session is `Desktop`'s
    // question, and it is asked again on every reopen: a daemon outlives a
    // logout, and the desktop that comes back need not be the one that left.
    pump::<_, Desktop>(&mut stream, generation, &session_token(&session_id))
}

/// The name a session's staging directory is made under.
///
/// The session id and nothing else: it is already unpredictable, and a
/// directory named after it is one a second session never lands in.
fn session_token(session_id: &[u8]) -> String {
    session_id
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
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
fn pump<S: Read + Write, C: GuestClipboard>(
    stream: &mut S,
    generation: u32,
    session: &str,
) -> Result<(), String> {
    let limits = Limits::new(0, 0);
    let mut exchange = Exchange::new();
    let mut files = files::Exchange::new(Policy::default(), Instant::now());
    let mut sequence = 0u32;
    let mut payload = Vec::new();
    // What the host's last selection produced, to answer the compositor's
    // transfer requests from. Never logged.
    let mut held: Vec<Piece> = Vec::new();
    // The tree being read out of this guest, and the transfer it answers.
    let mut source: Option<(u32, SourceTree)> = None;
    // The tree arriving from the host. Dropping it removes what it staged, so
    // a lost socket or a returning `?` leaves nothing behind.
    let mut staging: Option<Staging> = None;
    // The top-level paths of the last tree that arrived whole, which is what a
    // paste in the guest is answered from. Never logged.
    let mut held_files: Vec<PathBuf> = Vec::new();

    // The compositor may not be reachable yet -- this daemon can outlive a
    // logout -- so it is opened lazily and reopened when it closes.
    let mut clipboard: Option<(C, std::sync::mpsc::Receiver<Event>)> = None;
    let mut next_open = Instant::now();

    loop {
        let now = Instant::now();
        let mut ops = Vec::new();

        if clipboard.is_none() && now >= next_open {
            next_open = now + RETRY;
            match C::open().and_then(|(clipboard, events)| {
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

        let mut file_ops = Vec::new();

        match record::read(stream, &limits, &mut payload) {
            Ok(header) => {
                if header.generation == generation {
                    ops.extend(handle(&mut exchange, &header, &payload, now));
                    file_ops.extend(handle_file(&mut files, &header, &payload, now));
                }
            }
            Err(RecordError::Idle) => {}
            Err(error) => return Err(format!("the clipboard channel ended: {error}")),
        }

        if let Some((compositor, events)) = clipboard.as_ref() {
            loop {
                match events.try_recv() {
                    Ok(Event::PeerOffer {
                        kinds,
                        files: has_files,
                    }) => {
                        // The guest's selection is the local one from here.
                        // The kinds and never the bytes: this is what tells
                        // somebody whether a copy in the guest was seen at all.
                        eprintln!(
                            "vmlord-display-clipboard: the desktop offers {}",
                            kinds
                                .iter()
                                .map(|kind| kind.mime())
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                        ops.extend(exchange.local_offer(&kinds, now));
                        // Only to a host that has said what its limits are: a
                        // viewer without the capability has no name for a file
                        // record and would end the session over one.
                        if has_files && files.heard_policy() {
                            eprintln!("vmlord-display-clipboard: the desktop offers files");
                            file_ops.extend(files.local_offer(now));
                        }
                    }
                    Ok(Event::Transfer { kind, serial }) => {
                        answer_transfer(compositor, &held, kind, serial);
                    }
                    Ok(Event::TransferFiles { mime, serial }) => {
                        answer_files(compositor, &held_files, &mime, serial);
                    }
                    Ok(Event::Closed) | Err(TryRecvError::Disconnected) => {
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
        file_ops.extend(files.tick(now));

        // One entry or one chunk per turn round the loop, so that a directory
        // of a thousand files cannot hold the socket, the compositor's events
        // or the ordinary clipboard behind it.
        if let Some((transfer, tree)) = source.as_mut() {
            let transfer = *transfer;
            match tree.next() {
                Ok(Some(Produced::Entry { path, kind, size })) => {
                    file_ops.extend(files.produced_entry(transfer, &path, kind, size, now));
                }
                Ok(Some(Produced::Chunk(bytes))) => {
                    file_ops.extend(files.produced_chunk(transfer, bytes, now));
                }
                Ok(None) => {
                    source = None;
                    file_ops.extend(files.produced_complete(transfer, now));
                }
                Err(error) => {
                    // The name of what failed, never the name of the file.
                    eprintln!("vmlord-display-clipboard: the tree could not be read: {error}");
                    source = None;
                    file_ops.extend(files.produced_failed(transfer, FileCancelReason::IoFailed));
                }
            }
        }

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
                        .ok_or(ClipboardError::Idle)
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
                        && let Err(error) = clipboard.own(&kinds, !held_files.is_empty())
                    {
                        eprintln!(
                            "vmlord-display-clipboard: the desktop refused the selection: {error}"
                        );
                        held.clear();
                    }
                }
            }
        }

        let mut file_queue: VecDeque<FileOp> = file_ops.into();
        while let Some(op) = file_queue.pop_front() {
            match op {
                FileOp::Send(message) => {
                    let record = record_of_file(&message, sequence, generation);
                    sequence = sequence.wrapping_add(1);
                    if let Err(error) = record::write(stream, &record, &limits) {
                        return Err(format!(
                            "the clipboard channel could not be written: {error}"
                        ));
                    }
                }
                FileOp::Enumerate { transfer } => {
                    let opened = clipboard
                        .as_ref()
                        .ok_or_else(|| "no desktop clipboard".to_owned())
                        .and_then(|(clipboard, _)| {
                            clipboard
                                .read_mime(URI_LIST_MIME, CLIPBOARD_MAX_PAYLOAD as usize)
                                .map_err(|error| error.to_string())
                        })
                        .and_then(|list| parse_uri_list(&list).map_err(|error| error.to_string()))
                        .and_then(|paths| {
                            SourceTree::open(&paths, files.policy())
                                .map_err(|error| error.to_string())
                        });

                    match opened {
                        Ok(tree) => source = Some((transfer, tree)),
                        Err(reason) => {
                            eprintln!(
                                "vmlord-display-clipboard: the desktop's files could not be read: {reason}"
                            );
                            file_queue.extend(
                                files.produced_failed(transfer, FileCancelReason::Unavailable),
                            );
                        }
                    }
                }
                FileOp::CreateEntry {
                    transfer,
                    path,
                    kind,
                    size,
                } => {
                    let staged = match staging.as_mut() {
                        Some(staging) => staging.create_entry(&path, kind, size),
                        None => Staging::create(session, transfer).and_then(|mut fresh| {
                            let created = fresh.create_entry(&path, kind, size);
                            staging = Some(fresh);
                            created
                        }),
                    };

                    if let Err(error) = staged {
                        eprintln!("vmlord-display-clipboard: staging failed: {error}");
                        staging = None;
                        file_queue
                            .extend(files.staging_failed(transfer, FileCancelReason::IoFailed));
                    }
                }
                FileOp::WriteChunk { transfer, bytes } => {
                    let written = staging
                        .as_mut()
                        .ok_or_else(|| "nothing is being staged".to_owned())
                        .and_then(|staging| {
                            staging
                                .write_chunk(&bytes)
                                .map_err(|error| error.to_string())
                        });

                    if let Err(reason) = written {
                        eprintln!("vmlord-display-clipboard: staging failed: {reason}");
                        staging = None;
                        file_queue
                            .extend(files.staging_failed(transfer, FileCancelReason::IoFailed));
                    }
                }
                FileOp::Commit { transfer } => match staging.take().map(Staging::commit) {
                    Some(Ok(paths)) => {
                        eprintln!(
                            "vmlord-display-clipboard: taking a selection of {} file(s)",
                            paths.len()
                        );
                        held_files = paths;
                        let kinds: Vec<Kind> = held.iter().map(|piece| piece.kind).collect();
                        if let Some((clipboard, _)) = clipboard.as_ref()
                            && let Err(error) = clipboard.own(&kinds, true)
                        {
                            eprintln!(
                                "vmlord-display-clipboard: the desktop refused the files: {error}"
                            );
                            held_files.clear();
                        }
                    }
                    Some(Err(error)) => {
                        eprintln!("vmlord-display-clipboard: staging failed: {error}");
                        file_queue
                            .extend(files.staging_failed(transfer, FileCancelReason::IoFailed));
                    }
                    None => {}
                },
                FileOp::Abort { .. } => {
                    // Dropping it removes the whole partial tree.
                    staging = None;
                }
            }
        }
    }
}

/// Answers the compositor's request for the files this side owns.
fn answer_files(clipboard: &impl GuestClipboard, held: &[PathBuf], mime: &str, serial: u32) {
    if held.is_empty() {
        let _ = clipboard.refuse(serial);

        return;
    }

    let payloads = uri_lists(held);
    let bytes = if mime == GNOME_COPIED_MIME {
        payloads.gnome_copied
    } else {
        payloads.uri_list
    };

    if let Err(error) = clipboard.write(serial, &bytes) {
        eprintln!(
            "vmlord-display-clipboard: {} could not be handed to the desktop: {error}",
            mime
        );
    }
}

/// Answers the compositor's request for the selection this side owns.
fn answer_transfer(clipboard: &impl GuestClipboard, held: &[Piece], kind: Kind, serial: u32) {
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

/// What one file record off the channel means to the file exchange.
fn handle_file(
    exchange: &mut files::Exchange,
    header: &Header,
    payload: &[u8],
    now: Instant,
) -> Vec<FileOp> {
    match parse_file(header, payload) {
        Some(FileOutgoing::Policy(policy)) => {
            exchange.peer_policy(policy);

            Vec::new()
        }
        Some(FileOutgoing::Offer { serial }) => exchange.peer_offer(serial, now),
        Some(FileOutgoing::Request { serial, transfer }) => {
            exchange.peer_request(serial, transfer, now)
        }
        Some(FileOutgoing::Entry {
            transfer,
            path,
            kind,
            size,
        }) => exchange.peer_entry(transfer, &path, kind, size, now),
        Some(FileOutgoing::Chunk { transfer, chunk }) => exchange.peer_chunk(transfer, &chunk, now),
        Some(FileOutgoing::Complete { transfer }) => exchange.peer_complete(transfer, now),
        Some(FileOutgoing::Cancel { transfer, reason }) => exchange.peer_cancel(transfer, reason),
        None => Vec::new(),
    }
}

/// Reads a file record, or nothing at all for one this build has no name for.
fn parse_file(header: &Header, payload: &[u8]) -> Option<FileOutgoing> {
    match ClipboardRecord::try_from(i32::from(header.message_type)).ok()? {
        ClipboardRecord::FilePolicy => {
            let policy = ClipboardFilePolicy::decode(payload).ok()?;

            Some(FileOutgoing::Policy(Policy::new(
                policy.max_file_bytes,
                policy.max_transfer_bytes,
                policy.retention_seconds,
            )))
        }
        ClipboardRecord::FileOffer => {
            let offer = ClipboardFileOffer::decode(payload).ok()?;

            Some(FileOutgoing::Offer {
                serial: offer.serial,
            })
        }
        ClipboardRecord::FileRequest => {
            let request = ClipboardFileRequest::decode(payload).ok()?;

            Some(FileOutgoing::Request {
                serial: request.serial,
                transfer: request.transfer,
            })
        }
        ClipboardRecord::FileEntry => {
            let entry = ClipboardFileEntry::decode(payload).ok()?;

            Some(FileOutgoing::Entry {
                transfer: entry.transfer,
                path: entry.path,
                kind: EntryKind::from_wire(entry.kind)?,
                size: entry.size,
            })
        }
        ClipboardRecord::FileChunk => {
            let chunk = ClipboardFileChunk::decode(payload).ok()?;

            Some(FileOutgoing::Chunk {
                transfer: chunk.transfer,
                chunk: chunk.chunk,
            })
        }
        ClipboardRecord::FileComplete => {
            let complete = ClipboardFileComplete::decode(payload).ok()?;

            Some(FileOutgoing::Complete {
                transfer: complete.transfer,
            })
        }
        ClipboardRecord::FileCancel => {
            let cancel = ClipboardFileCancel::decode(payload).ok()?;

            Some(FileOutgoing::Cancel {
                transfer: cancel.transfer,
                reason: FileCancelReason::try_from(cancel.reason).unwrap_or_default(),
            })
        }
        _ => None,
    }
}

/// Wraps one file message as the record that carries it.
fn record_of_file(message: &FileOutgoing, sequence: u32, generation: u32) -> Record {
    let (message_type, payload) = match message {
        FileOutgoing::Policy(policy) => (
            ClipboardRecord::FilePolicy,
            ClipboardFilePolicy {
                max_file_bytes: policy.max_file_bytes(),
                max_transfer_bytes: policy.max_transfer_bytes(),
                retention_seconds: policy.retention_seconds(),
            }
            .encode_to_vec(),
        ),
        FileOutgoing::Offer { serial } => (
            ClipboardRecord::FileOffer,
            ClipboardFileOffer { serial: *serial }.encode_to_vec(),
        ),
        FileOutgoing::Request { serial, transfer } => (
            ClipboardRecord::FileRequest,
            ClipboardFileRequest {
                serial: *serial,
                transfer: *transfer,
            }
            .encode_to_vec(),
        ),
        FileOutgoing::Entry {
            transfer,
            path,
            kind,
            size,
        } => (
            ClipboardRecord::FileEntry,
            ClipboardFileEntry {
                transfer: *transfer,
                path: path.clone(),
                kind: kind.as_wire(),
                size: *size,
            }
            .encode_to_vec(),
        ),
        FileOutgoing::Chunk { transfer, chunk } => (
            ClipboardRecord::FileChunk,
            ClipboardFileChunk {
                transfer: *transfer,
                chunk: chunk.clone(),
            }
            .encode_to_vec(),
        ),
        FileOutgoing::Complete { transfer } => (
            ClipboardRecord::FileComplete,
            ClipboardFileComplete {
                transfer: *transfer,
            }
            .encode_to_vec(),
        ),
        FileOutgoing::Cancel { transfer, reason } => (
            ClipboardRecord::FileCancel,
            ClipboardFileCancel {
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
    fn every_file_message_survives_the_wire() {
        let messages = [
            FileOutgoing::Policy(Policy::new(1024, 4096, 3600)),
            FileOutgoing::Offer { serial: 7 },
            FileOutgoing::Request {
                serial: 7,
                transfer: 1,
            },
            FileOutgoing::Entry {
                transfer: 1,
                path: "tree/a.txt".to_owned(),
                kind: EntryKind::File,
                size: 3,
            },
            FileOutgoing::Chunk {
                transfer: 1,
                chunk: b"abc".to_vec(),
            },
            FileOutgoing::Complete { transfer: 1 },
            FileOutgoing::Cancel {
                transfer: 1,
                reason: FileCancelReason::FocusLost,
            },
        ];

        for message in messages {
            let record = record_of_file(&message, 0, 3);

            assert_eq!(record.header.channel, Channel::Clipboard);
            assert_eq!(record.header.generation, 3);
            assert_eq!(
                parse_file(&record.header, &record.payload).as_ref(),
                Some(&message),
                "{message:?} did not come back as itself"
            );
        }
    }

    #[test]
    fn a_file_entry_of_a_kind_this_build_has_no_name_for_is_ignored() {
        let record = Record::new(
            Channel::Clipboard,
            ClipboardRecord::FileEntry as u16,
            0,
            0,
            0,
            ClipboardFileEntry {
                transfer: 1,
                path: "a.txt".to_owned(),
                kind: 4242,
                size: 0,
            }
            .encode_to_vec(),
        );

        assert_eq!(parse_file(&record.header, &record.payload), None);
    }

    #[test]
    fn an_ordinary_record_is_not_a_file_one_and_the_other_way_round() {
        let ordinary = record_of(
            &Outgoing::Offer {
                serial: 1,
                mime_types: vec![Kind::Text.mime()],
            },
            0,
            0,
        );
        let file = record_of_file(&FileOutgoing::Offer { serial: 1 }, 0, 0);

        assert_eq!(parse_file(&ordinary.header, &ordinary.payload), None);
        assert_eq!(parse(&file.header, &file.payload), None);
    }

    #[test]
    fn a_staging_directory_is_named_after_the_session_and_nothing_else() {
        assert_eq!(session_token(&[0x0f, 0xa0, 0x01]), "0fa001");
    }

    #[test]
    fn a_record_this_build_has_no_name_for_is_ignored() {
        let record = Record::new(Channel::Clipboard, 4242, 0, 0, 0, Vec::new());

        assert_eq!(parse(&record.header, &record.payload), None);
    }
}
