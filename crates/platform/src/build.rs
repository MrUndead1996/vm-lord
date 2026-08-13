//! The VMs being created right now, and the threads creating them.
//!
//! Creating a VM takes minutes -- an image is fetched, a disk is written, a
//! compute system is made -- and it used to take them on the caller's thread,
//! which is the UI's. Here each creation gets a thread of its own, and the
//! registry is what the UI sees instead: a VM that exists as a build and not
//! yet as a VM.
//!
//! Modelled on `dhcp::DhcpService`, the only other background thread in
//! VMLord: a shared flag to stop by, a handle to join, and a `Drop` that does
//! both. There is no async runtime here and none anywhere in the project.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
};

use vmlord_core::{
    BuildMonitor, BuildStep, GpuMode, RepositoryError, SshAvailability, VmCreateRequest, VmGpuFacts,
    VmSource,
    VmState, VmSummary,
};

use crate::{com1_terminal::Com1Session, metadata::VmComputeSystemMapping};

/// A VM whose creation got as far as starting it: what the main thread has to
/// take over from the build thread.
///
/// The COM1 session and the compute system a start produces belong to the
/// repository, and the repository is only reachable behind `&mut self` -- so
/// the build thread parks them here and a reap hands them over.
pub(crate) struct StartedVm {
    pub(crate) mapping: VmComputeSystemMapping,
    pub(crate) session: Com1Session,
}

/// Every VM VMLord creates today is a Linux guest.
const OS_TYPE: &str = "Linux";

/// One VM being created.
struct Build {
    monitor: BuildMonitor,
    /// What the VM will be, for listing it before it is.
    request: VmCreateRequest,
    /// Set by the worker as it leaves, by whichever exit -- returning or
    /// panicking -- so that a build that died still stops being listed.
    finished: Arc<AtomicBool>,
    /// Filled in by the worker before it marks itself finished, and emptied by
    /// the reap that removes this build.
    outcome: Arc<Mutex<Option<StartedVm>>>,
    worker: Option<JoinHandle<()>>,
}

/// The VMs being created, by name.
#[derive(Default)]
pub(crate) struct BuildRegistry {
    builds: Mutex<HashMap<String, Build>>,
    /// What finished builds started, waiting for the main thread to take it
    /// over. Kept here rather than returned by `reap`, which runs inside every
    /// query -- a returned session that a caller dropped would silently cancel
    /// the console reader of a running VM.
    started: Mutex<Vec<StartedVm>>,
}

impl BuildRegistry {
    /// Whether a VM of this name is being created right now.
    pub(crate) fn contains(&self, name: &str) -> bool {
        // Before answering, not after: a build that is over holds neither a row
        // nor its name, and the caller asking is the one that would otherwise
        // refuse a retry of a build the user can no longer see.
        self.reap();
        self.lock().contains_key(name)
    }

    /// Starts `build` on a thread of its own, listing the VM as building until
    /// it returns.
    ///
    /// `build` must not touch the registry: it runs while nothing holds the
    /// lock, but the entry it belongs to is inserted by the caller of this
    /// function while the lock is held.
    pub(crate) fn start<F>(&self, request: VmCreateRequest, build: F) -> Result<(), RepositoryError>
    where
        F: FnOnce(&BuildMonitor) -> Option<StartedVm> + Send + 'static,
    {
        let mut builds = self.lock();
        if builds.contains_key(&request.name) {
            let error =
                RepositoryError::new(format!("VM \"{}\" is already being created", request.name));
            log::error!("{error}");
            return Err(error);
        }

        let monitor = BuildMonitor::new(first_step(&request.source));
        let finished = Arc::new(AtomicBool::new(false));
        let outcome = Arc::new(Mutex::new(None));
        let worker = std::thread::Builder::new()
            .name(format!("vmlord-build-{}", request.name))
            .spawn({
                let monitor = monitor.clone();
                let finished = Arc::clone(&finished);
                let outcome = Arc::clone(&outcome);
                move || {
                    // Set on the way out however the build leaves, panic
                    // included: an entry nobody clears is a row that never
                    // goes away. Dropped after the outcome is stored, so a reap
                    // that sees the flag also sees what the build produced.
                    let _finish = Finish(finished);
                    if let Some(started) = build(&monitor) {
                        *outcome
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(started);
                    }
                }
            })
            .map_err(|error| {
                let error = RepositoryError::new(format!(
                    "the thread creating VM \"{}\" could not be started: {error}",
                    request.name
                ));
                log::error!("{error}");
                error
            })?;

        log::info!("started creating VM \"{}\" in the background", request.name);
        builds.insert(
            request.name.clone(),
            Build {
                monitor,
                request,
                finished,
                outcome,
                worker: Some(worker),
            },
        );
        Ok(())
    }

    /// The VMs being created, as the list shows them.
    ///
    /// Sizes come from the request rather than from disk, because nothing of
    /// the VM is on disk yet to read them from.
    ///
    /// Builds that are over are cleared first, so a row disappears when its
    /// thread ends rather than when something next asks for diagnostics. A
    /// listing that lags behind is worse than late here: a build that succeeded
    /// has already written itself to the metadata store, so until it is cleared
    /// the same VM is in the list twice.
    pub(crate) fn summaries(&self) -> Vec<VmSummary> {
        self.reap();
        self.lock()
            .values()
            .map(|build| VmSummary {
                name: build.request.name.clone(),
                os_type: OS_TYPE.to_owned(),
                state: VmState::Building {
                    progress: build.monitor.snapshot(),
                },
                ram_mb: build.request.ram_mb,
                disk_gb: build.request.disk_gb,
                cpu_cores: build.request.cpu_cores,
                gpu_mode: GpuMode::None,
                // Nothing has been attached to a VM that is still being built.
                gpu: VmGpuFacts::default(),
                network_mode: build.request.network_mode,
                // A VM that does not exist answers nowhere.
                ip_address: None,
                // Whatever it was asked for, there is nothing to connect to
                // until the build has written the VM down.
                ssh: SshAvailability::Disabled,
            })
            .collect()
    }

    /// Asks the build of `name` to stop at its next checkpoint.
    ///
    /// Returning here does not mean the build is over: it means it has been
    /// told. The build rolls itself back and disappears from the list on its
    /// own.
    pub(crate) fn cancel(&self, name: &str) -> Result<(), RepositoryError> {
        let builds = self.lock();
        let Some(build) = builds.get(name) else {
            let error = RepositoryError::new(format!("VM \"{name}\" is not being created"));
            log::error!("{error}");
            return Err(error);
        };
        log::warn!("cancelling the creation of VM \"{name}\"");
        build.monitor.cancel();
        Ok(())
    }

    /// Refuses an operation on a VM that is still being created.
    ///
    /// "Not found" would be the wrong answer and the confusing one: the VM is
    /// in the list the user is looking at.
    pub(crate) fn refuse_if_building(&self, name: &str) -> Result<(), RepositoryError> {
        if !self.contains(name) {
            return Ok(());
        }
        let error = RepositoryError::new(format!("VM \"{name}\" is still being created"));
        log::error!("{error}");
        Err(error)
    }

    /// Removes and joins the builds that have finished.
    ///
    /// Joining a thread that has already left is immediate, and it is the only
    /// place its result is collected: a build reports what it did through the
    /// diagnostics, so there is nothing here to read but the end of the thread.
    ///
    /// Every query runs this first, so it must be called without the lock held
    /// -- it takes the lock itself, and a `Mutex` is not reentrant. Joining
    /// under the lock is safe because a build never touches the registry.
    pub(crate) fn reap(&self) {
        let mut builds = self.lock();
        let done: Vec<String> = builds
            .iter()
            .filter(|(_, build)| build.finished.load(Ordering::Relaxed))
            .map(|(name, _)| name.clone())
            .collect();
        for name in done {
            let Some(mut build) = builds.remove(&name) else {
                continue;
            };
            self.collect(&build);
            if let Some(worker) = build.worker.take()
                && worker.join().is_err()
            {
                log::error!("the thread creating VM \"{name}\" panicked");
            }
        }
    }

    /// Hands over the VMs that builds have started since this was last called.
    pub(crate) fn take_started(&self) -> Vec<StartedVm> {
        self.reap();
        std::mem::take(
            &mut *self
                .started
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    /// Moves what a finished build started into the registry's own queue.
    fn collect(&self, build: &Build) {
        let started = build
            .outcome
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(started) = started {
            self.started
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(started);
        }
    }

    /// Cancels every build and waits for all of them.
    ///
    /// Called as VMLord shuts down. Leaving without it would either kill a
    /// thread in the middle of writing a VHDX or hang the process waiting for
    /// one that was never told to stop.
    pub(crate) fn cancel_all_and_join(&self) {
        let mut builds = self.lock();
        for build in builds.values() {
            build.monitor.cancel();
        }
        for (name, mut build) in builds.drain() {
            self.collect(&build);
            if let Some(worker) = build.worker.take()
                && worker.join().is_err()
            {
                log::error!("the thread creating VM \"{name}\" panicked");
            }
        }
    }

    /// Recovers a poisoned lock rather than propagating the panic: a build
    /// that panicked must not take the list of VMs down with it.
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Build>> {
        self.builds
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The step a build of this source begins at, for the moments before its
/// thread has reported one of its own.
fn first_step(source: &VmSource) -> BuildStep {
    match source {
        VmSource::CloudImage { .. } => BuildStep::Downloading,
        VmSource::LocalMedia { .. } => BuildStep::WritingDisk,
    }
}

/// Marks a build as over as it is dropped, however its thread left.
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

    use vmlord_core::{
        BuildStep, CloudImage, GpuMode, NetworkMode, Provisioning, SshAccess, SshPort,
        VmCreateRequest, VmSource, VmState,
    };

    use uuid::Uuid;

    use super::{BuildRegistry, StartedVm};
    use crate::{com1_terminal::Com1Launcher, metadata::VmComputeSystemMapping};

    fn mapping(name: &str) -> VmComputeSystemMapping {
        VmComputeSystemMapping {
            vm_id: Uuid::from_u128(u128::from(name.len() as u64) + 1),
            vm_name: name.to_owned(),
            hcs_compute_system_id: format!("vmlord-{name}"),
            disk_gb: 20,
            endpoint_id: None,
            network_mode: NetworkMode::None,
            ssh: None,
        }
    }

    fn started(name: &str) -> StartedVm {
        let mapping = mapping(name);
        StartedVm {
            session: Com1Launcher::session_for_test(&mapping),
            mapping,
        }
    }

    #[test]
    fn a_build_hands_its_started_vm_to_whoever_reaps_it() {
        // The session and the compute-system handle live on the main thread, so
        // the build thread parks them here rather than holding them.
        let registry = BuildRegistry::default();

        registry
            .start(request("dev"), move |_| Some(started("dev")))
            .expect("the build should start");
        // `reap` runs inside every query, so the outcome has to survive one:
        // dropping a session is what cancels its reader.
        while registry.contains("dev") {
            std::thread::yield_now();
        }
        registry.reap();

        let handed = registry.take_started();

        assert_eq!(handed.len(), 1);
        assert_eq!(handed[0].mapping.vm_name, "dev");
        assert!(
            registry.take_started().is_empty(),
            "an outcome is handed over once"
        );
    }

    #[test]
    fn a_build_that_hands_back_nothing_leaves_nothing_to_collect() {
        let registry = BuildRegistry::default();

        registry
            .start(request("rolled-back"), |_| None)
            .expect("the build should start");
        registry.cancel_all_and_join();

        assert!(registry.take_started().is_empty());
    }

    fn request(name: &str) -> VmCreateRequest {
        VmCreateRequest {
            name: name.into(),
            source: VmSource::CloudImage {
                image: CloudImage {
                    profile: vmlord_core::ubuntu(),
                    release: "24.04".into(),
                },
                provisioning: Provisioning {
                    username: "dev".into(),
                    password: None,
                    ssh: SshAccess::Enabled {
                        deploy_key: true,
                        port: SshPort::DEFAULT,
                    },
                    locale: "en_US.UTF-8".into(),
                    keyboard: "us".into(),
                    timezone: "Europe/Moscow".into(),
                },
            },
            ram_mb: 2048,
            disk_gb: 20,
            cpu_cores: 2,
            gpu_mode: GpuMode::None,
            network_mode: NetworkMode::None,
        }
    }

    #[test]
    fn a_started_build_is_listed_as_building_until_it_finishes() {
        let registry = BuildRegistry::default();
        let release = Arc::new(AtomicBool::new(false));
        let held = Arc::clone(&release);
        registry
            .start(request("dev"), move |monitor| {
                monitor.report(BuildStep::WritingDisk);
                while !held.load(Ordering::Relaxed) {
                    std::thread::yield_now();
                }
                None
            })
            .expect("the build should start");

        let summaries = registry.summaries();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].name, "dev");
        assert_eq!(summaries[0].ram_mb, 2048);
        assert_eq!(summaries[0].disk_gb, 20);
        assert_eq!(summaries[0].cpu_cores, 2);
        assert_eq!(summaries[0].ip_address, None);
        assert!(matches!(summaries[0].state, VmState::Building { .. }));

        release.store(true, Ordering::Relaxed);
        registry.cancel_all_and_join();
        registry.reap();

        assert!(registry.summaries().is_empty());
    }

    /// A build that is over must leave the list on its own, without waiting for
    /// anything else to be called.
    ///
    /// It did not: the entry was removed only by `reap`, which runs from
    /// `take_diagnostics`. A cancelled build therefore kept its `Building` row
    /// until something asked for diagnostics, and a build that succeeded was
    /// listed twice in the meantime -- once from the metadata store it had just
    /// been written to, and once from here.
    #[test]
    fn a_finished_build_stops_being_listed_on_its_own() {
        let registry = BuildRegistry::default();
        registry
            .start(request("dev"), |_| None)
            .expect("the build should start");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !registry.summaries().is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "a build whose thread is over must not still be listed"
            );
            std::thread::yield_now();
        }

        assert!(
            !registry.contains("dev"),
            "and the name it held must be free again, or a retry is refused \
             for a build the user can no longer see"
        );
    }

    #[test]
    fn a_second_build_of_the_same_name_is_refused() {
        let registry = BuildRegistry::default();
        let release = Arc::new(AtomicBool::new(false));
        let held = Arc::clone(&release);
        registry
            .start(request("dev"), move |_| {
                while !held.load(Ordering::Relaxed) {
                    std::thread::yield_now();
                }
                None
            })
            .expect("the first build should start");

        let error = registry
            .start(request("dev"), |_| panic!("this build must never run"))
            .expect_err("two builds must not share a name and a directory");

        assert!(error.to_string().contains("dev"), "got {error}");
        release.store(true, Ordering::Relaxed);
        registry.cancel_all_and_join();
    }

    #[test]
    fn cancelling_sets_the_flag_the_build_polls() {
        let registry = BuildRegistry::default();
        let seen = Arc::new(AtomicBool::new(false));
        let reporter = Arc::clone(&seen);
        registry
            .start(request("dev"), move |monitor| {
                while !monitor.is_cancelled() {
                    std::thread::yield_now();
                }
                reporter.store(true, Ordering::Relaxed);
                None
            })
            .expect("the build should start");

        registry
            .cancel("dev")
            .expect("a running build is cancellable");
        registry.cancel_all_and_join();

        assert!(seen.load(Ordering::Relaxed));
    }

    #[test]
    fn cancelling_an_unknown_build_says_so() {
        let registry = BuildRegistry::default();

        let error = registry
            .cancel("ghost")
            .expect_err("there is nothing to cancel");

        assert!(error.to_string().contains("ghost"), "got {error}");
    }

    #[test]
    fn a_panicking_build_is_still_reaped() {
        let registry = BuildRegistry::default();
        registry
            .start(request("dev"), |_| panic!("the build thread panicked"))
            .expect("the build should start");

        registry.cancel_all_and_join();
        registry.reap();

        assert!(
            registry.summaries().is_empty(),
            "a build that panicked is over, and a row for it would never go away"
        );
    }

    #[test]
    fn operations_on_a_building_vm_are_refused_by_name() {
        let registry = BuildRegistry::default();
        let release = Arc::new(AtomicBool::new(false));
        let held = Arc::clone(&release);
        registry
            .start(request("dev"), move |_| {
                while !held.load(Ordering::Relaxed) {
                    std::thread::yield_now();
                }
                None
            })
            .expect("the build should start");

        let error = registry
            .refuse_if_building("dev")
            .expect_err("a VM that does not exist yet cannot be acted on");

        assert!(error.to_string().contains("dev"), "got {error}");
        assert!(
            error.to_string().contains("still being created"),
            "got {error}"
        );
        assert!(registry.refuse_if_building("other").is_ok());

        release.store(true, Ordering::Relaxed);
        registry.cancel_all_and_join();
    }
}
