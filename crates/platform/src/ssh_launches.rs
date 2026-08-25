//! The interactive SSH sessions being opened right now, one thread each.
//!
//! Opening a session is not the instant thing a click looks like: the guest's
//! address comes from HNS, the port is probed with a three-second timeout, and
//! a terminal host is started -- and all of it used to run on the caller's
//! thread, which is the UI's. A guest that had stopped answering froze the
//! window for the whole probe.
//!
//! Modelled on `shutdown_workers::ShutdownWorkers`, minus the part that carries
//! an answer back: a launch has nothing to tell the repository. What it has to
//! say -- the command a session opened with, or the reason none did -- goes
//! straight into the diagnostics buffer the UI already reads, from the thread
//! that learned it.

use std::{
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
};

use vmlord_core::RepositoryError;

/// One session being opened.
struct Worker {
    vm_name: String,
    /// Set by the worker as it leaves, by whichever exit -- returning or
    /// panicking -- so that a thread that died is still joined rather than
    /// left in the list forever.
    finished: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

/// The launches in flight.
///
/// Not keyed by VM: two shells into one guest is an ordinary thing to want, and
/// a second click while the first launch is still probing is a second session,
/// not a duplicate to refuse.
#[derive(Default)]
pub(crate) struct SshLaunches {
    workers: Mutex<Vec<Worker>>,
}

impl SshLaunches {
    /// Runs `launch` on a thread of its own.
    ///
    /// The threads that have already finished are joined first, so a session
    /// opened an hour ago is not still holding a handle: there is no refresh
    /// tick collecting these, because there is no outcome for one to collect.
    pub(crate) fn start<F>(&self, vm_name: &str, launch: F) -> Result<(), RepositoryError>
    where
        F: FnOnce() + Send + 'static,
    {
        let mut workers = self.lock();
        join_finished(&mut workers);

        let finished = Arc::new(AtomicBool::new(false));
        let handle = std::thread::Builder::new()
            .name(format!("vmlord-ssh-{vm_name}"))
            .spawn({
                let finished = Arc::clone(&finished);
                move || {
                    let _finish = Finish(finished);
                    launch();
                }
            })
            .map_err(|error| {
                let error = RepositoryError::new(format!(
                    "the thread opening an SSH session to VM \"{vm_name}\" could not be \
                     started: {error}"
                ));
                tracing::error!("{error}");
                error
            })?;

        tracing::debug!("opening an SSH session to VM \"{vm_name}\" in the background");
        workers.push(Worker {
            vm_name: vm_name.to_owned(),
            finished,
            handle: Some(handle),
        });
        Ok(())
    }

    /// Waits for every launch still in flight.
    ///
    /// Called as VMLord shuts down. A launch spends at most its probe timeout
    /// and the start of a terminal, and the session it opens outlives this
    /// process anyway -- it is a window of its own.
    pub(crate) fn join_all(&self) {
        for worker in self.lock().drain(..) {
            join(worker);
        }
    }

    /// Waits until every launch has run, so that a test can look at what they
    /// left behind.
    #[cfg(test)]
    pub(crate) fn wait_until_opened(&self) {
        loop {
            let done = self
                .lock()
                .iter()
                .all(|worker| worker.finished.load(Ordering::Relaxed));
            if done {
                return;
            }
            std::thread::yield_now();
        }
    }

    /// Recovers a poisoned lock rather than propagating the panic: a launch
    /// that panicked must not take the repository down with it.
    fn lock(&self) -> MutexGuard<'_, Vec<Worker>> {
        self.workers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Joins and drops every worker that has left.
fn join_finished(workers: &mut Vec<Worker>) {
    let mut index = 0;
    while index < workers.len() {
        if workers[index].finished.load(Ordering::Relaxed) {
            join(workers.remove(index));
        } else {
            index += 1;
        }
    }
}

/// Joins one worker's thread, reporting a panic rather than re-raising it.
fn join(mut worker: Worker) {
    if let Some(handle) = worker.handle.take()
        && handle.join().is_err()
    {
        tracing::error!(
            "the thread opening an SSH session to VM \"{}\" panicked",
            worker.vm_name
        );
    }
}

/// Marks a worker finished however its thread leaves.
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
        atomic::{AtomicBool, Ordering},
    };

    use super::SshLaunches;

    #[test]
    fn a_launch_runs_on_a_thread_of_its_own_and_is_waited_for() {
        let launches = SshLaunches::default();
        let opened = Arc::new(Mutex::new(Vec::new()));

        for name in ["dev", "test"] {
            let opened = Arc::clone(&opened);
            launches
                .start(name, move || opened.lock().unwrap().push(name))
                .expect("a thread can be started");
        }
        launches.join_all();

        let mut opened = opened.lock().unwrap().clone();
        opened.sort_unstable();
        assert_eq!(opened, ["dev", "test"]);
    }

    /// The caller returns before the work does -- that is the whole point of
    /// the thread, and the reason the UI keeps repainting.
    #[test]
    fn starting_a_launch_does_not_wait_for_it() {
        let launches = SshLaunches::default();
        let release = Arc::new(AtomicBool::new(false));
        let held = Arc::clone(&release);

        launches
            .start("dev", move || {
                while !held.load(Ordering::Relaxed) {
                    std::thread::yield_now();
                }
            })
            .expect("a thread can be started");

        release.store(true, Ordering::Relaxed);
        launches.join_all();
    }

    /// A launch that panicked must be joined rather than left in the list, and
    /// must not poison the repository that outlives it.
    #[test]
    fn a_launch_that_panicked_is_collected_rather_than_left_in_flight() {
        let launches = SshLaunches::default();

        launches
            .start("dev", || panic!("the terminal exploded"))
            .expect("a thread can be started");
        launches.wait_until_opened();
        launches
            .start("dev", || {})
            .expect("the panicked worker is joined rather than in the way");
        launches.join_all();
    }
}
