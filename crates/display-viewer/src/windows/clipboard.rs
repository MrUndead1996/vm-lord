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
    io::{Read, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, Sender, TryRecvError, channel},
    },
    thread::{self, JoinHandle},
    time::Instant,
};

use prost::Message as _;
use vmlord_display_protocol::{
    clipboard::{Exchange, Kind, Message as Outgoing, Op, Piece},
    record::{self, Channel, Header, Limits, Record},
    session::{HandedOver, Negotiated, Session},
    v1::{
        CancelReason, Capability, ClipboardCancel, ClipboardData, ClipboardOffer, ClipboardRecord,
        ClipboardRequest, Mode, ProtocolVersion,
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
        UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, HWND_MESSAGE, MSG,
            PM_REMOVE, PeekMessageW, RegisterClassW, WINDOW_EX_STYLE, WINDOW_STYLE,
            WM_CLIPBOARDUPDATE, WNDCLASSW,
        },
    },
    core::PCWSTR,
};

use crate::{
    clipboard::win32,
    launch::Handover,
    live::{BIND_BACKOFF, channel_key, read_awaited},
    windows::hvsocket::{CONNECT_TIMEOUT, HvSocket},
};

/// `CF_UNICODETEXT`, which is the only text format this carries.
const CF_UNICODETEXT: u32 = 13;

/// `CF_DIB`, which is what a picture is on this clipboard.
const CF_DIB: u32 = 8;

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
            log::warn!("the clipboard is not available: {reason}");
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
    // What this thread last put on the clipboard, so that the update Windows
    // sends back is not offered to the guest as something new.
    let mut written = 0;
    let mut held: Vec<Piece> = Vec::new();
    let mut payload = Vec::new();

    loop {
        window.pump();
        let now = Instant::now();
        let mut ops = Vec::new();

        match focus.try_recv() {
            Ok(Focus::Gained) => {
                focused = true;
                if owed {
                    owed = false;
                    ops.extend(offer_local(&mut exchange, html, now));
                }
            }
            Ok(Focus::Lost) => {
                focused = false;
                ops.extend(exchange.focus_lost(now));
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
            } else {
                owed = true;
            }
        }

        if socket.is_none() && now >= next_bind {
            match bind(&mut session, parameters, &mut greeted) {
                Ok(bound) => {
                    log::info!(
                        "the clipboard channel bound at generation {}",
                        session.generation(Channel::Clipboard)
                    );
                    socket = Some(bound);
                    exchange = Exchange::new();
                    held.clear();
                }
                Err(reason) => {
                    log::debug!("the clipboard channel could not bind: {reason}");
                    next_bind = now + BIND_BACKOFF;
                }
            }
        }

        if let Some(open) = socket.as_mut() {
            match record::read(open, &limits, &mut payload) {
                Ok(header) => {
                    if header.generation == session.generation(Channel::Clipboard) {
                        ops.extend(handle(&mut exchange, &header, &payload, now));
                    }
                }
                Err(record::RecordError::Idle) => {}
                Err(error) => {
                    log::info!("the clipboard channel ended: {error}");
                    socket = None;
                    next_bind = Instant::now() + BIND_BACKOFF;
                }
            }
        }

        ops.extend(exchange.tick(now));
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
        if lost {
            log::info!("the clipboard channel could not be written to");
            socket = None;
            next_bind = Instant::now() + BIND_BACKOFF;
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
                    log::debug!("a clipboard record could not be written: {error}");
                    socket = None;
                    lost = true;
                }
            }
            Op::Produce { kind, transfer } => match read_kind(kind, html) {
                Some(bytes) => {
                    log::debug!("sending {} bytes of {}", bytes.len(), kind.mime());
                    queue.extend(exchange.produced(transfer, bytes, Instant::now()));
                }
                None => queue.extend(exchange.unavailable(transfer)),
            },
            Op::Apply { pieces } => {
                log::debug!("taking a guest selection of {} format(s)", pieces.len());
                match apply(&pieces, html) {
                    Ok(sequence) => {
                        *written = sequence;
                        *held = pieces;
                    }
                    Err(reason) => log::warn!("the selection could not be applied: {reason}"),
                }
            }
        }
    }

    lost
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
                log::debug!("a picture could not be encoded: {error}");

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
    let mut formats: Vec<(u32, Vec<u8>)> = Vec::new();
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
                Err(error) => log::debug!("a picture could not be decoded: {error}"),
            },
        }
    }

    if formats.is_empty() {
        return Err("nothing in the selection could be converted".to_owned());
    }

    let _open = Clipboard::open().ok_or_else(|| "the clipboard is held elsewhere".to_owned())?;
    // SAFETY: the clipboard is open on this thread.
    unsafe { EmptyClipboard() }.map_err(|error| error.to_string())?;

    for (format, bytes) in formats {
        put(format, &bytes)?;
    }

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
