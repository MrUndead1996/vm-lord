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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mutter_adapter_speaks_the_seam() {
        fn assert_guest_clipboard<C: GuestClipboard>() {}

        assert_guest_clipboard::<crate::mutter::Clipboard>();
    }
}
