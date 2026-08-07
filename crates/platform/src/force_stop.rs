//! Forcibly stopping an HCS-backed virtual machine.

use std::time::Duration;

use vmlord_core::RepositoryError;

use crate::{HcsSystem, hcs::HCS_ACCESS_ALL, metadata::MetadataStore};

/// A termination needs nothing from the guest, so it completes as soon as HCS
/// has torn the VM down; the generous bound only guards against a wedged Host
/// Compute Service.
const FORCE_STOP_TIMEOUT: Duration = Duration::from_secs(60);

type SystemTerminator = Box<dyn Fn(&str) -> Result<(), RepositoryError>>;

/// Forcibly stops VMs known to [`MetadataStore`].
pub struct VmForceStopPipeline {
    system_terminator: SystemTerminator,
}

impl VmForceStopPipeline {
    /// Creates a pipeline backed by the real HCS API.
    #[must_use]
    pub fn production() -> Self {
        Self {
            system_terminator: Box::new(terminate_hcs_system),
        }
    }

    #[cfg(test)]
    fn for_test(terminator: impl Fn(&str) -> Result<(), RepositoryError> + 'static) -> Self {
        Self {
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
}

impl Default for VmForceStopPipeline {
    fn default() -> Self {
        Self::production()
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
    use vmlord_core::RepositoryError;

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
        terminations: Arc<Mutex<Vec<String>>>,
    }

    fn fixture(label: &str) -> Fixture {
        let root = temp_root(label);
        let mapping = VmComputeSystemMapping {
            vm_id: Uuid::new_v4(),
            vm_name: "dev".into(),
            hcs_compute_system_id: "vmlord-dev".into(),
            disk_gb: 20,
        };
        let store = MetadataStore::new(root.0.join("vm-mapping.json"));
        store
            .insert(mapping.clone())
            .expect("mapping should be persisted");

        Fixture {
            store,
            mapping,
            terminations: Arc::new(Mutex::new(Vec::new())),
            _root: root,
        }
    }

    fn pipeline(terminations: &Arc<Mutex<Vec<String>>>, fail: bool) -> VmForceStopPipeline {
        let terminations = Arc::clone(terminations);
        VmForceStopPipeline::for_test(move |id: &str| {
            terminations.lock().unwrap().push(id.to_owned());
            if fail {
                return Err(RepositoryError::new("injected termination failure"));
            }
            Ok(())
        })
    }

    #[test]
    fn terminates_the_compute_system_mapped_to_the_vm() {
        let fixture = fixture("happy");

        pipeline(&fixture.terminations, false)
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

        let error = pipeline(&fixture.terminations, false)
            .force_stop(&fixture.store, "missing-vm")
            .expect_err("an unmapped VM must not be stopped");

        assert!(error.to_string().contains("missing-vm"));
        assert!(fixture.terminations.lock().unwrap().is_empty());
    }

    #[test]
    fn propagates_a_termination_failure() {
        let fixture = fixture("failure");

        let error = pipeline(&fixture.terminations, true)
            .force_stop(&fixture.store, "dev")
            .expect_err("a failed force stop must be reported");

        assert!(error.to_string().contains("injected termination failure"));
        assert_eq!(fixture.terminations.lock().unwrap().len(), 1);
    }

    #[test]
    fn leaves_the_mapping_in_place_so_the_vm_can_be_started_again() {
        let fixture = fixture("still-mapped");

        pipeline(&fixture.terminations, false)
            .force_stop(&fixture.store, "dev")
            .expect("force stop should succeed");

        let mapping = fixture
            .store
            .find_by_vm_name("dev")
            .expect("the store should be readable")
            .expect("a forcibly stopped VM must stay known to the store");
        assert_eq!(mapping.hcs_compute_system_id, "vmlord-dev");
    }
}
