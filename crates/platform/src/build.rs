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
    BuildMonitor, BuildStep, GpuMode, RepositoryError, VmCreateRequest, VmSource, VmState,
    VmSummary,
};

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
    worker: Option<JoinHandle<()>>,
}

/// The VMs being created, by name.
#[derive(Default)]
pub(crate) struct BuildRegistry {
    builds: Mutex<HashMap<String, Build>>,
}

impl BuildRegistry {
    /// Whether a VM of this name is being created right now.
    pub(crate) fn contains(&self, name: &str) -> bool {
        self.lock().contains_key(name)
    }

    /// Starts `build` on a thread of its own, listing the VM as building until
    /// it returns.
    ///
    /// `build` must not touch the registry: it runs while nothing holds the
    /// lock, but the entry it belongs to is inserted by the caller of this
    /// function while the lock is held.
    pub(crate) fn start<F>(
        &self,
        request: VmCreateRequest,
        build: F,
    ) -> Result<(), RepositoryError>
    where
        F: FnOnce(&BuildMonitor) + Send + 'static,
    {
        let mut builds = self.lock();
        if builds.contains_key(&request.name) {
            let error = RepositoryError::new(format!(
                "VM \"{}\" is already being created",
                request.name
            ));
            log::error!("{error}");
            return Err(error);
        }

        let monitor = BuildMonitor::new(first_step(&request.source));
        let finished = Arc::new(AtomicBool::new(false));
        let worker = std::thread::Builder::new()
            .name(format!("vmlord-build-{}", request.name))
            .spawn({
                let monitor = monitor.clone();
                let finished = Arc::clone(&finished);
                move || {
                    // Set on the way out however the build leaves, panic
                    // included: an entry nobody clears is a row that never
                    // goes away.
                    let _finish = Finish(finished);
                    build(&monitor);
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
                worker: Some(worker),
            },
        );
        Ok(())
    }

    /// The VMs being created, as the list shows them.
    ///
    /// Sizes come from the request rather than from disk, because nothing of
    /// the VM is on disk yet to read them from.
    pub(crate) fn summaries(&self) -> Vec<VmSummary> {
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
                network_mode: build.request.network_mode,
                // A VM that does not exist answers nowhere.
                ip_address: None,
                ssh_port: None,
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
    pub(crate) fn reap(&self) {
        let mut builds = self.lock();
        let done: Vec<String> = builds
            .iter()
            .filter(|(_, build)| build.finished.load(Ordering::Relaxed))
            .map(|(name, _)| name.clone())
            .collect();
        for name in done {
            if let Some(mut build) = builds.remove(&name)
                && let Some(worker) = build.worker.take()
                && worker.join().is_err()
            {
                log::error!("the thread creating VM \"{name}\" panicked");
            }
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
        BuildStep, CloudImage, GpuMode, NetworkMode, Provisioning, SshAccess, VmCreateRequest,
        VmSource, VmState,
    };

    use super::BuildRegistry;

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
                    ssh: SshAccess::Enabled { deploy_key: true },
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
