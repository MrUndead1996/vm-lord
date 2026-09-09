//! The reboots in flight right now, one thread each.
//!
//! Asking a guest to reboot takes as long as the asking does -- HCS delivering
//! the request, or the agent's session serving the ask behind whatever work is
//! already on its one socket, which can be minutes of kernel build -- and it
//! must not take that on the caller's thread, which is the UI's: the window
//! would stop repainting until the answer came back. Here every request gets a
//! thread of its own and the repository collects the outcome on its next
//! refresh.
//!
//! Modelled on [`crate::shutdown_workers::ShutdownWorkers`], for the same
//! reasons: a flag the worker sets however it leaves, the outcome parked for
//! the main thread, and a join for every thread before the process goes. It is
//! kept separate from the graceful shutdowns so that stopping a VM that is
//! being asked to reboot -- the escalation when a reboot will not do -- is not
//! mistaken for asking the same VM twice.

use std::{
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
};

use uuid::Uuid;
use vmlord_core::RepositoryError;

/// One reboot request being delivered.
struct Worker {
    vm_id: Uuid,
    vm_name: String,
    /// Set by the worker as it leaves, by whichever exit -- returning or
    /// panicking -- so that a request whose thread died is still collected.
    finished: Arc<AtomicBool>,
    /// Filled in by the worker before it marks itself finished, and emptied by
    /// the collection that removes this worker.
    outcome: Arc<Mutex<Option<Result<(), RepositoryError>>>>,
    handle: Option<JoinHandle<()>>,
}

/// A reboot request that has been accepted, or has failed to be.
///
/// Named by VM name alone, because that is all its reader says: unlike a
/// shutdown's collection, nothing here is torn down by id.
pub(crate) struct FinishedReboot {
    pub(crate) vm_name: String,
    /// `Ok` means the request was accepted somewhere -- HCS delivered it, or
    /// the guest's own agent took it -- not that the guest has gone down or
    /// come back.
    pub(crate) result: Result<(), RepositoryError>,
}

/// The reboot requests being delivered, by the VM they were made for.
#[derive(Default)]
pub(crate) struct RebootWorkers {
    workers: Mutex<Vec<Worker>>,
}

impl RebootWorkers {
    /// Delivers `request` on a thread of its own.
    ///
    /// A VM that is already being asked to reboot is refused rather than asked
    /// twice: the second request would wait behind the first for the same
    /// answer, and the user clicking Reboot again means "it is taking long",
    /// not "ask once more".
    pub(crate) fn start<F>(
        &self,
        vm_id: Uuid,
        vm_name: &str,
        request: F,
    ) -> Result<(), RepositoryError>
    where
        F: FnOnce() -> Result<(), RepositoryError> + Send + 'static,
    {
        let mut workers = self.lock();
        if workers.iter().any(|worker| worker.vm_id == vm_id) {
            let error = RepositoryError::new(format!("VM \"{vm_name}\" is already being rebooted"));
            tracing::error!("{error}");
            return Err(error);
        }

        let finished = Arc::new(AtomicBool::new(false));
        let outcome = Arc::new(Mutex::new(None));
        let handle = std::thread::Builder::new()
            .name(format!("vmlord-reboot-{vm_name}"))
            .spawn({
                let finished = Arc::clone(&finished);
                let outcome = Arc::clone(&outcome);
                move || {
                    // Set on the way out however the thread leaves, panic
                    // included: a worker nobody collects is a VM that can never
                    // be asked to reboot again. Dropped after the outcome is
                    // stored, so whoever sees the flag also sees the answer.
                    let _finish = Finish(finished);
                    let result = request();
                    *outcome
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(result);
                }
            })
            .map_err(|error| {
                let error = RepositoryError::new(format!(
                    "the thread rebooting VM \"{vm_name}\" could not be started: {error}"
                ));
                tracing::error!("{error}");
                error
            })?;

        tracing::info!("asking VM \"{vm_name}\" to reboot in the background");
        workers.push(Worker {
            vm_id,
            vm_name: vm_name.to_owned(),
            finished,
            outcome,
            handle: Some(handle),
        });
        Ok(())
    }

    /// Hands over every request that has been answered since the last call,
    /// joining the thread that carried it.
    pub(crate) fn take_finished(&self) -> Vec<FinishedReboot> {
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

    /// Waits until every request in flight has been answered, leaving the
    /// answers where [`RebootWorkers::take_finished`] will find them.
    ///
    /// Tests only: in production the answers are collected on the next refresh,
    /// which is a second away and on the thread that can act on them.
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

    /// Waits for every request still in flight.
    ///
    /// Called as VMLord shuts down. A request being delivered holds a handle to
    /// a compute system, so leaving without it would tear that handle down
    /// under the thread using it. The wait is bounded by the pipeline's own
    /// timeouts -- HCS's minute, or the agent ask's longer budget -- which is
    /// what a slow answer costs here.
    pub(crate) fn join_all(&self) {
        for worker in self.lock().drain(..) {
            let vm_name = worker.vm_name.clone();
            if let Err(error) = collect(worker).result {
                tracing::warn!("VM \"{vm_name}\" was not asked to reboot: {error}");
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

/// Joins a worker's thread and reads the answer it parked.
fn collect(mut worker: Worker) -> FinishedReboot {
    if let Some(handle) = worker.handle.take()
        && handle.join().is_err()
    {
        tracing::error!(
            "the thread asking VM \"{}\" to reboot panicked",
            worker.vm_name
        );
    }
    let result = worker
        .outcome
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
        // A worker parks its answer before it marks itself finished, so the
        // only way to have none is to have died without producing one.
        .unwrap_or_else(|| {
            Err(RepositoryError::new(format!(
                "the thread asking VM \"{}\" to reboot ended without an answer",
                worker.vm_name
            )))
        });
    FinishedReboot {
        vm_name: worker.vm_name,
        result,
    }
}

/// Marks a request as answered as the thread carrying it is dropped, however it
/// left.
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

    use super::RebootWorkers;

    /// A request that blocks until the test lets it answer.
    fn blocking_request(
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
    fn a_request_is_delivered_off_the_callers_thread() {
        // The point of the whole module: the caller is the UI thread, and it
        // must come back before the answer does.
        let workers = RebootWorkers::default();
        let (request, release) = blocking_request(Ok(()));

        workers
            .start(Uuid::from_u128(1), "dev", request)
            .expect("the request should be dispatched");

        assert!(
            workers.take_finished().is_empty(),
            "the request cannot have been answered: nothing has released it yet"
        );
        release.send(()).expect("the worker should be waiting");
        workers.join_all();
    }

    #[test]
    fn a_vm_already_being_rebooted_is_not_asked_twice() {
        let workers = RebootWorkers::default();
        let (request, release) = blocking_request(Ok(()));
        workers
            .start(Uuid::from_u128(2), "dev", request)
            .expect("the first request should be dispatched");

        let error = workers
            .start(Uuid::from_u128(2), "dev", || Ok(()))
            .expect_err("a second request must be refused");

        assert!(error.to_string().contains("dev"), "{error}");
        assert!(error.to_string().contains("already"), "{error}");
        release.send(()).expect("the worker should be waiting");
        workers.join_all();
    }

    #[test]
    fn another_vm_may_be_rebooted_at_the_same_time() {
        let workers = RebootWorkers::default();
        let (request, release) = blocking_request(Ok(()));
        workers
            .start(Uuid::from_u128(3), "dev", request)
            .expect("the first request should be dispatched");

        workers
            .start(Uuid::from_u128(4), "build", || Ok(()))
            .expect("a different VM has its own request to make");

        release.send(()).expect("the worker should be waiting");
        workers.join_all();
    }

    #[test]
    fn a_failed_request_is_handed_over_with_its_reason() {
        let workers = RebootWorkers::default();
        workers
            .start(Uuid::from_u128(5), "dev", || {
                Err(RepositoryError::new("injected reboot failure"))
            })
            .expect("the request should be dispatched");

        let finished = wait_for_one(&workers);
        assert_eq!(finished.vm_name, "dev");
        let error = finished.result.expect_err("the request failed");
        assert!(error.to_string().contains("injected reboot failure"));
    }

    #[test]
    fn an_answered_request_is_handed_over_once_and_frees_the_vm() {
        let workers = RebootWorkers::default();
        workers
            .start(Uuid::from_u128(6), "dev", || Ok(()))
            .expect("the request should be dispatched");

        wait_for_one(&workers)
            .result
            .expect("the request was accepted");

        assert!(
            workers.take_finished().is_empty(),
            "a collected request is gone rather than reported twice"
        );
        // The VM is no longer being rebooted, so it can be asked again -- a
        // guest that never acted on the first request is rebooted by a second.
        workers
            .start(Uuid::from_u128(6), "dev", || Ok(()))
            .expect("the VM may be asked again once its request is over");
        workers.join_all();
    }

    #[test]
    fn a_worker_that_panicked_is_collected_rather_than_left_in_flight() {
        let workers = RebootWorkers::default();
        let panics = Arc::new(Mutex::new(()));
        workers
            .start(Uuid::from_u128(7), "dev", move || {
                let _guard = panics;
                panic!("the reboot thread died");
            })
            .expect("the request should be dispatched");

        let finished = wait_for_one(&workers);
        let error = finished.result.expect_err("a dead thread answered nothing");
        assert!(error.to_string().contains("dev"), "{error}");
    }

    /// Waits for the single request in flight to be answered.
    fn wait_for_one(workers: &RebootWorkers) -> super::FinishedReboot {
        loop {
            let mut finished = workers.take_finished();
            if let Some(one) = finished.pop() {
                return one;
            }
            std::thread::yield_now();
        }
    }
}
