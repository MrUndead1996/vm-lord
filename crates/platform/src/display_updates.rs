//! The display payload updates running right now, one thread each.
//!
//! An update is the longest thing a person can ask VMLord for: the guest builds
//! a kernel module against its own running kernel with DKMS, and the recipe's
//! budget for one is fifteen minutes. That cannot happen on the thread that
//! draws the window -- the same reason `ssh_launches` exists, arrived at with a
//! much larger number.
//!
//! Keyed by VM, unlike `ssh_launches` and for the opposite reason: two shells
//! into one guest is an ordinary thing to want, while two updates of one VM
//! would publish two versions into the one directory that VM exports and ask
//! one agent session twice. The name is what a second click is refused by, and
//! it is what the list of VMs reads to say that an update is under way.
//!
//! Nothing is carried back to the repository. What an update produces is the
//! display's own facts, which it records into the shared `DisplayRuns`, and one
//! line about how it ended, which it pushes into the diagnostics buffer the UI
//! already reads.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
};

use vmlord_core::RepositoryError;

/// One update in flight.
struct Update {
    /// Set by the worker as it leaves, by whichever exit -- returning or
    /// panicking -- so that an update whose thread died stops being listed
    /// rather than blocking every later one.
    finished: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

/// The display payload updates in flight, by VM name.
#[derive(Default)]
pub(crate) struct DisplayUpdates {
    updates: Mutex<HashMap<String, Update>>,
}

impl DisplayUpdates {
    /// Whether this VM's display payload is being updated right now.
    pub(crate) fn contains(&self, vm_name: &str) -> bool {
        // Before answering, not after: an update that is over holds no row, and
        // the caller asking is the one that would otherwise show a VM as
        // updating forever.
        self.reap();
        self.lock().contains_key(vm_name)
    }

    /// Runs `update` on a thread of its own, listing the VM as updating until
    /// it returns.
    ///
    /// # Errors
    ///
    /// [`RepositoryError`] if this VM is already being updated, or if the
    /// thread cannot be spawned.
    pub(crate) fn start<F>(&self, vm_name: &str, update: F) -> Result<(), RepositoryError>
    where
        F: FnOnce() + Send + 'static,
    {
        self.reap();
        let mut updates = self.lock();
        if updates.contains_key(vm_name) {
            let error = RepositoryError::new(format!(
                "the display payload of VM \"{vm_name}\" is already being updated"
            ));
            tracing::error!("{error}");
            return Err(error);
        }

        let finished = Arc::new(AtomicBool::new(false));
        let worker = std::thread::Builder::new()
            .name(format!("vmlord-display-update-{vm_name}"))
            .spawn({
                let finished = Arc::clone(&finished);
                move || {
                    let _finish = Finish(finished);
                    update();
                }
            })
            .map_err(|error| {
                let error = RepositoryError::new(format!(
                    "the thread updating the display payload of VM \"{vm_name}\" could not be \
                     started: {error}"
                ));
                tracing::error!("{error}");
                error
            })?;

        tracing::info!("updating the display payload of VM \"{vm_name}\" in the background");
        updates.insert(
            vm_name.to_owned(),
            Update {
                finished,
                worker: Some(worker),
            },
        );
        Ok(())
    }

    /// Waits for every update still in flight.
    ///
    /// Called as VMLord shuts down, after the agent sessions have been
    /// cancelled: an update waiting on a guest that no longer has a session
    /// finds its answer channel closed and returns, rather than sitting out the
    /// twenty minutes it was allowed.
    pub(crate) fn join_all(&self) {
        let mut updates = self.lock();
        for (vm_name, mut update) in updates.drain() {
            if let Some(worker) = update.worker.take()
                && worker.join().is_err()
            {
                tracing::error!(
                    "the thread updating the display payload of VM \"{vm_name}\" panicked"
                );
            }
        }
    }

    /// Removes and joins the updates that have finished.
    ///
    /// Called without the lock held -- it takes the lock itself, and a `Mutex`
    /// is not reentrant.
    fn reap(&self) {
        let mut updates = self.lock();
        let done: Vec<String> = updates
            .iter()
            .filter(|(_, update)| update.finished.load(Ordering::Relaxed))
            .map(|(vm_name, _)| vm_name.clone())
            .collect();
        for vm_name in done {
            let Some(mut update) = updates.remove(&vm_name) else {
                continue;
            };
            if let Some(worker) = update.worker.take()
                && worker.join().is_err()
            {
                tracing::error!(
                    "the thread updating the display payload of VM \"{vm_name}\" panicked"
                );
            }
        }
    }

    /// Recovers a poisoned lock rather than propagating the panic: an update
    /// that panicked must not take the list of VMs down with it.
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Update>> {
        self.updates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Marks an update finished as its thread leaves, by whichever exit.
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

    use super::DisplayUpdates;

    /// An update that does not return until the test lets it.
    fn held(release: &Arc<AtomicBool>) -> impl FnOnce() + Send + 'static + use<> {
        let held = Arc::clone(release);
        move || {
            while !held.load(Ordering::Relaxed) {
                std::thread::yield_now();
            }
        }
    }

    #[test]
    fn a_vm_being_updated_is_refused_a_second_update() {
        let updates = DisplayUpdates::default();
        let release = Arc::new(AtomicBool::new(false));
        updates
            .start("dev", held(&release))
            .expect("the first update must be accepted");

        let error = updates
            .start("dev", || panic!("a second update must not run"))
            .expect_err("one directory, one agent session, one update");

        assert!(error.to_string().contains("already"), "{error}");
        release.store(true, Ordering::Relaxed);
        updates.join_all();
    }

    #[test]
    fn a_vm_being_updated_is_listed_as_updating_until_it_is_over() {
        let updates = DisplayUpdates::default();
        let release = Arc::new(AtomicBool::new(false));
        updates.start("dev", held(&release)).expect("accepted");

        assert!(updates.contains("dev"));
        assert!(
            !updates.contains("other"),
            "another VM is not updating because this one is"
        );

        release.store(true, Ordering::Relaxed);
        updates.join_all();
        assert!(!updates.contains("dev"));
    }

    #[test]
    fn an_update_that_panicked_stops_being_listed_and_lets_the_next_one_run() {
        let updates = DisplayUpdates::default();
        updates
            .start("dev", || panic!("the update thread died"))
            .expect("accepted");
        updates.join_all();

        assert!(
            !updates.contains("dev"),
            "a row nobody clears is a VM that can never be updated again"
        );
        updates.start("dev", || ()).expect("accepted a second time");
        updates.join_all();
    }
}
