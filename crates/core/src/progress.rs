//! Progress a long-running operation publishes for the thread that draws it.
//!
//! Progress is a level rather than a stream of events: only the latest value
//! matters, and a value the UI never got round to reading costs nothing. That
//! is why this is a single overwritten slot and not the queue `VmEventSink`
//! uses for HCS events, where every event is a distinct fact whose loss is the
//! loss of information.

use std::{
    mem::Discriminant,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crate::RepositoryError;

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
///
/// Generic over what is being reported because a slot does not care: a
/// download reports its bytes, and creating a VM reports the step it is at,
/// through the same overwritten cell.
pub struct ProgressPublisher<P>(Arc<Mutex<Option<P>>>);

// Written out rather than derived: `#[derive(Clone)]` would demand `P: Clone`,
// and cloning a publisher clones the handle to the slot, never its contents.
impl<P> Clone for ProgressPublisher<P> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

// Likewise: an empty slot needs no `P: Default`, because it holds nothing yet.
impl<P> Default for ProgressPublisher<P> {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(None)))
    }
}

impl<P: Copy> ProgressPublisher<P> {
    /// Replaces whatever the last reported value was.
    pub fn publish(&self, value: P) {
        *self.lock() = Some(value);
    }

    /// Reports the last published value, or `None` before the first one.
    #[must_use]
    pub fn snapshot(&self) -> Option<P> {
        *self.lock()
    }

    /// Recovers a poisoned lock rather than propagating the panic.
    ///
    /// The slot holds a plain `Copy` value that a panic elsewhere cannot leave
    /// half-written, and losing all progress reporting because an unrelated
    /// thread panicked would be worse than reading it.
    fn lock(&self) -> MutexGuard<'_, Option<P>> {
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
pub struct ProgressThrottle<P> {
    publisher: ProgressPublisher<P>,
    min_interval: Duration,
    last: Option<(Instant, Discriminant<P>)>,
}

impl<P: Copy> ProgressThrottle<P> {
    /// How long two reports of the same phase must be apart.
    pub const DEFAULT_INTERVAL: Duration = Duration::from_millis(100);

    #[must_use]
    pub fn new(publisher: ProgressPublisher<P>) -> Self {
        Self::with_interval(publisher, Self::DEFAULT_INTERVAL)
    }

    #[must_use]
    pub fn with_interval(publisher: ProgressPublisher<P>, min_interval: Duration) -> Self {
        Self {
            publisher,
            min_interval,
            last: None,
        }
    }

    /// Publishes `value`, unless the same kind of value was published less
    /// than `min_interval` ago.
    ///
    /// A change of phase is never delayed: it is the transition the UI needs to
    /// see, and holding it back is what leaves a progress bar stuck just short
    /// of the end.
    pub fn publish(&mut self, value: P) {
        let kind = std::mem::discriminant(&value);
        if let Some((published_at, last_kind)) = self.last
            && last_kind == kind
            && published_at.elapsed() < self.min_interval
        {
            return;
        }
        self.publish_now(value);
    }

    /// Publishes `value` whatever the interval says.
    ///
    /// Used for the last value of a phase, which must land even if the
    /// preceding one was moments ago.
    pub fn publish_now(&mut self, value: P) {
        self.publisher.publish(value);
        self.last = Some((Instant::now(), std::mem::discriminant(&value)));
    }
}

/// Which step of creating a VM is running.
///
/// Four steps and no overall percentage: fetching an image, writing a disk and
/// handing the result to HCS are not commensurable, so a bar over all of them
/// would need a denominator that does not exist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildStep {
    /// Fetching the cloud image into the cache. Cloud images only.
    Downloading,
    /// Writing the system disk: an empty VHDX, or the image onto one.
    WritingDisk,
    /// Writing what the VM needs into its directory -- the key pair, the seed
    /// volume, the HCS configuration -- and granting the VM access to it.
    Provisioning,
    /// Creating the compute system and recording the VM in the metadata.
    Registering,
}

/// What creating a VM looks like from outside the thread doing it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuildProgress {
    pub step: BuildStep,
    /// The download's own progress. Only meaningful while `step` is
    /// `Downloading`, and `None` at every other step -- a stale byte count
    /// shown beside a later step would read as a download still running.
    pub download: Option<DownloadPhase>,
}

/// The channel between a VM being built and the thread watching it: what the
/// build is doing, and whether it has been told to stop.
///
/// Two slots rather than one because the byte counts are published deep inside
/// `vmlord-image`, which knows nothing of VMs, while the steps are published
/// around it. Joining them at the moment of reading costs one comparison;
/// joining them at the moment of writing would cost either a dependency the
/// wrong way round or a thread whose only job is forwarding.
#[derive(Clone)]
pub struct BuildMonitor {
    step: ProgressPublisher<BuildStep>,
    download: ProgressPublisher<DownloadPhase>,
    cancel: Arc<AtomicBool>,
}

impl BuildMonitor {
    /// Starts a monitor already reporting `initial`.
    ///
    /// There is no empty state: a build that has been accepted is at some step
    /// from the moment it is listed, and `initial` is the one its source
    /// begins with. The worker replaces it with its own first report as soon
    /// as it runs.
    #[must_use]
    pub fn new(initial: BuildStep) -> Self {
        let step = ProgressPublisher::default();
        step.publish(initial);
        Self {
            step,
            download: ProgressPublisher::default(),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Records the step the build has reached.
    pub fn report(&self, step: BuildStep) {
        log::debug!("a VM build reached {step:?}");
        self.step.publish(step);
    }

    /// The slot the image download publishes its bytes into.
    #[must_use]
    pub fn downloads(&self) -> &ProgressPublisher<DownloadPhase> {
        &self.download
    }

    /// The flag the long steps poll, for handing down to them.
    #[must_use]
    pub fn cancel_flag(&self) -> &AtomicBool {
        &self.cancel
    }

    /// Asks the build to stop at its next checkpoint.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Turns a cancellation into the error the build fails with.
    ///
    /// Cancelling is an ordinary failure on purpose: it then takes the same
    /// rollback every other failure takes, instead of a second cleanup path
    /// that can drift away from the first.
    pub fn check_cancelled(&self) -> Result<(), RepositoryError> {
        if self.is_cancelled() {
            return Err(RepositoryError::new("creating the VM was cancelled"));
        }
        Ok(())
    }

    /// What the watching thread shows for this build right now.
    #[must_use]
    pub fn snapshot(&self) -> BuildProgress {
        let step = self.step.snapshot().unwrap_or(BuildStep::Downloading);
        BuildProgress {
            step,
            download: match step {
                BuildStep::Downloading => self.download.snapshot(),
                _ => None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{BuildMonitor, BuildStep, DownloadPhase, ProgressPublisher, ProgressThrottle};

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

    /// The slot carries whatever a long operation reports, not downloads alone:
    /// #64 publishes the step a VM's creation is at through the same type.
    #[test]
    fn a_publisher_carries_any_copyable_value() {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        enum Step {
            First,
            Second,
        }

        let publisher = ProgressPublisher::<Step>::default();
        assert_eq!(publisher.snapshot(), None);

        publisher.publish(Step::First);
        publisher.publish(Step::Second);

        assert_eq!(publisher.snapshot(), Some(Step::Second));
    }

    #[test]
    fn a_monitor_reports_the_step_it_was_started_at() {
        let monitor = BuildMonitor::new(BuildStep::WritingDisk);

        let progress = monitor.snapshot();

        assert_eq!(progress.step, BuildStep::WritingDisk);
        assert_eq!(
            progress.download, None,
            "a build that never downloads anything has no bytes to show"
        );
    }

    #[test]
    fn a_monitor_shows_downloaded_bytes_only_while_downloading() {
        let monitor = BuildMonitor::new(BuildStep::Downloading);
        monitor.downloads().publish(DownloadPhase::Downloading {
            downloaded: 10,
            total: Some(100),
        });

        assert_eq!(
            monitor.snapshot().download,
            Some(DownloadPhase::Downloading {
                downloaded: 10,
                total: Some(100),
            })
        );

        monitor.report(BuildStep::WritingDisk);

        assert_eq!(
            monitor.snapshot(),
            super::BuildProgress {
                step: BuildStep::WritingDisk,
                download: None,
            },
            "the download's last phase must not be shown beside a later step"
        );
    }

    #[test]
    fn a_clone_of_a_monitor_shares_the_step_and_the_cancellation() {
        let monitor = BuildMonitor::new(BuildStep::Downloading);
        let worker_side = monitor.clone();

        worker_side.report(BuildStep::Registering);
        monitor.cancel();

        assert_eq!(monitor.snapshot().step, BuildStep::Registering);
        assert!(worker_side.is_cancelled());
    }

    #[test]
    fn check_cancelled_names_the_cancellation_as_the_cause() {
        let monitor = BuildMonitor::new(BuildStep::Downloading);
        assert!(monitor.check_cancelled().is_ok());

        monitor.cancel();

        let error = monitor
            .check_cancelled()
            .expect_err("a cancelled build must not be allowed to continue");
        assert!(error.to_string().contains("cancelled"), "got {error}");
    }
}
