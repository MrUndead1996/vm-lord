//! Which frame a window wears, and which one it wears while it fills a
//! monitor.
//!
//! Borderless full screen is two things: a rectangle, which is the monitor the
//! window is on, and a frame, which is nothing at all. The rectangle is Win32's
//! to answer. The frame is arithmetic on two style words, and getting it wrong
//! is what leaves a title bar or a raised edge drawn over the guest's picture
//! -- so it lives here, away from the window, where it can be read and tested
//! on any machine.
//!
//! The styles are written out rather than taken from the `windows` crate for
//! the same reason. They are numbers Win32 has never changed, and
//! `matches_win32` checks each one against the crate's own on Windows.
//!
//! What is *given back* on the way out is not computed: it is the pair of
//! words read off the window before it was changed, kept whole. A frame that
//! is restored by recomputing it is a frame that drifts.

/// A window's two style words, the way `GetWindowLongPtr` answers them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    /// `GWL_STYLE`.
    pub style: u32,
    /// `GWL_EXSTYLE`.
    pub ex_style: u32,
}

/// `WS_POPUP`: a window that draws no frame of its own.
pub const POPUP: u32 = 0x8000_0000;

/// `WS_OVERLAPPEDWINDOW`: the caption, the sizing border, the system menu and
/// the two size buttons -- everything the user grabs a normal window by.
pub const OVERLAPPED_WINDOW: u32 = 0x00CF_0000;

/// `WS_MAXIMIZE` and `WS_MINIMIZE`.
///
/// These are not frames but states, and a window still carrying one cannot be
/// moved: Win32 sizes a maximised window to its monitor's *work area* and
/// ignores what `SetWindowPos` asks for, which is a full screen with the
/// taskbar still on top of it. They come off on the way in and come back with
/// the placement, which is what remembers that the window was maximised.
pub const STATES: u32 = 0x0100_0000 | 0x2000_0000;

/// The extended styles that draw an edge around the client area:
/// `WS_EX_DLGMODALFRAME`, `WS_EX_WINDOWEDGE`, `WS_EX_CLIENTEDGE` and
/// `WS_EX_STATICEDGE`.
///
/// `WS_EX_WINDOWEDGE` is the one that matters, and the one nobody asks for:
/// Win32 adds it to any window created with a caption, so a viewer that took
/// only `WS_OVERLAPPEDWINDOW` off `GWL_STYLE` still had a raised border drawn
/// around a picture that was supposed to be the whole monitor.
pub const EDGES: u32 = 0x0000_0001 | 0x0000_0100 | 0x0000_0200 | 0x0002_0000;

/// The frame a window wears while it fills a monitor.
///
/// Everything not named here is left alone: `WS_VISIBLE` and `WS_CLIPSIBLINGS`
/// belong to the window rather than to its frame, and a window that came back
/// invisible would be a viewer that had vanished.
#[must_use]
pub fn borderless(frame: Frame) -> Frame {
    Frame {
        style: (frame.style & !(OVERLAPPED_WINDOW | STATES)) | POPUP,
        ex_style: frame.ex_style & !EDGES,
    }
}

/// Whether this window is wearing the borderless frame.
#[must_use]
pub const fn is_borderless(style: u32) -> bool {
    style & POPUP != 0
}

/// Whether this window is maximised or minimised, and so has to be restored
/// down before it can be moved onto a monitor.
///
/// Read from the style before [`borderless`] takes the bits off, and acted on
/// before the new frame goes on: `ShowWindow(SW_RESTORE)` on a window that is
/// already a `WS_POPUP` would be Win32 restoring a state it no longer sees.
#[must_use]
pub const fn is_a_state(style: u32) -> bool {
    style & STATES != 0
}

#[cfg(test)]
mod tests {
    use super::{EDGES, Frame, OVERLAPPED_WINDOW, POPUP, STATES, borderless, is_a_state};

    /// `WS_VISIBLE | WS_CLIPSIBLINGS | WS_OVERLAPPEDWINDOW`, which is what
    /// `CreateWindowExW` leaves on the viewer's window.
    const OPENED: u32 = 0x1000_0000 | 0x0400_0000 | OVERLAPPED_WINDOW;

    /// `WS_EX_WINDOWEDGE`, which Win32 adds to that window without being asked.
    const OPENED_EX: u32 = 0x0000_0100;

    fn opened() -> Frame {
        Frame {
            style: OPENED,
            ex_style: OPENED_EX,
        }
    }

    #[test]
    fn a_full_screen_window_has_no_caption_and_no_sizing_border() {
        let full = borderless(opened());

        assert_eq!(
            full.style & OVERLAPPED_WINDOW,
            0,
            "the title bar and the border are what borderless means"
        );
        assert_eq!(full.style & POPUP, POPUP);
    }

    #[test]
    fn a_full_screen_window_has_no_raised_edge_either() {
        // The bug this module was written for: `WS_EX_WINDOWEDGE` is on every
        // window that ever had a caption, and taking the caption off does not
        // take the edge off with it.
        let full = borderless(opened());

        assert_eq!(
            full.ex_style & EDGES,
            0,
            "an edge drawn round the monitor is a frame the user can see"
        );
    }

    #[test]
    fn what_the_window_is_rather_than_what_it_wears_is_left_alone() {
        let full = borderless(Frame {
            style: OPENED,
            // `WS_EX_NOREDIRECTIONBITMAP`: nothing to do with the frame.
            ex_style: OPENED_EX | 0x0020_0000,
        });

        assert_eq!(full.style & 0x1000_0000, 0x1000_0000, "still visible");
        assert_eq!(full.style & 0x0400_0000, 0x0400_0000, "still clipping");
        assert_eq!(full.ex_style & 0x0020_0000, 0x0020_0000);
    }

    #[test]
    fn a_maximised_window_stops_being_maximised_on_the_way_in() {
        // Win32 sizes a maximised window to the work area and ignores
        // `SetWindowPos`, so a full screen entered from one would be a full
        // screen with the taskbar still drawn over it.
        let maximised = Frame {
            style: OPENED | 0x0100_0000,
            ex_style: OPENED_EX,
        };
        assert!(is_a_state(maximised.style));

        assert_eq!(borderless(maximised).style & STATES, 0);
    }

    #[test]
    fn a_minimised_window_is_a_state_too() {
        assert!(is_a_state(OPENED | 0x2000_0000));
        assert!(!is_a_state(OPENED));
    }

    #[test]
    fn the_frame_to_come_back_to_is_the_one_that_was_read_not_one_worked_out() {
        // What leaving full screen puts back is the saved pair, untouched:
        // `borderless` never sees it again, so a window that was maximised
        // comes back maximised and one that was not does not.
        let before = opened();

        let _ = borderless(before);

        assert_eq!(before, opened());
    }

    #[test]
    fn asking_twice_asks_for_the_same_frame() {
        // `set_fullscreen` guards against this, but a frame that changed under
        // a second call would mean the guard was the only thing holding it.
        let once = borderless(opened());

        assert_eq!(borderless(once), once);
    }

    #[cfg(windows)]
    #[test]
    fn the_numbers_here_are_the_numbers_win32_uses() {
        use windows::Win32::UI::WindowsAndMessaging::{
            WS_EX_CLIENTEDGE, WS_EX_DLGMODALFRAME, WS_EX_STATICEDGE, WS_EX_WINDOWEDGE, WS_MAXIMIZE,
            WS_MINIMIZE, WS_OVERLAPPEDWINDOW, WS_POPUP,
        };

        assert_eq!(POPUP, WS_POPUP.0);
        assert_eq!(OVERLAPPED_WINDOW, WS_OVERLAPPEDWINDOW.0);
        assert_eq!(STATES, WS_MAXIMIZE.0 | WS_MINIMIZE.0);
        assert_eq!(
            EDGES,
            WS_EX_DLGMODALFRAME.0 | WS_EX_WINDOWEDGE.0 | WS_EX_CLIENTEDGE.0 | WS_EX_STATICEDGE.0
        );
    }
}
