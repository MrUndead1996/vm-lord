//! GNOME's clipboard, through the one interface that reaches it from outside a
//! Wayland client. This module is the Mutter implementation of
//! [`crate::guest_clipboard::GuestClipboard`].
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
    collections::HashMap,
    os::fd::OwnedFd,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::Duration,
};

use vmlord_display_protocol::clipboard::Kind;
use zbus::{
    blocking::{Connection, MessageIterator, Proxy},
    zvariant::{OwnedObjectPath, Value},
};

use crate::{
    clipboard_pipe::{drain, fill},
    guest_clipboard::{
        ClipboardError, Event, GNOME_COPIED_MIME, GuestClipboard, URI_LIST_MIME, kinds_of,
        offers_files,
    },
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

/// The name the tests that anchor this module know the shared error by.
pub type MutterError = ClipboardError;

impl From<zbus::Error> for ClipboardError {
    fn from(error: zbus::Error) -> Self {
        Self::Compositor(error.to_string())
    }
}

/// One RemoteDesktop session, opened for its clipboard alone.
pub struct Clipboard {
    session: Proxy<'static>,
}

impl GuestClipboard for Clipboard {
    /// Creates a RemoteDesktop session, starts it, and begins turning its
    /// signals into events.
    ///
    /// # Errors
    ///
    /// [`ClipboardError::Compositor`] if there is no session bus, no
    /// compositor on it, or the session cannot be created or started -- which
    /// is what a guest with nobody logged in looks like.
    fn open() -> Result<(Self, Receiver<Event>), ClipboardError> {
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

    /// `EnableClipboard` with no mime types, which is what watching is here.
    ///
    /// # Errors
    ///
    /// [`ClipboardError::Compositor`] if the call is refused.
    fn listen(&self) -> Result<(), ClipboardError> {
        // A map, not a list of pairs: these options are `a{sv}` on the wire,
        // and a `Vec` of tuples serialises as `a(sv)`, which mutter refuses.
        let options: HashMap<&str, Value<'_>> = HashMap::new();

        self.session
            .call::<_, _, ()>("EnableClipboard", &(options,))?;

        Ok(())
    }

    /// `SetSelection` with these formats, which makes this side the owner --
    /// and Mutter refuses `SelectionRead` on a selection its caller owns, so
    /// this stays a listener until the host actually sends a selection.
    ///
    /// `files` adds [`URI_LIST_MIME`] and [`GNOME_COPIED_MIME`], the two names
    /// a file selection carries under GNOME.
    ///
    /// # Errors
    ///
    /// [`ClipboardError::Compositor`] if the call is refused.
    fn own(&self, kinds: &[Kind], files: bool) -> Result<(), ClipboardError> {
        let mut mimes: Vec<&str> = kinds.iter().map(|kind| kind.mime()).collect();
        if files {
            mimes.push(URI_LIST_MIME);
            mimes.push(GNOME_COPIED_MIME);
        }
        let options = HashMap::from([("mime-types", Value::from(mimes))]);

        self.session.call::<_, _, ()>("SetSelection", &(options,))?;

        Ok(())
    }

    /// `SelectionRead`, whose descriptor is non-blocking: the first read of it
    /// usually answers `EAGAIN`, so the read is the poll loop `drain` below.
    ///
    /// # Errors
    ///
    /// The same as [`GuestClipboard::read`].
    fn read_mime(&self, mime: &str, cap: usize) -> Result<Vec<u8>, ClipboardError> {
        let descriptor: zbus::zvariant::OwnedFd = self.session.call("SelectionRead", &(mime,))?;

        drain(&OwnedFd::from(descriptor), cap, DEADLINE)
    }

    /// `SelectionWrite`, then `SelectionWriteDone` with the outcome.
    ///
    /// # Errors
    ///
    /// [`ClipboardError::Compositor`] if either call is refused and
    /// [`ClipboardError::Transfer`] if the descriptor cannot be written.
    fn write(&self, serial: u32, bytes: &[u8]) -> Result<(), ClipboardError> {
        let descriptor: zbus::zvariant::OwnedFd =
            self.session.call("SelectionWrite", &(serial,))?;
        let outcome = fill(&OwnedFd::from(descriptor), bytes);

        // Told either way: a transfer left unanswered is an application in the
        // guest waiting on a descriptor that will never be closed.
        self.session
            .call::<_, _, ()>("SelectionWriteDone", &(serial, outcome.is_ok()))?;

        outcome
    }

    /// `SelectionWriteDone` without success, which is what refusing is here.
    ///
    /// # Errors
    ///
    /// [`ClipboardError::Compositor`] if the call is refused.
    fn refuse(&self, serial: u32) -> Result<(), ClipboardError> {
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
    let options: HashMap<String, zbus::zvariant::OwnedValue> = message.body().deserialize().ok()?;

    let owned = options
        .get("session-is-owner")
        .and_then(|value| bool::try_from(value.clone()).ok())
        .unwrap_or(false);
    if owned {
        return None;
    }

    let mimes = options
        .get("mime-types")
        .map(|value| strings_in(value))
        .unwrap_or_default();
    let kinds = kinds_of(&mimes);
    let files = offers_files(&mimes);
    if kinds.is_empty() && !files {
        // Ordinary -- a guest copying a spreadsheet cell offers a dozen
        // formats this build carries none of -- and the one thing that
        // separates it from a selection nobody noticed at all. The count and
        // never the names: a mime type can carry a file name.
        eprintln!(
            "vmlord-display-clipboard: the desktop changed to {} format(s), none carried",
            mimes.len()
        );

        return None;
    }

    Some(Event::PeerOffer { kinds, files })
}

/// Something in the guest is asking for the selection this side owns.
fn transfer(message: &zbus::Message) -> Option<Event> {
    let (mime, serial): (String, u32) = message.body().deserialize().ok()?;

    if let Some(kind) = Kind::from_mime(&mime) {
        return Some(Event::Transfer { kind, serial });
    }
    if mime == URI_LIST_MIME || mime == GNOME_COPIED_MIME {
        return Some(Event::TransferFiles { mime, serial });
    }

    None
}

/// Every string anywhere inside one `a{sv}` value.
///
/// Written as a walk rather than a conversion because of what mutter actually
/// sends: `mime-types` arrives with the signature `(as)` -- a *structure*
/// wrapping the array, not the array -- so every direct conversion to a list
/// of strings answers with nothing, and an ownership change looks like an
/// offer of no formats at all. The walk also sees through the variant a value
/// in a dictionary is wrapped in, which is the other shape this arrives in.
fn strings_in(value: &zbus::zvariant::Value<'_>) -> Vec<String> {
    match value {
        Value::Str(text) => vec![text.to_string()],
        Value::Value(inner) => strings_in(inner),
        Value::Array(array) => array.iter().flat_map(strings_in).collect(),
        Value::Structure(structure) => structure.fields().iter().flat_map(strings_in).collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_structure_wrapped_array_still_names_its_mime_types() {
        // The shape mutter actually sends `mime-types` in: `(as)`, a structure
        // wrapping the array rather than the array. A direct conversion sees
        // nothing here, which is what the walk exists to fix.
        let wrapped = Value::from(zbus::zvariant::Structure::from((vec![
            "text/plain;charset=utf-8".to_owned(),
            "image/png".to_owned(),
        ],)));

        assert_eq!(
            strings_in(&wrapped),
            vec![
                "text/plain;charset=utf-8".to_owned(),
                "image/png".to_owned()
            ]
        );
    }

    #[test]
    fn a_value_that_names_no_string_walks_to_nothing() {
        assert!(strings_in(&Value::from(7u32)).is_empty());
    }
}
