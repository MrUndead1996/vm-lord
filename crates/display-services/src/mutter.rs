//! GNOME's clipboard, through the one interface that reaches it from outside a
//! Wayland client.
//!
//! `org.gnome.Mutter.RemoteDesktop` is what `gnome-remote-desktop` drives, and
//! it has carried a clipboard since GNOME 42 -- the whole compatibility matrix.
//! Three things about it are worth knowing before reading this file, none of
//! them written down anywhere and all of them established against a running
//! guest:
//!
//!   * a session may be created and started with **no ScreenCast session**
//!     beside it, which is why the guest's clipboard is a small daemon rather
//!     than a screen-sharing stack;
//!   * `EnableClipboard` with no mime types makes this a listener, and with
//!     mime types makes it the owner -- and Mutter refuses `SelectionRead` on a
//!     selection the caller owns ("Tried to read own selection"), so the two
//!     states are not interchangeable and this side must be a listener whenever
//!     the guest is the source;
//!   * the descriptor `SelectionRead` returns is **non-blocking**, and the
//!     first read of it usually answers `EAGAIN`. Reading it is a poll loop,
//!     which is also where the cap and the deadline live.
//!
//! Nothing here is policy. What may cross and how large it may be is
//! [`vmlord_display_protocol::clipboard`]; this only speaks to the compositor.

use std::{
    error::Error,
    fmt,
    io::{self, Read},
    os::fd::{AsRawFd, OwnedFd},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

use vmlord_display_protocol::clipboard::Kind;
use zbus::{
    blocking::{Connection, MessageIterator, Proxy},
    zvariant::{OwnedObjectPath, Value},
};

/// The bus name every call here goes to.
const NAME: &str = "org.gnome.Mutter.RemoteDesktop";

/// The object that hands out sessions.
const ROOT: &str = "/org/gnome/Mutter/RemoteDesktop";

/// The interface a session speaks.
const SESSION: &str = "org.gnome.Mutter.RemoteDesktop.Session";

/// How long one transfer may take before it is abandoned.
///
/// The same five seconds the protocol's own inactivity limit uses: a guest
/// application that never answers a selection request must not hold this
/// process, and the host is told the same thing either way.
const DEADLINE: Duration = Duration::from_secs(5);

/// What the compositor says happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The guest's selection changed and this side does not own it.
    PeerOffer {
        /// What it can produce, of what may be carried.
        kinds: Vec<Kind>,
    },
    /// Something in the guest wants the selection this side owns.
    Transfer {
        /// Which format it asked for.
        kind: Kind,
        /// The serial to answer with.
        serial: u32,
    },
    /// The compositor closed the session. The daemon opens another when a
    /// session exists again.
    Closed,
}

/// A clipboard call that did not work.
#[derive(Debug)]
pub enum MutterError {
    /// The bus, the session or one call on it failed.
    Bus(String),
    /// A selection larger than this side will carry.
    TooLarge,
    /// Nothing arrived before the deadline.
    Idle,
    /// A descriptor could not be read or written.
    Transfer(io::Error),
}

impl fmt::Display for MutterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bus(detail) => write!(formatter, "the compositor's clipboard failed: {detail}"),
            Self::TooLarge => {
                formatter.write_str("the selection is larger than this build carries")
            }
            Self::Idle => formatter.write_str("the selection did not arrive in time"),
            Self::Transfer(error) => {
                write!(formatter, "a selection descriptor failed: {error}")
            }
        }
    }
}

impl Error for MutterError {}

impl From<zbus::Error> for MutterError {
    fn from(error: zbus::Error) -> Self {
        Self::Bus(error.to_string())
    }
}

/// One RemoteDesktop session, opened for its clipboard alone.
pub struct Clipboard {
    session: Proxy<'static>,
}

impl Clipboard {
    /// Creates a session, starts it, and begins turning signals into events.
    ///
    /// # Errors
    ///
    /// [`MutterError::Bus`] if there is no session bus, no compositor on it, or
    /// the session cannot be created or started -- which is what a guest with
    /// nobody logged in looks like.
    pub fn open() -> Result<(Self, Receiver<Event>), MutterError> {
        let connection = Connection::session()?;
        let root = Proxy::new(&connection, NAME, ROOT, NAME)?;
        let path: OwnedObjectPath = root.call("CreateSession", &())?;

        let session: Proxy<'static> = Proxy::new(
            &connection,
            NAME.to_owned(),
            path.clone(),
            SESSION.to_owned(),
        )?;
        session.call::<_, _, ()>("Start", &())?;

        let (sender, receiver) = mpsc::channel();
        let signals = MessageIterator::for_match_rule(
            zbus::MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .sender(NAME)?
                .path(path.clone())?
                .interface(SESSION)?
                .build(),
            &connection,
            None,
        )?;
        thread::spawn(move || pump_signals(signals, &sender));

        Ok((Self { session }, receiver))
    }

    /// Watches the guest's selection without owning it.
    ///
    /// # Errors
    ///
    /// [`MutterError::Bus`] if the call is refused.
    pub fn listen(&self) -> Result<(), MutterError> {
        let options: Vec<(&str, Value<'_>)> = Vec::new();

        self.session
            .call::<_, _, ()>("EnableClipboard", &(options,))?;

        Ok(())
    }

    /// Takes the guest's selection, offering these formats.
    ///
    /// # Errors
    ///
    /// [`MutterError::Bus`] if the call is refused.
    pub fn own(&self, kinds: &[Kind]) -> Result<(), MutterError> {
        let mimes: Vec<&str> = kinds.iter().map(|kind| kind.mime()).collect();
        let options = vec![("mime-types", Value::from(mimes))];

        self.session.call::<_, _, ()>("SetSelection", &(options,))?;

        Ok(())
    }

    /// Reads one format of the guest's selection, up to `cap` bytes.
    ///
    /// # Errors
    ///
    /// [`MutterError::Bus`] if the call is refused -- which is what owning the
    /// selection this asks for looks like -- [`MutterError::TooLarge`] past
    /// `cap`, [`MutterError::Idle`] if nothing arrives in time, and
    /// [`MutterError::Transfer`] if the descriptor fails.
    pub fn read(&self, kind: Kind, cap: usize) -> Result<Vec<u8>, MutterError> {
        let descriptor: zbus::zvariant::OwnedFd =
            self.session.call("SelectionRead", &(kind.mime(),))?;

        drain(&OwnedFd::from(descriptor), cap, DEADLINE)
    }

    /// Answers a transfer of the selection this side owns.
    ///
    /// # Errors
    ///
    /// [`MutterError::Bus`] if either call is refused and
    /// [`MutterError::Transfer`] if the descriptor cannot be written.
    pub fn write(&self, serial: u32, bytes: &[u8]) -> Result<(), MutterError> {
        let descriptor: zbus::zvariant::OwnedFd =
            self.session.call("SelectionWrite", &(serial,))?;
        let outcome = fill(&OwnedFd::from(descriptor), bytes);

        // Told either way: a transfer left unanswered is an application in the
        // guest waiting on a descriptor that will never be closed.
        self.session
            .call::<_, _, ()>("SelectionWriteDone", &(serial, outcome.is_ok()))?;

        outcome
    }

    /// Refuses a transfer this side cannot answer.
    ///
    /// # Errors
    ///
    /// [`MutterError::Bus`] if the call is refused.
    pub fn refuse(&self, serial: u32) -> Result<(), MutterError> {
        self.session
            .call::<_, _, ()>("SelectionWriteDone", &(serial, false))?;

        Ok(())
    }
}

/// Turns the session's signals into [`Event`]s until the connection ends.
fn pump_signals(signals: MessageIterator, sender: &Sender<Event>) {
    for message in signals {
        let Ok(message) = message else {
            continue;
        };
        let header = message.header();
        let Some(member) = header.member() else {
            continue;
        };

        let event = match member.as_str() {
            "SelectionOwnerChanged" => owner_changed(&message),
            "SelectionTransfer" => transfer(&message),
            "Closed" => Some(Event::Closed),
            _ => None,
        };

        if let Some(event) = event
            && sender.send(event).is_err()
        {
            return;
        }
    }
}

/// The guest's selection changed, unless this side is what changed it.
///
/// `session-is-owner` is how the echo is suppressed here: an ownership change
/// this daemon caused comes back as one of these, and forwarding it would put
/// what the host just sent straight back on the wire.
fn owner_changed(message: &zbus::Message) -> Option<Event> {
    let options: std::collections::HashMap<String, zbus::zvariant::OwnedValue> =
        message.body().deserialize().ok()?;

    let owned = options
        .get("session-is-owner")
        .and_then(|value| bool::try_from(value.clone()).ok())
        .unwrap_or(false);
    if owned {
        return None;
    }

    let mimes: Vec<String> = options
        .get("mime-types")
        .and_then(|value| Vec::<String>::try_from(value.clone()).ok())
        .unwrap_or_default();
    let kinds = kinds_of(&mimes);
    if kinds.is_empty() {
        return None;
    }

    Some(Event::PeerOffer { kinds })
}

/// Something in the guest is asking for the selection this side owns.
fn transfer(message: &zbus::Message) -> Option<Event> {
    let (mime, serial): (String, u32) = message.body().deserialize().ok()?;

    Kind::from_mime(&mime).map(|kind| Event::Transfer { kind, serial })
}

/// The kinds these mime types name, in the protocol's canonical order.
fn kinds_of(mimes: &[String]) -> Vec<Kind> {
    let mut kinds = Vec::new();
    for kind in [Kind::Text, Kind::Html, Kind::Bmp, Kind::Png] {
        if mimes.iter().any(|mime| mime == kind.mime()) {
            kinds.push(kind);
        }
    }

    kinds
}

/// Reads a descriptor that is not blocking, and may not be ready for a while.
fn drain<R: AsRawFd>(source: &R, cap: usize, deadline: Duration) -> Result<Vec<u8>, MutterError> {
    let mut file = unsafe_borrowed(source);
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 16 * 1024];
    let until = Instant::now() + deadline;

    loop {
        match file.read(&mut buffer) {
            Ok(0) => return Ok(bytes),
            Ok(read) => {
                if bytes.len() + read > cap {
                    return Err(MutterError::TooLarge);
                }
                bytes.extend_from_slice(&buffer[..read]);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= until {
                    return Err(MutterError::Idle);
                }
                wait(source.as_raw_fd(), libc::POLLIN);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(MutterError::Transfer(error)),
        }
    }
}

/// Writes a whole selection to a descriptor that may not take it all at once.
fn fill(sink: &OwnedFd, bytes: &[u8]) -> Result<(), MutterError> {
    use std::io::Write;

    let mut file = unsafe_borrowed(sink);
    let mut rest = bytes;

    while !rest.is_empty() {
        match file.write(rest) {
            Ok(0) => {
                return Err(MutterError::Transfer(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "the reader took nothing",
                )));
            }
            Ok(written) => rest = &rest[written..],
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                wait(file.as_raw_fd(), libc::POLLOUT);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(MutterError::Transfer(error)),
        }
    }

    Ok(())
}

/// Waits until a descriptor is ready, or a second passes.
fn wait(descriptor: libc::c_int, events: libc::c_short) {
    let mut poll = libc::pollfd {
        fd: descriptor,
        events,
        revents: 0,
    };

    // SAFETY: one live `pollfd` describing a descriptor the caller owns, and a
    // count that matches. A timeout means the loop above checks its deadline.
    unsafe {
        libc::poll(&raw mut poll, 1, 1000);
    }
}

/// A `File` over a descriptor this function does not own.
///
/// The descriptor belongs to the caller, which closes it when it drops; the
/// file is wrapped in `ManuallyDrop` so that reading through it does not close
/// something twice.
fn unsafe_borrowed<F: AsRawFd>(descriptor: &F) -> std::mem::ManuallyDrop<std::fs::File> {
    use std::os::fd::FromRawFd;

    // SAFETY: the descriptor is live for as long as the caller holds it, and
    // the file this makes is never dropped, so it never closes it.
    std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(descriptor.as_raw_fd()) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_allowlisted_mime_types_reach_a_kind() {
        let offered = vec![
            "text/uri-list".to_owned(),
            "image/png".to_owned(),
            "text/plain;charset=utf-8".to_owned(),
        ];

        assert_eq!(kinds_of(&offered), vec![Kind::Text, Kind::Png]);
    }

    #[test]
    fn an_offer_of_files_alone_names_no_kind() {
        assert!(kinds_of(&["text/uri-list".to_owned()]).is_empty());
    }

    #[test]
    fn a_read_stops_at_its_cap() {
        let (reader, writer) = pipe();
        std::thread::spawn(move || {
            use std::io::Write;
            let mut sink = std::fs::File::from(writer);
            let _ = sink.write_all(&[b'x'; 40]);
        });

        assert!(matches!(
            drain(&reader, 16, Duration::from_secs(2)),
            Err(MutterError::TooLarge)
        ));
    }

    #[test]
    fn a_read_that_never_arrives_gives_up() {
        let (reader, writer) = pipe();

        let outcome = drain(&reader, 1024, Duration::from_millis(50));

        drop(writer);
        assert!(matches!(outcome, Err(MutterError::Idle)));
    }

    #[test]
    fn a_read_takes_everything_up_to_the_close() {
        let (reader, writer) = pipe();
        std::thread::spawn(move || {
            use std::io::Write;
            let mut sink = std::fs::File::from(writer);
            let _ = sink.write_all(b"a selection");
        });

        assert_eq!(
            drain(&reader, 1024, Duration::from_secs(2)).expect("a readable pipe"),
            b"a selection"
        );
    }

    /// A non-blocking pipe, which is what `SelectionRead` hands back.
    fn pipe() -> (OwnedFd, OwnedFd) {
        use std::os::fd::FromRawFd;

        let mut ends = [0 as libc::c_int; 2];
        // SAFETY: `ends` is two live ints, which is what `pipe2` fills.
        let made = unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
        assert_eq!(made, 0, "a pipe");

        // SAFETY: `pipe2` succeeded, so both are descriptors this owns.
        unsafe { (OwnedFd::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1])) }
    }
}
