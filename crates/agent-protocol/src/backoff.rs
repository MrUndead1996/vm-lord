//! How long either peer waits before offering the socket again.
//!
//! Both ends of this connection retry, for different reasons: the guest
//! reconnects when the host is not there, and the host waits before serving a
//! guest that connected and dropped without authenticating. Neither may retry
//! as fast as its thread can loop, and the rhythm they use is the same one, so
//! it is written here rather than twice.
//!
//! The rule is deliberately not a table of error classes. A refused connect, a
//! host that hung up during the handshake and a revision that could not be
//! negotiated are all "the peer is not talking to me", and the only question a
//! retry has to answer is how soon to ask again. A session that authenticated
//! is the one thing that proves the other side is there, so that -- and nothing
//! else -- is what starts the wait over.

use std::time::Duration;

/// The wait after a connection that had been working, and the first wait after
/// one that had not.
///
/// A second rather than nothing: a peer that is in the middle of shutting down
/// closes every connection it is offered, and retrying with no wait at all
/// would spin against it.
pub const FIRST_DELAY: Duration = Duration::from_secs(1);

/// The longest this ever waits between attempts.
///
/// Half a minute is the bound on how long after a VMLord restart a guest's
/// agent comes back. Short enough that a user who reopens VMLord sees its VMs'
/// agents appear while they are still looking at the window, and long enough
/// that an agent waiting out a host that is gone for the evening costs nothing.
pub const MAX_DELAY: Duration = Duration::from_secs(30);

/// The wait between one connection and the next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Backoff {
    next: Duration,
}

impl Backoff {
    /// A backoff that has not waited for anything yet.
    #[must_use]
    pub const fn new() -> Self {
        Self { next: FIRST_DELAY }
    }

    /// How long to wait now that a connection has ended.
    ///
    /// `authenticated` is whether that connection got as far as a session both
    /// peers had proved themselves on. One that did starts the wait over; one
    /// that did not doubles it, up to [`MAX_DELAY`].
    pub fn after(&mut self, authenticated: bool) -> Duration {
        if authenticated {
            self.next = FIRST_DELAY;
            return FIRST_DELAY;
        }

        let delay = self.next;
        self.next = delay.saturating_mul(2).min(MAX_DELAY);
        delay
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{Backoff, FIRST_DELAY, MAX_DELAY};
    use std::time::Duration;

    #[test]
    fn a_peer_that_is_not_answering_is_asked_less_and_less_often() {
        let mut backoff = Backoff::new();

        assert_eq!(backoff.after(false), FIRST_DELAY);
        assert_eq!(backoff.after(false), Duration::from_secs(2));
        assert_eq!(backoff.after(false), Duration::from_secs(4));
    }

    #[test]
    fn the_wait_stops_growing_at_the_cap() {
        let mut backoff = Backoff::new();
        for _ in 0..20 {
            backoff.after(false);
        }

        assert_eq!(backoff.after(false), MAX_DELAY);
        assert_eq!(backoff.after(false), MAX_DELAY);
    }

    #[test]
    fn a_session_that_authenticated_starts_the_wait_over() {
        // Without this, a host that restarts twice in an evening is retried at
        // the cap rather than at once, for no reason but its own uptime.
        let mut backoff = Backoff::new();
        for _ in 0..5 {
            backoff.after(false);
        }

        assert_eq!(backoff.after(true), FIRST_DELAY);
        assert_eq!(backoff.after(false), FIRST_DELAY);
        assert_eq!(backoff.after(false), Duration::from_secs(2));
    }
}
