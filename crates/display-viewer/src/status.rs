//! Where the session is, in the words the window puts on the screen.
//!
//! One budget governs every path into a non-running state: thirty seconds of
//! active retry from the moment the state began, and then a `Failed` screen
//! with two working buttons. The clock is a parameter rather than a field, so
//! that the table below is tested at whatever time a test likes and the window
//! passes the time it drew at.

use std::time::{Duration, Instant};

/// How long a state that is not running retries before it gives up.
///
/// The ticket's thirty seconds. Long enough for a guest whose services are
/// restarting, short enough that a user is not left watching a word.
pub const RETRY_BUDGET: Duration = Duration::from_secs(30);

/// The height of a button on the failed screen, in pixels.
const BUTTON_HEIGHT: i32 = 36;

/// The width of a button on the failed screen, in pixels.
const BUTTON_WIDTH: i32 = 120;

/// The gap between the two buttons, in pixels.
const BUTTON_GAP: i32 = 16;

/// How far below the middle of the window the buttons sit.
const BUTTON_OFFSET: i32 = 48;

/// What the viewer is doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Spawned, and about to try the control socket.
    Starting,
    /// Trying the control socket. The guest's services are absent or restarting.
    Waiting,
    /// Connected, and relaying a handshake it does not parse.
    Authenticating,
    /// Frames are decoding. The overlay is gone.
    Running,
    /// Something dropped and is being replaced.
    Reconnecting,
    /// The budget ran out, or something happened that patience will not fix.
    Failed(String),
    /// The VM is not running. Not a failure, and not retried.
    Gone,
}

impl Status {
    /// Whether this state runs under [`RETRY_BUDGET`].
    fn is_retrying(&self) -> bool {
        matches!(
            self,
            Self::Starting | Self::Waiting | Self::Authenticating | Self::Reconnecting
        )
    }
}

/// What happened to the session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// The control socket connected and the handshake is under way.
    Connected,
    /// Both peers proved themselves and the frame channel is bound.
    Established,
    /// A frame or input socket dropped and will be rebound.
    ChannelLost,
    /// Control dropped, which needs a new session and so a new handshake.
    ControlLost,
    /// A new session is needed and VMLord is not there to run the handshake.
    NoParent,
    /// The compute system is gone: a stopped VM rather than a fault.
    PartitionGone,
    /// The user pressed Retry.
    Retry,
}

/// The state machine behind the overlay.
pub struct Progress {
    status: Status,
    /// When the current state began, which is what its budget is measured from.
    entered: Instant,
}

impl Progress {
    /// A viewer that has just started.
    #[must_use]
    pub fn new(now: Instant) -> Self {
        Self {
            status: Status::Starting,
            entered: now,
        }
    }

    /// Where the session is.
    #[must_use]
    pub fn status(&self) -> &Status {
        &self.status
    }

    /// Whether frames are on the screen.
    #[must_use]
    pub fn is_running(&self) -> bool {
        matches!(self.status, Status::Running)
    }

    /// The word the overlay puts on the screen.
    #[must_use]
    pub fn label(&self) -> &str {
        match self.status {
            Status::Starting => "Starting",
            Status::Waiting => "Waiting",
            Status::Authenticating => "Authenticating",
            Status::Running => "Running",
            Status::Reconnecting => "Reconnecting",
            Status::Failed(_) => "Failed",
            Status::Gone => "Closing",
        }
    }

    /// Moves the machine on, if `event` means anything where it is.
    pub fn on(&mut self, event: Event, now: Instant) {
        let next = match (&self.status, event) {
            // A VM that is not running closes the window; nothing is retried.
            (_, Event::PartitionGone) => Status::Gone,
            (Status::Gone, _) => return,
            (_, Event::Retry) => Status::Starting,
            (_, Event::NoParent) => {
                Status::Failed("VMLord is no longer running, and a new session needs it".to_owned())
            }
            (Status::Failed(_), _) => return,
            (_, Event::Connected) => Status::Authenticating,
            (_, Event::Established) => Status::Running,
            (_, Event::ChannelLost | Event::ControlLost) => Status::Reconnecting,
        };

        self.enter(next, now);
    }

    /// Lets time pass, which is the only thing that produces a failure.
    pub fn tick(&mut self, now: Instant) {
        // Starting is the first instant of the wait rather than a state of its
        // own, so the budget it began carries on into Waiting rather than
        // starting again there. Every other state that retries gets its own.
        if matches!(self.status, Status::Starting) {
            tracing::info!("the display session is Waiting");
            self.status = Status::Waiting;
        }

        if !self.status.is_retrying() {
            return;
        }

        if now.duration_since(self.entered) >= RETRY_BUDGET {
            let reason = format!(
                "{} for {} seconds without reaching the guest's display services",
                self.label(),
                RETRY_BUDGET.as_secs()
            );
            self.enter(Status::Failed(reason), now);
        }
    }

    /// Enters a state, restarting the budget with it.
    fn enter(&mut self, status: Status, now: Instant) {
        if self.status == status {
            return;
        }

        tracing::info!("the display session is {status:?}");
        self.status = status;
        self.entered = now;
    }
}

/// A button on the failed screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    /// Start the cycle again with a fresh budget.
    Retry,
    /// Close the window.
    Cancel,
}

/// Where the two buttons sit in a window of this size, as `(x, y, w, h)`.
///
/// Plain arithmetic rather than a control: there are two rectangles on one
/// screen, and a hit test over them is shorter than anything that would draw
/// them for us -- and it is testable without a window.
#[must_use]
pub fn buttons(width: i32, height: i32) -> [(Button, (i32, i32, i32, i32)); 2] {
    let y = height / 2 + BUTTON_OFFSET;
    let total = BUTTON_WIDTH * 2 + BUTTON_GAP;
    let left = (width - total) / 2;

    [
        (Button::Retry, (left, y, BUTTON_WIDTH, BUTTON_HEIGHT)),
        (
            Button::Cancel,
            (
                left + BUTTON_WIDTH + BUTTON_GAP,
                y,
                BUTTON_WIDTH,
                BUTTON_HEIGHT,
            ),
        ),
    ]
}

/// Which button, if any, a click at `(x, y)` landed on.
#[must_use]
pub fn hit_test(width: i32, height: i32, x: i32, y: i32) -> Option<Button> {
    buttons(width, height)
        .into_iter()
        .find(|(_, (bx, by, bw, bh))| x >= *bx && x < bx + bw && y >= *by && y < by + bh)
        .map(|(button, _)| button)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{Button, Event, Progress, RETRY_BUDGET, Status, hit_test};

    #[test]
    fn a_viewer_starts_by_waiting_for_the_guest() {
        let now = Instant::now();
        let mut progress = Progress::new(now);
        assert_eq!(progress.status(), &Status::Starting);

        progress.tick(now + Duration::from_millis(1));
        assert_eq!(progress.status(), &Status::Waiting);
    }

    #[test]
    fn a_connection_authenticates_and_then_runs() {
        let now = Instant::now();
        let mut progress = Progress::new(now);

        progress.on(Event::Connected, now);
        assert_eq!(progress.status(), &Status::Authenticating);

        progress.on(Event::Established, now);
        assert_eq!(progress.status(), &Status::Running);
        assert!(progress.is_running());
    }

    #[test]
    fn a_running_session_that_loses_a_channel_reconnects() {
        let now = Instant::now();
        let mut progress = Progress::new(now);
        progress.on(Event::Connected, now);
        progress.on(Event::Established, now);

        progress.on(Event::ChannelLost, now);
        assert_eq!(progress.status(), &Status::Reconnecting);
        assert!(!progress.is_running());
    }

    #[test]
    fn a_running_session_that_loses_control_reconnects_too() {
        let now = Instant::now();
        let mut progress = Progress::new(now);
        progress.on(Event::Connected, now);
        progress.on(Event::Established, now);

        progress.on(Event::ControlLost, now);
        assert_eq!(progress.status(), &Status::Reconnecting);
    }

    #[test]
    fn a_state_that_never_succeeds_fails_when_its_budget_runs_out() {
        let now = Instant::now();
        let mut progress = Progress::new(now);
        progress.tick(now + Duration::from_millis(1));

        progress.tick(now + RETRY_BUDGET - Duration::from_millis(1));
        assert_eq!(progress.status(), &Status::Waiting);

        progress.tick(now + RETRY_BUDGET);
        assert!(matches!(progress.status(), Status::Failed(_)));
    }

    #[test]
    fn the_budget_starts_again_at_every_state_it_governs() {
        let now = Instant::now();
        let mut progress = Progress::new(now);
        progress.tick(now + Duration::from_millis(1));

        let late = now + RETRY_BUDGET - Duration::from_millis(1);
        progress.on(Event::Connected, late);

        // Authenticating began at `late`, so the budget it runs under is its
        // own rather than what was left of the wait's.
        progress.tick(late + RETRY_BUDGET - Duration::from_millis(1));
        assert_eq!(progress.status(), &Status::Authenticating);
        progress.tick(late + RETRY_BUDGET);
        assert!(matches!(progress.status(), Status::Failed(_)));
    }

    #[test]
    fn retry_starts_the_cycle_again_with_a_fresh_budget() {
        let now = Instant::now();
        let mut progress = Progress::new(now);
        progress.tick(now + RETRY_BUDGET);
        assert!(matches!(progress.status(), Status::Failed(_)));

        let pressed = now + RETRY_BUDGET + Duration::from_secs(60);
        progress.on(Event::Retry, pressed);
        assert_eq!(progress.status(), &Status::Starting);

        progress.tick(pressed + Duration::from_millis(1));
        assert_eq!(progress.status(), &Status::Waiting);
        progress.tick(pressed + RETRY_BUDGET - Duration::from_millis(1));
        assert_eq!(progress.status(), &Status::Waiting);
    }

    #[test]
    fn a_failed_state_stays_failed_until_it_is_retried() {
        let now = Instant::now();
        let mut progress = Progress::new(now);
        progress.on(Event::ControlLost, now);
        progress.tick(now + RETRY_BUDGET);

        let Status::Failed(first) = progress.status().clone() else {
            panic!("the budget ran out");
        };
        progress.tick(now + RETRY_BUDGET * 4);

        assert_eq!(progress.status(), &Status::Failed(first));
    }

    #[test]
    fn a_viewer_whose_parent_is_gone_fails_at_once_rather_than_waiting() {
        let now = Instant::now();
        let mut progress = Progress::new(now);
        progress.on(Event::Connected, now);
        progress.on(Event::Established, now);

        progress.on(Event::NoParent, now);

        assert!(matches!(progress.status(), Status::Failed(_)));
    }

    #[test]
    fn a_stopped_vm_is_not_a_failure() {
        let now = Instant::now();
        let mut progress = Progress::new(now);

        progress.on(Event::PartitionGone, now);

        assert_eq!(progress.status(), &Status::Gone);
        // And it stays there: nothing is retried for a VM that is not running.
        progress.tick(now + RETRY_BUDGET * 4);
        assert_eq!(progress.status(), &Status::Gone);
    }

    #[test]
    fn every_state_has_a_word_for_itself() {
        let now = Instant::now();
        let mut progress = Progress::new(now);

        for (event, label) in [
            (Event::Connected, "Authenticating"),
            (Event::Established, "Running"),
            (Event::ChannelLost, "Reconnecting"),
        ] {
            progress.on(event, now);
            assert_eq!(progress.label(), label);
        }
    }

    #[test]
    fn the_buttons_are_hit_tested_by_rectangle() {
        let (width, height) = (800, 600);
        let mut found = Vec::new();

        for (button, (x, y, w, h)) in super::buttons(width, height) {
            // The middle of each rectangle answers with its own button.
            assert_eq!(hit_test(width, height, x + w / 2, y + h / 2), Some(button));
            found.push(button);
        }

        assert_eq!(found, vec![Button::Retry, Button::Cancel]);
        assert_eq!(hit_test(width, height, 0, 0), None);
        assert_eq!(hit_test(width, height, width, height), None);
    }
}
