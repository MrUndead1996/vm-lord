//! Creating a VM from end to end: build it, start it, and wait until its guest
//! says it is ready.
//!
//! Creation used to end at a registered compute system, which is where VMLord
//! stops working and well before the VM does anything. A build that ends there
//! reports success for a guest whose cloud-init may still fail minutes later,
//! so the cycle carries on to the only fact worth reporting: the guest
//! answered.

use std::path::Path;

use vmlord_core::{BuildMonitor, BuildStep, RepositoryError, VmCreateRequest, VmSource};

use crate::{
    build::StartedVm,
    com1_terminal::{Com1Launcher, Com1Session},
    create::{CloudDiskImporter, VmCreationPipeline},
    delete::VmDeletionPipeline,
    display_runs::DisplayRuns,
    force_stop::VmForceStopPipeline,
    gpu_runs::GpuRuns,
    guest_ready::{GuestReadiness, GuestReady, ReadinessFailure, ReadinessTimeouts, com1_tail},
    metadata::{MetadataStore, VmComputeSystemMapping},
    start::VmStartPipeline,
};

/// How a creation ended, in the terms the repository turns into diagnostics.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CycleOutcome {
    Ready,
    /// The guest is up, but cloud-init finished degraded.
    Degraded {
        detail: String,
    },
    /// The VM is up and answers SSH, and that is all the wait could establish:
    /// without a deploy key there is nobody to ask whether cloud-init finished.
    /// A success with a caveat rather than a failure -- the VM is there, and a
    /// person can log into it.
    Unverified {
        detail: String,
    },
    /// The VM exists and runs, but its guest never reported readiness. It is
    /// deliberately left in place: its serial log is the only account of what
    /// went wrong, and removing the VM would remove that account with it.
    NotReady {
        reason: String,
    },
    /// Nothing usable was created; whatever had been built is gone.
    Failed {
        reason: String,
    },
    Cancelled,
}

/// The result of a cycle, and whatever the main thread has to take over.
pub(crate) struct CycleReport {
    pub(crate) outcome: CycleOutcome,
    /// `None` when the VM was rolled back: there is nothing left to hold.
    pub(crate) started: Option<StartedVm>,
}

/// Whether this VM has a guest that can be asked about its own readiness.
///
/// Installation media carries no cloud-init and no user of VMLord's making:
/// there is nobody in that guest to ask, and a person is going to install the
/// system by hand. Everything a cloud image needs for the wait -- who to log in
/// as, on which port, with which credential -- is on the VM's stored
/// configuration by the time it starts.
fn has_a_guest_to_ask(request: &VmCreateRequest) -> bool {
    match &request.source {
        VmSource::CloudImage { .. } => true,
        VmSource::LocalMedia { .. } => false,
    }
}

/// Whether a rollback has a running VM to stop before it can remove it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Running {
    Yes,
    No,
}

type Creation = Box<
    dyn Fn(
            &MetadataStore,
            &VmCreateRequest,
            &Path,
            &BuildMonitor,
        ) -> Result<VmComputeSystemMapping, RepositoryError>
        + Send
        + Sync,
>;
type Start =
    Box<dyn Fn(&MetadataStore, &str, &Path) -> Result<Com1Session, RepositoryError> + Send + Sync>;
type ForceStop = Box<dyn Fn(&MetadataStore, &str) -> Result<(), RepositoryError> + Send + Sync>;
type Delete = Box<dyn Fn(&MetadataStore, &str, &Path) -> Result<(), RepositoryError> + Send + Sync>;

/// Creates a VM, starts it, and waits for its guest.
pub(crate) struct VmBuildCycle {
    create: Creation,
    start: Start,
    readiness: GuestReadiness,
    force_stop: ForceStop,
    delete: Delete,
}

impl VmBuildCycle {
    /// A cycle backed by the real creation, start, stop and deletion
    /// pipelines.
    ///
    /// The COM1 launcher is passed in rather than made here for the same
    /// reason [`VmStartPipeline::production`] takes one: the repository owns
    /// the sessions a start opens, and both halves have to be the same
    /// launcher.
    #[must_use]
    pub(crate) fn production(
        cloud_disk: CloudDiskImporter,
        com1: Com1Launcher,
        timeouts: ReadinessTimeouts,
        gpu_runs: GpuRuns,
        display_runs: DisplayRuns,
        storage_root: &Path,
    ) -> Self {
        let creation = VmCreationPipeline::production(cloud_disk);
        // The same pipeline a later start uses, so that a VM created with a
        // GPU gets one on its first boot rather than only on its second.
        let start =
            VmStartPipeline::production(com1).for_vms_under(storage_root, gpu_runs, display_runs);
        let force_stop = VmForceStopPipeline::production();
        let deletion = VmDeletionPipeline::production();
        Self {
            create: Box::new(move |store, request, vm_directory, monitor| {
                creation.create(store, request, vm_directory, monitor)
            }),
            start: Box::new(move |store, vm_name, vm_directory| {
                start.start(store, vm_name, vm_directory)
            }),
            readiness: GuestReadiness::production(timeouts),
            force_stop: Box::new(move |store, vm_name| force_stop.force_stop(store, vm_name)),
            delete: Box::new(move |store, vm_name, vm_directory| {
                deletion.delete(store, vm_name, vm_directory, true)
            }),
        }
    }

    /// Replaces the wait's timeouts with the user's own.
    ///
    /// A setter rather than an argument of `production`, because the cloud
    /// image importer this cycle owns is a boxed closure that cannot be
    /// cloned: rebuilding the whole cycle to change four durations would mean
    /// taking that importer apart again. The readiness is stateless, so
    /// replacing it whole costs nothing.
    pub(crate) fn set_timeouts(&mut self, timeouts: ReadinessTimeouts) {
        self.readiness = GuestReadiness::production(timeouts);
    }

    /// Runs the whole cycle for `request`, reporting each step through
    /// `monitor`.
    pub(crate) fn run(
        &self,
        store: &MetadataStore,
        request: &VmCreateRequest,
        vm_directory: &Path,
        monitor: &BuildMonitor,
    ) -> CycleReport {
        let mapping = match (self.create)(store, request, vm_directory, monitor) {
            Ok(mapping) => mapping,
            Err(error) => {
                // The creation pipeline rolls itself back, so there is nothing
                // here left to undo.
                log::error!("creating VM \"{}\" failed: {error}", request.name);
                return CycleReport {
                    outcome: CycleOutcome::Failed {
                        reason: error.to_string(),
                    },
                    started: None,
                };
            }
        };

        if monitor.is_cancelled() {
            return self.roll_back(store, request, vm_directory, Running::No);
        }

        monitor.report(BuildStep::Starting);
        let session = match (self.start)(store, &request.name, vm_directory) {
            Ok(session) => session,
            Err(error) => {
                log::error!(
                    "VM \"{}\" was created but could not be started: {error}",
                    request.name
                );
                self.roll_back(store, request, vm_directory, Running::No);
                // Not the rollback's own outcome: "could not be started" must
                // not reach the user as "you cancelled it".
                return CycleReport {
                    outcome: CycleOutcome::Failed {
                        reason: error.to_string(),
                    },
                    started: None,
                };
            }
        };

        // Re-read: starting the VM is what gives it an endpoint, and the
        // address the wait polls hangs off exactly that.
        let mapping = store
            .find_by_vm_name(&request.name)
            .ok()
            .flatten()
            .unwrap_or(mapping);

        let started = StartedVm { mapping, session };
        if !has_a_guest_to_ask(request) {
            log::info!(
                "VM \"{}\" is created and started; it boots installation media, so there is no \
                 guest to ask about readiness",
                request.name
            );
            return CycleReport {
                outcome: CycleOutcome::Ready,
                started: Some(started),
            };
        }

        monitor.report(BuildStep::AwaitingGuest);
        let waited = self.readiness.wait(&started.mapping, vm_directory, monitor);
        match waited {
            Ok(GuestReady::Ready) => {
                log::info!("VM \"{}\" is created, started and ready", request.name);
                CycleReport {
                    outcome: CycleOutcome::Ready,
                    started: Some(started),
                }
            }
            Ok(GuestReady::Degraded { detail }) => CycleReport {
                outcome: CycleOutcome::Degraded { detail },
                started: Some(started),
            },
            Ok(GuestReady::Unverified { detail }) => CycleReport {
                outcome: CycleOutcome::Unverified { detail },
                started: Some(started),
            },
            Err(ReadinessFailure::Cancelled) => {
                // Dropping the session with the VM is right: the console it
                // reads is about to stop existing.
                drop(started);
                self.roll_back(store, request, vm_directory, Running::Yes)
            }
            Err(failure) => {
                let mut reason = failure.to_string();
                if let Some(tail) = com1_tail(vm_directory) {
                    reason.push_str("\n\nThe end of the VM's serial console:\n");
                    reason.push_str(&tail);
                }
                CycleReport {
                    outcome: CycleOutcome::NotReady { reason },
                    started: Some(started),
                }
            }
        }
    }

    /// Undoes a creation that has already produced a VM.
    ///
    /// `running` says whether the VM was started, and so whether it has to be
    /// stopped before it can be removed. Failures here are logged rather than
    /// propagated: the cycle's answer is that the creation was undone, and a
    /// residue that could not be cleared is a complaint about the host rather
    /// than a different outcome for the build.
    fn roll_back(
        &self,
        store: &MetadataStore,
        request: &VmCreateRequest,
        vm_directory: &Path,
        running: Running,
    ) -> CycleReport {
        log::warn!(
            "rolling back the creation of VM \"{}\": whatever it had built is being removed",
            request.name
        );
        if running == Running::Yes
            && let Err(error) = (self.force_stop)(store, &request.name)
        {
            log::error!(
                "VM \"{}\" could not be stopped while its creation was rolled back: {error}",
                request.name
            );
        }
        if let Err(error) = (self.delete)(store, &request.name, vm_directory) {
            log::error!(
                "VM \"{}\" could not be removed while its creation was rolled back: {error}",
                request.name
            );
        }
        CycleReport {
            outcome: CycleOutcome::Cancelled,
            started: None,
        }
    }

    #[cfg(test)]
    fn for_test(
        create: impl Fn(
            &MetadataStore,
            &VmCreateRequest,
            &Path,
            &BuildMonitor,
        ) -> Result<VmComputeSystemMapping, RepositoryError>
        + Send
        + Sync
        + 'static,
        start: impl Fn(&MetadataStore, &str, &Path) -> Result<Com1Session, RepositoryError>
        + Send
        + Sync
        + 'static,
        readiness: GuestReadiness,
        force_stop: impl Fn(&MetadataStore, &str) -> Result<(), RepositoryError> + Send + Sync + 'static,
        delete: impl Fn(&MetadataStore, &str, &Path) -> Result<(), RepositoryError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            create: Box::new(create),
            start: Box::new(start),
            readiness,
            force_stop: Box::new(force_stop),
            delete: Box::new(delete),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
        time::Duration,
    };

    use uuid::Uuid;
    use vmlord_core::{
        BuildMonitor, BuildStep, CloudImage, GpuMode, NetworkMode, Provisioning, RepositoryError,
        SshAccess, SshAuthentication, SshConfig, SshPort, VmCreateRequest, VmSource,
    };

    use super::{CycleOutcome, VmBuildCycle};
    use crate::{
        com1_terminal::{Com1Launcher, Com1Session},
        guest_ready::{GuestReadiness, GuestReady, ReadinessFailure, ReadinessTimeouts, SshRun},
        metadata::{MetadataStore, VmComputeSystemMapping},
    };

    /// What the fake pipelines were asked to do, in order.
    #[derive(Clone, Default)]
    struct Calls(Arc<Mutex<Vec<&'static str>>>);

    impl Calls {
        fn record(&self, call: &'static str) {
            self.0.lock().unwrap().push(call);
        }

        fn taken(&self) -> Vec<&'static str> {
            self.0.lock().unwrap().clone()
        }
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
                    username: "machi".into(),
                    password: None,
                    ssh: SshAccess::Enabled {
                        deploy_key: true,
                        port: SshPort::DEFAULT,
                    },
                    locale: "en_US.UTF-8".into(),
                    keyboard: "us".into(),
                    timezone: "Europe/Moscow".into(),
                    desktop: vmlord_core::DesktopProfile::Headless,
                },
            },
            ram_mb: 2048,
            disk_gb: 20,
            cpu_cores: 2,
            gpu_mode: GpuMode::None,
            network_mode: NetworkMode::Nat,
        }
    }

    /// The VM the creation pipeline records: the SSH access `request` asks for,
    /// as `Provisioning::ssh_config` derives it, because that is what the wait
    /// reads to know how to reach the guest.
    fn mapping(name: &str) -> VmComputeSystemMapping {
        mapping_with(name, Some(SshAuthentication::VmlordKey))
    }

    fn mapping_with(name: &str, ssh: Option<SshAuthentication>) -> VmComputeSystemMapping {
        VmComputeSystemMapping {
            vm_id: Uuid::nil(),
            vm_name: name.to_owned(),
            hcs_compute_system_id: format!("vmlord-{name}"),
            disk_gb: 20,
            endpoint_id: None,
            network_mode: NetworkMode::Nat,
            ssh: ssh.map(|authentication| SshConfig {
                username: "machi".into(),
                port: SshPort::DEFAULT,
                authentication,
            }),
            gpu_mode: GpuMode::None,
            desktop_profile: vmlord_core::DesktopProfile::Headless,
            display_provisioning: vmlord_core::DisplayProvisioning::NotRequested,
            display_mode: None,
            guest_target: None,
        }
    }

    fn session(name: &str) -> Com1Session {
        Com1Launcher::session_for_test(&mapping(name))
    }

    /// A store and a VM directory of this test's own, removed as it ends.
    struct TempRoot(PathBuf);

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temp_root(label: &str) -> (TempRoot, MetadataStore, PathBuf) {
        let root =
            std::env::temp_dir().join(format!("vmlord-cycle-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let vm_directory = root.join("dev");
        fs::create_dir_all(&vm_directory).expect("the VM directory should be created");
        let store = MetadataStore::new(root.join("vm-mapping.json"));
        (TempRoot(root), store, vm_directory)
    }

    fn monitor() -> BuildMonitor {
        BuildMonitor::new(BuildStep::Downloading)
    }

    /// A readiness whose every wait ends the same way.
    fn readiness_answering(run: SshRun) -> GuestReadiness {
        GuestReadiness::for_test(
            ReadinessTimeouts::default(),
            |_| Ok(Some("10.0.0.2".parse().unwrap())),
            |_, _, _| Ok(()),
            move |_, _, _| match &run {
                SshRun::Cancelled => Ok(SshRun::Cancelled),
                SshRun::Exited {
                    code,
                    transcript_tail,
                } => Ok(SshRun::Exited {
                    code: *code,
                    transcript_tail: transcript_tail.clone(),
                }),
            },
            || Duration::ZERO,
            |_| {},
        )
    }

    fn ready() -> GuestReadiness {
        readiness_answering(SshRun::Exited {
            code: Some(0),
            transcript_tail: "status: done".to_owned(),
        })
    }

    /// A cycle whose creation and start succeed, and whose rollback is recorded
    /// rather than performed.
    fn cycle(calls: &Calls, readiness: GuestReadiness) -> VmBuildCycle {
        cycle_creating(calls, readiness, mapping("dev"))
    }

    /// The same cycle, for a VM recorded with an SSH access of its own.
    fn cycle_creating(
        calls: &Calls,
        readiness: GuestReadiness,
        created: VmComputeSystemMapping,
    ) -> VmBuildCycle {
        let create = {
            let calls = calls.clone();
            move |_: &MetadataStore, _: &VmCreateRequest, _: &Path, _: &BuildMonitor| {
                calls.record("create");
                Ok(created.clone())
            }
        };
        let start = {
            let calls = calls.clone();
            move |_: &MetadataStore, _: &str, _: &Path| {
                calls.record("start");
                Ok(session("dev"))
            }
        };
        let force_stop = {
            let calls = calls.clone();
            move |_: &MetadataStore, _: &str| {
                calls.record("force_stop");
                Ok(())
            }
        };
        let delete = {
            let calls = calls.clone();
            move |_: &MetadataStore, _: &str, _: &Path| {
                calls.record("delete");
                Ok(())
            }
        };
        VmBuildCycle::for_test(create, start, readiness, force_stop, delete)
    }

    #[test]
    fn a_ready_guest_ends_the_cycle_with_the_vm_running() {
        let (_root, store, vm_directory) = temp_root("ready");
        let calls = Calls::default();
        let monitor = monitor();

        let report = cycle(&calls, ready()).run(&store, &request("dev"), &vm_directory, &monitor);

        assert_eq!(report.outcome, CycleOutcome::Ready);
        assert!(report.started.is_some(), "a ready VM is handed over");
        assert_eq!(calls.taken(), ["create", "start"]);
        assert_eq!(
            monitor.snapshot().step,
            BuildStep::AwaitingGuest,
            "the build reports the step it is actually on"
        );
    }

    #[test]
    fn a_degraded_guest_still_leaves_a_usable_vm() {
        let (_root, store, vm_directory) = temp_root("degraded");
        let calls = Calls::default();
        let readiness = readiness_answering(SshRun::Exited {
            code: Some(2),
            transcript_tail: "status: degraded done\nerror: cc_package_update failed".to_owned(),
        });

        let report =
            cycle(&calls, readiness).run(&store, &request("dev"), &vm_directory, &monitor());

        let CycleOutcome::Degraded { detail } = report.outcome else {
            panic!("a degraded guest is still a created VM");
        };
        assert!(detail.contains("cc_package_update"), "{detail}");
        assert!(report.started.is_some());
        assert_eq!(
            calls.taken(),
            ["create", "start"],
            "a degraded guest must not be rolled back"
        );
    }

    /// A password-only VM is a finished build: it exists, it runs, and a person
    /// can log into it. What it is not is a VM whose cloud-init anybody
    /// watched, so the cycle carries that caveat out rather than reporting the
    /// same "ready" a key-mode build earns.
    #[test]
    fn a_password_only_guest_finishes_its_build_with_a_caveat() {
        let (_root, store, vm_directory) = temp_root("password-only");
        let calls = Calls::default();
        let readiness = GuestReadiness::for_test(
            ReadinessTimeouts::default(),
            |_| Ok(Some("10.0.0.2".parse().unwrap())),
            |_, _, _| Ok(()),
            |_, _, _| panic!("a password nobody can type must not be attempted"),
            || Duration::ZERO,
            |_| {},
        );

        let report = cycle_creating(
            &calls,
            readiness,
            mapping_with("dev", Some(SshAuthentication::Password)),
        )
        .run(&store, &request("dev"), &vm_directory, &monitor());

        let CycleOutcome::Unverified { detail } = report.outcome else {
            panic!("a guest nobody asked is neither ready nor failed");
        };
        assert!(detail.contains("cloud-init"), "{detail}");
        assert!(
            report.started.is_some(),
            "the VM is there and a person can log into it"
        );
        assert_eq!(
            calls.taken(),
            ["create", "start"],
            "an unverified guest must not be rolled back"
        );
    }

    #[test]
    fn a_guest_that_never_becomes_ready_keeps_its_vm_and_its_evidence() {
        let (_root, store, vm_directory) = temp_root("not-ready");
        fs::write(
            crate::layout::com1_log_path(&vm_directory),
            "[   12.3] cloud-init[900]: failed to fetch the package list\n",
        )
        .unwrap();
        let calls = Calls::default();
        let readiness = readiness_answering(SshRun::Exited {
            code: None,
            transcript_tail: "....".to_owned(),
        });

        let report =
            cycle(&calls, readiness).run(&store, &request("dev"), &vm_directory, &monitor());

        let CycleOutcome::NotReady { reason } = report.outcome else {
            panic!("a guest that never answered is not a ready one");
        };
        assert!(
            reason.contains(&ReadinessFailure::TimedOut.to_string()),
            "{reason}"
        );
        assert!(
            reason.contains("failed to fetch the package list"),
            "the serial log is the only account of what went wrong: {reason}"
        );
        assert!(
            report.started.is_some(),
            "the VM stays, and so does its console"
        );
        assert_eq!(
            calls.taken(),
            ["create", "start"],
            "removing the VM would remove the evidence"
        );
    }

    #[test]
    fn a_cancelled_wait_stops_and_removes_the_vm_it_had_started() {
        let (_root, store, vm_directory) = temp_root("cancelled-wait");
        let calls = Calls::default();

        let report = cycle(&calls, readiness_answering(SshRun::Cancelled)).run(
            &store,
            &request("dev"),
            &vm_directory,
            &monitor(),
        );

        assert_eq!(report.outcome, CycleOutcome::Cancelled);
        assert!(
            report.started.is_none(),
            "a rolled-back VM is not handed over"
        );
        assert_eq!(
            calls.taken(),
            ["create", "start", "force_stop", "delete"],
            "a running VM is stopped before it is removed"
        );
    }

    #[test]
    fn a_cancellation_between_creation_and_start_removes_what_was_created() {
        let (_root, store, vm_directory) = temp_root("cancelled-early");
        let calls = Calls::default();
        let monitor = monitor();
        let create = {
            let calls = calls.clone();
            let monitor = monitor.clone();
            move |_: &MetadataStore, _: &VmCreateRequest, _: &Path, _: &BuildMonitor| {
                calls.record("create");
                monitor.cancel();
                Ok(mapping("dev"))
            }
        };
        let cycle = VmBuildCycle::for_test(
            create,
            {
                let calls = calls.clone();
                move |_: &MetadataStore, _: &str, _: &Path| {
                    calls.record("start");
                    Ok(session("dev"))
                }
            },
            ready(),
            {
                let calls = calls.clone();
                move |_: &MetadataStore, _: &str| {
                    calls.record("force_stop");
                    Ok(())
                }
            },
            {
                let calls = calls.clone();
                move |_: &MetadataStore, _: &str, _: &Path| {
                    calls.record("delete");
                    Ok(())
                }
            },
        );

        let report = cycle.run(&store, &request("dev"), &vm_directory, &monitor);

        assert_eq!(report.outcome, CycleOutcome::Cancelled);
        assert_eq!(
            calls.taken(),
            ["create", "delete"],
            "nothing was started, so there is nothing to stop"
        );
    }

    #[test]
    fn a_start_that_fails_rolls_the_whole_creation_back() {
        let (_root, store, vm_directory) = temp_root("start-failed");
        let calls = Calls::default();
        let cycle = VmBuildCycle::for_test(
            {
                let calls = calls.clone();
                move |_: &MetadataStore, _: &VmCreateRequest, _: &Path, _: &BuildMonitor| {
                    calls.record("create");
                    Ok(mapping("dev"))
                }
            },
            {
                let calls = calls.clone();
                move |_: &MetadataStore, _: &str, _: &Path| {
                    calls.record("start");
                    Err(RepositoryError::new("HCS refused to start the VM"))
                }
            },
            ready(),
            {
                let calls = calls.clone();
                move |_: &MetadataStore, _: &str| {
                    calls.record("force_stop");
                    Ok(())
                }
            },
            {
                let calls = calls.clone();
                move |_: &MetadataStore, _: &str, _: &Path| {
                    calls.record("delete");
                    Ok(())
                }
            },
        );

        let report = cycle.run(&store, &request("dev"), &vm_directory, &monitor());

        let CycleOutcome::Failed { reason } = report.outcome else {
            panic!("a VM that never started is not a created one");
        };
        assert!(reason.contains("HCS refused"), "{reason}");
        assert!(report.started.is_none());
        assert_eq!(
            calls.taken(),
            ["create", "start", "delete"],
            "there was nothing running to stop"
        );
    }

    #[test]
    fn a_creation_that_fails_needs_no_rollback_of_its_own() {
        let (_root, store, vm_directory) = temp_root("create-failed");
        let calls = Calls::default();
        let cycle = VmBuildCycle::for_test(
            {
                let calls = calls.clone();
                move |_: &MetadataStore, _: &VmCreateRequest, _: &Path, _: &BuildMonitor| {
                    calls.record("create");
                    Err(RepositoryError::new("the image could not be downloaded"))
                }
            },
            |_: &MetadataStore, _: &str, _: &Path| panic!("a VM that was not created cannot start"),
            ready(),
            {
                let calls = calls.clone();
                move |_: &MetadataStore, _: &str| {
                    calls.record("force_stop");
                    Ok(())
                }
            },
            {
                let calls = calls.clone();
                move |_: &MetadataStore, _: &str, _: &Path| {
                    calls.record("delete");
                    Ok(())
                }
            },
        );

        let report = cycle.run(&store, &request("dev"), &vm_directory, &monitor());

        let CycleOutcome::Failed { reason } = report.outcome else {
            panic!("a creation that failed is a failed cycle");
        };
        assert!(reason.contains("could not be downloaded"), "{reason}");
        assert_eq!(
            calls.taken(),
            ["create"],
            "the creation pipeline rolls itself back"
        );
    }

    #[test]
    fn a_rollback_that_fails_is_reported_but_does_not_change_the_outcome() {
        let (_root, store, vm_directory) = temp_root("rollback-failed");
        let calls = Calls::default();
        let cycle = VmBuildCycle::for_test(
            {
                let calls = calls.clone();
                move |_: &MetadataStore, _: &VmCreateRequest, _: &Path, _: &BuildMonitor| {
                    calls.record("create");
                    Ok(mapping("dev"))
                }
            },
            {
                let calls = calls.clone();
                move |_: &MetadataStore, _: &str, _: &Path| {
                    calls.record("start");
                    Ok(session("dev"))
                }
            },
            readiness_answering(SshRun::Cancelled),
            {
                let calls = calls.clone();
                move |_: &MetadataStore, _: &str| {
                    calls.record("force_stop");
                    Err(RepositoryError::new("the VM would not stop"))
                }
            },
            {
                let calls = calls.clone();
                move |_: &MetadataStore, _: &str, _: &Path| {
                    calls.record("delete");
                    Err(RepositoryError::new("the directory is in use"))
                }
            },
        );

        let report = cycle.run(&store, &request("dev"), &vm_directory, &monitor());

        assert_eq!(
            report.outcome,
            CycleOutcome::Cancelled,
            "a residue that could not be cleared is a complaint about the host, \
             not a different answer for the build"
        );
        assert_eq!(calls.taken(), ["create", "start", "force_stop", "delete"]);
    }

    #[test]
    fn installation_media_has_no_guest_to_wait_for() {
        let (_root, store, vm_directory) = temp_root("local-media");
        let calls = Calls::default();
        let mut request = request("dev");
        request.source = VmSource::LocalMedia {
            path: r"C:\iso\ubuntu.iso".into(),
        };
        let readiness = GuestReadiness::for_test(
            ReadinessTimeouts::default(),
            |_| panic!("installation media carries no cloud-init to ask"),
            |_, _, _| panic!("installation media carries no cloud-init to ask"),
            |_, _, _| panic!("installation media carries no key of ours to log in with"),
            || Duration::ZERO,
            |_| {},
        );

        let report = cycle(&calls, readiness).run(&store, &request, &vm_directory, &monitor());

        assert_eq!(report.outcome, CycleOutcome::Ready);
        assert!(report.started.is_some());
        assert_eq!(calls.taken(), ["create", "start"]);
    }

    #[test]
    fn a_ready_guest_is_never_confused_with_a_degraded_one() {
        assert_ne!(
            CycleOutcome::Ready,
            CycleOutcome::Degraded {
                detail: String::new()
            }
        );
        assert_ne!(
            GuestReady::Ready,
            GuestReady::Degraded {
                detail: String::new()
            }
        );
    }
}
