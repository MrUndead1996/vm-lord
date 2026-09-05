//! The seam between the clipboard daemon and whatever compositor protocol a
//! desktop offers.
//!
//! One trait, because Mutter's `org.gnome.Mutter.RemoteDesktop` and wlroots'
//! `wlr-data-control` are different protocols with different
//! selection-ownership models, not two spellings of one call. The daemon's
//! loop speaks only this; what may cross and how large it may be is
//! [`vmlord_display_protocol::clipboard`].

use std::{error::Error, fmt, io, sync::mpsc::Receiver};

use vmlord_display_protocol::clipboard::Kind;

/// How a file selection is named in a uri-list, which is what most of the
/// desktop reads.
pub const URI_LIST_MIME: &str = "text/uri-list";

/// How GNOME's file manager names one, with the operation on the first line.
pub const GNOME_COPIED_MIME: &str = "x-special/gnome-copied-files";

/// The kinds these mime types name, in the protocol's canonical order.
///
/// The allowlist is the whole point: a guest copying a spreadsheet cell offers
/// a dozen formats, and only these four ever cross.
#[must_use]
pub fn kinds_of(mimes: &[String]) -> Vec<Kind> {
    let mut kinds = Vec::new();
    for kind in [Kind::Text, Kind::Html, Kind::Bmp, Kind::Png] {
        if mimes.iter().any(|mime| mime == kind.mime()) {
            kinds.push(kind);
        }
    }

    kinds
}

/// Whether a selection names files, whichever of the two formats it uses.
#[must_use]
pub fn offers_files(mimes: &[String]) -> bool {
    mimes
        .iter()
        .any(|mime| mime == URI_LIST_MIME || mime == GNOME_COPIED_MIME)
}

/// What the compositor says happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The guest's selection changed and this side does not own it.
    PeerOffer {
        /// What it can produce, of what may be carried.
        kinds: Vec<Kind>,
        /// Whether it also names files.
        files: bool,
    },
    /// Something in the guest wants the selection this side owns.
    Transfer {
        /// Which format it asked for.
        kind: Kind,
        /// The serial to answer with.
        serial: u32,
    },
    /// Something in the guest wants the files of the selection this side owns.
    TransferFiles {
        /// Which of the two file formats it asked for.
        mime: String,
        /// The serial to answer with.
        serial: u32,
    },
    /// The compositor closed the session. The daemon opens another when a
    /// session exists again.
    Closed,
}

/// A clipboard call that did not work.
#[derive(Debug)]
pub enum ClipboardError {
    /// The compositor, the session or one call on it failed.
    Compositor(String),
    /// A selection larger than this side will carry.
    TooLarge,
    /// Nothing arrived before the deadline.
    Idle,
    /// A descriptor could not be read or written.
    Transfer(io::Error),
}

impl fmt::Display for ClipboardError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Compositor(detail) => {
                write!(formatter, "the compositor's clipboard failed: {detail}")
            }
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

impl Error for ClipboardError {}

/// What a compositor's clipboard can do, whichever protocol drives it.
///
/// The daemon's loop speaks only this; each implementation carries the rest of
/// what its protocol demands.
pub trait GuestClipboard {
    /// Opens the desktop's clipboard and begins turning its signals into
    /// events.
    ///
    /// # Errors
    ///
    /// [`ClipboardError::Compositor`] if the clipboard cannot be opened --
    /// which is what a guest with nobody logged in looks like.
    fn open() -> Result<(Self, Receiver<Event>), ClipboardError>
    where
        Self: Sized;

    /// Watches the guest's selection without owning it.
    ///
    /// # Errors
    ///
    /// [`ClipboardError::Compositor`] if the call is refused.
    fn listen(&self) -> Result<(), ClipboardError>;

    /// Takes the guest's selection, offering these formats.
    ///
    /// `files` adds the two names a file selection is offered under, which is
    /// what a file manager in the guest looks for and what nothing else does.
    ///
    /// # Errors
    ///
    /// [`ClipboardError::Compositor`] if the call is refused.
    fn own(&self, kinds: &[Kind], files: bool) -> Result<(), ClipboardError>;

    /// Reads one format of the guest's selection, up to `cap` bytes.
    ///
    /// # Errors
    ///
    /// [`ClipboardError::Compositor`] if the request is refused --
    /// [`ClipboardError::TooLarge`] past `cap`, [`ClipboardError::Idle`] if
    /// nothing arrives in time, and [`ClipboardError::Transfer`] if the
    /// descriptor fails.
    fn read(&self, kind: Kind, cap: usize) -> Result<Vec<u8>, ClipboardError> {
        self.read_mime(kind.mime(), cap)
    }

    /// Reads one format of the guest's selection by name, up to `cap` bytes.
    ///
    /// The untyped edge, for the file formats: their bodies are lists of
    /// paths, not clipboard payloads, and no file's contents ever pass through
    /// here.
    ///
    /// # Errors
    ///
    /// The same as [`GuestClipboard::read`].
    fn read_mime(&self, mime: &str, cap: usize) -> Result<Vec<u8>, ClipboardError>;

    /// Answers a transfer of the selection this side owns.
    ///
    /// # Errors
    ///
    /// [`ClipboardError::Compositor`] if the request is refused and
    /// [`ClipboardError::Transfer`] if the descriptor cannot be written.
    fn write(&self, serial: u32, bytes: &[u8]) -> Result<(), ClipboardError>;

    /// Refuses a transfer this side cannot answer.
    ///
    /// # Errors
    ///
    /// [`ClipboardError::Compositor`] if the call is refused.
    fn refuse(&self, serial: u32) -> Result<(), ClipboardError>;
}

/// The clipboard of whichever desktop this guest is running.
///
/// The choice is made from what the session offers rather than from the name
/// of a desktop: a GNOME session announces `org.gnome.Mutter.RemoteDesktop` on
/// its bus and no data-control protocol on its registry, a wlroots one the
/// reverse, and a session with neither has nobody logged into it. Deciding by
/// name would need a table of desktops to keep up to date, and would be wrong
/// the first time a compositor grew the other protocol.
///
/// Data-control first, because it is the cheaper question -- one round trip on
/// a socket the session already has -- and because a compositor that offers it
/// is one whose clipboard lives outside the compositor's own D-Bus name.
pub enum Desktop {
    /// A wlroots-style compositor: Hyprland, Sway, and the rest.
    DataControl(crate::data_control::Clipboard),
    /// GNOME.
    Mutter(crate::mutter::Clipboard),
}

impl GuestClipboard for Desktop {
    /// Opens whichever clipboard this session has.
    ///
    /// # Errors
    ///
    /// [`ClipboardError::Compositor`] naming both refusals if neither answers,
    /// which is what a guest with nobody logged in looks like.
    fn open() -> Result<(Self, Receiver<Event>), ClipboardError> {
        let data_control = match crate::data_control::Clipboard::open() {
            Ok((clipboard, events)) => {
                eprintln!("vmlord-display-clipboard: the session speaks data-control");

                return Ok((Self::DataControl(clipboard), events));
            }
            Err(error) => error,
        };

        match crate::mutter::Clipboard::open() {
            Ok((clipboard, events)) => {
                eprintln!("vmlord-display-clipboard: the session speaks Mutter's RemoteDesktop");

                Ok((Self::Mutter(clipboard), events))
            }
            // Both, because either one alone reads as the wrong diagnosis:
            // "no compositor on the bus" in a Hyprland guest sends a reader
            // looking for GNOME.
            Err(mutter) => Err(ClipboardError::Compositor(format!(
                "no data-control ({data_control}) and no Mutter ({mutter})"
            ))),
        }
    }

    fn listen(&self) -> Result<(), ClipboardError> {
        match self {
            Self::DataControl(clipboard) => clipboard.listen(),
            Self::Mutter(clipboard) => clipboard.listen(),
        }
    }

    fn own(&self, kinds: &[Kind], files: bool) -> Result<(), ClipboardError> {
        match self {
            Self::DataControl(clipboard) => clipboard.own(kinds, files),
            Self::Mutter(clipboard) => clipboard.own(kinds, files),
        }
    }

    fn read_mime(&self, mime: &str, cap: usize) -> Result<Vec<u8>, ClipboardError> {
        match self {
            Self::DataControl(clipboard) => clipboard.read_mime(mime, cap),
            Self::Mutter(clipboard) => clipboard.read_mime(mime, cap),
        }
    }

    fn write(&self, serial: u32, bytes: &[u8]) -> Result<(), ClipboardError> {
        match self {
            Self::DataControl(clipboard) => clipboard.write(serial, bytes),
            Self::Mutter(clipboard) => clipboard.write(serial, bytes),
        }
    }

    fn refuse(&self, serial: u32) -> Result<(), ClipboardError> {
        match self {
            Self::DataControl(clipboard) => clipboard.refuse(serial),
            Self::Mutter(clipboard) => clipboard.refuse(serial),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_adapters_speak_the_seam() {
        fn assert_guest_clipboard<C: GuestClipboard>() {}

        assert_guest_clipboard::<crate::mutter::Clipboard>();
        assert_guest_clipboard::<crate::data_control::Clipboard>();
        assert_guest_clipboard::<Desktop>();
    }

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
    fn an_offer_of_files_is_seen_under_either_of_its_names() {
        assert!(offers_files(&[URI_LIST_MIME.to_owned()]));
        assert!(offers_files(&[GNOME_COPIED_MIME.to_owned()]));
        assert!(!offers_files(&["text/plain;charset=utf-8".to_owned()]));
    }
}
