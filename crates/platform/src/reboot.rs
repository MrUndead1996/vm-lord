//! Rebooting an HCS-backed virtual machine in place.

use std::{sync::Arc, time::Duration};

use vmlord_core::RepositoryError;

use crate::{
    HcsSystem,
    agent::RebootChannel,
    hcs::{HCS_ACCESS_ALL, HcsRebootFailure},
    metadata::MetadataStore,
};

/// The same bound a shutdown waits on, for the same reason: the wait is on
/// delivery, which takes seconds, and only a wedged Host Compute Service
/// outruns a minute.
const REBOOT_TIMEOUT: Duration = Duration::from_secs(60);

/// `Send + Sync` because the wait belongs on a thread of its own: a reboot
/// request is delivered by [`crate::reboot_workers::RebootWorkers`], which
/// shares the pipeline with the worker carrying it.
type SystemRebooter = Arc<dyn Fn(&str) -> Result<(), RebootFailure> + Send + Sync>;

/// Why a reboot did not happen.
///
/// `Unsupported` is HCS saying it has no way to deliver the request to this
/// guest -- the one failure with another way forward. `Other` is everything
/// else: a host that cannot reach its own compute system is not made better by
/// asking the guest behind its back.
#[derive(Debug)]
pub(crate) enum RebootFailure {
    Unsupported(RepositoryError),
    Other(RepositoryError),
}

/// Requests in-place reboots of VMs known to [`MetadataStore`].
pub struct VmRebootPipeline {
    system_rebooter: SystemRebooter,
}

impl VmRebootPipeline {
    /// Creates a pipeline backed by the real HCS API.
    #[must_use]
    pub fn production() -> Self {
        Self {
            system_rebooter: Arc::new(reboot_hcs_system),
        }
    }

    #[cfg(test)]
    fn for_test(
        rebooter: impl Fn(&str) -> Result<(), RebootFailure> + Send + Sync + 'static,
    ) -> Self {
        Self {
            system_rebooter: Arc::new(rebooter),
        }
    }

    /// Asks the guest of the VM named `vm_name` to reboot in place.
    ///
    /// Returning `Ok` means HCS accepted and delivered the request, not that
    /// the guest has gone down or come back: the VM keeps running until the
    /// guest acts on the request, and nothing about the run is torn down on
    /// the way down. This blocks until HCS answers, for up to
    /// [`REBOOT_TIMEOUT`], so it belongs on a thread of its own.
    pub(crate) fn reboot(&self, store: &MetadataStore, vm_name: &str) -> Result<(), RebootFailure> {
        let mapping = store
            .find_by_vm_name(vm_name)
            .map_err(RebootFailure::Other)?
            .ok_or_else(|| {
                let error =
                    RepositoryError::new(format!("no HCS mapping found for VM \"{vm_name}\""));
                tracing::error!("{error}");
                RebootFailure::Other(error)
            })?;

        tracing::info!(
            "requesting a reboot of VM \"{}\" ({}) as HCS compute system \"{}\"",
            mapping.vm_name,
            mapping.vm_id,
            mapping.hcs_compute_system_id
        );

        (self.system_rebooter)(&mapping.hcs_compute_system_id)?;

        tracing::info!(
            "the guest of VM \"{}\" ({}) accepted the reboot request",
            mapping.vm_name,
            mapping.vm_id
        );
        Ok(())
    }
}

impl Default for VmRebootPipeline {
    fn default() -> Self {
        Self::production()
    }
}

fn reboot_hcs_system(id: &str) -> Result<(), RebootFailure> {
    // The system handle must outlive the reboot operation it issued.
    let system = HcsSystem::open(id, HCS_ACCESS_ALL).map_err(RebootFailure::Other)?;
    system
        .reboot_and_wait(REBOOT_TIMEOUT)
        .map_err(|failure| match failure {
            HcsRebootFailure::Unsupported(error) => RebootFailure::Unsupported(error),
            HcsRebootFailure::Failed(error) => RebootFailure::Other(error),
        })
}

/// Reboots a VM, asking its own agent when HCS cannot deliver the request.
///
/// The order is deliberate. HCS first, because it is the standard path and the
/// one that needs nothing from the guest but its drivers; the agent second,
/// because it works where HCS cannot -- a VM whose configuration predates the
/// integration services (#70) -- and says nothing about the host. Any failure
/// but [`RebootFailure::Unsupported`] is returned as it is, because the guest
/// cannot answer a host problem.
///
/// `Ok` means a reboot was accepted somewhere and will happen in the guest's
/// own time: sessions and consoles stay as they are, and the agent reconnects
/// when the guest is back.
///
/// # Errors
///
/// [`RepositoryError`] naming both paths when neither worked, with the one way
/// left -- stopping the VM and starting it again -- said in the same breath.
pub(crate) fn reboot_with_agent_fallback(
    pipeline: &VmRebootPipeline,
    store: &MetadataStore,
    vm_name: &str,
    agent: Option<RebootChannel>,
) -> Result<(), RepositoryError> {
    match pipeline.reboot(store, vm_name) {
        Ok(()) => Ok(()),
        Err(RebootFailure::Unsupported(hcs_reason)) => {
            // The one failure with another way forward: the guest can be asked
            // to reboot itself, through the agent it is already speaking to.
            let outcome = agent.map_or_else(
                || {
                    Err(RepositoryError::new(format!(
                        "VMLord is not listening for an agent of VM \"{vm_name}\""
                    )))
                },
                |channel| channel.ask(),
            );
            match outcome {
                Ok(()) => {
                    tracing::info!(
                        "the agent of VM \"{vm_name}\" accepted the reboot request HCS could \
                         not deliver"
                    );
                    Ok(())
                }
                Err(agent_reason) => {
                    let error = RepositoryError::new(format!(
                        "{hcs_reason}. Asking the guest's own agent did not work either: \
                         {agent_reason}. Stop the VM and start it again for the same effect"
                    ));
                    tracing::error!("{error}");
                    Err(error)
                }
            }
        }
        Err(RebootFailure::Other(error)) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
            mpsc,
        },
        thread,
        time::Duration,
    };

    use uuid::Uuid;
    use vmlord_core::{NetworkMode, RepositoryError};

    use super::{RebootFailure, VmRebootPipeline, reboot_with_agent_fallback};
    use crate::agent::RebootChannel;
    use crate::metadata::{MetadataStore, VmComputeSystemMapping};

    struct TempRoot(PathBuf);

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temp_root(label: &str) -> TempRoot {
        static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "vmlord-reboot-test-{label}-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("test root should be created");
        TempRoot(path)
    }

    struct Fixture {
        _root: TempRoot,
        store: MetadataStore,
        mapping: VmComputeSystemMapping,
        reboots: Arc<Mutex<Vec<String>>>,
    }

    fn fixture(label: &str) -> Fixture {
        let root = temp_root(label);
        let mapping = VmComputeSystemMapping {
            vm_id: Uuid::new_v4(),
            vm_name: "dev".into(),
            hcs_compute_system_id: "vmlord-dev".into(),
            disk_gb: 20,
            endpoint_id: None,
            network_mode: NetworkMode::None,
            ssh: None,
            ssh_daemon: None,
            gpu_mode: vmlord_core::GpuMode::None,
            desktop_profile: vmlord_core::DesktopProfile::Headless,
            display_provisioning: vmlord_core::DisplayProvisioning::NotRequested,
            display_mode: None,
            guest_target: None,
        };
        let store = MetadataStore::new(root.0.join("vm-mapping.json"));
        store
            .insert(mapping.clone())
            .expect("mapping should be persisted");

        Fixture {
            store,
            mapping,
            reboots: Arc::new(Mutex::new(Vec::new())),
            _root: root,
        }
    }

    /// A pipeline whose HCS half records every id it is asked to reboot and
    /// always answers `outcome`.
    fn pipeline(
        reboots: &Arc<Mutex<Vec<String>>>,
        outcome: impl Fn() -> Result<(), RebootFailure> + Send + Sync + Clone + 'static,
    ) -> VmRebootPipeline {
        let reboots = Arc::clone(reboots);
        VmRebootPipeline::for_test(move |id: &str| {
            reboots.lock().unwrap().push(id.to_owned());
            outcome()
        })
    }

    /// What HCS says when the request was delivered.
    fn hcs_delivered() -> Result<(), RebootFailure> {
        Ok(())
    }

    /// What HCS says when the guest offers no service to carry the request.
    fn unsupported_hcs() -> Result<(), RebootFailure> {
        Err(RebootFailure::Unsupported(RepositoryError::new(
            "HCS cannot deliver a reboot request (injected)",
        )))
    }

    /// What HCS says when it is failing for a reason of its own.
    fn failed_hcs() -> Result<(), RebootFailure> {
        Err(RebootFailure::Other(RepositoryError::new(
            "injected host failure",
        )))
    }

    /// An agent that answers one ask with `answer`, the way a session thread
    /// would. The handle says whether the ask ever arrived.
    fn agent_answering(
        vm_name: &str,
        answer: Result<(), String>,
    ) -> (RebootChannel, thread::JoinHandle<()>) {
        let (reboots, pending_reboots): (
            mpsc::Sender<crate::agent::AgentReboot>,
            mpsc::Receiver<crate::agent::AgentReboot>,
        ) = mpsc::channel();
        let served = thread::spawn(move || {
            let reboot = pending_reboots
                .recv_timeout(Duration::from_secs(5))
                .expect("the ask reaches the session queue");
            reboot.answer.send(answer).expect("the asker waits");
        });
        (RebootChannel::for_test(vm_name, true, reboots), served)
    }

    #[test]
    fn reboots_the_compute_system_mapped_to_the_vm() {
        let fixture = fixture("happy");

        pipeline(&fixture.reboots, hcs_delivered)
            .reboot(&fixture.store, "dev")
            .expect("reboot should succeed");

        assert_eq!(
            fixture.reboots.lock().unwrap().as_slice(),
            std::slice::from_ref(&fixture.mapping.hcs_compute_system_id)
        );
    }

    #[test]
    fn rejects_an_unmapped_vm_without_touching_hcs() {
        let fixture = fixture("unmapped");

        let error = pipeline(&fixture.reboots, hcs_delivered)
            .reboot(&fixture.store, "missing-vm")
            .expect_err("an unmapped VM must not be rebooted");

        let super::RebootFailure::Other(error) = error else {
            panic!("an unmapped VM is a refusal, not a failure with a way around it");
        };
        assert!(error.to_string().contains("missing-vm"));
        assert!(fixture.reboots.lock().unwrap().is_empty());
    }

    #[test]
    fn an_unsupported_hcs_reboot_is_answered_by_the_guests_own_agent() {
        let fixture = fixture("fallback-ok");
        let (channel, served) = agent_answering("dev", Ok(()));

        reboot_with_agent_fallback(
            &pipeline(&fixture.reboots, unsupported_hcs),
            &fixture.store,
            "dev",
            Some(channel),
        )
        .expect("the agent answered what HCS could not deliver");

        served.join().expect("the agent was asked");
        assert_eq!(fixture.reboots.lock().unwrap().as_slice(), ["vmlord-dev"]);
    }

    #[test]
    fn an_unsupported_hcs_reboot_with_no_agent_to_ask_is_refused() {
        let fixture = fixture("fallback-none");

        let error = reboot_with_agent_fallback(
            &pipeline(&fixture.reboots, unsupported_hcs),
            &fixture.store,
            "dev",
            None,
        )
        .expect_err("with no agent there is nobody left to ask");

        let message = error.to_string();
        assert!(message.contains("HCS cannot deliver"), "{message}");
        assert!(message.contains("not listening for an agent"), "{message}");
        assert!(
            message.contains("Stop the VM and start it again"),
            "{message}"
        );
    }

    #[test]
    fn a_reboot_both_paths_refused_names_both_reasons() {
        let fixture = fixture("fallback-refused");
        let (channel, served) = agent_answering(
            "dev",
            Err("systemctl reboot could not be queued".to_owned()),
        );

        let error = reboot_with_agent_fallback(
            &pipeline(&fixture.reboots, unsupported_hcs),
            &fixture.store,
            "dev",
            Some(channel),
        )
        .expect_err("both paths said no");

        served.join().expect("the agent was asked");
        let message = error.to_string();
        assert!(message.contains("HCS cannot deliver"), "{message}");
        assert!(
            message.contains("systemctl reboot could not be queued"),
            "{message}"
        );
        assert!(
            message.contains("Stop the VM and start it again"),
            "{message}"
        );
    }

    #[test]
    fn an_unsupported_hcs_reboot_of_an_offline_agent_is_refused() {
        let fixture = fixture("fallback-offline");
        let (reboots, _pending) = mpsc::channel();
        let channel = RebootChannel::for_test("dev", false, reboots);

        let error = reboot_with_agent_fallback(
            &pipeline(&fixture.reboots, unsupported_hcs),
            &fixture.store,
            "dev",
            Some(channel),
        )
        .expect_err("an agent with no session is nobody to ask");

        let message = error.to_string();
        assert!(message.contains("no open session"), "{message}");
        assert!(
            message.contains("Stop the VM and start it again"),
            "{message}"
        );
    }

    #[test]
    fn a_host_failure_is_reported_without_asking_the_guest() {
        let fixture = fixture("fallback-other");
        // A channel whose queue is already closed: were the fallback to ask
        // anyway, the ask would fail fast and its reason would surface here.
        let (reboots, pending_reboots) = mpsc::channel();
        drop(pending_reboots);
        let channel = RebootChannel::for_test("dev", true, reboots);

        let error = reboot_with_agent_fallback(
            &pipeline(&fixture.reboots, failed_hcs),
            &fixture.store,
            "dev",
            Some(channel),
        )
        .expect_err("a broken HCS is not the guest's to fix");

        let message = error.to_string();
        assert!(message.contains("injected host failure"), "{message}");
        assert!(!message.contains("agent"), "{message}");
    }
}
