//! When a window that is being dragged becomes a request to the guest.
//!
//! A drag is hundreds of `WM_SIZE` messages, and each one taken at face value
//! would be a mode set, a hotplug, a compositor commit and a keyframe. So a
//! size is *held* until it has stopped moving, and only then asked for.
//!
//! The other half of this module's job is the loop that would otherwise
//! close. The guest answers a request with the size it actually came up at,
//! which is not always the size that was asked for -- it is rounded to what
//! `drm_cvt_mode` builds, clamped to what the output drives, and a window
//! whose client area is 1727 wide gets 1720 back. A viewer that compared that
//! answer against its own window and asked again would ask forever. So what is
//! compared is request against request: a size already asked for is never
//! asked for twice, and the difference between the answer and the window is
//! what the letterbox is for.
//!
//! That comparison is also what tells the two directions apart. Since #136 a
//! mode can be chosen rather than dragged -- from the window's *Resolution*
//! submenu, or from the settings inside the guest -- and on such a choice the
//! window is the one that has to move. Both arrive as the same `StreamConfig`,
//! so what separates them is whether the geometry is the one the last request
//! would have produced: the guest's rounding rule is arithmetic, mirrored here,
//! and anything else is the guest going its own way.
//!
//! No Win32 here, so the rule is tested without a window.

use std::time::{Duration, Instant};

use crate::display_modes::{MAX_HEIGHT, MAX_WIDTH, MIN_HEIGHT, MIN_WIDTH};

/// How long a size must hold still before it is asked for.
///
/// Long enough that a drag across the screen is one request rather than
/// three hundred, short enough that letting go feels like it took effect.
pub const DEBOUNCE: Duration = Duration::from_millis(250);

/// The pending resize, and what has already been asked for.
pub struct Resize {
    /// The size last seen, and when it will be due if it does not move again.
    pending: Option<((u32, u32), Instant)>,
    /// The last size asked for, which is never asked for twice running.
    requested: Option<(u32, u32)>,
}

impl Resize {
    /// A debounce that has seen nothing.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: None,
            requested: None,
        }
    }

    /// Records the client area, restarting the wait.
    ///
    /// A size with no pixels in it is a window being minimised, not a request:
    /// it is dropped rather than held, so that restoring the window does not
    /// send one.
    pub fn observe(&mut self, width: u32, height: u32, now: Instant) {
        if width == 0 || height == 0 {
            self.pending = None;

            return;
        }

        self.pending = Some(((width, height), now + DEBOUNCE));
    }

    /// The size to ask the guest for, if one has come due.
    ///
    /// Answers once per size: a request that is already outstanding, or one
    /// the guest is already on, is not made again.
    pub fn due(&mut self, now: Instant) -> Option<(u32, u32)> {
        let (size, deadline) = self.pending?;
        if now < deadline {
            return None;
        }
        self.pending = None;
        if self.requested == Some(size) {
            return None;
        }
        self.requested = Some(size);

        Some(size)
    }

    /// Whether the guest reporting this geometry is it going its own way.
    ///
    /// The answer to a size the viewer asked for is not: that is the request
    /// rounded to what the output builds, and a window that resized itself to
    /// it would be the loop this module exists to keep open.
    ///
    /// Nor is anything before the first request of a session. The guest is
    /// still on whatever mode the last one left it on, the window's own size is
    /// on its way to it, and a window that took that stale geometry would give
    /// up being the authority it is about to exercise.
    #[must_use]
    pub fn is_guests_own(&self, width: u32, height: u32) -> bool {
        match self.requested {
            None => false,
            Some((wanted_width, wanted_height)) => {
                admissible(wanted_width, wanted_height) != Some((width, height))
            }
        }
    }

    /// Takes a size the guest is already on as the one last asked for.
    ///
    /// What follows the window moving to the guest's own choice of mode: the
    /// `WM_SIZE` that move raises would otherwise settle into a request that
    /// asked the guest to undo it -- and on a mode too large for the monitor,
    /// where the window is smaller than the geometry, that request would be the
    /// user's choice being taken back from them.
    pub fn assume(&mut self, width: u32, height: u32) {
        self.pending = None;
        self.requested = Some((width, height));
    }

    /// Forgets what has been asked for, so the window is asked for again.
    ///
    /// What a new session means: the guest that was told is not the guest
    /// that is listening now.
    pub fn forget(&mut self) {
        self.pending = None;
        self.requested = None;
    }
}

impl Default for Resize {
    fn default() -> Self {
        Self::new()
    }
}

/// What the guest makes of a size the viewer asks for.
///
/// The broker's own rule, mirrored: clamped to what the output drives, then
/// rounded down to what `drm_cvt_mode` builds -- a width to a multiple of
/// eight, a height to an even number -- and refused outright when there is not
/// enough left. It is duplicated rather than shared because the two ends are
/// separate binaries on separate machines; `display-services`' `output` module
/// is the original, and the tests below pin the numbers.
#[must_use]
fn admissible(width: u32, height: u32) -> Option<(u32, u32)> {
    if width < MIN_WIDTH || height < MIN_HEIGHT {
        return None;
    }

    let width = (width.min(MAX_WIDTH) / 8) * 8;
    let height = (height.min(MAX_HEIGHT) / 2) * 2;
    if width < MIN_WIDTH || height < MIN_HEIGHT {
        return None;
    }

    Some((width, height))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{DEBOUNCE, Resize};

    #[test]
    fn a_size_that_has_not_settled_is_not_asked_for() {
        let start = Instant::now();
        let mut resize = Resize::new();

        resize.observe(1280, 720, start);

        assert_eq!(
            resize.due(start + DEBOUNCE - Duration::from_millis(1)),
            None
        );
    }

    #[test]
    fn a_size_that_settled_is_asked_for_once() {
        let start = Instant::now();
        let mut resize = Resize::new();

        resize.observe(1280, 720, start);

        assert_eq!(resize.due(start + DEBOUNCE), Some((1280, 720)));
        assert_eq!(
            resize.due(start + DEBOUNCE * 4),
            None,
            "a size already asked for is not asked for again"
        );
    }

    #[test]
    fn a_drag_across_the_screen_is_one_request_rather_than_all_of_them() {
        // Each size taken at face value would be a mode set, a hotplug, a
        // commit and a keyframe.
        let start = Instant::now();
        let mut resize = Resize::new();

        for step in 0..200u32 {
            resize.observe(
                800 + step * 4,
                600,
                start + Duration::from_millis(u64::from(step) * 4),
            );
        }
        let dragging = start + Duration::from_millis(800);

        assert_eq!(resize.due(dragging), None);
        assert_eq!(resize.due(dragging + DEBOUNCE), Some((1596, 600)));
    }

    #[test]
    fn the_guest_answering_with_a_size_of_its_own_does_not_start_it_again() {
        // The guest rounds to what its modes are built on, so the answer is
        // often not the request. A viewer that compared the answer against its
        // own window would ask forever.
        let start = Instant::now();
        let mut resize = Resize::new();
        resize.observe(1727, 971, start);
        assert_eq!(resize.due(start + DEBOUNCE), Some((1727, 971)));

        // The window has not moved, so nothing new is observed; the guest
        // reporting 1720x970 changes nothing here.
        assert_eq!(resize.due(start + DEBOUNCE * 10), None);
    }

    #[test]
    fn a_minimised_window_is_not_a_request_for_no_pixels() {
        let start = Instant::now();
        let mut resize = Resize::new();
        resize.observe(1280, 720, start);
        resize.observe(0, 0, start + Duration::from_millis(10));

        assert_eq!(resize.due(start + DEBOUNCE * 4), None);
    }

    #[test]
    fn the_answer_to_a_request_is_not_the_guest_going_its_own_way() {
        // 1727x971 asked for, 1720x970 applied: the rounding is the guest's,
        // and a window that moved to it would be chasing its own request.
        let start = Instant::now();
        let mut resize = Resize::new();
        resize.observe(1727, 971, start);
        assert_eq!(resize.due(start + DEBOUNCE), Some((1727, 971)));

        assert!(!resize.is_guests_own(1720, 970));
    }

    #[test]
    fn a_mode_nobody_asked_for_is_the_guest_going_its_own_way() {
        // The *Resolution* submenu and the settings inside the guest both land
        // here, and both are a size the window has to take.
        let start = Instant::now();
        let mut resize = Resize::new();
        resize.observe(1848, 1048, start);
        assert_eq!(resize.due(start + DEBOUNCE), Some((1848, 1048)));

        assert!(resize.is_guests_own(2560, 1440));
    }

    #[test]
    fn a_request_the_guest_had_to_clamp_is_still_its_answer() {
        // A window dragged wider than the output drives comes back at the
        // limit, which is the request and not a choice of the guest's.
        let start = Instant::now();
        let mut resize = Resize::new();
        resize.observe(3000, 1600, start);
        assert_eq!(resize.due(start + DEBOUNCE), Some((3000, 1600)));

        assert!(!resize.is_guests_own(2560, 1440));
    }

    #[test]
    fn nothing_asked_for_yet_is_never_the_guest_going_its_own_way() {
        // The first `StreamConfig` of a session is the mode the last one left
        // behind, with the window's own size still on its way to the guest.
        let resize = Resize::new();

        assert!(!resize.is_guests_own(2560, 1440));
    }

    #[test]
    fn the_window_moving_to_the_guests_mode_does_not_ask_for_it_back() {
        // The `WM_SIZE` that move raises would otherwise settle into a request
        // -- and on a mode too large for the monitor the window is smaller than
        // the geometry, so that request would undo what the user picked.
        let start = Instant::now();
        let mut resize = Resize::new();

        resize.assume(1920, 1040);
        resize.observe(1920, 1040, start);

        assert_eq!(resize.due(start + DEBOUNCE), None);
    }

    #[test]
    fn a_new_session_is_told_the_size_again() {
        let start = Instant::now();
        let mut resize = Resize::new();
        resize.observe(1280, 720, start);
        assert_eq!(resize.due(start + DEBOUNCE), Some((1280, 720)));

        resize.forget();
        resize.observe(1280, 720, start + DEBOUNCE);

        assert_eq!(
            resize.due(start + DEBOUNCE * 2),
            Some((1280, 720)),
            "the guest that was told is not the guest that is listening now"
        );
    }
}
