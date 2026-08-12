//! Removing the resources a VMLord VM is made of.
//!
//! Creation rollback and deletion tear the same two things down -- the HCS
//! compute system and the VM directory -- and report a partial failure the same
//! way, so both drive them from here.

use std::{fs, path::Path, time::Duration};

use uuid::Uuid;
use vmlord_core::RepositoryError;

use crate::{
    HcsSystem,
    hcn_endpoint::HcnEndpoint,
    hcs::HCS_ACCESS_ALL,
    metadata::{MetadataStore, VmComputeSystemMapping},
};

/// A teardown needs nothing from the guest, so it completes as soon as HCS has
/// torn the compute system down; the bound only guards against a wedged Host
/// Compute Service.
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(30);

/// How a pipeline reaches HCS to tear a compute system down.
///
/// Injected rather than called directly so the pipelines can be tested without
/// a Hyper-V host.
///
/// `Send + Sync` because the creation pipeline holds one and runs on a thread
/// of its own.
pub(crate) type SystemTeardown = Box<dyn Fn(&str) -> Result<(), RepositoryError> + Send + Sync>;

/// How a pipeline reaches HNS to delete a VM's endpoint.
///
/// Injected for the same reason as [`SystemTeardown`]: a deletion has to be
/// testable without a Hyper-V host.
pub(crate) type EndpointTeardown = Box<dyn Fn(Uuid) -> Result<(), RepositoryError> + Send + Sync>;

/// Terminates the compute system `id`, treating one HCS does not know as
/// already gone.
///
/// A compute system exists only while it is created or running: HCS destroys it
/// as the VM stops. A VM that is not running therefore routinely has none, and
/// that is a fact about its state rather than a failure to remove it.
pub(crate) fn teardown_compute_system(id: &str) -> Result<(), RepositoryError> {
    let Some(system) = HcsSystem::open_if_present(id, HCS_ACCESS_ALL)? else {
        log::debug!("HCS compute system \"{id}\" is already gone; nothing to tear down");
        return Ok(());
    };
    system.terminate_and_wait(TEARDOWN_TIMEOUT)
}

/// Removes the endpoints in VMLord's network that no recorded VM owns.
///
/// Orphans accumulate for real: an endpoint's identifier is allocated by the
/// caller and written to [`MetadataStore`] only after HNS has created the
/// endpoint, so a VMLord that dies in between leaves behind an endpoint nothing
/// will ever find again. It holds an address out of the network's subnet for as
/// long as HNS has it.
///
/// Only the endpoints in VMLord's own network are considered at all, and among
/// those only the ones no mapping names. The network itself is never deleted --
/// not even when the last VM is gone -- because re-creating it would re-pick the
/// subnet and move every guest's address, which is the whole thing a long-lived
/// endpoint exists to avoid.
///
/// This runs from `initialize`, before this process has started any VM, so it
/// cannot collect an endpoint of its own. Two cases can still put a live VM's
/// endpoint on the list: a second VMLord process creating one at that exact
/// moment, and a store that is not the one the endpoint was recorded in --
/// VMLord's VM storage directory is the user's to change, and the mappings live
/// under it. Both cost the same and no more: that VM's next start finds its
/// recorded endpoint gone and creates another, so the guest changes address
/// rather than losing its VM.
pub(crate) fn remove_orphan_endpoints(store: &MetadataStore) -> Result<(), RepositoryError> {
    let orphans = orphan_endpoints(&HcnEndpoint::list_in_vmlord_network()?, &store.list()?);
    if orphans.is_empty() {
        log::debug!("every endpoint in the VMLord network belongs to a VM VMLord knows");
        return Ok(());
    }

    // Info rather than a diagnostic: collecting these is routine housekeeping
    // with nothing for the user to do about it, and the endpoints belong to VMs
    // that no longer exist.
    log::info!(
        "removing {} endpoint(s) in the VMLord network that no VM owns",
        orphans.len()
    );
    let mut failures = Vec::new();
    for id in orphans {
        if let Err(error) = HcnEndpoint::delete(id) {
            failures.push(format!("{id}: {error}"));
        }
    }

    if !failures.is_empty() {
        return Err(combine_failures(
            "some endpoints that no VM owns were not removed",
            failures,
        ));
    }
    Ok(())
}

/// The listed endpoints that no mapping names.
///
/// The mappings are the whole of what VMLord knows about its endpoints, so an
/// endpoint no mapping names is one no VM can ever use again.
fn orphan_endpoints(listed: &[Uuid], mappings: &[VmComputeSystemMapping]) -> Vec<Uuid> {
    listed
        .iter()
        .filter(|id| {
            !mappings
                .iter()
                .any(|mapping| mapping.endpoint_id == Some(**id))
        })
        .copied()
        .collect()
}

/// Removes `vm_directory` and everything under it, treating an absent directory
/// as already removed.
pub(crate) fn remove_vm_directory(vm_directory: &Path) -> Result<(), RepositoryError> {
    if !vm_directory.exists() {
        log::debug!("VM directory {} is already gone", vm_directory.display());
        return Ok(());
    }

    fs::remove_dir_all(vm_directory).map_err(|error| {
        let error = RepositoryError::new(format!(
            "failed to remove VM directory {}: {error}",
            vm_directory.display()
        ));
        log::error!("{error}");
        error
    })?;
    log::debug!("removed VM directory {}", vm_directory.display());
    Ok(())
}

/// Folds the failures a best-effort cleanup collected into a single error.
pub(crate) fn combine_failures(prefix: &str, failures: Vec<String>) -> RepositoryError {
    let error = RepositoryError::new(format!("{prefix}: {}", failures.join("; ")));
    log::error!("{error}");
    error
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    use uuid::Uuid;
    use vmlord_core::NetworkMode;

    use super::{combine_failures, orphan_endpoints, remove_vm_directory};
    use crate::metadata::VmComputeSystemMapping;

    fn mapping(vm_name: &str, endpoint_id: Option<Uuid>) -> VmComputeSystemMapping {
        VmComputeSystemMapping {
            vm_id: Uuid::new_v4(),
            vm_name: vm_name.into(),
            hcs_compute_system_id: format!("vmlord-{vm_name}"),
            disk_gb: 20,
            endpoint_id,
            network_mode: NetworkMode::Nat,
            ssh: None,
        }
    }

    struct TempRoot(PathBuf);

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temp_root(label: &str) -> TempRoot {
        static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "vmlord-cleanup-test-{label}-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("test root should be created");
        TempRoot(path)
    }

    #[test]
    fn removes_a_vm_directory_with_everything_under_it() {
        let root = temp_root("populated");
        let vm_directory = root.0.join("vm");
        fs::create_dir_all(vm_directory.join("disks")).expect("disks directory should be created");
        fs::write(vm_directory.join("config.json"), b"{}")
            .expect("configuration should be written");
        fs::write(vm_directory.join("disks").join("system.vhdx"), b"vhdx")
            .expect("disk should be written");

        remove_vm_directory(&vm_directory).expect("a populated VM directory should be removed");

        assert!(!vm_directory.exists());
    }

    /// The key must not outlive the VM it belongs to: nothing else ever
    /// deletes it, so deleting the directory is the whole of its lifecycle.
    #[test]
    fn a_removed_vm_takes_its_ssh_key_with_it() {
        let root = temp_root("keys");
        let vm_directory = root.0.join("vm");
        let pair = vmlord_keys::generate("dev-linux").expect("a key pair should be generated");
        crate::vm_key::write_key_pair(&vm_directory, &pair)
            .expect("the key pair should be written");

        remove_vm_directory(&vm_directory).expect("a VM directory with a key should be removed");

        assert!(!crate::layout::ssh_key_path(&vm_directory).exists());
        assert!(!vm_directory.exists());
    }

    #[test]
    fn an_absent_vm_directory_is_already_removed() {
        let root = temp_root("absent");

        remove_vm_directory(&root.0.join("never-created"))
            .expect("an absent VM directory must not be reported as a failure");
    }

    #[test]
    fn an_endpoint_no_mapping_names_is_an_orphan() {
        let owned = Uuid::new_v4();
        let orphan = Uuid::new_v4();

        assert_eq!(
            orphan_endpoints(&[owned, orphan], &[mapping("dev", Some(owned))]),
            vec![orphan]
        );
    }

    #[test]
    fn every_endpoint_a_mapping_names_is_kept() {
        // These are the endpoints of live VMs, including stopped ones: an
        // endpoint outlives its VM's stops, and collecting it would hand the
        // guest a new address on its next start.
        let dev = Uuid::new_v4();
        let build = Uuid::new_v4();

        assert_eq!(
            orphan_endpoints(
                &[dev, build],
                &[mapping("dev", Some(dev)), mapping("build", Some(build))]
            ),
            Vec::<Uuid>::new()
        );
    }

    #[test]
    fn a_vm_that_has_never_started_orphans_nothing() {
        // A VM with no endpoint recorded owns no endpoint, so it neither keeps
        // one alive nor makes one an orphan.
        let orphan = Uuid::new_v4();

        assert_eq!(
            orphan_endpoints(&[orphan], &[mapping("never-started", None)]),
            vec![orphan]
        );
    }

    #[test]
    fn nothing_listed_leaves_nothing_to_remove() {
        assert_eq!(
            orphan_endpoints(&[], &[mapping("dev", None)]),
            Vec::<Uuid>::new()
        );
    }

    #[test]
    fn combined_failures_name_every_step_that_failed() {
        let error = combine_failures(
            "deletion of VM \"dev\" did not complete",
            vec!["teardown failed".into(), "removal failed".into()],
        );

        let message = error.to_string();
        assert!(message.contains("deletion of VM \"dev\" did not complete"));
        assert!(message.contains("teardown failed"));
        assert!(message.contains("removal failed"));
    }
}
