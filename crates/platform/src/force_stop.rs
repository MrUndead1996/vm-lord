//! Forcibly stopping an HCS-backed virtual machine.

use std::time::Duration;

use uuid::Uuid;
use vmlord_core::{NetworkMode, RepositoryError};

use crate::{
    HcsSystem,
    hcs::HCS_ACCESS_ALL,
    metadata::{MetadataStore, VmComputeSystemMapping},
};

/// A termination needs nothing from the guest, so it completes as soon as HCS
/// has torn the VM down; the generous bound only guards against a wedged Host
/// Compute Service.
const FORCE_STOP_TIMEOUT: Duration = Duration::from_secs(60);

type AdapterDetacher = Box<dyn Fn(&str, Uuid) -> Result<(), RepositoryError> + Send + Sync>;
type SystemTerminator = Box<dyn Fn(&str) -> Result<(), RepositoryError> + Send + Sync>;

/// Forcibly stops VMs known to [`MetadataStore`].
pub struct VmForceStopPipeline {
    adapter_detacher: AdapterDetacher,
    system_terminator: SystemTerminator,
}

impl VmForceStopPipeline {
    /// Creates a pipeline backed by the real HCS API.
    #[must_use]
    pub fn production() -> Self {
        Self {
            adapter_detacher: Box::new(detach_network_adapter),
            system_terminator: Box::new(terminate_hcs_system),
        }
    }

    #[cfg(test)]
    fn for_test(
        detacher: impl Fn(&str, Uuid) -> Result<(), RepositoryError> + Send + Sync + 'static,
        terminator: impl Fn(&str) -> Result<(), RepositoryError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            adapter_detacher: Box::new(detacher),
            system_terminator: Box::new(terminator),
        }
    }

    /// Stops the VM named `vm_name` without involving its guest, the
    /// equivalent of pulling a physical machine's power cord.
    ///
    /// This is the fallback for a guest that cannot or will not service a
    /// graceful shutdown (see [`crate::VmShutdownPipeline`]), so it discards
    /// whatever the guest had not yet flushed to disk.
    ///
    /// A VM on the network loses its adapter first. HNS keeps an endpoint
    /// attached to the compute system it was handed to even after HCS destroys
    /// that system, so terminating with the adapter in place leaves the
    /// endpoint occupied and the next start fails with
    /// `HCN_E_ENDPOINT_ALREADY_ATTACHED`.
    ///
    /// HCS destroys the compute system as it stops, exactly as it does when a
    /// guest powers itself off, but nothing the VM is made of is lost: its
    /// disks, its stored configuration and its [`MetadataStore`] mapping all
    /// survive, and [`crate::VmStartPipeline`] rebuilds the compute system from
    /// them on the next start.
    pub fn force_stop(&self, store: &MetadataStore, vm_name: &str) -> Result<(), RepositoryError> {
        let mapping = store.find_by_vm_name(vm_name)?.ok_or_else(|| {
            let error = RepositoryError::new(format!("no HCS mapping found for VM \"{vm_name}\""));
            log::error!("{error}");
            error
        })?;

        log::info!(
            "forcibly stopping VM \"{}\" ({}) as HCS compute system \"{}\"",
            mapping.vm_name,
            mapping.vm_id,
            mapping.hcs_compute_system_id
        );

        self.detach_adapter(&mapping);

        (self.system_terminator)(&mapping.hcs_compute_system_id).inspect_err(|error| {
            log::error!(
                "failed to forcibly stop VM \"{}\": {error}",
                mapping.vm_name
            );
        })?;

        log::info!(
            "forcibly stopped VM \"{}\" ({})",
            mapping.vm_name,
            mapping.vm_id
        );
        Ok(())
    }

    /// Takes the VM's adapter off before the compute system is destroyed.
    ///
    /// A failure is reported but not propagated: force stop is the last way to
    /// stop a wedged VM, and refusing to terminate because the adapter would
    /// not come off would take that away. The consequence of a detach that did
    /// not happen -- an endpoint HNS still considers attached -- is recovered
    /// by [`crate::VmStartPipeline`] on the next start.
    fn detach_adapter(&self, mapping: &VmComputeSystemMapping) {
        let attached = (mapping.network_mode == NetworkMode::Nat)
            .then_some(mapping.endpoint_id)
            .flatten();
        let Some(endpoint_id) = attached else {
            log::debug!(
                "VM \"{}\" has no attached endpoint; terminating it without a detach",
                mapping.vm_name
            );
            return;
        };

        if let Err(error) = (self.adapter_detacher)(&mapping.hcs_compute_system_id, endpoint_id) {
            log::warn!(
                "the adapter of VM \"{}\" could not be detached before it was forcibly stopped: \
                 {error}; HNS keeps endpoint {endpoint_id} attached to the compute system being \
                 destroyed, so the next start has to replace the endpoint and the guest may be \
                 offered a different address",
                mapping.vm_name
            );
        }
    }
}

impl Default for VmForceStopPipeline {
    fn default() -> Self {
        Self::production()
    }
}

/// Takes a running VM's adapter off, treating a compute system HCS no longer
/// knows as nothing to detach.
fn detach_network_adapter(id: &str, endpoint_id: Uuid) -> Result<(), RepositoryError> {
    match HcsSystem::open_if_present(id, HCS_ACCESS_ALL)? {
        Some(system) => system.remove_network_adapter(endpoint_id),
        None => {
            log::debug!("HCS no longer knows compute system \"{id}\"; nothing to detach");
            Ok(())
        }
    }
}

fn terminate_hcs_system(id: &str) -> Result<(), RepositoryError> {
    // The system handle must outlive the termination operation it issued.
    let system = HcsSystem::open(id, HCS_ACCESS_ALL)?;
    system.terminate_and_wait(FORCE_STOP_TIMEOUT)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
        },
    };

    use uuid::Uuid;
    use vmlord_core::{NetworkMode, RepositoryError};

    use super::VmForceStopPipeline;
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
            "vmlord-force-stop-test-{label}-{}-{}",
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
        /// Every step in the order it ran, so a test can assert that the
        /// adapter came off before the VM was destroyed rather than only that
        /// both happened.
        steps: Arc<Mutex<Vec<&'static str>>>,
        detaches: Arc<Mutex<Vec<(String, Uuid)>>>,
        terminations: Arc<Mutex<Vec<String>>>,
    }

    fn fixture(label: &str) -> Fixture {
        fixture_with(label, NetworkMode::None, None)
    }

    fn fixture_with(label: &str, network_mode: NetworkMode, endpoint_id: Option<Uuid>) -> Fixture {
        let root = temp_root(label);
        let mapping = VmComputeSystemMapping {
            vm_id: Uuid::new_v4(),
            vm_name: "dev".into(),
            hcs_compute_system_id: "vmlord-dev".into(),
            disk_gb: 20,
            endpoint_id,
            network_mode,
        };
        let store = MetadataStore::new(root.0.join("vm-mapping.json"));
        store
            .insert(mapping.clone())
            .expect("mapping should be persisted");

        Fixture {
            store,
            mapping,
            steps: Arc::new(Mutex::new(Vec::new())),
            detaches: Arc::new(Mutex::new(Vec::new())),
            terminations: Arc::new(Mutex::new(Vec::new())),
            _root: root,
        }
    }

    /// Which collaborators fail; by default none of them do.
    #[derive(Clone, Copy, Default)]
    struct Behavior {
        fail_detach: bool,
        fail_terminate: bool,
    }

    fn pipeline(fixture: &Fixture, behavior: Behavior) -> VmForceStopPipeline {
        let detach_steps = Arc::clone(&fixture.steps);
        let detaches = Arc::clone(&fixture.detaches);
        let terminate_steps = Arc::clone(&fixture.steps);
        let terminations = Arc::clone(&fixture.terminations);
        VmForceStopPipeline::for_test(
            move |id: &str, endpoint_id: Uuid| {
                detach_steps.lock().unwrap().push("detach");
                detaches.lock().unwrap().push((id.to_owned(), endpoint_id));
                if behavior.fail_detach {
                    return Err(RepositoryError::new("injected detach failure"));
                }
                Ok(())
            },
            move |id: &str| {
                terminate_steps.lock().unwrap().push("terminate");
                terminations.lock().unwrap().push(id.to_owned());
                if behavior.fail_terminate {
                    return Err(RepositoryError::new("injected termination failure"));
                }
                Ok(())
            },
        )
    }

    #[test]
    fn terminates_the_compute_system_mapped_to_the_vm() {
        let fixture = fixture("happy");

        pipeline(&fixture, Behavior::default())
            .force_stop(&fixture.store, "dev")
            .expect("force stop should succeed");

        assert_eq!(
            fixture.terminations.lock().unwrap().as_slice(),
            std::slice::from_ref(&fixture.mapping.hcs_compute_system_id)
        );
    }

    #[test]
    fn rejects_an_unmapped_vm_without_touching_hcs() {
        let fixture = fixture("unmapped");

        let error = pipeline(&fixture, Behavior::default())
            .force_stop(&fixture.store, "missing-vm")
            .expect_err("an unmapped VM must not be stopped");

        assert!(error.to_string().contains("missing-vm"));
        assert!(fixture.terminations.lock().unwrap().is_empty());
    }

    #[test]
    fn propagates_a_termination_failure() {
        let fixture = fixture("failure");

        let error = pipeline(
            &fixture,
            Behavior {
                fail_terminate: true,
                ..Behavior::default()
            },
        )
        .force_stop(&fixture.store, "dev")
        .expect_err("a failed force stop must be reported");

        assert!(error.to_string().contains("injected termination failure"));
        assert_eq!(fixture.terminations.lock().unwrap().len(), 1);
    }

    #[test]
    fn leaves_the_mapping_in_place_so_the_vm_can_be_started_again() {
        let fixture = fixture("still-mapped");

        pipeline(&fixture, Behavior::default())
            .force_stop(&fixture.store, "dev")
            .expect("force stop should succeed");

        let mapping = fixture
            .store
            .find_by_vm_name("dev")
            .expect("the store should be readable")
            .expect("a forcibly stopped VM must stay known to the store");
        assert_eq!(mapping.hcs_compute_system_id, "vmlord-dev");
    }

    #[test]
    fn a_nat_vm_loses_its_adapter_before_the_compute_system_is_destroyed() {
        // HNS keeps the endpoint attached to a compute system HCS has already
        // destroyed, so detaching after the termination is too late: the
        // endpoint stays occupied and the next start fails.
        let endpoint_id = Uuid::new_v4();
        let fixture = fixture_with("nat-order", NetworkMode::Nat, Some(endpoint_id));

        pipeline(&fixture, Behavior::default())
            .force_stop(&fixture.store, "dev")
            .expect("force stop should succeed");

        assert_eq!(
            fixture.steps.lock().unwrap().clone(),
            vec!["detach", "terminate"]
        );
        assert_eq!(
            fixture.detaches.lock().unwrap().clone(),
            vec![(fixture.mapping.hcs_compute_system_id.clone(), endpoint_id)]
        );
    }

    #[test]
    fn a_vm_without_networking_is_terminated_without_a_detach() {
        let fixture = fixture("no-network");

        pipeline(&fixture, Behavior::default())
            .force_stop(&fixture.store, "dev")
            .expect("force stop should succeed");

        assert!(fixture.detaches.lock().unwrap().is_empty());
        assert_eq!(fixture.steps.lock().unwrap().clone(), vec!["terminate"]);
    }

    #[test]
    fn a_nat_vm_that_never_started_has_no_endpoint_to_detach() {
        // `endpoint_id` is only recorded by the first start, so a NAT VM
        // terminated before it ever ran has nothing attached.
        let fixture = fixture_with("nat-unstarted", NetworkMode::Nat, None);

        pipeline(&fixture, Behavior::default())
            .force_stop(&fixture.store, "dev")
            .expect("force stop should succeed");

        assert!(fixture.detaches.lock().unwrap().is_empty());
        assert_eq!(fixture.steps.lock().unwrap().clone(), vec!["terminate"]);
    }

    #[test]
    fn a_failed_detach_still_stops_the_vm() {
        // Force stop is the last way to stop a wedged VM; refusing to terminate
        // because the adapter would not come off would take that away. The
        // consequence -- an occupied endpoint -- is what the start's recovery
        // exists for.
        let endpoint_id = Uuid::new_v4();
        let fixture = fixture_with("detach-failure", NetworkMode::Nat, Some(endpoint_id));

        pipeline(
            &fixture,
            Behavior {
                fail_detach: true,
                ..Behavior::default()
            },
        )
        .force_stop(&fixture.store, "dev")
        .expect("a failed detach must not keep the VM running");

        assert_eq!(fixture.terminations.lock().unwrap().len(), 1);
    }

    #[test]
    fn rejects_an_unmapped_vm_without_detaching_anything() {
        let fixture = fixture("unmapped-detach");

        pipeline(&fixture, Behavior::default())
            .force_stop(&fixture.store, "missing-vm")
            .expect_err("an unmapped VM must not be stopped");

        assert!(fixture.detaches.lock().unwrap().is_empty());
    }
}
