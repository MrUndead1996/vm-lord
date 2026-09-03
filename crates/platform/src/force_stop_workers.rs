//! The forced stops in flight right now, one thread each.
//!
//! Terminating a VM takes as long as HCS takes to tear the compute system down
//! -- moments when the Host Compute Service is healthy, and up to the force
//! stop pipeline's timeout when it is wedged -- and it used to take that on the
//! caller's thread, which is the UI's: the window stopped repainting until the
//! termination came back, and force stop is the one thing a user reaches for
//! precisely because a VM is already misbehaving. Here every termination gets a
//! thread of its own and the repository collects the outcome on its next
//! refresh, where the run's handles can be given up.
//!
//! Modelled on [`crate::shutdown_workers::ShutdownWorkers`], for the same
//! reasons: a flag the worker sets however it leaves, the outcome parked for
//! the main thread, and a join for every thread before the process goes. It is
//! kept separate from the graceful shutdowns so that forcing a stop while a
//! guest is still being asked to shut down -- the escalation when the request
//! is taking too long -- is not mistaken for asking the same VM twice.

use std::{
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
};

use uuid::Uuid;
use vmlord_core::RepositoryError;

/// One forced stop being carried out.
struct Worker {
    vm_id: Uuid,
    vm_name: String,
    /// Set by the worker as it leaves, by whichever exit -- returning or
    /// panicking -- so that a termination whose thread died is still collected.
    finished: Arc<AtomicBool>,
    /// Filled in by the worker before it marks itself finished, and emptied by
    /// the collection that removes this worker.
    outcome: Arc<Mutex<Option<Result<(), RepositoryError>>>>,
    handle: Option<JoinHandle<()>>,
}

/// A forced stop that has been carried out, or has failed to be.
pub(crate) struct FinishedForceStop {
    pub(crate) vm_id: Uuid,
    pub(crate) vm_name: String,
    /// `Ok` means HCS tore the compute system down: the VM has actually
    /// stopped, and what belonged to its run can be given up.
    pub(crate) result: Result<(), RepositoryError>,
}

/// The forced stops being carried out, by the VM they were made for.
#[derive(Default)]
pub(crate) struct ForceStopWorkers {
    workers: Mutex<Vec<Worker>>,
}

impl ForceStopWorkers {
    /// Carries out `termination` on a thread of its own.
    ///
    /// A VM that is already being forcibly stopped is refused rather than
    /// terminated twice: the second termination would wait behind the first for
    /// the same compute system to go, and the user clicking Force Stop again
    /// means "it is taking long", not "tear it down once more".
    pub(crate) fn start<F>(
        &self,
        vm_id: Uuid,
        vm_name: &str,
        termination: F,
    ) -> Result<(), RepositoryError>
    where
        F: FnOnce() -> Result<(), RepositoryError> + Send + 'static,
    {
        let mut workers = self.lock();
        if workers.iter().any(|worker| worker.vm_id == vm_id) {
            let error = RepositoryError::new(format!(
                "VM \"{vm_name}\" is already being forcibly stopped"
            ));
            tracing::error!("{error}");
            return Err(error);
        }

        let finished = Arc::new(AtomicBool::new(false));
        let outcome = Arc::new(Mutex::new(None));
        let handle = std::thread::Builder::new()
            .name(format!("vmlord-force-stop-{vm_name}"))
            .spawn({
                let finished = Arc::clone(&finished);
                let outcome = Arc::clone(&outcome);
                move || {
                    // Set on the way out however the thread leaves, panic
                    // included: a worker nobody collects is a VM that can never
                    // be forcibly stopped again. Dropped after the outcome is
                    // stored, so whoever sees the flag also sees the answer.
                    let _finish = Finish(finished);
                    let result = termination();
                    *outcome
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(result);
                }
            })
            .map_err(|error| {
                let error = RepositoryError::new(format!(
                    "the thread forcibly stopping VM \"{vm_name}\" could not be started: {error}"
                ));
                tracing::error!("{error}");
                error
            })?;

        tracing::info!("forcibly stopping VM \"{vm_name}\" in the background");
        workers.push(Worker {
            vm_id,
            vm_name: vm_name.to_owned(),
            finished,
            outcome,
            handle: Some(handle),
        });
        Ok(())
    }

    /// Hands over every termination that has finished since the last call,
    /// joining the thread that carried it.
    pub(crate) fn take_finished(&self) -> Vec<FinishedForceStop> {
        let mut workers = self.lock();
        let mut finished = Vec::new();
        let mut index = 0;
        while index < workers.len() {
            if workers[index].finished.load(Ordering::Relaxed) {
                finished.push(collect(workers.remove(index)));
            } else {
                index += 1;
            }
        }
        finished
    }

    /// Waits until every termination in flight has finished, leaving the
    /// outcomes where [`ForceStopWorkers::take_finished`] will find them.
    ///
    /// Tests only: in production the outcomes are collected on the next
    /// refresh, which is a second away and on the thread that can act on them.
    #[cfg(test)]
    pub(crate) fn wait_until_answered(&self) {
        loop {
            let answered = self
                .lock()
                .iter()
                .all(|worker| worker.finished.load(Ordering::Relaxed));
            if answered {
                return;
            }
            std::thread::yield_now();
        }
    }

    /// Waits for every termination still in flight.
    ///
    /// Called as VMLord shuts down. A termination in progress holds a handle to
    /// a compute system, so leaving without it would tear that handle down
    /// under the thread using it. The wait is bounded by the force stop
    /// pipeline's own timeout, which is what a wedged Host Compute Service costs
    /// here.
    pub(crate) fn join_all(&self) {
        for worker in self.lock().drain(..) {
            let vm_name = worker.vm_name.clone();
            if let Err(error) = collect(worker).result {
                tracing::warn!("VM \"{vm_name}\" was not forcibly stopped: {error}");
            }
        }
    }

    /// Recovers a poisoned lock rather than propagating the panic: a worker
    /// that panicked must not take the repository down with it.
    fn lock(&self) -> MutexGuard<'_, Vec<Worker>> {
        self.workers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Joins a worker's thread and reads the outcome it parked.
fn collect(mut worker: Worker) -> FinishedForceStop {
    if let Some(handle) = worker.handle.take()
        && handle.join().is_err()
    {
        tracing::error!(
            "the thread forcibly stopping VM \"{}\" panicked",
            worker.vm_name
        );
    }
    let result = worker
        .outcome
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
        // A worker parks its outcome before it marks itself finished, so the
        // only way to have none is to have died without producing one.
        .unwrap_or_else(|| {
            Err(RepositoryError::new(format!(
                "the thread forcibly stopping VM \"{}\" ended without an outcome",
                worker.vm_name
            )))
        });
    FinishedForceStop {
        vm_id: worker.vm_id,
        vm_name: worker.vm_name,
        result,
    }
}

/// Marks a termination as finished as the thread carrying it is dropped,
/// however it left.
struct Finish(Arc<AtomicBool>);

impl Drop for Finish {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex,
        mpsc::{Receiver, Sender, channel},
    };

    use uuid::Uuid;
    use vmlord_core::RepositoryError;

    use super::ForceStopWorkers;

    /// A termination that blocks until the test lets it finish.
    fn blocking_termination(
        result: Result<(), RepositoryError>,
    ) -> (
        impl FnOnce() -> Result<(), RepositoryError> + Send + 'static,
        Sender<()>,
    ) {
        let (release, released): (Sender<()>, Receiver<()>) = channel();
        (
            move || {
                let _ = released.recv();
                result
            },
            release,
        )
    }

    #[test]
    fn a_termination_runs_off_the_callers_thread() {
        // The point of the whole module: the caller is the UI thread, and it
        // must come back before the termination does.
        let workers = ForceStopWorkers::default();
        let (termination, release) = blocking_termination(Ok(()));

        workers
            .start(Uuid::from_u128(1), "dev", termination)
            .expect("the termination should be dispatched");

        assert!(
            workers.take_finished().is_empty(),
            "the termination cannot have finished: nothing has released it yet"
        );
        release.send(()).expect("the worker should be waiting");
        workers.join_all();
    }

    #[test]
    fn a_vm_already_being_forcibly_stopped_is_not_terminated_twice() {
        let workers = ForceStopWorkers::default();
        let (termination, release) = blocking_termination(Ok(()));
        workers
            .start(Uuid::from_u128(2), "dev", termination)
            .expect("the first termination should be dispatched");

        let error = workers
            .start(Uuid::from_u128(2), "dev", || Ok(()))
            .expect_err("a second termination must be refused");

        assert!(error.to_string().contains("dev"), "{error}");
        assert!(error.to_string().contains("already"), "{error}");
        release.send(()).expect("the worker should be waiting");
        workers.join_all();
    }

    #[test]
    fn another_vm_may_be_forcibly_stopped_at_the_same_time() {
        let workers = ForceStopWorkers::default();
        let (termination, release) = blocking_termination(Ok(()));
        workers
            .start(Uuid::from_u128(3), "dev", termination)
            .expect("the first termination should be dispatched");

        workers
            .start(Uuid::from_u128(4), "build", || Ok(()))
            .expect("a different VM has its own compute system to tear down");

        release.send(()).expect("the worker should be waiting");
        workers.join_all();
    }

    #[test]
    fn a_failed_termination_is_handed_over_with_its_reason() {
        let workers = ForceStopWorkers::default();
        workers
            .start(Uuid::from_u128(5), "dev", || {
                Err(RepositoryError::new("injected termination failure"))
            })
            .expect("the termination should be dispatched");

        let finished = wait_for_one(&workers);
        assert_eq!(finished.vm_id, Uuid::from_u128(5));
        assert_eq!(finished.vm_name, "dev");
        let error = finished.result.expect_err("the termination failed");
        assert!(error.to_string().contains("injected termination failure"));
    }

    #[test]
    fn a_finished_termination_is_handed_over_once_and_frees_the_vm() {
        let workers = ForceStopWorkers::default();
        workers
            .start(Uuid::from_u128(6), "dev", || Ok(()))
            .expect("the termination should be dispatched");

        wait_for_one(&workers)
            .result
            .expect("the termination finished");

        assert!(
            workers.take_finished().is_empty(),
            "a collected termination is gone rather than reported twice"
        );
        // The VM is no longer being forcibly stopped, so it can be terminated
        // again -- a first termination that somehow failed can be retried.
        workers
            .start(Uuid::from_u128(6), "dev", || Ok(()))
            .expect("the VM may be terminated again once its termination is over");
        workers.join_all();
    }

    #[test]
    fn a_worker_that_panicked_is_collected_rather_than_left_in_flight() {
        let workers = ForceStopWorkers::default();
        let panics = Arc::new(Mutex::new(()));
        workers
            .start(Uuid::from_u128(7), "dev", move || {
                let _guard = panics;
                panic!("the force stop thread died");
            })
            .expect("the termination should be dispatched");

        let finished = wait_for_one(&workers);
        let error = finished.result.expect_err("a dead thread produced nothing");
        assert!(error.to_string().contains("dev"), "{error}");
    }

    /// Waits for the single termination in flight to finish.
    fn wait_for_one(workers: &ForceStopWorkers) -> super::FinishedForceStop {
        loop {
            let mut finished = workers.take_finished();
            if let Some(one) = finished.pop() {
                return one;
            }
            std::thread::yield_now();
        }
    }
}
