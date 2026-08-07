//! Gracefully shutting down an HCS-backed virtual machine.

use std::time::Duration;

use vmlord_core::RepositoryError;

use crate::{HcsSystem, hcs::HCS_ACCESS_ALL, metadata::MetadataStore};

/// A shutdown operation completes once HCS has delivered the request to the
/// guest, well before the guest OS has finished powering off; the generous
/// bound only guards against a wedged Host Compute Service.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(60);

type SystemShutter = Box<dyn Fn(&str) -> Result<(), RepositoryError>>;

/// Requests a graceful guest shutdown of VMs known to [`MetadataStore`].
pub struct VmShutdownPipeline {
    system_shutter: SystemShutter,
}

impl VmShutdownPipeline {
    /// Creates a pipeline backed by the real HCS API.
    #[must_use]
    pub fn production() -> Self {
        Self {
            system_shutter: Box::new(shut_down_hcs_system),
        }
    }

    #[cfg(test)]
    fn for_test(shutter: impl Fn(&str) -> Result<(), RepositoryError> + 'static) -> Self {
        Self {
            system_shutter: Box::new(shutter),
        }
    }

    /// Asks the guest of the VM named `vm_name` to shut down.
    ///
    /// Returning `Ok` means HCS accepted and delivered the request, not that
    /// the guest has powered off: a guest without integration services, or one
    /// refusing the request, keeps running. Callers that must guarantee the VM
    /// stops need to fall back to a forced stop.
    pub fn shutdown(&self, store: &MetadataStore, vm_name: &str) -> Result<(), RepositoryError> {
        let mapping = store.find_by_vm_name(vm_name)?.ok_or_else(|| {
            let error = RepositoryError::new(format!("no HCS mapping found for VM \"{vm_name}\""));
            log::error!("{error}");
            error
        })?;

        log::info!(
            "requesting graceful shutdown of VM \"{}\" ({}) as HCS compute system \"{}\"",
            mapping.vm_name,
            mapping.vm_id,
            mapping.hcs_compute_system_id
        );

        (self.system_shutter)(&mapping.hcs_compute_system_id).inspect_err(|error| {
            log::error!("failed to shut down VM \"{}\": {error}", mapping.vm_name);
        })?;

        log::info!(
            "the guest of VM \"{}\" ({}) accepted the shutdown request",
            mapping.vm_name,
            mapping.vm_id
        );
        Ok(())
    }
}

impl Default for VmShutdownPipeline {
    fn default() -> Self {
        Self::production()
    }
}

fn shut_down_hcs_system(id: &str) -> Result<(), RepositoryError> {
    // The system handle must outlive the shutdown operation it issued.
    let system = HcsSystem::open(id, HCS_ACCESS_ALL)?;
    system.shutdown_and_wait(SHUTDOWN_TIMEOUT)
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

    use super::VmShutdownPipeline;
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
            "vmlord-shutdown-test-{label}-{}-{}",
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
        shutdowns: Arc<Mutex<Vec<String>>>,
    }

    fn fixture(label: &str) -> Fixture {
        let root = temp_root(label);
        let mapping = VmComputeSystemMapping {
            vm_id: Uuid::new_v4(),
            vm_name: "dev".into(),
            hcs_compute_system_id: "vmlord-dev".into(),
        };
        let store = MetadataStore::new(root.0.join("vm-mapping.json"));
        store
            .insert(mapping.clone())
            .expect("mapping should be persisted");

        Fixture {
            store,
            mapping,
            shutdowns: Arc::new(Mutex::new(Vec::new())),
            _root: root,
        }
    }

    fn pipeline(shutdowns: &Arc<Mutex<Vec<String>>>, fail: bool) -> VmShutdownPipeline {
        let shutdowns = Arc::clone(shutdowns);
        VmShutdownPipeline::for_test(move |id: &str| {
            shutdowns.lock().unwrap().push(id.to_owned());
            if fail {
                return Err(RepositoryError::new("injected shutdown failure"));
            }
            Ok(())
        })
    }

    #[test]
    fn shuts_down_the_compute_system_mapped_to_the_vm() {
        let fixture = fixture("happy");

        pipeline(&fixture.shutdowns, false)
            .shutdown(&fixture.store, "dev")
            .expect("shutdown should succeed");

        assert_eq!(
            fixture.shutdowns.lock().unwrap().as_slice(),
            std::slice::from_ref(&fixture.mapping.hcs_compute_system_id)
        );
    }

    #[test]
    fn rejects_an_unmapped_vm_without_touching_hcs() {
        let fixture = fixture("unmapped");

        let error = pipeline(&fixture.shutdowns, false)
            .shutdown(&fixture.store, "missing-vm")
            .expect_err("an unmapped VM must not be shut down");

        assert!(error.to_string().contains("missing-vm"));
        assert!(fixture.shutdowns.lock().unwrap().is_empty());
    }

    #[test]
    fn propagates_a_shutdown_failure() {
        let fixture = fixture("failure");

        let error = pipeline(&fixture.shutdowns, true)
            .shutdown(&fixture.store, "dev")
            .expect_err("a failed shutdown must be reported");

        assert!(error.to_string().contains("injected shutdown failure"));
        assert_eq!(fixture.shutdowns.lock().unwrap().len(), 1);
    }
}
