//! The window's clipboard: one thread, one socket, one message-only window.
//!
//! A thread of its own rather than work on the session thread, because both of
//! the things it does can take a while and neither may hold a frame: reading a
//! selection out of the Windows clipboard can block behind whatever application
//! last wrote it, and a picture crosses the wire in hundreds of records.
//!
//! What decides anything is [`vmlord_display_protocol::clipboard`], which the
//! guest runs too. What is here is the two edges: Win32 on one side and a bound
//! clipboard channel on the other.
//!
//! No line it writes carries a byte of a selection, at any level. A kind, a
//! byte count and an outcome are what a clipboard problem is diagnosed from.

use std::{
    collections::VecDeque,
    io::{Read, Write},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, Sender, TryRecvError, channel},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime},
};

use prost::Message as _;
use vmlord_display_protocol::{
    clipboard::{
        Exchange, Kind, Message as Outgoing, Op, Piece,
        files::{self, EntryKind, Message as FileOutgoing, Op as FileOp, Policy},
    },
    record::{self, Channel, Header, Limits, Record},
    session::{HandedOver, Negotiated, Session},
    v1::{
        CancelReason, Capability, ClipboardCancel, ClipboardData, ClipboardFileCancel,
        ClipboardFileChunk, ClipboardFileComplete, ClipboardFileEntry, ClipboardFileOffer,
        ClipboardFilePolicy, ClipboardFileRequest, ClipboardOffer, ClipboardRecord,
        ClipboardRequest, FileCancelReason, Mode, ProtocolVersion,
    },
};
use windows::{
    Win32::{
        Foundation::{HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, WPARAM},
        System::{
            DataExchange::{
                AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData,
                GetClipboardSequenceNumber, IsClipboardFormatAvailable, OpenClipboard,
                RegisterClipboardFormatW, SetClipboardData,
            },
            Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock},
        },
        UI::Shell::HDROP,
        UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, HWND_MESSAGE, MSG,
            PM_REMOVE, PeekMessageW, RegisterClassW, WINDOW_EX_STYLE, WINDOW_STYLE,
            WM_CLIPBOARDUPDATE, WNDCLASSW,
        },
    },
    core::PCWSTR,
};

use crate::{
    clipboard::{
        files::{FileError, Produced, SourceTree, Staging, cleanup, dropfiles_of, staging_root},
        win32,
    },
    launch::{FilePolicy, Handover},
    live::{BIND_BACKOFF, channel_key, read_awaited},
    windows::hvsocket::{CONNECT_TIMEOUT, HvSocket},
};

/// `CF_UNICODETEXT`, which is the only text format this carries.
const CF_UNICODETEXT: u32 = 13;

/// `CF_DIB`, which is what a picture is on this clipboard.
const CF_DIB: u32 = 8;

/// `CF_HDROP`, which is what a selection of files is on this clipboard.
const CF_HDROP: u32 = 15;

/// Whether the desktop's clipboard has changed since the loop last looked.
///
/// A static because a window procedure is a bare function and there is exactly
/// one clipboard thread in a viewer process.
static CHANGED: AtomicBool = AtomicBool::new(false);

/// What the window tells this thread about itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    /// The window has the keyboard, so the clipboard follows it.
    Gained,
    /// It does not, so nothing crosses in either direction.
    Lost,
}

/// What one clipboard thread is started with.
pub struct Parameters {
    /// The compute system this session's sockets are opened on.
    pub runtime_id: [u8; 16],
    /// The vsock port the guest's clipboard daemon listens on.
    pub port: u32,
    /// The session VMLord handed over, which carries every channel key.
    pub handover: Handover,
    /// The limits and completed-file lifetime this host was configured with.
    pub file_policy: FilePolicy,
}

/// Starts the clipboard thread, and returns what tells it about focus.
///
/// The thread ends when the returned sender is dropped, which is when the
/// window is closing.
#[must_use]
pub fn spawn(parameters: Parameters) -> (JoinHandle<()>, Sender<Focus>) {
    let (sender, receiver) = channel();
    let handle = thread::spawn(move || {
        if let Err(reason) = serve(&parameters, &receiver) {
            // One line, and the session carries on without a clipboard: a
            // viewer that cannot paste still shows a desktop and still types.
            tracing::warn!("the clipboard is not available: {reason}");
            // The window still has to be able to drop its sender, so the
            // thread drains rather than exits with a live channel.
            while receiver.recv().is_ok() {}
        }
    });

    (handle, sender)
}

/// The thread's body.
fn serve(parameters: &Parameters, focus: &Receiver<Focus>) -> Result<(), String> {
    let mut session = session_of(&parameters.handover)?;
    let window = MessageWindow::new()?;
    let html = html_format();

    let limits = Limits::new(0, 0);
    let mut exchange = Exchange::new();
    let mut files = Files::new(parameters);
    files.sweep();
    let mut socket: Option<HvSocket> = None;
    let mut next_bind = Instant::now();
    // Whether a hello has ever gone down this channel. The guest burns a
    // generation the moment it reads one, so every attempt after the first has
    // to carry a higher one -- whether or not the attempt it belonged to bound.
    let mut greeted = false;
    let mut focused = false;
    // A host selection that changed while the window was unfocused, to announce
    // when it comes back: the guest is told what is on the clipboard now, not
    // what was on it while somebody was working elsewhere.
    let mut owed = false;
    // The guest's last announcement while the window was unfocused, held until
    // it comes back. The model is pull, so holding the announcement holds the
    // selection with it: nothing is asked for and nothing crosses.
    let mut awaited: Option<ClipboardOffer> = None;
    // What this thread last put on the clipboard, so that the update Windows
    // sends back is not offered to the guest as something new.
    let mut written = 0;
    let mut held: Vec<Piece> = Vec::new();
    let mut payload = Vec::new();

    loop {
        window.pump();
        let now = Instant::now();
        let mut ops = Vec::new();

        let mut file_ops = Vec::new();

        match focus.try_recv() {
            Ok(Focus::Gained) => {
                focused = true;
                if owed {
                    owed = false;
                    ops.extend(offer_local(&mut exchange, html, now));
                    file_ops.extend(files.offer_local(now));
                }
                if let Some(offer) = awaited.take() {
                    ops.extend(exchange.peer_offer(offer.serial, &offer.mime_types, now));
                }
            }
            Ok(Focus::Lost) => {
                focused = false;
                ops.extend(exchange.focus_lost(now));
                file_ops.extend(files.focus_lost(now));
            }
            Err(TryRecvError::Disconnected) => return Ok(()),
            Err(TryRecvError::Empty) => {}
        }

        if CHANGED.swap(false, Ordering::Relaxed) {
            let sequence = sequence_number();
            if sequence == written {
                // This side's own write coming back. Offering it to the guest
                // is the echo that would bounce a selection forever.
            } else if focused {
                ops.extend(offer_local(&mut exchange, html, now));
                file_ops.extend(files.offer_local(now));
            } else {
                owed = true;
            }
        }

        if socket.is_none() && now >= next_bind {
            match bind(&mut session, parameters, &mut greeted) {
                Ok(bound) => {
                    tracing::info!(
                        "the clipboard channel bound at generation {}",
                        session.generation(Channel::Clipboard)
                    );
                    socket = Some(bound);
                    exchange = Exchange::new();
                    files.rebind(parameters);
                    held.clear();
                    // The guest offers nothing until it has been told what the
                    // limits are, so this is what opens the file clipboard.
                    file_ops.extend(files.announce());
                }
                Err(reason) => {
                    tracing::debug!("the clipboard channel could not bind: {reason}");
                    next_bind = now + BIND_BACKOFF;
                }
            }
        }

        if let Some(open) = socket.as_mut() {
            match record::read(open, &limits, &mut payload) {
                Ok(header) => {
                    if header.generation != session.generation(Channel::Clipboard) {
                        // A record from a connection that has been replaced.
                    } else if let Some(offer) = unfocused_offer(focused, &header, &payload) {
                        // The rule is the same in both directions: nothing
                        // crosses into a window that does not have the
                        // keyboard. Kept, not dropped, so that what the guest
                        // copied is there when somebody comes back to it.
                        awaited = Some(offer);
                    } else {
                        ops.extend(handle(&mut exchange, &header, &payload, now));
                        file_ops.extend(files.handle(focused, &header, &payload, now));
                    }
                }
                Err(record::RecordError::Idle) => {}
                Err(error) => {
                    tracing::info!("the clipboard channel ended: {error}");
                    socket = None;
                    next_bind = Instant::now() + BIND_BACKOFF;
                }
            }
        }

        ops.extend(exchange.tick(now));
        file_ops.extend(files.tick(now));
        // One entry or one chunk per turn round the loop, so that a directory
        // of a thousand files cannot hold focus, the socket or an ordinary
        // selection behind it.
        file_ops.extend(files.step(now));

        let lost = carry_out(
            ops,
            &mut exchange,
            &mut session,
            socket.as_mut(),
            &limits,
            html,
            &mut held,
            &mut written,
        );
        let file_lost = files.carry_out(
            file_ops,
            &mut session,
            socket.as_mut(),
            &limits,
            html,
            &held,
            &mut written,
        );

        if lost || file_lost {
            tracing::info!("the clipboard channel could not be written to");
            socket = None;
            next_bind = Instant::now() + BIND_BACKOFF;
            // Whatever was half-written goes with the connection it belonged
            // to; the tree is removed rather than left in the profile.
            files.forget();
        }
    }
}

/// Does everything the exchange asked for, in order.
///
/// Answers whether the socket was lost on the way. It is the caller that owns
/// it, so saying so is the only way this can put it down: a write that failed
/// into a socket the loop went on using would fail silently for ever.
#[allow(clippy::too_many_arguments)]
#[must_use]
fn carry_out<S: Read + Write>(
    ops: Vec<Op>,
    exchange: &mut Exchange,
    session: &mut Session,
    mut socket: Option<&mut S>,
    limits: &Limits,
    html: u32,
    held: &mut Vec<Piece>,
    written: &mut u32,
) -> bool {
    // A queue rather than a list: producing a selection appends the chunks that
    // carry it, and those follow whatever is already waiting.
    let mut queue: std::collections::VecDeque<Op> = ops.into();
    let mut lost = false;

    while let Some(op) = queue.pop_front() {
        match op {
            Op::Send(message) => {
                let Some(open) = socket.as_deref_mut() else {
                    continue;
                };
                let Ok(sequence) = session.take_channel_sequence(Channel::Clipboard) else {
                    continue;
                };
                let record = record_of(&message, sequence, session.generation(Channel::Clipboard));
                if let Err(error) = record::write(open, &record, limits) {
                    tracing::debug!("a clipboard record could not be written: {error}");
                    socket = None;
                    lost = true;
                }
            }
            Op::Produce { kind, transfer } => match read_kind(kind, html) {
                Some(bytes) => {
                    tracing::debug!("sending {} bytes of {}", bytes.len(), kind.mime());
                    queue.extend(exchange.produced(transfer, bytes, Instant::now()));
                }
                None => queue.extend(exchange.unavailable(transfer)),
            },
            Op::Apply { pieces } => {
                tracing::debug!("taking a guest selection of {} format(s)", pieces.len());
                match apply(&pieces, html) {
                    Ok(sequence) => {
                        *written = sequence;
                        *held = pieces;
                    }
                    Err(reason) => tracing::warn!("the selection could not be applied: {reason}"),
                }
            }
        }
    }

    lost
}

/// The file clipboard of one viewer window.
///
/// Everything a file transfer needs that an ordinary selection does not: the
/// portable state machine, the tree being read out of this desktop, the tree
/// arriving from the guest, and the paths of the last tree that arrived whole.
struct Files {
    exchange: files::Exchange,
    /// Whether the session settled the capability at all. Without it, not one
    /// file record goes down the channel: a guest from before it has no name
    /// for them and would end the session over one.
    enabled: bool,
    /// The name this session's staging directories are made under.
    session: String,
    /// Where those directories are made. A field rather than a call, so that
    /// nothing but a session with a profile behind it writes into one.
    base: Option<PathBuf>,
    /// How long a committed tree outlives the transfer that made it.
    retention: Duration,
    /// The tree being read out of this desktop, and the transfer it answers.
    source: Option<(u32, SourceTree)>,
    /// The tree arriving from the guest. Dropping it removes what it staged.
    staging: Option<Staging>,
    /// The top-level paths of the last tree that arrived whole, which is what
    /// `CF_HDROP` names. Never logged.
    staged: Vec<PathBuf>,
}

impl Files {
    /// The file clipboard this session's capabilities and settings allow.
    fn new(parameters: &Parameters) -> Self {
        let policy = policy_of(parameters.file_policy);

        Self {
            exchange: files::Exchange::new(policy, Instant::now()),
            enabled: parameters
                .handover
                .capabilities
                .contains(&i32::from(Capability::FileClipboard)),
            session: session_token(&parameters.handover.session_id),
            base: staging_root(),
            retention: Duration::from_secs(policy.retention_seconds()),
            source: None,
            staging: None,
            staged: Vec::new(),
        }
    }

    /// Removes what earlier sessions left in this user's profile.
    ///
    /// Clipboard data outlives the process that put it there, so this is the
    /// only place a committed tree is ever deleted for being old.
    fn sweep(&self) {
        if !self.enabled {
            return;
        }
        let Some(root) = staging_root() else {
            return;
        };

        cleanup(&root, SystemTime::now(), self.retention);
    }

    /// A new connection means a new exchange, and nothing carried over.
    fn rebind(&mut self, parameters: &Parameters) {
        let staged = std::mem::take(&mut self.staged);
        *self = Self::new(parameters);
        // What is on the desktop's clipboard stays on it: the paths are still
        // there, and a paste after a reconnect still finds them.
        self.staged = staged;
    }

    /// Says what this host's limits are, once a channel is up to say it on.
    fn announce(&mut self) -> Vec<FileOp> {
        if !self.enabled {
            return Vec::new();
        }

        self.exchange.announce()
    }

    /// The desktop's selection has files in it.
    fn offer_local(&mut self, now: Instant) -> Vec<FileOp> {
        if !self.enabled || !files_available() {
            return Vec::new();
        }

        self.exchange.local_offer(now)
    }

    /// One file record off the channel.
    ///
    /// Nothing crosses into a window without the keyboard, so while unfocused
    /// only the guest's limits and its cancellations are taken.
    fn handle(
        &mut self,
        focused: bool,
        header: &Header,
        payload: &[u8],
        now: Instant,
    ) -> Vec<FileOp> {
        if !self.enabled {
            return Vec::new();
        }

        match parse_file(header, payload) {
            Some(FileOutgoing::Policy(policy)) => {
                self.exchange.peer_policy(policy);

                Vec::new()
            }
            Some(FileOutgoing::Cancel { transfer, reason }) => {
                self.exchange.peer_cancel(transfer, reason)
            }
            _ if !focused => Vec::new(),
            Some(FileOutgoing::Offer { serial }) => self.exchange.peer_offer(serial, now),
            Some(FileOutgoing::Request { serial, transfer }) => {
                self.exchange.peer_request(serial, transfer, now)
            }
            Some(FileOutgoing::Entry {
                transfer,
                path,
                kind,
                size,
            }) => self.exchange.peer_entry(transfer, &path, kind, size, now),
            Some(FileOutgoing::Chunk { transfer, chunk }) => {
                self.exchange.peer_chunk(transfer, &chunk, now)
            }
            Some(FileOutgoing::Complete { transfer }) => self.exchange.peer_complete(transfer, now),
            None => Vec::new(),
        }
    }

    /// Cancels both directions and removes what was being staged.
    fn focus_lost(&mut self, now: Instant) -> Vec<FileOp> {
        self.source = None;

        self.exchange.focus_lost(now)
    }

    /// Cancels whichever transfer has stopped moving.
    fn tick(&mut self, now: Instant) -> Vec<FileOp> {
        self.exchange.tick(now)
    }

    /// One entry, or one chunk, of the tree being read out of this desktop.
    fn step(&mut self, now: Instant) -> Vec<FileOp> {
        let Some((transfer, tree)) = self.source.as_mut() else {
            return Vec::new();
        };
        let transfer = *transfer;

        match tree.next() {
            Ok(Some(Produced::Entry { path, kind, size })) => self
                .exchange
                .produced_entry(transfer, &path, kind, size, now),
            Ok(Some(Produced::Chunk(bytes))) => self.exchange.produced_chunk(transfer, bytes, now),
            Ok(None) => {
                self.source = None;

                self.exchange.produced_complete(transfer, now)
            }
            Err(error) => {
                // What failed, never what it was called.
                tracing::debug!("a copied tree could not be read: {error}");
                self.source = None;

                self.exchange.produced_failed(transfer, reason_of(&error))
            }
        }
    }

    /// Drops a transfer that a lost connection took with it.
    fn forget(&mut self) {
        self.source = None;
        self.staging = None;
    }

    /// Does everything the file exchange asked for, in order.
    ///
    /// Answers whether the socket was lost on the way, as [`carry_out`] does.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    fn carry_out<S: Read + Write>(
        &mut self,
        ops: Vec<FileOp>,
        session: &mut Session,
        mut socket: Option<&mut S>,
        limits: &Limits,
        html: u32,
        held: &[Piece],
        written: &mut u32,
    ) -> bool {
        let mut queue: VecDeque<FileOp> = ops.into();
        let mut lost = false;

        while let Some(op) = queue.pop_front() {
            match op {
                FileOp::Send(message) => {
                    let Some(open) = socket.as_deref_mut() else {
                        continue;
                    };
                    let Ok(sequence) = session.take_channel_sequence(Channel::Clipboard) else {
                        continue;
                    };
                    let record =
                        record_of_file(&message, sequence, session.generation(Channel::Clipboard));
                    if let Err(error) = record::write(open, &record, limits) {
                        tracing::debug!("a clipboard record could not be written: {error}");
                        socket = None;
                        lost = true;
                    }
                }
                FileOp::Enumerate { transfer } => {
                    match hdrop_selection()
                        .ok_or(FileError::NoName)
                        .and_then(|paths| SourceTree::open(&paths, self.exchange.policy()))
                    {
                        Ok(tree) => self.source = Some((transfer, tree)),
                        Err(error) => {
                            tracing::debug!("the desktop's files could not be read: {error}");
                            queue.extend(
                                self.exchange
                                    .produced_failed(transfer, FileCancelReason::Unavailable),
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
                    let staged = match self.staging.as_mut() {
                        Some(staging) => staging.create_entry(&path, kind, size),
                        None => self
                            .base
                            .clone()
                            .ok_or(FileError::NoProfile)
                            .and_then(|base| Staging::create_at(&base, &self.session, transfer))
                            .and_then(|mut fresh| {
                                let created = fresh.create_entry(&path, kind, size);
                                self.staging = Some(fresh);
                                created
                            }),
                    };

                    if let Err(error) = staged {
                        tracing::debug!("a tree could not be staged: {error}");
                        self.staging = None;
                        queue.extend(self.exchange.staging_failed(transfer, reason_of(&error)));
                    }
                }
                FileOp::WriteChunk { transfer, bytes } => {
                    let written = self
                        .staging
                        .as_mut()
                        .ok_or(FileError::Changed)
                        .and_then(|staging| staging.write_chunk(&bytes));

                    if let Err(error) = written {
                        tracing::debug!("a tree could not be staged: {error}");
                        self.staging = None;
                        queue.extend(self.exchange.staging_failed(transfer, reason_of(&error)));
                    }
                }
                FileOp::Commit { transfer } => match self.staging.take().map(Staging::commit) {
                    Some(Ok(paths)) => {
                        tracing::debug!("taking a guest selection of {} file(s)", paths.len());
                        self.staged = paths;
                        // The files and whatever formats the ordinary
                        // selection had, under one `EmptyClipboard`.
                        match apply_with(held, html, &self.staged) {
                            Ok(sequence) => *written = sequence,
                            Err(reason) => {
                                tracing::warn!("the files could not be applied: {reason}");
                                self.staged.clear();
                            }
                        }
                    }
                    Some(Err(error)) => {
                        tracing::debug!("a tree could not be staged: {error}");
                        queue.extend(self.exchange.staging_failed(transfer, reason_of(&error)));
                    }
                    None => {}
                },
                FileOp::Abort { .. } => {
                    // Dropping it removes the whole partial tree.
                    self.staging = None;
                }
            }
        }

        lost
    }
}

/// The limits the state machine holds, from what the launch carried.
fn policy_of(policy: FilePolicy) -> Policy {
    Policy::new(
        policy.max_file_bytes,
        policy.max_transfer_bytes,
        policy.retention_seconds,
    )
}

/// Why a transfer ended, from what the filesystem said.
fn reason_of(error: &FileError) -> FileCancelReason {
    match error {
        FileError::Unsupported => FileCancelReason::UnsafeEntry,
        FileError::TooLarge | FileError::TooMany => FileCancelReason::TooLarge,
        FileError::Path(_) | FileError::NoName => FileCancelReason::InvalidPath,
        _ => FileCancelReason::IoFailed,
    }
}

/// The name this session's staging directories are made under.
fn session_token(session_id: &[u8]) -> String {
    session_id
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Whether the desktop's clipboard is holding a selection of files.
fn files_available() -> bool {
    // SAFETY: a plain query about a format number.
    unsafe { IsClipboardFormatAvailable(CF_HDROP) }.is_ok()
}

/// The paths the desktop's `CF_HDROP` names.
fn hdrop_selection() -> Option<Vec<PathBuf>> {
    let _open = Clipboard::open()?;

    // SAFETY: the clipboard is open on this thread; the handle belongs to the
    // clipboard and is only read here.
    let handle = unsafe { GetClipboardData(CF_HDROP) }.ok()?;

    crate::windows::files::hdrop_paths(HDROP(handle.0)).ok()
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

/// What the host's clipboard has, as an offer.
fn offer_local(exchange: &mut Exchange, html: u32, now: Instant) -> Vec<Op> {
    let kinds = available(html);
    if kinds.is_empty() {
        return Vec::new();
    }

    exchange.local_offer(&kinds, now)
}

/// The session this thread drives its own channel with.
///
/// A second [`Session`] beside the one the session thread holds, over the same
/// hand-over: the viewer already has all three keys, so this is not another
/// credential, and the two never touch the same channel.
fn session_of(handover: &Handover) -> Result<Session, String> {
    let session_id = handover
        .session_id
        .as_slice()
        .try_into()
        .map_err(|_| "the hand-over's session id is not sixteen bytes".to_owned())?;
    let negotiated = Negotiated {
        version: ProtocolVersion {
            major: handover.version_major,
            minor: handover.version_minor,
        },
        capabilities: handover
            .capabilities
            .iter()
            .filter_map(|value| Capability::try_from(*value).ok())
            .collect(),
        mode: Mode::try_from(handover.mode).unwrap_or(Mode::Desktop),
        width: handover.width,
        height: handover.height,
        tile_size: handover.tile_size,
    };
    if !negotiated.capabilities.contains(&Capability::Clipboard) {
        return Err("this session has no clipboard".to_owned());
    }

    Ok(Session::established_host(HandedOver {
        session_id,
        negotiated,
        frame_key: channel_key(&handover.frame_key, "frame")?,
        input_key: channel_key(&handover.input_key, "input")?,
        clipboard_key: channel_key(&handover.clipboard_key, "clipboard")?,
        control_sequence: handover.control_sequence,
    }))
}

/// Opens the clipboard socket and runs the three-record bind on it.
///
/// `greeted` is what makes a second attempt possible at all. The guest records
/// the generation of every hello it reads and refuses anything that does not
/// climb, so an attempt that failed has still spent one: after the first, the
/// generation is advanced whether the last attempt bound or not.
fn bind(
    session: &mut Session,
    parameters: &Parameters,
    greeted: &mut bool,
) -> Result<HvSocket, String> {
    let mut socket = HvSocket::connect(&parameters.runtime_id, parameters.port, CONNECT_TIMEOUT)
        .map_err(|error| error.to_string())?;
    let limits = Limits::new(0, 0);

    let hello = if std::mem::replace(greeted, true) {
        session.reconnect_channel(Channel::Clipboard)
    } else {
        session.open_channel(Channel::Clipboard)
    }
    .map_err(|error| error.to_string())?;
    record::write(&mut socket, &hello, &limits).map_err(|error| error.to_string())?;

    let mut payload = Vec::new();
    let header = read_awaited(&mut socket, &limits, &mut payload)?;
    let outcome = session
        .handle(&header, &payload)
        .map_err(|error| error.to_string())?;
    if let Some(reply) = outcome.reply {
        record::write(&mut socket, &reply, &limits).map_err(|error| error.to_string())?;
    }
    if outcome.event != vmlord_display_protocol::session::Event::ChannelBound(Channel::Clipboard) {
        return Err("the clipboard channel did not bind".to_owned());
    }

    Ok(socket)
}

/// The offer in this record, if it is one and the window cannot take it.
///
/// A background VM must not be able to put anything on the clipboard of the
/// desktop it is running on, any more than it can read what is copied there.
fn unfocused_offer(focused: bool, header: &Header, payload: &[u8]) -> Option<ClipboardOffer> {
    if focused || header.message_type != ClipboardRecord::Offer as u16 {
        return None;
    }

    ClipboardOffer::decode(payload).ok()
}

/// What one record off the channel means to the exchange.
fn handle(exchange: &mut Exchange, header: &Header, payload: &[u8], now: Instant) -> Vec<Op> {
    match ClipboardRecord::try_from(i32::from(header.message_type)) {
        Ok(ClipboardRecord::Offer) => match ClipboardOffer::decode(payload) {
            Ok(offer) => exchange.peer_offer(offer.serial, &offer.mime_types, now),
            Err(_) => Vec::new(),
        },
        Ok(ClipboardRecord::Request) => match ClipboardRequest::decode(payload) {
            Ok(request) => {
                exchange.peer_request(request.serial, &request.mime_type, request.transfer, now)
            }
            Err(_) => Vec::new(),
        },
        Ok(ClipboardRecord::Data) => match ClipboardData::decode(payload) {
            Ok(data) => exchange.peer_data(data.transfer, &data.chunk, data.last, now),
            Err(_) => Vec::new(),
        },
        Ok(ClipboardRecord::Cancel) => match ClipboardCancel::decode(payload) {
            Ok(cancel) => exchange.peer_cancel(
                cancel.transfer,
                CancelReason::try_from(cancel.reason).unwrap_or_default(),
            ),
            Err(_) => Vec::new(),
        },
        _ => Vec::new(),
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

/// The registered `HTML Format`, which has no constant.
fn html_format() -> u32 {
    let name: Vec<u16> = "HTML Format\0".encode_utf16().collect();

    // SAFETY: a NUL-terminated name that outlives the call.
    unsafe { RegisterClipboardFormatW(PCWSTR(name.as_ptr())) }
}

/// What the desktop's clipboard is holding, of what may be carried.
fn available(html: u32) -> Vec<Kind> {
    let mut kinds = Vec::new();
    // SAFETY: each is a plain query about a format number.
    unsafe {
        if IsClipboardFormatAvailable(CF_UNICODETEXT).is_ok() {
            kinds.push(Kind::Text);
        }
        if IsClipboardFormatAvailable(html).is_ok() {
            kinds.push(Kind::Html);
        }
        if IsClipboardFormatAvailable(CF_DIB).is_ok() {
            kinds.push(Kind::Bmp);
        }
    }

    kinds
}

/// Reads one kind off the desktop's clipboard, as the wire carries it.
fn read_kind(kind: Kind, html: u32) -> Option<Vec<u8>> {
    let _open = Clipboard::open()?;

    match kind {
        Kind::Text => {
            let bytes = global_bytes(CF_UNICODETEXT)?;
            let units: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect();

            Some(win32::utf8_of(&units))
        }
        Kind::Html => win32::html_of_cf_html(&global_bytes(html)?),
        Kind::Bmp => Some(win32::bmp_of_dib(&global_bytes(CF_DIB)?)),
        Kind::Png => match win32::png_of_bmp(&win32::bmp_of_dib(&global_bytes(CF_DIB)?)) {
            Ok(png) => Some(png),
            Err(error) => {
                tracing::debug!("a picture could not be encoded: {error}");

                None
            }
        },
    }
}

/// Puts a whole guest selection on the desktop's clipboard.
///
/// Every format at once, under one `EmptyClipboard`: a paste that found the
/// text but not the picture would be a selection this side took apart.
fn apply(pieces: &[Piece], html: u32) -> Result<u32, String> {
    apply_with(pieces, html, &[])
}

/// Puts a guest selection, and the files staged for it, on the clipboard.
///
/// The paths are the staged tree's top level and nothing below it: what a
/// paste creates is what was copied, not the directory it was staged in.
fn apply_with(pieces: &[Piece], html: u32, paths: &[PathBuf]) -> Result<u32, String> {
    let mut formats: Vec<(u32, Vec<u8>)> = Vec::new();
    if !paths.is_empty() {
        formats.push((CF_HDROP, dropfiles_of(paths)));
    }
    for piece in pieces {
        match piece.kind {
            Kind::Text => {
                let units = win32::utf16_of(&piece.bytes);
                let mut bytes = Vec::with_capacity(units.len() * 2);
                for unit in units {
                    bytes.extend_from_slice(&unit.to_le_bytes());
                }
                formats.push((CF_UNICODETEXT, bytes));
            }
            Kind::Html => formats.push((html, win32::cf_html_of(&piece.bytes))),
            Kind::Bmp => {
                if let Some(dib) = win32::dib_of_bmp(&piece.bytes) {
                    formats.push((CF_DIB, dib));
                }
            }
            Kind::Png => match win32::bmp_of_png(&piece.bytes) {
                // A picture that will not convert is dropped rather than
                // failing the paste: the text beside it is still worth having.
                Ok(bmp) => {
                    if let Some(dib) = win32::dib_of_bmp(&bmp) {
                        formats.push((CF_DIB, dib));
                    }
                }
                Err(error) => tracing::debug!("a picture could not be decoded: {error}"),
            },
        }
    }

    if formats.is_empty() {
        return Err("nothing in the selection could be converted".to_owned());
    }

    let open = Clipboard::open().ok_or_else(|| "the clipboard is held elsewhere".to_owned())?;
    // SAFETY: the clipboard is open on this thread.
    unsafe { EmptyClipboard() }.map_err(|error| error.to_string())?;

    for (format, bytes) in formats {
        put(format, &bytes)?;
    }

    // Closed before the number is read, and that order is the whole of the
    // echo suppression: Windows advances the sequence number when the
    // clipboard is closed, so a number taken while it is still open is the one
    // from *before* this write. Taking it there made every applied selection
    // look like somebody else's copy, and the host offered the guest its own
    // selection straight back.
    drop(open);

    Ok(sequence_number())
}

/// Copies one format's bytes into global memory the clipboard takes over.
fn put(format: u32, bytes: &[u8]) -> Result<(), String> {
    // SAFETY: a moveable allocation of a known size.
    let memory = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len()) }.map_err(|error| {
        format!(
            "the clipboard could not be given {} bytes: {error}",
            bytes.len()
        )
    })?;

    // SAFETY: `memory` was just allocated and is not locked.
    let pointer = unsafe { GlobalLock(memory) };
    if pointer.is_null() {
        return Err("the clipboard's memory could not be locked".to_owned());
    }
    // SAFETY: `pointer` is valid for `bytes.len()` bytes, which is what was
    // allocated, and the two do not overlap.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.cast::<u8>(), bytes.len()) };
    // SAFETY: `memory` is locked exactly once.
    let _ = unsafe { GlobalUnlock(memory) };

    // SAFETY: the clipboard is open and the handle is one it takes ownership
    // of; on failure this side still owns it and lets it leak rather than
    // freeing memory the clipboard may have taken.
    unsafe { SetClipboardData(format, Some(HANDLE(memory.0))) }
        .map_err(|error| format!("a clipboard format could not be set: {error}"))?;

    Ok(())
}

/// The bytes behind one clipboard format, copied out.
fn global_bytes(format: u32) -> Option<Vec<u8>> {
    // SAFETY: the clipboard is open on this thread; the handle belongs to the
    // clipboard and is only read here.
    let handle = unsafe { GetClipboardData(format) }.ok()?;
    let memory = HGLOBAL(handle.0);

    // SAFETY: a clipboard handle to global memory.
    let pointer = unsafe { GlobalLock(memory) };
    if pointer.is_null() {
        return None;
    }
    // SAFETY: as above.
    let size = unsafe { GlobalSize(memory) };
    // SAFETY: `pointer` is valid for `size` bytes while the lock is held.
    let bytes = unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), size) }.to_vec();
    // SAFETY: locked exactly once above.
    let _ = unsafe { GlobalUnlock(memory) };

    Some(bytes)
}

/// What the clipboard is on now, which is how this side knows its own writes.
fn sequence_number() -> u32 {
    // SAFETY: a plain query.
    unsafe { GetClipboardSequenceNumber() }
}

/// The clipboard, open for as long as this lives.
struct Clipboard;

impl Clipboard {
    /// Opens it, waiting briefly for whoever else has it.
    fn open() -> Option<Self> {
        for _ in 0..10 {
            // SAFETY: a null window means the clipboard is opened for this
            // thread, which is the one that closes it.
            if unsafe { OpenClipboard(None) }.is_ok() {
                return Some(Self);
            }
            thread::sleep(std::time::Duration::from_millis(20));
        }

        None
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        // SAFETY: this type exists only while the clipboard is open.
        let _ = unsafe { CloseClipboard() };
    }
}

/// A window with no pixels, for the one message that has to arrive somewhere.
struct MessageWindow {
    hwnd: HWND,
}

impl MessageWindow {
    /// Creates it and asks Windows for clipboard updates.
    fn new() -> Result<Self, String> {
        let class: Vec<u16> = "VMLordDisplayClipboard\0".encode_utf16().collect();
        let descriptor = WNDCLASSW {
            lpfnWndProc: Some(procedure),
            lpszClassName: PCWSTR(class.as_ptr()),
            ..Default::default()
        };
        // SAFETY: a valid class whose name outlives the call. A class that is
        // already registered fails, which is fine: the window below needs the
        // name rather than this call's success.
        unsafe { RegisterClassW(&raw const descriptor) };

        // SAFETY: `HWND_MESSAGE` makes a window with no screen presence, which
        // is what receives `WM_CLIPBOARDUPDATE` without ever being seen.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(class.as_ptr()),
                PCWSTR(class.as_ptr()),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                None,
                None,
            )
        }
        .map_err(|error| format!("the clipboard window could not be made: {error}"))?;

        // SAFETY: `hwnd` is this thread's window.
        unsafe { AddClipboardFormatListener(hwnd) }
            .map_err(|error| format!("the clipboard could not be watched: {error}"))?;

        Ok(Self { hwnd })
    }

    /// Takes whatever messages have arrived.
    fn pump(&self) {
        let mut message = MSG::default();
        // SAFETY: `message` lives across each call and `hwnd` is this thread's.
        while unsafe { PeekMessageW(&raw mut message, Some(self.hwnd), 0, 0, PM_REMOVE) }.as_bool()
        {
            // SAFETY: as above.
            unsafe { DispatchMessageW(&raw const message) };
        }
    }
}

impl Drop for MessageWindow {
    fn drop(&mut self) {
        // SAFETY: `hwnd` is this thread's window and is destroyed once.
        let _ = unsafe { DestroyWindow(self.hwnd) };
    }
}

/// The window procedure: one message matters, and it sets one flag.
extern "system" fn procedure(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if message == WM_CLIPBOARDUPDATE {
        CHANGED.store(true, Ordering::Relaxed);

        return LRESULT(0);
    }

    // SAFETY: the parameters are the ones Windows passed in.
    unsafe { DefWindowProcW(hwnd, message, w, l) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file-capable session, with limits small enough to reach in a test.
    fn file_parameters(capable: bool) -> Parameters {
        let mut handover = handover();
        if capable {
            handover
                .capabilities
                .push(i32::from(Capability::FileClipboard));
        }

        Parameters {
            runtime_id: [0; 16],
            port: 5000,
            handover,
            file_policy: FilePolicy {
                max_file_bytes: 1024,
                max_transfer_bytes: 4096,
                retention_seconds: 3600,
            },
        }
    }

    /// The record a guest's file offer arrives as.
    fn file_offer(serial: u32) -> Record {
        record_of_file(&FileOutgoing::Offer { serial }, 0, 0)
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
            let record = record_of_file(&message, 0, 5);

            assert_eq!(record.header.channel, Channel::Clipboard);
            assert_eq!(record.header.generation, 5);
            assert_eq!(
                parse_file(&record.header, &record.payload).as_ref(),
                Some(&message),
                "{message:?} did not come back as itself"
            );
        }
    }

    #[test]
    fn a_session_without_the_capability_puts_no_file_record_on_the_wire() {
        let parameters = file_parameters(false);
        let mut files = Files::new(&parameters);
        let offer = file_offer(7);

        assert_eq!(files.announce(), Vec::new());
        assert_eq!(files.offer_local(Instant::now()), Vec::new());
        assert_eq!(
            files.handle(true, &offer.header, &offer.payload, Instant::now()),
            Vec::new()
        );
    }

    #[test]
    fn a_file_capable_session_states_its_limits_when_the_channel_binds() {
        let parameters = file_parameters(true);
        let mut files = Files::new(&parameters);

        assert_eq!(
            files.announce(),
            vec![FileOp::Send(FileOutgoing::Policy(Policy::new(
                1024, 4096, 3600
            )))]
        );
        assert_eq!(files.announce(), Vec::new());
    }

    #[test]
    fn a_window_without_the_keyboard_asks_the_guest_for_nothing() {
        let parameters = file_parameters(true);
        let mut files = Files::new(&parameters);
        let policy = record_of_file(&FileOutgoing::Policy(Policy::new(1024, 4096, 3600)), 0, 0);
        let offer = file_offer(7);
        let now = Instant::now();

        files.handle(false, &policy.header, &policy.payload, now);

        assert_eq!(
            files.handle(false, &offer.header, &offer.payload, now),
            Vec::new(),
            "an unfocused window asked for a tree"
        );
        assert_eq!(
            files.handle(true, &offer.header, &offer.payload, now),
            vec![FileOp::Send(FileOutgoing::Request {
                serial: 7,
                transfer: 1,
            })]
        );
    }

    #[test]
    fn a_window_that_lost_the_keyboard_ends_the_tree_it_was_taking() {
        let parameters = file_parameters(true);
        let mut files = Files::new(&parameters);
        let policy = record_of_file(&FileOutgoing::Policy(Policy::new(1024, 4096, 3600)), 0, 0);
        let offer = file_offer(7);
        let now = Instant::now();

        files.handle(true, &policy.header, &policy.payload, now);
        files.handle(true, &offer.header, &offer.payload, now);

        assert_eq!(
            files.focus_lost(now),
            vec![
                FileOp::Send(FileOutgoing::Cancel {
                    transfer: 1,
                    reason: FileCancelReason::FocusLost,
                }),
                FileOp::Abort { transfer: 1 },
            ]
        );
    }

    #[test]
    fn no_line_about_a_tree_carries_a_name_from_it() {
        let parameters = file_parameters(true);
        let mut files = Files::new(&parameters);
        let policy = record_of_file(&FileOutgoing::Policy(Policy::new(1024, 4096, 3600)), 0, 0);
        let offer = file_offer(7);
        let now = Instant::now();
        let sentinel = "vmlord-sentinel-name.txt";
        // Never the real profile: a test that staged there would leave a
        // directory in the user's clipboard staging root.
        let base =
            std::env::temp_dir().join(format!("vmlord-clipboard-log-test-{}", std::process::id()));
        files.base = Some(base.clone());

        files.handle(true, &policy.header, &policy.payload, now);
        let ops = files.handle(true, &offer.header, &offer.payload, now);
        assert!(!ops.is_empty(), "the offer was not asked for");

        let entry = record_of_file(
            &FileOutgoing::Entry {
                transfer: 1,
                path: sentinel.to_owned(),
                kind: EntryKind::File,
                size: 3,
            },
            0,
            0,
        );
        let create = files.handle(true, &entry.header, &entry.payload, now);
        // The same entry twice: the second cannot be created new, which is the
        // failure whose line this test is about.
        let again = files.handle(true, &entry.header, &entry.payload, now);

        let ((), records) = crate::log::capture::capture(|| {
            let mut session = session_of(&parameters.handover).expect("a session");
            let mut nothing: Option<&mut std::io::Cursor<Vec<u8>>> = None;
            let _ = files.carry_out(
                create,
                &mut session,
                nothing.take(),
                &Limits::new(0, 0),
                0,
                &[],
                &mut 0,
            );
            let _ = files.carry_out(
                again,
                &mut session,
                nothing,
                &Limits::new(0, 0),
                0,
                &[],
                &mut 0,
            );
        });

        assert!(
            !records.contains(sentinel),
            "a name from the tree reached the log: {records}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_session_without_the_capability_is_refused() {
        let mut handover = handover();
        handover.capabilities = vec![i32::from(Capability::CursorStream)];

        assert!(session_of(&handover).is_err());
    }

    #[test]
    fn a_session_with_the_capability_carries_its_clipboard_key() {
        let session = session_of(&handover()).expect("a clipboard session");

        assert_eq!(session.generation(Channel::Clipboard), 0);
    }

    #[test]
    fn a_message_becomes_a_record_on_the_clipboard_channel() {
        let record = record_of(
            &Outgoing::Offer {
                serial: 1,
                mime_types: vec![Kind::Text.mime()],
            },
            3,
            2,
        );

        assert_eq!(record.header.channel, Channel::Clipboard);
        assert_eq!(record.header.message_type, ClipboardRecord::Offer as u16);
        assert_eq!(record.header.sequence, 3);
        assert_eq!(record.header.generation, 2);
    }

    #[test]
    fn an_offer_is_held_back_from_a_window_without_the_keyboard() {
        let offer = ClipboardOffer {
            serial: 4,
            mime_types: vec![Kind::Text.mime().to_owned()],
        };
        let header = Header {
            channel: Channel::Clipboard,
            message_type: ClipboardRecord::Offer as u16,
            length: 0,
            sequence: 0,
            base: 0,
            checksum: 0,
            generation: 0,
        };
        let payload = offer.encode_to_vec();

        assert_eq!(
            unfocused_offer(false, &header, &payload).map(|held| held.serial),
            Some(4),
            "a background window takes nothing the guest copies"
        );
        assert!(
            unfocused_offer(true, &header, &payload).is_none(),
            "a focused window handles it the ordinary way"
        );

        let data = Header {
            message_type: ClipboardRecord::Data as u16,
            ..header
        };
        assert!(
            unfocused_offer(false, &data, &payload).is_none(),
            "only an announcement waits; everything else is the exchange's"
        );
    }

    #[test]
    fn a_write_that_failed_says_the_socket_is_gone() {
        // A socket the loop keeps using after a failed write is a clipboard
        // that is silently dead for the rest of the session.
        struct Closed;
        impl Read for Closed {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Ok(0)
            }
        }
        impl Write for Closed {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let mut session = session_of(&handover()).expect("a clipboard session");
        let mut exchange = Exchange::new();
        let mut held = Vec::new();
        let mut written = 0;
        let mut socket = Closed;

        let lost = carry_out(
            vec![Op::Send(Outgoing::Offer {
                serial: 1,
                mime_types: vec![Kind::Text.mime()],
            })],
            &mut exchange,
            &mut session,
            Some(&mut socket),
            &Limits::new(0, 0),
            0,
            &mut held,
            &mut written,
        );

        assert!(
            lost,
            "a failed write has to reach the loop that owns the socket"
        );
    }

    #[test]
    fn every_attempt_after_the_first_climbs_a_generation() {
        // What the guest enforces: it remembers the generation of every hello
        // it reads, so a second attempt at the one it already refused can
        // never bind. `bind` needs a socket, so this is the half of it that
        // decides the generation.
        let mut session = session_of(&handover()).expect("a clipboard session");
        let mut greeted = false;

        for expected in 0..3 {
            let hello = if std::mem::replace(&mut greeted, true) {
                session.reconnect_channel(Channel::Clipboard)
            } else {
                session.open_channel(Channel::Clipboard)
            }
            .expect("a hello");

            assert_eq!(hello.header.generation, expected);
        }
    }

    fn handover() -> Handover {
        Handover {
            session_id: vec![7; 16],
            frame_key: vec![1; 32],
            input_key: vec![2; 32],
            clipboard_key: vec![3; 32],
            version_major: 1,
            version_minor: 0,
            capabilities: vec![i32::from(Capability::Clipboard)],
            mode: i32::from(Mode::Desktop),
            width: 1920,
            height: 1080,
            tile_size: 32,
            control_sequence: 4,
        }
    }
}
