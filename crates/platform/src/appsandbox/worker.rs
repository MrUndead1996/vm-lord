//! Transaction boundary for importing one copied AppSandbox VM.
//!
//! File copying is still reversible. Guest conversion is not: once the first
//! mutating conversion command may have run, every failure is retained for
//! journal-driven recovery and the copied disk is never deleted.

use std::{cell::Cell, path::Path, sync::atomic::AtomicBool};

use vmlord_core::{
    AppSandboxImportProgress, AppSandboxImportStage, BuildMonitor, ProgressPublisher,
    RepositoryError,
};

use super::{BootstrapVm, GuestIdentity, ImportJournal, JournalStage};
use crate::{
    build::StartedVm,
    cleanup,
    metadata::{CompletedImport, VmComputeSystemMapping},
};

type CopyAction =
    Box<dyn Fn(&AtomicBool, &dyn Fn(u64, u64)) -> Result<(), RepositoryError> + Send + Sync>;
type PromoteAction = Box<dyn Fn() -> Result<(), RepositoryError> + Send + Sync>;
type BootstrapAction = Box<dyn Fn() -> Result<BootstrapVm, RepositoryError> + Send + Sync>;
type BootstrapStartAction =
    Box<dyn Fn(&mut BootstrapVm) -> Result<(), RepositoryError> + Send + Sync>;
type ConversionAction =
    Box<dyn Fn(&BootstrapVm) -> Result<GuestIdentity, RepositoryError> + Send + Sync>;
type RestartAction =
    Box<dyn Fn(VmComputeSystemMapping) -> Result<StartedVm, RepositoryError> + Send + Sync>;
type VerifyAction = Box<dyn Fn(&StartedVm) -> Result<(), RepositoryError> + Send + Sync>;
type FinalizeAction =
    Box<dyn Fn(&VmComputeSystemMapping) -> Result<(), RepositoryError> + Send + Sync>;
type RollbackAction = Box<dyn Fn(&Path, Option<&str>) -> Result<(), RepositoryError> + Send + Sync>;

/// Every side effect of an import, injected at the lifecycle boundary.
///
/// Task 9 wires the production pipelines. Keeping the seams here makes the
/// rollback/retain table executable without HCS, HNS, SSH or a large VHDX.
pub(crate) struct ImportWorkerActions {
    pub(crate) copy: CopyAction,
    pub(crate) promote: PromoteAction,
    pub(crate) bootstrap: BootstrapAction,
    pub(crate) start_bootstrap: BootstrapStartAction,
    pub(crate) convert: ConversionAction,
    pub(crate) restart: RestartAction,
    pub(crate) verify: VerifyAction,
    pub(crate) finalize: FinalizeAction,
    pub(crate) rollback: RollbackAction,
}

/// What a worker hands back to its registry.
pub(crate) enum ImportWorkerOutcome {
    /// The running VM and its console session are ready for repository
    /// ownership; ordinary metadata is durable and the journal is gone.
    Complete { started: StartedVm },
    /// Conversion may have changed the guest, so the copy remains recoverable.
    /// A second boot may already be running even though a later durable step
    /// failed; hand it back rather than dropping its ownership on the worker.
    NeedsAttention {
        error: RepositoryError,
        started: Option<StartedVm>,
    },
    /// No guest mutation began, so all VMLord-owned destination state was
    /// removed. The AppSandbox source is never part of that cleanup request.
    RolledBack { error: RepositoryError },
}

/// Runs one durable import transaction.
pub(crate) struct ImportWorker {
    journal: ImportJournal,
    progress: ProgressPublisher<AppSandboxImportProgress>,
    actions: ImportWorkerActions,
    /// Whether a failure before guest conversion must keep what is on disk.
    ///
    /// A first attempt owns everything it made and removes all of it. A retry
    /// runs against a copy the user was shown and asked to keep, so the same
    /// failure must not silently destroy it -- discarding a retained import is
    /// a separate command they have to give.
    retain_on_failure: bool,
    #[cfg(test)]
    after_conversion: Option<Box<dyn Fn() + Send + Sync>>,
}

impl ImportWorker {
    pub(crate) fn new(
        journal: ImportJournal,
        progress: ProgressPublisher<AppSandboxImportProgress>,
        actions: ImportWorkerActions,
    ) -> Self {
        Self {
            journal,
            progress,
            actions,
            retain_on_failure: false,
            #[cfg(test)]
            after_conversion: None,
        }
    }

    /// Marks this run as the resumption of an import already retained on disk.
    pub(crate) const fn resumed(mut self) -> Self {
        self.retain_on_failure = true;
        self
    }

    #[cfg(test)]
    fn with_after_conversion(mut self, hook: impl Fn() + Send + Sync + 'static) -> Self {
        self.after_conversion = Some(Box::new(hook));
        self
    }

    /// Executes the transaction. [`BuildMonitor`] is deliberately only the
    /// cancellation/lifetime carrier; all user-facing stages are published as
    /// [`AppSandboxImportProgress`].
    pub(crate) fn run(mut self, monitor: &BuildMonitor) -> ImportWorkerOutcome {
        if let Err(error) = self.journal.validate_destination() {
            // An untrusted target must never reach recursive cleanup.
            let copied = Cell::new(0);
            let total = Cell::new(None);
            return self.needs_attention(error, None, &copied, &total);
        }

        if let Err(error) = self.transition(JournalStage::Validating) {
            return self.rollback(error, None);
        }
        if let Err(error) = check_cancelled(monitor) {
            return self.rollback(error, None);
        }

        if let Err(error) = self.transition(JournalStage::Copying) {
            return self.rollback(error, None);
        }
        let copied = Cell::new(0_u64);
        let total = Cell::new(None);
        let publisher = self.progress.clone();
        let publish = |copied_bytes, total_bytes| {
            copied.set(copied_bytes);
            total.set(Some(total_bytes));
            publisher.publish(AppSandboxImportProgress {
                stage: AppSandboxImportStage::Copying,
                copied_bytes,
                total_bytes: Some(total_bytes),
            });
        };
        if let Err(error) = (self.actions.copy)(monitor.cancel_flag(), &publish) {
            return self.rollback(error, None);
        }
        if let Err(error) = check_cancelled(monitor) {
            return self.rollback(error, None);
        }
        if let Err(error) = (self.actions.promote)() {
            return self.rollback(error, None);
        }

        if let Err(error) = self.transition_with_bytes(JournalStage::Creating, &copied, &total) {
            return self.rollback(error, None);
        }
        let mut bootstrap = match (self.actions.bootstrap)() {
            Ok(bootstrap) => bootstrap,
            Err(error) => return self.rollback(error, None),
        };
        let hcs_id = bootstrap.hcs_compute_system_id.clone();

        if let Err(error) =
            self.transition_with_bytes(JournalStage::BootstrapStarting, &copied, &total)
        {
            return self.rollback(error, Some(&hcs_id));
        }
        if let Err(error) = (self.actions.start_bootstrap)(&mut bootstrap) {
            return self.rollback(error, Some(&hcs_id));
        }
        if let Err(error) = check_cancelled(monitor) {
            return self.rollback(error, Some(&hcs_id));
        }

        // The durable transition precedes the first command that may mutate
        // the guest. From the call below onward cleanup must retain the copy.
        if let Err(error) = self.transition_with_bytes(JournalStage::Converting, &copied, &total) {
            return self.rollback(error, Some(&hcs_id));
        }
        let converted = (self.actions.convert)(&bootstrap);
        // Whatever the conversion did or did not confirm, it wrote it to the
        // same journal file. Taking the stale in-memory copy forward would
        // undo every step a resumption could have skipped.
        if let Err(error) = self.journal.reload() {
            tracing::warn!("the import journal could not be re-read after the conversion: {error}");
        }
        let identity = match converted {
            Ok(identity) => identity,
            Err(error) => return self.needs_attention(error, None, &copied, &total),
        };

        #[cfg(test)]
        if let Some(hook) = &self.after_conversion {
            hook();
        }
        if let Err(error) = check_cancelled(monitor) {
            return self.needs_attention(error, None, &copied, &total);
        }

        let resources = self.journal.requested_resources();
        let ssh = self.journal.bootstrap_ssh();
        let mapping = VmComputeSystemMapping::from_completed_import(&CompletedImport {
            bootstrap: &bootstrap.mapping,
            ssh_username: &ssh.username,
            ssh_port: match vmlord_core::SshPort::new(ssh.port) {
                Ok(port) => port,
                Err(error) => return self.needs_attention(error, None, &copied, &total),
            },
            gpu_mode: self.journal.desired_gpu(),
            desktop_profile: resources.desktop_profile,
            distribution: identity.distribution(),
            release: identity.release(),
            architecture: identity.architecture(),
            kernel_release: identity.kernel_release(),
        });

        if let Err(error) = self.transition_with_bytes(JournalStage::Restarting, &copied, &total) {
            return self.needs_attention(error, None, &copied, &total);
        }
        let started = match (self.actions.restart)(mapping) {
            Ok(started) => started,
            Err(error) => return self.needs_attention(error, None, &copied, &total),
        };

        if let Err(error) = self.transition_with_bytes(JournalStage::Verifying, &copied, &total) {
            return self.needs_attention(error, Some(started), &copied, &total);
        }
        if let Err(error) = (self.actions.verify)(&started) {
            return self.needs_attention(error, Some(started), &copied, &total);
        }

        // Ordinary metadata is the final published VM state. The journal is
        // removed only after that write is durable, so a crash can always find
        // either a recoverable import or a complete ordinary VM.
        if let Err(error) = (self.actions.finalize)(&started.mapping) {
            return self.needs_attention(error, Some(started), &copied, &total);
        }
        if let Err(error) = self.journal_completion() {
            return self.needs_attention(error, Some(started), &copied, &total);
        }

        self.publish(AppSandboxImportStage::Complete, &copied, &total);
        ImportWorkerOutcome::Complete { started }
    }

    fn transition(&mut self, stage: JournalStage) -> Result<(), RepositoryError> {
        let copied = Cell::new(0);
        let total = Cell::new(None);
        self.transition_with_bytes(stage, &copied, &total)
    }

    fn transition_with_bytes(
        &mut self,
        stage: JournalStage,
        copied: &Cell<u64>,
        total: &Cell<Option<u64>>,
    ) -> Result<(), RepositoryError> {
        self.journal.set_stage(stage);
        self.journal.save()?;
        self.publish(stage.import_stage(), copied, total);
        Ok(())
    }

    fn publish(&self, stage: AppSandboxImportStage, copied: &Cell<u64>, total: &Cell<Option<u64>>) {
        self.progress.publish(AppSandboxImportProgress {
            stage,
            copied_bytes: copied.get(),
            total_bytes: total.get(),
        });
    }

    fn journal_completion(&mut self) -> Result<(), RepositoryError> {
        self.journal.set_stage(JournalStage::Complete);
        self.journal.save()?;
        self.journal.remove()
    }

    fn rollback(&mut self, error: RepositoryError, hcs_id: Option<&str>) -> ImportWorkerOutcome {
        if self.retain_on_failure {
            // The copy this run resumed from was already retained once. Only an
            // explicit discard may remove it.
            let copied = Cell::new(0);
            let total = Cell::new(None);
            return self.needs_attention(error, None, &copied, &total);
        }
        if let Err(cleanup_error) = (self.actions.rollback)(self.journal.destination(), hcs_id) {
            let error = cleanup::combine_failures(
                "the AppSandbox import failed before guest conversion and rollback was incomplete",
                vec![error.to_string(), cleanup_error.to_string()],
            );
            let copied = Cell::new(0);
            let total = Cell::new(None);
            return self.needs_attention(error, None, &copied, &total);
        }
        if let Err(journal_error) = self.journal.remove() {
            let error = cleanup::combine_failures(
                "the AppSandbox import failed before guest conversion and rollback was incomplete",
                vec![error.to_string(), journal_error.to_string()],
            );
            let copied = Cell::new(0);
            let total = Cell::new(None);
            return self.needs_attention(error, None, &copied, &total);
        }
        ImportWorkerOutcome::RolledBack { error }
    }

    fn needs_attention(
        &mut self,
        error: RepositoryError,
        started: Option<StartedVm>,
        copied: &Cell<u64>,
        total: &Cell<Option<u64>>,
    ) -> ImportWorkerOutcome {
        self.journal.set_stage(JournalStage::NeedsAttention);
        let error = match self.journal.save() {
            Ok(()) => error,
            Err(journal_error) => cleanup::combine_failures(
                "the AppSandbox import needs attention and its journal could not record that",
                vec![error.to_string(), journal_error.to_string()],
            ),
        };
        self.publish(AppSandboxImportStage::NeedsAttention, copied, total);
        ImportWorkerOutcome::NeedsAttention { error, started }
    }
}

fn check_cancelled(monitor: &BuildMonitor) -> Result<(), RepositoryError> {
    if monitor.is_cancelled() {
        Err(RepositoryError::new("the AppSandbox import was cancelled"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::{Arc, Mutex},
    };

    use uuid::Uuid;
    use vmlord_core::{
        AppSandboxImportProgress, AppSandboxImportStage, AppSandboxSourceId, BuildMonitor,
        BuildStep, DesktopProfile, GpuMode, ProgressPublisher, RepositoryError,
    };

    use super::{ImportWorker, ImportWorkerActions, ImportWorkerOutcome};
    use crate::{
        Com1Launcher, Com1LogMode,
        appsandbox::{
            BootstrapSshFacts, BootstrapVm, GuestIdentity, ImportJournal, ImportJournalDetails,
            ImportResources, SourceFingerprint,
        },
        build::StartedVm,
        metadata::VmComputeSystemMapping,
    };

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum FailurePoint {
        None,
        Copy,
        Promote,
        Bootstrap,
        BootstrapStart,
        Conversion,
        Restart,
        AgentVerification,
        Metadata,
        CancelDuringCopy,
        BootstrapStartAndRollback,
    }

    #[derive(Clone, Default)]
    struct Calls(Arc<Mutex<Vec<&'static str>>>);

    impl Calls {
        fn push(&self, name: &'static str) {
            self.0.lock().unwrap().push(name);
        }

        fn snapshot(&self) -> Vec<&'static str> {
            self.0.lock().unwrap().clone()
        }
    }

    struct Fixture {
        root: PathBuf,
        source: PathBuf,
        destination: PathBuf,
        import_id: Uuid,
        monitor: BuildMonitor,
        progress: ProgressPublisher<AppSandboxImportProgress>,
        calls: Calls,
        cleanup_targets: Arc<Mutex<Vec<PathBuf>>>,
        cleanup_systems: Arc<Mutex<Vec<Option<String>>>>,
        failure: FailurePoint,
    }

    impl Fixture {
        fn new(label: &str, failure: FailurePoint) -> Self {
            let root = std::env::temp_dir().join(format!(
                "vmlord-appsandbox-worker-{label}-{}",
                Uuid::new_v4()
            ));
            let source = root.join("appsandbox").join("source.vhdx");
            let destination = root.join("vms").join("imported");
            fs::create_dir_all(source.parent().unwrap()).unwrap();
            fs::create_dir_all(destination.parent().unwrap()).unwrap();
            fs::write(&source, b"source stays here").unwrap();
            Self {
                root,
                source,
                destination,
                import_id: Uuid::new_v4(),
                monitor: BuildMonitor::new(BuildStep::WritingDisk),
                progress: ProgressPublisher::default(),
                calls: Calls::default(),
                cleanup_targets: Arc::new(Mutex::new(Vec::new())),
                cleanup_systems: Arc::new(Mutex::new(Vec::new())),
                failure,
            }
        }

        fn journal(&self) -> ImportJournal {
            ImportJournal::create(
                self.root.join("vms"),
                ImportJournalDetails {
                    import_id: self.import_id,
                    source_fingerprint: SourceFingerprint {
                        source_id: AppSandboxSourceId::from_stable_hash("source").unwrap(),
                        disk_path: self.source.clone(),
                        vm_ordinal: 1,
                    },
                    destination: self.destination.clone(),
                    requested_resources: ImportResources {
                        ram_mb: 4096,
                        cpu_cores: 4,
                        disk_gb: 80,
                        desktop_profile: DesktopProfile::Gnome,
                    },
                    desired_gpu: GpuMode::Default,
                    bootstrap_ssh: BootstrapSshFacts {
                        username: "sandbox".into(),
                        port: 2222,
                    },
                },
            )
            .unwrap()
        }

        fn worker(&self) -> ImportWorker {
            let calls = self.calls.clone();
            let copy_destination = self.destination.clone();
            let source = self.source.clone();
            let copy_failure = self.failure == FailurePoint::Copy;
            let cancel_during_copy = self.failure == FailurePoint::CancelDuringCopy;
            let promote_failure = self.failure == FailurePoint::Promote;
            let bootstrap_failure = self.failure == FailurePoint::Bootstrap;
            let start_failure = matches!(
                self.failure,
                FailurePoint::BootstrapStart | FailurePoint::BootstrapStartAndRollback
            );
            let conversion_failure = self.failure == FailurePoint::Conversion;
            let restart_failure = self.failure == FailurePoint::Restart;
            let verify_failure = self.failure == FailurePoint::AgentVerification;
            let metadata_failure = self.failure == FailurePoint::Metadata;
            let rollback_failure = self.failure == FailurePoint::BootstrapStartAndRollback;
            let cleanup_targets = Arc::clone(&self.cleanup_targets);
            let cleanup_systems = Arc::clone(&self.cleanup_systems);

            ImportWorker::new(
                self.journal(),
                self.progress.clone(),
                ImportWorkerActions {
                    copy: Box::new(move |cancel, publish| {
                        calls.push("copy");
                        if copy_failure {
                            return Err(RepositoryError::new("copy failed"));
                        }
                        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                            return Err(RepositoryError::new("copy cancelled"));
                        }
                        fs::create_dir_all(copy_destination.join("disks")).unwrap();
                        fs::copy(
                            &source,
                            copy_destination.join("disks").join("system.vhdx.staged"),
                        )
                        .unwrap();
                        publish(16, 16);
                        if cancel_during_copy {
                            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                            return Err(RepositoryError::new("copy cancelled"));
                        }
                        Ok(())
                    }),
                    promote: Box::new({
                        let calls = self.calls.clone();
                        let destination = self.destination.clone();
                        move || {
                            calls.push("promote");
                            if promote_failure {
                                return Err(RepositoryError::new("promotion failed"));
                            }
                            fs::rename(
                                destination.join("disks").join("system.vhdx.staged"),
                                destination.join("disks").join("system.vhdx"),
                            )
                            .unwrap();
                            Ok(())
                        }
                    }),
                    bootstrap: Box::new({
                        let calls = self.calls.clone();
                        move || {
                            calls.push("bootstrap");
                            if bootstrap_failure {
                                return Err(RepositoryError::new("bootstrap failed"));
                            }
                            Ok(bootstrap_vm())
                        }
                    }),
                    start_bootstrap: Box::new({
                        let calls = self.calls.clone();
                        move |bootstrap| {
                            calls.push("bootstrap-start");
                            if start_failure {
                                return Err(RepositoryError::new("bootstrap start failed"));
                            }
                            bootstrap.mapping.endpoint_id = Some(Uuid::from_u128(9));
                            Ok(())
                        }
                    }),
                    convert: Box::new({
                        let calls = self.calls.clone();
                        move |_bootstrap| {
                            calls.push("convert");
                            if conversion_failure {
                                return Err(RepositoryError::new("conversion failed"));
                            }
                            Ok(GuestIdentity::observed(
                                "ubuntu",
                                "26.04",
                                "x86_64",
                                "7.0.0-14-generic",
                                "Ubuntu 26.04",
                            ))
                        }
                    }),
                    restart: Box::new({
                        let calls = self.calls.clone();
                        let destination = self.destination.clone();
                        move |mapping| {
                            calls.push("restart");
                            if restart_failure {
                                return Err(RepositoryError::new("restart failed"));
                            }
                            Ok(started_vm(mapping, &destination))
                        }
                    }),
                    verify: Box::new({
                        let calls = self.calls.clone();
                        move |_mapping| {
                            calls.push("verify");
                            if verify_failure {
                                return Err(RepositoryError::new("agent verification failed"));
                            }
                            Ok(())
                        }
                    }),
                    finalize: Box::new({
                        let calls = self.calls.clone();
                        move |_mapping| {
                            calls.push("metadata");
                            if metadata_failure {
                                return Err(RepositoryError::new("metadata failed"));
                            }
                            Ok(())
                        }
                    }),
                    rollback: Box::new({
                        let calls = self.calls.clone();
                        move |destination, hcs_id| {
                            calls.push("rollback");
                            cleanup_targets.lock().unwrap().push(destination.to_owned());
                            cleanup_systems
                                .lock()
                                .unwrap()
                                .push(hcs_id.map(str::to_owned));
                            if rollback_failure {
                                return Err(RepositoryError::new("rollback failed"));
                            }
                            if destination.exists() {
                                fs::remove_dir_all(destination).unwrap();
                            }
                            Ok(())
                        }
                    }),
                },
            )
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn bootstrap_vm() -> BootstrapVm {
        BootstrapVm {
            vm_id: Uuid::from_u128(7),
            hcs_compute_system_id: "vmlord-imported".into(),
            mapping: VmComputeSystemMapping {
                vm_id: Uuid::from_u128(7),
                vm_name: "imported".into(),
                hcs_compute_system_id: "vmlord-imported".into(),
                disk_gb: 80,
                endpoint_id: None,
                network_mode: vmlord_core::NetworkMode::Nat,
                ssh: None,
                ssh_daemon: None,
                gpu_mode: GpuMode::None,
                desktop_profile: DesktopProfile::Headless,
                display_provisioning: vmlord_core::DisplayProvisioning::NotRequested,
                display_mode: None,
                guest_target: None,
            },
        }
    }

    fn started_vm(mapping: VmComputeSystemMapping, vm_directory: &std::path::Path) -> StartedVm {
        let launcher =
            Com1Launcher::for_test(PathBuf::from(r"C:\VMLord\vmlord-com1.exe"), |_command| {
                Ok(())
            });
        let session = launcher
            .launch(&mapping, vm_directory, Com1LogMode::Truncate)
            .unwrap();
        StartedVm { mapping, session }
    }

    #[test]
    fn a_resumed_run_keeps_the_copy_a_first_attempt_would_have_removed() {
        // The user was shown this import as needing attention and chose to
        // retry it. A pre-conversion failure on that retry must leave the copy
        // exactly where the previous failure left it: discarding is a separate
        // command.
        let fixture = Fixture::new("resumed-retain", FailurePoint::BootstrapStart);
        let journal_path = fixture.journal().path();

        let outcome = fixture.worker().resumed().run(&fixture.monitor);

        assert!(matches!(
            outcome,
            ImportWorkerOutcome::NeedsAttention { .. }
        ));
        assert!(
            fixture.cleanup_targets.lock().unwrap().is_empty(),
            "a resumed run must not roll back the copy it resumed from"
        );
        assert!(fixture.destination.exists());
        assert!(fixture.source.exists());
        assert!(journal_path.exists());
    }

    #[test]
    fn failure_before_promotion_rolls_back_only_the_owned_destination() {
        let fixture = Fixture::new("pre-promotion", FailurePoint::Promote);
        let journal_path = fixture.journal().path();

        let outcome = fixture.worker().run(&fixture.monitor);

        assert!(matches!(outcome, ImportWorkerOutcome::RolledBack { .. }));
        assert!(fixture.source.exists());
        assert!(!fixture.destination.exists());
        assert_eq!(fixture.cleanup_systems.lock().unwrap().clone(), vec![None]);
        assert_eq!(
            fixture.cleanup_targets.lock().unwrap().clone(),
            vec![fixture.destination.clone()]
        );
        assert!(
            !journal_path.exists(),
            "a completed rollback is not recoverable work"
        );
    }

    #[test]
    fn cleanup_refuses_a_nested_or_reserved_target_before_any_side_effect() {
        let mut fixture = Fixture::new("contained-cleanup", FailurePoint::None);
        fixture.destination = fixture.root.join("vms").join("nested").join("imported");
        fs::create_dir_all(&fixture.destination).unwrap();
        fs::write(fixture.destination.join("keep"), b"not an exact VM target").unwrap();

        let outcome = fixture.worker().run(&fixture.monitor);

        assert!(matches!(
            outcome,
            ImportWorkerOutcome::NeedsAttention { .. }
        ));
        assert!(fixture.destination.join("keep").exists());
        assert!(fixture.cleanup_targets.lock().unwrap().is_empty());
        assert!(fixture.calls.snapshot().is_empty());
    }

    #[test]
    fn failed_pre_conversion_cleanup_keeps_the_journal_and_copy_for_attention() {
        let fixture = Fixture::new("rollback-failed", FailurePoint::BootstrapStartAndRollback);
        let journal_path = fixture.journal().path();

        let outcome = fixture.worker().run(&fixture.monitor);

        assert!(matches!(
            outcome,
            ImportWorkerOutcome::NeedsAttention { .. }
        ));
        assert!(fixture.destination.exists());
        assert!(fixture.source.exists());
        assert!(journal_path.exists());
        assert_eq!(
            fixture.progress.snapshot().unwrap().stage,
            AppSandboxImportStage::NeedsAttention
        );
    }

    #[test]
    fn failure_after_promotion_but_before_guest_mutation_still_rolls_back() {
        let fixture = Fixture::new("bootstrap-start", FailurePoint::BootstrapStart);

        let outcome = fixture.worker().run(&fixture.monitor);

        assert!(matches!(outcome, ImportWorkerOutcome::RolledBack { .. }));
        assert!(fixture.source.exists());
        assert!(!fixture.destination.exists());
        assert_eq!(
            fixture.cleanup_systems.lock().unwrap().clone(),
            vec![Some("vmlord-imported".to_owned())]
        );
    }

    #[test]
    fn post_conversion_failure_preserves_copy_as_needs_attention() {
        let fixture = Fixture::new("post-conversion", FailurePoint::AgentVerification);

        let outcome = fixture.worker().run(&fixture.monitor);

        assert!(matches!(
            outcome,
            ImportWorkerOutcome::NeedsAttention { .. }
        ));
        assert!(fixture.destination.join("disks/system.vhdx").exists());
        assert!(fixture.source.exists());
        assert!(fixture.cleanup_targets.lock().unwrap().is_empty());
        assert_eq!(
            fixture.progress.snapshot().unwrap().stage,
            AppSandboxImportStage::NeedsAttention
        );
    }

    #[test]
    fn cancellation_during_copy_uses_the_pre_conversion_rollback() {
        let fixture = Fixture::new("cancel-copy", FailurePoint::CancelDuringCopy);

        let outcome = fixture.worker().run(&fixture.monitor);

        assert!(matches!(outcome, ImportWorkerOutcome::RolledBack { .. }));
        assert!(fixture.calls.snapshot().contains(&"copy"));
        assert!(fixture.source.exists());
        assert!(!fixture.destination.exists());
    }

    #[test]
    fn cancellation_after_conversion_begins_preserves_the_copy() {
        let fixture = Fixture::new("cancel-conversion", FailurePoint::None);
        let monitor = fixture.monitor.clone();
        let calls = fixture.calls.clone();
        let worker = fixture.worker().with_after_conversion(move || {
            calls.push("cancel-after-conversion");
            monitor.cancel();
        });

        let outcome = worker.run(&fixture.monitor);

        assert!(matches!(
            outcome,
            ImportWorkerOutcome::NeedsAttention { .. }
        ));
        assert!(fixture.destination.join("disks/system.vhdx").exists());
        assert!(fixture.cleanup_targets.lock().unwrap().is_empty());
    }

    #[test]
    fn success_verifies_then_publishes_metadata_and_removes_the_journal() {
        let fixture = Fixture::new("success", FailurePoint::None);
        let journal_path = fixture.journal().path();
        let worker = fixture.worker();

        let outcome = worker.run(&fixture.monitor);

        assert!(matches!(outcome, ImportWorkerOutcome::Complete { .. }));
        let calls = fixture.calls.snapshot();
        assert_eq!(
            calls,
            [
                "copy",
                "promote",
                "bootstrap",
                "bootstrap-start",
                "convert",
                "restart",
                "verify",
                "metadata"
            ]
        );
        assert!(!journal_path.exists());
        assert_eq!(
            fixture.progress.snapshot().unwrap().stage,
            AppSandboxImportStage::Complete
        );
    }

    #[test]
    fn metadata_failure_keeps_the_verified_copy_recoverable() {
        let fixture = Fixture::new("metadata", FailurePoint::Metadata);
        let journal_path = fixture.journal().path();

        let outcome = fixture.worker().run(&fixture.monitor);

        assert!(matches!(
            outcome,
            ImportWorkerOutcome::NeedsAttention { .. }
        ));
        assert!(fixture.destination.join("disks/system.vhdx").exists());
        assert!(journal_path.exists());
    }
}
