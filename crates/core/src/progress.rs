//! Progress a long-running operation publishes for the thread that draws it.
//!
//! Progress is a level rather than a stream of events: only the latest value
//! matters, and a value the UI never got round to reading costs nothing. That
//! is why this is a single overwritten slot and not the queue `VmEventSink`
//! uses for HCS events, where every event is a distinct fact whose loss is the
//! loss of information.

use std::{
    mem::Discriminant,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

/// What a download is doing right now.
///
/// There is no `Failed` variant on purpose. A failure is the `Err` of the
/// operation, and mirroring it here would create a second source of truth that
/// can disagree with the first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DownloadPhase {
    /// The request has been made and no byte of the body has arrived yet.
    Connecting,
    /// Bytes are arriving. `total` is `None` when the server sent no length.
    Downloading { downloaded: u64, total: Option<u64> },
    /// The bytes on disk are being hashed to check them against the expected sum.
    Verifying { hashed: u64, total: u64 },
    /// The image is in the cache and verified.
    Completed,
}

/// The slot a worker writes progress into and the UI thread reads.
///
/// Cloning shares the slot: the worker holds one clone while the UI reads
/// through another.
#[derive(Clone, Default)]
pub struct ProgressPublisher(Arc<Mutex<Option<DownloadPhase>>>);

impl ProgressPublisher {
    /// Replaces whatever the last reported phase was.
    pub fn publish(&self, phase: DownloadPhase) {
        *self.lock() = Some(phase);
    }

    /// Reports the last published phase, or `None` before the first one.
    #[must_use]
    pub fn snapshot(&self) -> Option<DownloadPhase> {
        *self.lock()
    }

    /// Recovers a poisoned lock rather than propagating the panic.
    ///
    /// The slot holds a plain `Copy` value that a panic elsewhere cannot leave
    /// half-written, and losing all progress reporting because an unrelated
    /// thread panicked would be worse than reading it.
    fn lock(&self) -> MutexGuard<'_, Option<DownloadPhase>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Rate-limits publishing so a read loop does not take the lock per chunk.
///
/// A download reads in 64 KiB chunks, so an image of a few hundred megabytes
/// would otherwise publish thousands of times. What the UI can actually show is
/// bounded by its frame rate, so anything faster is waste.
pub struct ProgressThrottle {
    publisher: ProgressPublisher,
    min_interval: Duration,
    last: Option<(Instant, Discriminant<DownloadPhase>)>,
}

impl ProgressThrottle {
    /// How long two reports of the same phase must be apart.
    pub const DEFAULT_INTERVAL: Duration = Duration::from_millis(100);

    #[must_use]
    pub fn new(publisher: ProgressPublisher) -> Self {
        Self::with_interval(publisher, Self::DEFAULT_INTERVAL)
    }

    #[must_use]
    pub fn with_interval(publisher: ProgressPublisher, min_interval: Duration) -> Self {
        Self {
            publisher,
            min_interval,
            last: None,
        }
    }

    /// Publishes `phase`, unless the same kind of phase was published less than
    /// `min_interval` ago.
    ///
    /// A change of phase is never delayed: it is the transition the UI needs to
    /// see, and holding it back is what leaves a progress bar stuck just short
    /// of the end.
    pub fn publish(&mut self, phase: DownloadPhase) {
        let kind = std::mem::discriminant(&phase);
        if let Some((published_at, last_kind)) = self.last
            && last_kind == kind
            && published_at.elapsed() < self.min_interval
        {
            return;
        }
        self.publish_now(phase);
    }

    /// Publishes `phase` whatever the interval says.
    ///
    /// Used for the last value of a phase, which must land even if the
    /// preceding one was moments ago.
    pub fn publish_now(&mut self, phase: DownloadPhase) {
        self.publisher.publish(phase);
        self.last = Some((Instant::now(), std::mem::discriminant(&phase)));
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{DownloadPhase, ProgressPublisher, ProgressThrottle};

    #[test]
    fn a_publisher_starts_empty_and_then_reports_the_last_phase() {
        let publisher = ProgressPublisher::default();
        assert_eq!(publisher.snapshot(), None);

        publisher.publish(DownloadPhase::Connecting);
        publisher.publish(DownloadPhase::Downloading {
            downloaded: 10,
            total: Some(100),
        });

        assert_eq!(
            publisher.snapshot(),
            Some(DownloadPhase::Downloading {
                downloaded: 10,
                total: Some(100),
            }),
            "a later snapshot replaces the earlier one rather than queueing behind it"
        );
    }

    #[test]
    fn a_clone_of_a_publisher_shares_the_snapshot() {
        let publisher = ProgressPublisher::default();
        let worker_side = publisher.clone();

        worker_side.publish(DownloadPhase::Completed);

        assert_eq!(publisher.snapshot(), Some(DownloadPhase::Completed));
    }

    #[test]
    fn a_publisher_survives_a_panic_in_another_holder() {
        let publisher = ProgressPublisher::default();
        let poisoner = publisher.clone();
        let _ = std::thread::spawn(move || {
            poisoner.publish(DownloadPhase::Connecting);
            panic!("a worker panicked while VMLord was downloading");
        })
        .join();

        publisher.publish(DownloadPhase::Completed);

        assert_eq!(
            publisher.snapshot(),
            Some(DownloadPhase::Completed),
            "losing all progress reporting because an unrelated thread panicked \
             would be worse than reading through the poisoned lock"
        );
    }

    #[test]
    fn a_throttle_without_an_interval_publishes_everything() {
        let publisher = ProgressPublisher::default();
        let mut throttle = ProgressThrottle::with_interval(publisher.clone(), Duration::ZERO);

        throttle.publish(DownloadPhase::Downloading {
            downloaded: 1,
            total: None,
        });
        throttle.publish(DownloadPhase::Downloading {
            downloaded: 2,
            total: None,
        });

        assert_eq!(
            publisher.snapshot(),
            Some(DownloadPhase::Downloading {
                downloaded: 2,
                total: None,
            })
        );
    }

    #[test]
    fn a_throttle_drops_a_repeat_of_the_same_phase_inside_the_interval() {
        let publisher = ProgressPublisher::default();
        let mut throttle =
            ProgressThrottle::with_interval(publisher.clone(), Duration::from_secs(3600));

        throttle.publish(DownloadPhase::Downloading {
            downloaded: 1,
            total: None,
        });
        throttle.publish(DownloadPhase::Downloading {
            downloaded: 2,
            total: None,
        });

        assert_eq!(
            publisher.snapshot(),
            Some(DownloadPhase::Downloading {
                downloaded: 1,
                total: None,
            }),
            "publishing every read would take the lock tens of thousands of times per image"
        );
    }

    #[test]
    fn a_throttle_never_delays_a_change_of_phase() {
        let publisher = ProgressPublisher::default();
        let mut throttle =
            ProgressThrottle::with_interval(publisher.clone(), Duration::from_secs(3600));

        throttle.publish(DownloadPhase::Downloading {
            downloaded: 1,
            total: None,
        });
        throttle.publish(DownloadPhase::Verifying {
            hashed: 0,
            total: 1,
        });

        assert_eq!(
            publisher.snapshot(),
            Some(DownloadPhase::Verifying {
                hashed: 0,
                total: 1,
            }),
            "a throttled phase change would leave the UI stuck at 97% forever"
        );
    }

    #[test]
    fn publish_now_ignores_the_interval() {
        let publisher = ProgressPublisher::default();
        let mut throttle =
            ProgressThrottle::with_interval(publisher.clone(), Duration::from_secs(3600));

        throttle.publish(DownloadPhase::Downloading {
            downloaded: 1,
            total: Some(2),
        });
        throttle.publish_now(DownloadPhase::Downloading {
            downloaded: 2,
            total: Some(2),
        });

        assert_eq!(
            publisher.snapshot(),
            Some(DownloadPhase::Downloading {
                downloaded: 2,
                total: Some(2),
            }),
            "the last value of a phase must land, or the bar stops short of the end"
        );
    }
}
