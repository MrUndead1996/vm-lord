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
//! No Win32 here, so the rule is tested without a window.

use std::time::{Duration, Instant};

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
