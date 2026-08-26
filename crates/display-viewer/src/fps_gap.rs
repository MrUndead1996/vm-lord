//! When a picture that is arriving too slowly is worth saying out loud.
//!
//! The guest's output has a refresh rate, and a session that delivers a small
//! fraction of it is a desktop that feels broken in a way no error reports:
//! nothing failed, the frames are simply not arriving. What is measured here
//! is frames that were decoded *and* presented, against the refresh the
//! compositor actually committed -- not the one that was asked for.
//!
//! One warning per gap, and a gap has to hold for [`SUSTAIN`] before it counts.
//! A window being dragged, a keyframe after a resize and a guest that just
//! logged in are all a second or two of nothing, and a diagnostic for each of
//! them would be noise nobody reads.
//!
//! No Win32 and no clock of its own, so the rule is tested without either.

use std::time::{Duration, Instant};

use crate::display_modes::DisplayMode;

/// How long each measurement covers.
///
/// Long enough that one late frame is not a rate, short enough that the
/// sustain below is measured in whole seconds rather than estimated.
pub const WINDOW: Duration = Duration::from_secs(1);

/// How long a gap must hold before it is reported, and how long a recovery
/// must hold before another one can be.
pub const SUSTAIN: Duration = Duration::from_secs(10);

/// A gap that held long enough to be worth reporting.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GapWarning {
    /// The mode the guest is on, which is what the refresh belongs to.
    pub mode: DisplayMode,
    /// The frames per second actually presented.
    pub fps: f64,
    /// What share of the mode's refresh that was.
    pub percent: u32,
}

/// The measurement, and what it has already said.
pub struct FpsGap {
    /// Delivered FPS below this share of the committed refresh is a gap.
    threshold_percent: u8,
    /// The measurement in progress: when it started and the count it started
    /// at. `None` before the first sample and after every reset.
    window: Option<(Instant, u64)>,
    /// When the run of below-threshold measurements began.
    below_since: Option<Instant>,
    /// When the run of recovered measurements began, while one is being
    /// waited out.
    above_since: Option<Instant>,
    /// Whether the gap now in progress has already been reported.
    warned: bool,
}

impl FpsGap {
    /// A measurement that has seen nothing, against `threshold_percent`.
    #[must_use]
    pub fn new(threshold_percent: u8) -> Self {
        Self {
            threshold_percent,
            window: None,
            below_since: None,
            above_since: None,
            warned: false,
        }
    }

    /// Forgets everything measured so far.
    ///
    /// What a new session, a rebound channel and a minimised window all mean:
    /// frames that did not arrive because nobody was asking for them are not
    /// a gap, and a counter from the session before is not a rate.
    pub fn reset(&mut self) {
        self.window = None;
        self.below_since = None;
        self.above_since = None;
        self.warned = false;
    }

    /// Takes the presented-frame counter, and reports a gap that has held.
    ///
    /// `mode` is the mode the guest confirmed. `None` -- nothing committed
    /// yet -- pauses the measurement rather than counting against it.
    pub fn sample(
        &mut self,
        now: Instant,
        presented_frames: u64,
        mode: Option<DisplayMode>,
    ) -> Option<GapWarning> {
        let Some(mode) = mode.filter(|mode| mode.refresh_hz > 0) else {
            self.reset();

            return None;
        };

        let Some((start, frames)) = self.window else {
            self.window = Some((now, presented_frames));

            return None;
        };
        let elapsed = now.saturating_duration_since(start);
        if elapsed < WINDOW {
            return None;
        }
        self.window = Some((now, presented_frames));

        let fps = presented_frames.saturating_sub(frames) as f64 / elapsed.as_secs_f64();
        let share = fps * 100.0 / f64::from(mode.refresh_hz);
        if share >= f64::from(self.threshold_percent) {
            self.below_since = None;
            // A gap that was reported is rearmed only by a recovery as long as
            // the gap had to be: a session hovering on the threshold is one
            // warning, not one every ten seconds.
            let recovered = *self.above_since.get_or_insert(start);
            if self.warned && now.saturating_duration_since(recovered) >= SUSTAIN {
                self.warned = false;
                self.above_since = None;
            }

            return None;
        }

        self.above_since = None;
        // The window's start, not this instant: the picture was already slow
        // for everything the window covered.
        let since = *self.below_since.get_or_insert(start);
        if self.warned || now.saturating_duration_since(since) < SUSTAIN {
            return None;
        }
        self.warned = true;

        Some(GapWarning {
            mode,
            fps,
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            percent: share.round().max(0.0) as u32,
        })
    }
}

/// How a gap reads to whoever is watching the application's diagnostics.
#[must_use]
pub fn detail(vm_name: &str, warning: GapWarning) -> String {
    format!(
        "The display of VM \"{vm_name}\" is delivering {:.0} frames per second at {}x{}@{} Hz, \
         which is {}% of the mode's refresh",
        warning.fps,
        warning.mode.width,
        warning.mode.height,
        warning.mode.refresh_hz,
        warning.percent
    )
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{FpsGap, SUSTAIN, detail};
    use crate::display_modes::DisplayMode;

    fn mode(refresh_hz: u32) -> Option<DisplayMode> {
        DisplayMode::new(1920, 1080, refresh_hz)
    }

    /// Feeds one second of `fps` frames, and answers what that measurement did.
    fn second(gap: &mut FpsGap, at: &mut (Instant, u64), fps: u64, refresh_hz: u32) -> bool {
        at.0 += Duration::from_secs(1);
        at.1 += fps;

        gap.sample(at.0, at.1, mode(refresh_hz)).is_some()
    }

    /// A measurement started at zero, with its first window open.
    fn started(threshold_percent: u8) -> (FpsGap, (Instant, u64)) {
        let mut gap = FpsGap::new(threshold_percent);
        let start = Instant::now();
        assert!(
            gap.sample(start, 0, mode(144)).is_none(),
            "the first sample opens a window"
        );

        (gap, (start, 0))
    }

    #[test]
    fn a_gap_that_holds_for_the_sustain_is_reported_once() {
        let (mut gap, mut at) = started(50);

        // 71 of 144 is 49.3%, which is under the threshold.
        for _ in 0..9 {
            assert!(!second(&mut gap, &mut at, 71, 144), "not yet sustained");
        }
        assert!(second(&mut gap, &mut at, 71, 144), "ten seconds of it");
        for _ in 0..20 {
            assert!(!second(&mut gap, &mut at, 71, 144), "and said once");
        }
    }

    #[test]
    fn a_rate_on_the_threshold_is_not_a_gap() {
        let (mut gap, mut at) = started(50);

        // 72 of 144 is exactly half, which is what the threshold allows.
        for _ in 0..30 {
            assert!(!second(&mut gap, &mut at, 72, 144));
        }
    }

    #[test]
    fn a_recovery_shorter_than_the_sustain_does_not_arm_another_warning() {
        let (mut gap, mut at) = started(50);
        for _ in 0..9 {
            let _ = second(&mut gap, &mut at, 71, 144);
        }
        assert!(second(&mut gap, &mut at, 71, 144));

        // Five seconds of a healthy picture, then the gap again.
        for _ in 0..5 {
            assert!(!second(&mut gap, &mut at, 144, 144));
        }
        for _ in 0..20 {
            assert!(
                !second(&mut gap, &mut at, 71, 144),
                "one warning, not one every ten seconds"
            );
        }
    }

    #[test]
    fn a_full_interval_of_recovery_arms_the_next_warning() {
        let (mut gap, mut at) = started(50);
        for _ in 0..9 {
            let _ = second(&mut gap, &mut at, 71, 144);
        }
        assert!(second(&mut gap, &mut at, 71, 144));

        for _ in 0..11 {
            assert!(!second(&mut gap, &mut at, 144, 144));
        }
        for _ in 0..9 {
            assert!(!second(&mut gap, &mut at, 71, 144));
        }
        assert!(
            second(&mut gap, &mut at, 71, 144),
            "a new gap, reported again"
        );
    }

    #[test]
    fn a_session_that_starts_again_is_measured_from_nothing() {
        // The counter from the session before is not a rate, and frames that
        // did not arrive because nobody was asking for them are not a gap.
        let (mut gap, mut at) = started(50);
        for _ in 0..9 {
            let _ = second(&mut gap, &mut at, 71, 144);
        }

        gap.reset();
        at.0 += Duration::from_secs(1);
        assert!(gap.sample(at.0, at.1, mode(144)).is_none());
        for _ in 0..9 {
            assert!(
                !second(&mut gap, &mut at, 71, 144),
                "the sustain starts again"
            );
        }
        assert!(second(&mut gap, &mut at, 71, 144));
    }

    #[test]
    fn nothing_is_measured_until_the_guest_confirms_a_mode() {
        let mut gap = FpsGap::new(50);
        let start = Instant::now();

        assert_eq!(gap.sample(start, 0, None), None);
        assert_eq!(gap.sample(start + SUSTAIN * 2, 0, None), None);
    }

    #[test]
    fn a_warning_names_the_mode_the_rate_is_measured_against() {
        let (mut gap, mut at) = started(50);
        for _ in 0..9 {
            let _ = second(&mut gap, &mut at, 71, 144);
        }
        at.0 += Duration::from_secs(1);
        at.1 += 71;
        let warning = gap.sample(at.0, at.1, mode(144)).expect("a sustained gap");

        assert_eq!(warning.percent, 49);
        assert_eq!(warning.mode.refresh_hz, 144);
        let detail = detail("Ubuntu", warning);
        assert!(detail.contains("1920x1080@144 Hz"), "{detail}");
        assert!(detail.contains("49%"), "{detail}");
    }
}
