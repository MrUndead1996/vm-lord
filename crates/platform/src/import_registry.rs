//! The AppSandbox imports running right now, and the threads running them.
//!
//! The same shape as [`crate::build::BuildRegistry`], for the same reasons: an
//! import takes minutes -- a disk is copied, a guest is converted, a VM is
//! booted twice -- and none of that may run on the UI's thread. What differs is
//! what an import leaves behind when it fails. A build that fails leaves
//! nothing; an import that fails after the guest was touched leaves a copied
//! disk and a journal, and those are on disk rather than in this registry, so
//! that they outlive the process that made them.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
};

use vmlord_core::{
    AppSandboxImportProgress, AppSandboxImportRequest, AppSandboxImportStage, BuildMonitor,
    BuildProgress, BuildStep, DesktopProfile, DisplayProvisioning, GpuMode, NetworkMode,
    ProgressPublisher, RepositoryError, SshAvailability, Subsystem, VmDisplayFacts, VmGpuFacts,
    VmState, VmSummary,
};

use crate::{appsandbox::ImportWorkerOutcome, build::StartedVm};

/// Every VM VMLord imports today is a Linux guest.
const OS_TYPE: &str = "Linux";

/// What the VM list can say about an import before the VM exists.
///
/// Taken from the discovered candidate rather than from disk: the copy is what
/// puts the disk there, and until it finishes there is nothing to measure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ImportListing {
    pub(crate) ram_mb: u32,
    pub(crate) cpu_cores: u32,
    pub(crate) disk_gb: u32,
    pub(crate) desktop_profile: DesktopProfile,
    pub(crate) network_mode: NetworkMode,
}

/// One import being run.
struct Import {
    monitor: BuildMonitor,
    request: AppSandboxImportRequest,
    listing: ImportListing,
    progress: ProgressPublisher<AppSandboxImportProgress>,
    /// Set by the worker as it leaves, by whichever exit -- returning or
    /// panicking -- so that an import that died still stops being listed.
    finished: Arc<AtomicBool>,
    /// Filled in by the worker before it marks itself finished, and emptied by
    /// the reap that removes this import.
    outcome: Arc<Mutex<Option<ImportWorkerOutcome>>>,
    worker: Option<JoinHandle<()>>,
}

/// The imports being run, by destination VM name.
#[derive(Default)]
pub(crate) struct ImportRegistry {
    imports: Mutex<HashMap<String, Import>>,
    /// What finished imports left running, waiting for the main thread to take
    /// it over. Kept here rather than returned by `reap`, which runs inside
    /// every query -- a returned session that a caller dropped would silently
    /// cancel the console reader of a running VM.
    started: Mutex<Vec<StartedVm>>,
}

impl ImportRegistry {
    /// Whether a VM of this name is being imported right now.
    pub(crate) fn contains(&self, name: &str) -> bool {
        // Before answering, not after: an import that is over holds neither a
        // row nor its name, and the caller asking is the one that would
        // otherwise refuse a retry the user can no longer see.
        self.reap();
        self.lock().contains_key(name)
    }

    /// Starts `import` on a thread of its own, listing the destination as
    /// building until it returns.
    ///
    /// `import` must not touch the registry: it runs while nothing holds the
    /// lock, but the entry it belongs to is inserted by the caller of this
    /// function while the lock is held.
    pub(crate) fn start<F>(
        &self,
        request: AppSandboxImportRequest,
        listing: ImportListing,
        import: F,
    ) -> Result<(), RepositoryError>
    where
        F: FnOnce(
                &BuildMonitor,
                &ProgressPublisher<AppSandboxImportProgress>,
            ) -> ImportWorkerOutcome
            + Send
            + 'static,
    {
        let mut imports = self.lock();
        if imports.contains_key(&request.destination_name) {
            let error = RepositoryError::new(format!(
                "VM \"{}\" is already being imported",
                request.destination_name
            ));
            tracing::error!("{error}");
            return Err(error);
        }

        let monitor = BuildMonitor::new(BuildStep::WritingDisk);
        let progress = ProgressPublisher::default();
        // The stage the transaction begins at, so a row that appears before the
        // worker's first publish is not a row with no progress at all.
        progress.publish(AppSandboxImportProgress {
            stage: AppSandboxImportStage::Validating,
            copied_bytes: 0,
            total_bytes: None,
        });
        let finished = Arc::new(AtomicBool::new(false));
        let outcome = Arc::new(Mutex::new(None));
        let worker = std::thread::Builder::new()
            .name(format!("vmlord-import-{}", request.destination_name))
            .spawn({
                let monitor = monitor.clone();
                let progress = progress.clone();
                let finished = Arc::clone(&finished);
                let outcome = Arc::clone(&outcome);
                move || {
                    // Set on the way out however the import leaves, panic
                    // included: an entry nobody clears is a row that never goes
                    // away. Dropped after the outcome is stored, so a reap that
                    // sees the flag also sees what the import produced.
                    let _finish = Finish(finished);
                    let produced = import(&monitor, &progress);
                    *outcome
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(produced);
                }
            })
            .map_err(|error| {
                let error = RepositoryError::new(format!(
                    "the thread importing VM \"{}\" could not be started: {error}",
                    request.destination_name
                ));
                tracing::error!("{error}");
                error
            })?;

        tracing::info!(
            "started importing VM \"{}\" in the background",
            request.destination_name
        );
        imports.insert(
            request.destination_name.clone(),
            Import {
                monitor,
                request,
                listing,
                progress,
                finished,
                outcome,
                worker: Some(worker),
            },
        );
        Ok(())
    }

    /// The VMs being imported, as the list shows them.
    ///
    /// Imports that are over are cleared first, for the same reason a build's
    /// are: an import that succeeded has already written ordinary metadata, so
    /// until it is cleared the same VM is in the list twice.
    pub(crate) fn summaries(&self) -> Vec<VmSummary> {
        self.reap();
        self.lock()
            .values()
            .map(|import| VmSummary {
                name: import.request.destination_name.clone(),
                os_type: OS_TYPE.to_owned(),
                state: VmState::Building {
                    progress: BuildProgress {
                        step: import
                            .progress
                            .snapshot()
                            .map_or(BuildStep::WritingDisk, |progress| {
                                creation_step(progress.stage)
                            }),
                        // No import downloads anything: its bytes come off a
                        // local disk.
                        download: None,
                    },
                },
                ram_mb: import.listing.ram_mb,
                disk_gb: import.listing.disk_gb,
                cpu_cores: import.listing.cpu_cores,
                // The first boot of an imported guest has neither, and the
                // second boot's are not this VM's facts until it exists.
                gpu_mode: GpuMode::None,
                gpu: VmGpuFacts::default(),
                desktop_profile: import.listing.desktop_profile,
                display_provisioning: DisplayProvisioning::requested(
                    import.listing.desktop_profile,
                ),
                display: VmDisplayFacts::default(),
                network_mode: import.listing.network_mode,
                // A VM that does not exist yet answers nowhere.
                ip_address: None,
                // The guest's own daemon is reachable during the import, but
                // only by the conversion and only with the source
                // application's key. Nothing a person could connect with
                // exists until the import is over.
                ssh: SshAvailability::Disabled,
            })
            .collect()
    }

    /// The latest published progress of one import, for the application layer.
    pub(crate) fn progress(&self, name: &str) -> Option<AppSandboxImportProgress> {
        self.reap();
        self.lock()
            .get(name)
            .and_then(|import| import.progress.snapshot())
    }

    /// Asks the import of `name` to stop at its next checkpoint.
    ///
    /// Returning here does not mean the import is over: it means it has been
    /// told. Whether stopping rolls the copy back or retains it for recovery is
    /// the worker's decision, taken from how far the guest was changed.
    pub(crate) fn cancel(&self, name: &str) -> Result<(), RepositoryError> {
        let imports = self.lock();
        let Some(import) = imports.get(name) else {
            let error = RepositoryError::new(format!("VM \"{name}\" is not being imported"));
            tracing::error!("{error}");
            return Err(error);
        };
        tracing::warn!("cancelling the import of VM \"{name}\"");
        import.monitor.cancel();
        Ok(())
    }

    /// Refuses an operation on a VM that is still being imported.
    ///
    /// "Not found" would be the wrong answer and the confusing one: the VM is
    /// in the list the user is looking at.
    pub(crate) fn refuse_if_importing(&self, name: &str) -> Result<(), RepositoryError> {
        if !self.contains(name) {
            return Ok(());
        }
        let error = RepositoryError::new(format!("VM \"{name}\" is still being imported"));
        tracing::error!("{error}");
        Err(error)
    }

    /// Removes and joins the imports that have finished.
    ///
    /// Every query runs this first, so it must be called without the lock held
    /// -- it takes the lock itself, and a `Mutex` is not reentrant. Joining
    /// under the lock is safe because an import never touches the registry.
    pub(crate) fn reap(&self) {
        let mut imports = self.lock();
        let done: Vec<String> = imports
            .iter()
            .filter(|(_, import)| import.finished.load(Ordering::Relaxed))
            .map(|(name, _)| name.clone())
            .collect();
        for name in done {
            let Some(mut import) = imports.remove(&name) else {
                continue;
            };
            self.collect(&name, &import);
            if let Some(worker) = import.worker.take()
                && worker.join().is_err()
            {
                tracing::error!("the thread importing VM \"{name}\" panicked");
            }
        }
    }

    /// Hands over the VMs that imports have left running since this was last
    /// called.
    pub(crate) fn take_started(&self) -> Vec<StartedVm> {
        self.reap();
        std::mem::take(
            &mut *self
                .started
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    /// Takes over a second boot the moment it is running, before the import
    /// that started it has finished.
    ///
    /// The verification an import ends with asks the guest whether its agent
    /// has mounted the display and GPU shares. That agent connects to the host,
    /// and nothing on the host listens for it until the VM has been handed
    /// over -- so an import that held its second boot until the end was
    /// verifying a guest whose agent could not reach anybody. It is the same
    /// handover, taken at the moment the VM starts rather than at the moment
    /// the import stops.
    pub(crate) fn hand_over(&self, started: StartedVm) {
        self.started
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(started);
    }

    /// Reports what a finished import did.
    fn collect(&self, name: &str, import: &Import) {
        let outcome = import
            .outcome
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        // An import runs on its own thread, which is inside no operation's
        // span, so every message names the VM.
        match outcome {
            Some(ImportWorkerOutcome::Complete) => vmlord_core::diagnostic!(
                Info,
                Subsystem::Hcs,
                vm = name,
                "Imported VM \"{name}\" from AppSandbox and verified it"
            ),
            Some(ImportWorkerOutcome::NeedsAttention { error }) => vmlord_core::diagnostic!(
                Error,
                Subsystem::Hcs,
                vm = name,
                "Importing VM \"{name}\" stopped after the copied guest was changed, so its \
                 copy was kept for you to retry or discard: {error}"
            ),
            Some(ImportWorkerOutcome::RolledBack { error }) => vmlord_core::diagnostic!(
                Error,
                Subsystem::Hcs,
                vm = name,
                "Importing VM \"{name}\" failed before the copied guest was changed, so \
                 everything it had made was removed: {error}"
            ),
            // The worker panicked before it could store one. The join below
            // reports the panic.
            None => {}
        }
    }

    /// Cancels every import and waits for all of them.
    ///
    /// Called as VMLord shuts down. Leaving without it would either kill a
    /// thread in the middle of copying a VHDX or converting a guest, or hang
    /// the process waiting for one that was never told to stop.
    pub(crate) fn cancel_all_and_join(&self) {
        let mut imports = self.lock();
        for import in imports.values() {
            import.monitor.cancel();
        }
        for (name, mut import) in imports.drain() {
            self.collect(&name, &import);
            if let Some(worker) = import.worker.take()
                && worker.join().is_err()
            {
                tracing::error!("the thread importing VM \"{name}\" panicked");
            }
        }
    }

    /// Recovers a poisoned lock rather than propagating the panic: an import
    /// that panicked must not take the list of VMs down with it.
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Import>> {
        self.imports
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The creation step an import stage is shown as in the VM list.
///
/// [`VmState::Building`] carries the vocabulary of creating a VM, which has no
/// word for copying a disk out of another application or for converting a
/// guest over SSH. The import's own stage is published separately and is what
/// the import UI reads; this map exists only so that the one bar the VM list
/// draws never runs backwards. That is why every guest-side stage before the
/// second boot is `Starting` rather than the `Provisioning` it resembles:
/// truthful ordering is worth more here than a truthful word.
const fn creation_step(stage: AppSandboxImportStage) -> BuildStep {
    match stage {
        // The copy is this import's disk write, and validation is what it is
        // about to do.
        AppSandboxImportStage::Validating | AppSandboxImportStage::Copying => {
            BuildStep::WritingDisk
        }
        AppSandboxImportStage::Creating => BuildStep::Registering,
        AppSandboxImportStage::BootstrapStarting
        | AppSandboxImportStage::Converting
        | AppSandboxImportStage::Restarting => BuildStep::Starting,
        AppSandboxImportStage::Verifying
        | AppSandboxImportStage::NeedsAttention
        | AppSandboxImportStage::Complete => BuildStep::AwaitingGuest,
    }
}

/// Marks an import finished however its thread leaves, panic included.
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

    use uuid::Uuid;
    use vmlord_core::{
        AppSandboxImportProgress, AppSandboxImportRequest, AppSandboxImportStage,
        AppSandboxSourceId, BuildStep, DesktopProfile, GpuMode, NetworkMode, RepositoryError,
        VmState,
    };

    use super::{ImportListing, ImportRegistry};
    use crate::{
        appsandbox::ImportWorkerOutcome, build::StartedVm, com1_terminal::Com1Launcher,
        metadata::VmComputeSystemMapping,
    };

    fn request(name: &str) -> AppSandboxImportRequest {
        AppSandboxImportRequest {
            source_id: AppSandboxSourceId::from_stable_hash(format!("source-{name}")).unwrap(),
            destination_name: name.to_owned(),
        }
    }

    const fn listing() -> ImportListing {
        ImportListing {
            ram_mb: 4096,
            cpu_cores: 4,
            disk_gb: 80,
            desktop_profile: DesktopProfile::Gnome,
            network_mode: NetworkMode::Nat,
        }
    }

    fn mapping(name: &str) -> VmComputeSystemMapping {
        VmComputeSystemMapping {
            vm_id: Uuid::from_u128(1),
            vm_name: name.to_owned(),
            hcs_compute_system_id: format!("vmlord-{name}"),
            disk_gb: 80,
            endpoint_id: None,
            network_mode: NetworkMode::Nat,
            ssh: None,
            ssh_daemon: None,
            gpu_mode: GpuMode::None,
            desktop_profile: DesktopProfile::Headless,
            display_provisioning: vmlord_core::DisplayProvisioning::NotRequested,
            display_mode: None,
            guest_target: None,
        }
    }

    fn started(name: &str) -> StartedVm {
        let mapping = mapping(name);
        StartedVm {
            session: Com1Launcher::session_for_test(&mapping),
            mapping,
        }
    }

    /// Blocks the worker until the test lets it finish, so that an import can
    /// be observed while it is still running.
    struct Gate(Arc<AtomicBool>);

    impl Gate {
        fn new() -> Self {
            Self(Arc::new(AtomicBool::new(false)))
        }

        fn open(&self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    fn drain(registry: &ImportRegistry, name: &str) {
        while registry.contains(name) {
            std::thread::yield_now();
        }
        registry.reap();
    }

    #[test]
    fn a_started_import_is_listed_as_building_until_it_finishes() {
        let registry = ImportRegistry::default();
        let gate = Gate::new();
        let held = Arc::clone(&gate.0);

        registry
            .start(request("ubuntu-copy"), listing(), move |_, progress| {
                progress.publish(AppSandboxImportProgress {
                    stage: AppSandboxImportStage::Converting,
                    copied_bytes: 0,
                    total_bytes: None,
                });
                while !held.load(Ordering::Relaxed) {
                    std::thread::yield_now();
                }
                ImportWorkerOutcome::Complete
            })
            .expect("the import should start");

        // Whatever the worker has published, the row must exist from the moment
        // `start` returns.
        let summaries = registry.summaries();
        assert_eq!(summaries.len(), 1);
        let summary = &summaries[0];
        assert_eq!(summary.name, "ubuntu-copy");
        assert_eq!(summary.ram_mb, 4096);
        assert_eq!(summary.disk_gb, 80);
        assert_eq!(summary.cpu_cores, 4);
        assert_eq!(summary.network_mode, NetworkMode::Nat);
        assert!(matches!(summary.state, VmState::Building { .. }));

        gate.open();
        drain(&registry, "ubuntu-copy");
        assert!(
            registry.summaries().is_empty(),
            "a finished import stops being listed on its own"
        );
    }

    #[test]
    fn the_listed_step_follows_the_published_import_stage() {
        let registry = ImportRegistry::default();
        let gate = Gate::new();
        let held = Arc::clone(&gate.0);

        registry
            .start(request("staged"), listing(), move |_, progress| {
                progress.publish(AppSandboxImportProgress {
                    stage: AppSandboxImportStage::Verifying,
                    copied_bytes: 1,
                    total_bytes: Some(1),
                });
                while !held.load(Ordering::Relaxed) {
                    std::thread::yield_now();
                }
                ImportWorkerOutcome::RolledBack {
                    error: RepositoryError::new("stopped"),
                }
            })
            .expect("the import should start");

        while registry
            .progress("staged")
            .is_none_or(|progress| progress.stage != AppSandboxImportStage::Verifying)
        {
            std::thread::yield_now();
        }
        let VmState::Building { progress } = registry.summaries()[0].state else {
            panic!("an import in flight is a building VM");
        };
        assert_eq!(progress.step, BuildStep::AwaitingGuest);
        assert_eq!(
            progress.download, None,
            "an import copies from a local disk and downloads nothing"
        );

        gate.open();
        drain(&registry, "staged");
    }

    #[test]
    fn a_second_import_of_the_same_name_is_refused() {
        let registry = ImportRegistry::default();
        let gate = Gate::new();
        let held = Arc::clone(&gate.0);

        registry
            .start(request("ubuntu-copy"), listing(), move |_, _| {
                while !held.load(Ordering::Relaxed) {
                    std::thread::yield_now();
                }
                ImportWorkerOutcome::RolledBack {
                    error: RepositoryError::new("stopped"),
                }
            })
            .expect("the first import should start");

        let error = registry
            .start(request("ubuntu-copy"), listing(), |_, _| {
                unreachable!("a duplicate import must never reach a thread")
            })
            .expect_err("a second import of the same name should be refused");

        assert!(error.to_string().contains("already being imported"));
        gate.open();
        drain(&registry, "ubuntu-copy");
    }

    #[test]
    fn cancelling_sets_the_flag_the_import_polls() {
        let registry = ImportRegistry::default();
        let observed = Arc::new(Mutex::new(None));
        let recorded = Arc::clone(&observed);

        registry
            .start(request("ubuntu-copy"), listing(), move |monitor, _| {
                while !monitor.is_cancelled() {
                    std::thread::yield_now();
                }
                *recorded.lock().unwrap() = Some(true);
                ImportWorkerOutcome::RolledBack {
                    error: RepositoryError::new("cancelled"),
                }
            })
            .expect("the import should start");

        registry
            .cancel("ubuntu-copy")
            .expect("an import in flight can be cancelled");
        drain(&registry, "ubuntu-copy");

        assert_eq!(*observed.lock().unwrap(), Some(true));
    }

    #[test]
    fn cancelling_an_unknown_import_says_so() {
        let registry = ImportRegistry::default();

        let error = registry
            .cancel("nothing")
            .expect_err("there is no such import");

        assert!(error.to_string().contains("is not being imported"));
    }

    #[test]
    fn a_second_boot_is_handed_over_while_the_import_that_started_it_runs() {
        // Not when the import ends. What the import does next is ask the guest
        // whether its agent has mounted the payload shares, and that agent has
        // nobody to connect to until the VM has been taken over -- so an import
        // that held on to its second boot could never pass its own checks.
        let registry = ImportRegistry::default();

        registry.hand_over(started("ubuntu-copy"));

        let handed = registry.take_started();

        assert_eq!(handed.len(), 1);
        assert_eq!(handed[0].mapping.vm_name, "ubuntu-copy");
        assert!(
            registry.take_started().is_empty(),
            "a VM is handed over once"
        );
    }

    #[test]
    fn a_needs_attention_import_leaves_the_vm_it_already_handed_over() {
        // The handover happened when the second boot started, so a later
        // failure has nothing left to give up -- and, in particular, does not
        // close the console of a guest that is still running.
        let registry = ImportRegistry::default();
        registry.hand_over(started("half-done"));
        let handed = registry.take_started();

        registry
            .start(request("half-done"), listing(), |_, _| {
                ImportWorkerOutcome::NeedsAttention {
                    error: RepositoryError::new("metadata could not be written"),
                }
            })
            .expect("the import should start");
        drain(&registry, "half-done");

        assert_eq!(handed.len(), 1);
        assert!(
            registry.take_started().is_empty(),
            "the failure has nothing to hand over a second time"
        );
    }

    #[test]
    fn a_rolled_back_import_leaves_nothing_to_collect() {
        let registry = ImportRegistry::default();

        registry
            .start(request("rolled-back"), listing(), |_, _| {
                ImportWorkerOutcome::RolledBack {
                    error: RepositoryError::new("no space"),
                }
            })
            .expect("the import should start");
        registry.cancel_all_and_join();

        assert!(registry.take_started().is_empty());
    }

    #[test]
    fn a_panicking_import_is_still_reaped() {
        let registry = ImportRegistry::default();

        registry
            .start(request("doomed"), listing(), |_, _| {
                panic!("the import thread died");
            })
            .expect("the import should start");
        drain(&registry, "doomed");

        assert!(registry.summaries().is_empty());
        assert!(registry.take_started().is_empty());
    }

    #[test]
    fn operations_on_an_importing_vm_are_refused_by_name() {
        let registry = ImportRegistry::default();
        let gate = Gate::new();
        let held = Arc::clone(&gate.0);

        registry
            .start(request("ubuntu-copy"), listing(), move |_, _| {
                while !held.load(Ordering::Relaxed) {
                    std::thread::yield_now();
                }
                ImportWorkerOutcome::RolledBack {
                    error: RepositoryError::new("stopped"),
                }
            })
            .expect("the import should start");

        let error = registry
            .refuse_if_importing("ubuntu-copy")
            .expect_err("a VM being imported is not one to operate on");
        assert!(error.to_string().contains("still being imported"));
        assert!(registry.refuse_if_importing("something-else").is_ok());

        gate.open();
        drain(&registry, "ubuntu-copy");
    }

    #[test]
    fn cancelling_everything_joins_every_import_thread() {
        let registry = ImportRegistry::default();
        let left = Arc::new(AtomicBool::new(false));
        let marker = Arc::clone(&left);

        registry
            .start(request("ubuntu-copy"), listing(), move |monitor, _| {
                while !monitor.is_cancelled() {
                    std::thread::yield_now();
                }
                marker.store(true, Ordering::Relaxed);
                ImportWorkerOutcome::RolledBack {
                    error: RepositoryError::new("cancelled"),
                }
            })
            .expect("the import should start");

        registry.cancel_all_and_join();

        assert!(
            left.load(Ordering::Relaxed),
            "the worker must have finished before this returns"
        );
        assert!(registry.summaries().is_empty());
    }
}
