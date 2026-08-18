//! The VMs being started right now, and the threads starting them.
//!
//! A start became a thread when GPU-PV joined it. Staging a payload unpacks an
//! archive on a cold cache and hashes the whole staged tree on every start,
//! and neither belongs on the thread that draws the window -- the same reason
//! `build` exists, arrived at a second time.
//!
//! What a start produces is left here for the main thread to take over: the
//! COM1 session and the compute-system handle belong to the repository, which
//! is reachable only behind `&mut self`, and a session dropped on the way out
//! would cancel the console of a VM that is running.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
};

use vmlord_core::RepositoryError;

use crate::build::StartedVm;

/// One VM being started.
struct Start {
    /// Set by the worker as it leaves, by whichever exit -- returning or
    /// panicking -- so that a start that died still stops being listed.
    finished: Arc<AtomicBool>,
    /// Filled in by the worker before it marks itself finished, and emptied by
    /// the reap that removes this start.
    outcome: Arc<Mutex<Option<StartedVm>>>,
    worker: Option<JoinHandle<()>>,
}

/// The VMs being started, by name.
#[derive(Default)]
pub(crate) struct StartRegistry {
    starts: Mutex<HashMap<String, Start>>,
    /// What finished starts produced, waiting for the main thread to take it
    /// over. Kept here rather than returned by `reap`, which runs inside every
    /// query -- a returned session that a caller dropped would silently cancel
    /// the console reader of a running VM.
    started: Mutex<Vec<StartedVm>>,
}

impl StartRegistry {
    /// Whether a VM of this name is being started right now.
    pub(crate) fn contains(&self, vm_name: &str) -> bool {
        // Before answering, not after: a start that is over holds neither a row
        // nor its name, and the caller asking is the one that would otherwise
        // refuse an operation on a VM that is no longer starting.
        self.reap();
        self.lock().contains_key(vm_name)
    }

    /// Runs `start` on a thread of its own, listing the VM as starting until
    /// it returns.
    ///
    /// `start` must not touch the registry: it runs while nothing holds the
    /// lock, but the entry it belongs to is inserted while the lock is held.
    ///
    /// # Errors
    ///
    /// [`RepositoryError`] if this VM is already being started, or if the
    /// thread cannot be spawned.
    pub(crate) fn start<F>(&self, vm_name: &str, start: F) -> Result<(), RepositoryError>
    where
        F: FnOnce() -> Option<StartedVm> + Send + 'static,
    {
        let mut starts = self.lock();
        if starts.contains_key(vm_name) {
            let error = RepositoryError::new(format!("VM \"{vm_name}\" is already being started"));
            log::error!("{error}");
            return Err(error);
        }

        let finished = Arc::new(AtomicBool::new(false));
        let outcome = Arc::new(Mutex::new(None));
        let worker = std::thread::Builder::new()
            .name(format!("vmlord-start-{vm_name}"))
            .spawn({
                let finished = Arc::clone(&finished);
                let outcome = Arc::clone(&outcome);
                move || {
                    // Set on the way out however the start leaves, panic
                    // included: an entry nobody clears is a VM listed as
                    // starting forever. Dropped after the outcome is stored, so
                    // a reap that sees the flag also sees what the start
                    // produced.
                    let _finish = Finish(finished);
                    if let Some(started) = start() {
                        *outcome
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(started);
                    }
                }
            })
            .map_err(|error| {
                let error = RepositoryError::new(format!(
                    "the thread starting VM \"{vm_name}\" could not be started: {error}"
                ));
                log::error!("{error}");
                error
            })?;

        log::info!("started VM \"{vm_name}\" in the background");
        starts.insert(
            vm_name.to_owned(),
            Start {
                finished,
                outcome,
                worker: Some(worker),
            },
        );
        Ok(())
    }

    /// Refuses an operation on a VM that is in the middle of starting.
    ///
    /// "Not found" would be the wrong answer and the confusing one: the VM is
    /// in the list the user is looking at, and it is about to be running.
    pub(crate) fn refuse_if_starting(&self, vm_name: &str) -> Result<(), RepositoryError> {
        if !self.contains(vm_name) {
            return Ok(());
        }
        let error = RepositoryError::new(format!("VM \"{vm_name}\" is still starting"));
        log::error!("{error}");
        Err(error)
    }

    /// Hands over the VMs that starts have produced since this was last called.
    pub(crate) fn take_started(&self) -> Vec<StartedVm> {
        self.reap();
        std::mem::take(
            &mut *self
                .started
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    /// Waits for every start and collects what they produced.
    ///
    /// Called as VMLord shuts down, and by the tests that need a start to be
    /// over before they assert on it. A start is not cancellable: HCS has
    /// either been asked to run the system or has not, and abandoning the
    /// thread halfway would leave nobody to take over the console it opened.
    pub(crate) fn join_all(&self) {
        let mut starts = self.lock();
        for (vm_name, mut start) in starts.drain() {
            Self::collect_into(&self.started, &start);
            if let Some(worker) = start.worker.take()
                && worker.join().is_err()
            {
                log::error!("the thread starting VM \"{vm_name}\" panicked");
            }
        }
    }

    /// Removes and joins the starts that have finished.
    ///
    /// Every query runs this first, so it must be called without the lock held
    /// -- it takes the lock itself, and a `Mutex` is not reentrant. Joining
    /// under the lock is safe because a start never touches the registry.
    fn reap(&self) {
        let mut starts = self.lock();
        let done: Vec<String> = starts
            .iter()
            .filter(|(_, start)| start.finished.load(Ordering::Relaxed))
            .map(|(vm_name, _)| vm_name.clone())
            .collect();
        for vm_name in done {
            let Some(mut start) = starts.remove(&vm_name) else {
                continue;
            };
            Self::collect_into(&self.started, &start);
            if let Some(worker) = start.worker.take()
                && worker.join().is_err()
            {
                log::error!("the thread starting VM \"{vm_name}\" panicked");
            }
        }
    }

    /// Moves what a finished start produced into the registry's own queue.
    fn collect_into(started: &Mutex<Vec<StartedVm>>, start: &Start) {
        let outcome = start
            .outcome
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(outcome) = outcome {
            started
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(outcome);
        }
    }

    /// Recovers a poisoned lock rather than propagating the panic: a start
    /// that panicked must not take the list of VMs down with it.
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Start>> {
        self.starts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Marks a start finished as its thread leaves, by whichever exit.
struct Finish(Arc<AtomicBool>);

impl Drop for Finish {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use super::StartRegistry;

    /// A start that does not return until the test lets it.
    fn held(
        release: &Arc<AtomicBool>,
    ) -> impl FnOnce() -> Option<crate::build::StartedVm> + Send + 'static + use<> {
        let held = Arc::clone(release);
        move || {
            while !held.load(Ordering::Relaxed) {
                std::thread::yield_now();
            }
            None
        }
    }

    #[test]
    fn a_vm_being_started_is_refused_a_second_start() {
        let registry = StartRegistry::default();
        let release = Arc::new(AtomicBool::new(false));
        registry
            .start("dev", held(&release))
            .expect("the first start must be accepted");

        let error = registry
            .start("dev", || None)
            .expect_err("a VM must not be started twice at once");

        assert!(error.to_string().contains("already"), "{error}");
        release.store(true, Ordering::Relaxed);
        registry.join_all();
    }

    #[test]
    fn a_start_that_is_over_stops_being_listed() {
        let registry = StartRegistry::default();
        registry.start("dev", || None).expect("accepted");

        registry.join_all();

        assert!(
            !registry.contains("dev"),
            "a start that has ended holds neither a row nor its name"
        );
    }

    #[test]
    fn a_start_that_panicked_still_stops_being_listed() {
        let registry = StartRegistry::default();
        registry
            .start("dev", || panic!("the start thread died"))
            .expect("accepted");

        registry.join_all();

        assert!(
            !registry.contains("dev"),
            "an entry nobody clears is a VM listed as starting forever"
        );
    }

    #[test]
    fn another_vm_is_not_refused_because_this_one_is_starting() {
        let registry = StartRegistry::default();
        let release = Arc::new(AtomicBool::new(false));
        registry.start("dev", held(&release)).expect("accepted");

        assert!(registry.refuse_if_starting("other").is_ok());
        assert!(registry.refuse_if_starting("dev").is_err());

        release.store(true, Ordering::Relaxed);
        registry.join_all();
    }

    #[test]
    fn a_start_in_flight_is_listed_until_it_ends() {
        let registry = StartRegistry::default();
        let release = Arc::new(AtomicBool::new(false));
        registry.start("dev", held(&release)).expect("accepted");

        assert!(registry.contains("dev"));

        release.store(true, Ordering::Relaxed);
        registry.join_all();
        assert!(!registry.contains("dev"));
    }
}
