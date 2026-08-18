//! Deleting an HCS-backed virtual machine and everything it is made of.

use std::{fs, path::Path};

#[cfg(test)]
use uuid::Uuid;
use vmlord_core::RepositoryError;

use crate::{
    cleanup::{self, EndpointTeardown, SystemTeardown},
    hcn_endpoint::HcnEndpoint,
    layout,
    metadata::MetadataStore,
};

/// Deletes VMs known to [`MetadataStore`].
pub struct VmDeletionPipeline {
    system_teardown: SystemTeardown,
    endpoint_teardown: EndpointTeardown,
}

impl VmDeletionPipeline {
    /// Creates a pipeline backed by the real HCS and HNS APIs.
    #[must_use]
    pub fn production() -> Self {
        Self {
            system_teardown: Box::new(cleanup::teardown_compute_system),
            endpoint_teardown: Box::new(HcnEndpoint::delete),
        }
    }

    #[cfg(test)]
    fn for_test(
        teardown: impl Fn(&str) -> Result<(), RepositoryError> + Send + Sync + 'static,
        endpoint_teardown: impl Fn(Uuid) -> Result<(), RepositoryError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            system_teardown: Box::new(teardown),
            endpoint_teardown: Box::new(endpoint_teardown),
        }
    }

    /// Removes everything VMLord created for the VM named `vm_name`: its HCS
    /// compute system, its endpoint in VMLord's shared network, its files under
    /// `vm_directory`, and its mapping.
    ///
    /// The steps run in that order and each one runs even if an earlier one
    /// failed, because a resource left behind is not a reason to leave the
    /// others behind too. The mapping is removed last and only when nothing
    /// failed: a VM whose resources are still partly present stays known to
    /// VMLord, stays visible to the user, and can be deleted again. Removing it
    /// from the store first would turn a partial failure into orphaned files
    /// and compute systems the application can no longer reach.
    ///
    /// The endpoint goes after the compute system it may still be attached to,
    /// and it is the VM's only network resource to remove: the shared network
    /// stays, whether or not this was the last VM in it. Re-creating it would
    /// re-pick the subnet and move every guest's address, which is exactly what
    /// keeping an endpoint for the life of its VM exists to avoid.
    ///
    /// With `delete_disks` the whole VM directory goes; without it the disks are
    /// left for the user and everything that only served the VM itself -- its
    /// configuration and its SSH identity -- still goes, as [`remove_files`]
    /// describes. The image the VM was installed from is never touched: it
    /// belongs to the user, not to the VM.
    pub fn delete(
        &self,
        store: &MetadataStore,
        vm_name: &str,
        vm_directory: &Path,
        delete_disks: bool,
    ) -> Result<(), RepositoryError> {
        let mapping = store.find_by_vm_name(vm_name)?.ok_or_else(|| {
            let error = RepositoryError::new(format!("no HCS mapping found for VM \"{vm_name}\""));
            log::error!("{error}");
            error
        })?;

        log::info!(
            "deleting VM \"{}\" ({}) as HCS compute system \"{}\", {}",
            mapping.vm_name,
            mapping.vm_id,
            mapping.hcs_compute_system_id,
            if delete_disks {
                "disks included"
            } else {
                "keeping its disks"
            }
        );

        let mut failures = Vec::new();
        if let Err(error) = (self.system_teardown)(&mapping.hcs_compute_system_id) {
            failures.push(format!("its compute system was not torn down: {error}"));
        }
        if let Some(endpoint_id) = mapping.endpoint_id
            && let Err(error) = (self.endpoint_teardown)(endpoint_id)
        {
            failures.push(format!(
                "its endpoint {endpoint_id} was not deleted: {error}"
            ));
        }
        if let Err(error) = remove_files(vm_directory, delete_disks) {
            failures.push(format!("its files were not removed: {error}"));
        }

        if !failures.is_empty() {
            log::warn!(
                "VM \"{}\" ({}) stays known to VMLord because its deletion did not complete",
                mapping.vm_name,
                mapping.vm_id
            );
            return Err(cleanup::combine_failures(
                &format!("deletion of VM \"{}\" did not complete", mapping.vm_name),
                failures,
            ));
        }

        store.remove(mapping.vm_id)?;
        if !delete_disks {
            log::warn!(
                "the disks of VM \"{}\" were kept under {}",
                mapping.vm_name,
                vm_directory.display()
            );
        }
        log::info!("deleted VM \"{}\" ({})", mapping.vm_name, mapping.vm_id);
        Ok(())
    }
}

impl Default for VmDeletionPipeline {
    fn default() -> Self {
        Self::production()
    }
}

/// Removes the VM's files, honouring the user's choice about its disks.
///
/// Keeping the disks keeps the disks and nothing else that only made sense
/// while the VM existed. The HCS configuration describes a compute system that
/// is gone, and the VM's SSH identity -- its key pair and the host keys VMLord
/// learned for it -- belongs to a guest nobody can reach through VMLord any
/// more: a private key with no owner is worth removing on its own, and a
/// `known_hosts` file kept past its VM would only pin the keys of a host that no
/// longer answers. The kept disks are the user's to attach elsewhere, and a
/// guest booted from them brings its own `authorized_keys` with it, so it is
/// theirs to give a key of their own.
///
/// The agent secret goes with them, for the same reason: it authenticates an
/// agent as this VM, and the VM it would speak for no longer exists. The
/// guest's own copy rides along in the seed, which stays with the disks it
/// provisioned.
///
/// `cloud-init-status.log` and `com1.log` stay: they record what the VM did
/// rather than how to log into it, which is exactly what a person who kept the
/// disks may still need to read.
fn remove_files(vm_directory: &Path, delete_disks: bool) -> Result<(), RepositoryError> {
    if delete_disks {
        return cleanup::remove_vm_directory(vm_directory);
    }

    // Each removal runs even if an earlier one failed, for the reason the whole
    // deletion works that way: one file left behind is not a reason to leave the
    // others.
    let mut failures = Vec::new();
    if let Err(error) = remove_file_if_present(
        &layout::configuration_path(vm_directory),
        "the HCS configuration",
    ) {
        failures.push(error.to_string());
    }
    if let Err(error) = remove_directory_if_present(
        &layout::ssh_keys_directory(vm_directory),
        "the SSH key pair",
    ) {
        failures.push(error.to_string());
    }
    if let Err(error) = remove_file_if_present(
        &layout::ssh_known_hosts_path(vm_directory),
        "the learned SSH host keys",
    ) {
        failures.push(error.to_string());
    }
    if let Err(error) =
        remove_file_if_present(&layout::agent_secret_path(vm_directory), "the agent secret")
    {
        failures.push(error.to_string());
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(cleanup::combine_failures(
            "some files of the deleted VM were not removed",
            failures,
        ))
    }
}

/// Removes `path`, treating a file that is not there as already removed.
fn remove_file_if_present(path: &Path, description: &str) -> Result<(), RepositoryError> {
    if !path.exists() {
        log::debug!(
            "{description} of the deleted VM at {} is already gone",
            path.display()
        );
        return Ok(());
    }
    fs::remove_file(path).map_err(|error| {
        let error = RepositoryError::new(format!(
            "failed to remove {description} of the deleted VM at {}: {error}",
            path.display()
        ));
        log::error!("{error}");
        error
    })?;
    log::debug!(
        "removed {description} of the deleted VM at {}",
        path.display()
    );
    Ok(())
}

/// Removes `path` and everything under it, treating an absent directory as
/// already removed.
fn remove_directory_if_present(path: &Path, description: &str) -> Result<(), RepositoryError> {
    if !path.exists() {
        log::debug!(
            "{description} of the deleted VM at {} is already gone",
            path.display()
        );
        return Ok(());
    }
    fs::remove_dir_all(path).map_err(|error| {
        let error = RepositoryError::new(format!(
            "failed to remove {description} of the deleted VM at {}: {error}",
            path.display()
        ));
        log::error!("{error}");
        error
    })?;
    log::debug!(
        "removed {description} of the deleted VM at {}",
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
        },
    };

    use uuid::Uuid;
    use vmlord_core::{NetworkMode, RepositoryError};

    use super::VmDeletionPipeline;
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
            "vmlord-delete-test-{label}-{}-{}",
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
        vm_directory: PathBuf,
        teardowns: Arc<Mutex<Vec<String>>>,
        endpoint_teardowns: Arc<Mutex<Vec<Uuid>>>,
        /// What was torn down, in the order the pipeline asked for it.
        order: Arc<Mutex<Vec<&'static str>>>,
    }

    /// A VM as it looks on disk once creation is done and it has been started
    /// once: a configuration document, a system disk, the SSH key pair it was
    /// given, the host key VMLord learned from it, and the readiness transcript
    /// of its first boot.
    fn fixture(label: &str) -> Fixture {
        fixture_with(label, None)
    }

    /// The same VM, with the endpoint a first start would have recorded for it.
    fn fixture_with(label: &str, endpoint_id: Option<Uuid>) -> Fixture {
        let root = temp_root(label);
        let vm_directory = root.0.join("dev");
        fs::create_dir_all(vm_directory.join("disks")).expect("disks directory should be created");
        fs::write(vm_directory.join("config.json"), b"{}")
            .expect("configuration should be written");
        fs::write(vm_directory.join("disks").join("system.vhdx"), b"vhdx")
            .expect("system disk should be written");
        fs::create_dir_all(crate::layout::ssh_keys_directory(&vm_directory))
            .expect("keys directory should be created");
        fs::write(crate::layout::ssh_key_path(&vm_directory), b"private")
            .expect("private key should be written");
        fs::write(crate::layout::ssh_public_key_path(&vm_directory), b"public")
            .expect("public key should be written");
        fs::write(crate::layout::ssh_known_hosts_path(&vm_directory), b"host")
            .expect("known hosts should be written");
        fs::write(
            crate::layout::cloud_init_status_log_path(&vm_directory),
            b"status: done",
        )
        .expect("readiness transcript should be written");
        fs::write(crate::layout::agent_secret_path(&vm_directory), b"c2VjcmV0")
            .expect("agent secret should be written");

        let mapping = VmComputeSystemMapping {
            vm_id: Uuid::new_v4(),
            vm_name: "dev".into(),
            hcs_compute_system_id: "vmlord-dev".into(),
            disk_gb: 20,
            endpoint_id,
            network_mode: if endpoint_id.is_some() {
                NetworkMode::Nat
            } else {
                NetworkMode::None
            },
            ssh: None,
            gpu_mode: vmlord_core::GpuMode::None,
            guest_target: None,
        };
        let store = MetadataStore::new(root.0.join("vm-mapping.json"));
        store
            .insert(mapping.clone())
            .expect("mapping should be persisted");

        Fixture {
            store,
            mapping,
            vm_directory,
            teardowns: Arc::new(Mutex::new(Vec::new())),
            endpoint_teardowns: Arc::new(Mutex::new(Vec::new())),
            order: Arc::new(Mutex::new(Vec::new())),
            _root: root,
        }
    }

    fn pipeline(fixture: &Fixture, fail: bool) -> VmDeletionPipeline {
        pipeline_with(fixture, fail, false)
    }

    fn pipeline_with(fixture: &Fixture, fail: bool, fail_endpoint: bool) -> VmDeletionPipeline {
        let teardowns = Arc::clone(&fixture.teardowns);
        let endpoint_teardowns = Arc::clone(&fixture.endpoint_teardowns);
        let system_order = Arc::clone(&fixture.order);
        let endpoint_order = Arc::clone(&fixture.order);
        VmDeletionPipeline::for_test(
            move |id: &str| {
                teardowns.lock().unwrap().push(id.to_owned());
                system_order.lock().unwrap().push("system");
                if fail {
                    return Err(RepositoryError::new("injected teardown failure"));
                }
                Ok(())
            },
            move |endpoint_id: Uuid| {
                endpoint_teardowns.lock().unwrap().push(endpoint_id);
                endpoint_order.lock().unwrap().push("endpoint");
                if fail_endpoint {
                    return Err(RepositoryError::new("injected endpoint failure"));
                }
                Ok(())
            },
        )
    }

    #[test]
    fn removes_the_compute_system_the_directory_and_the_mapping() {
        let fixture = fixture("happy");

        pipeline(&fixture, false)
            .delete(&fixture.store, "dev", &fixture.vm_directory, true)
            .expect("deletion should succeed");

        assert_eq!(
            fixture.teardowns.lock().unwrap().as_slice(),
            std::slice::from_ref(&fixture.mapping.hcs_compute_system_id)
        );
        assert!(!fixture.vm_directory.exists());
        assert!(
            fixture
                .store
                .find_by_vm_name("dev")
                .expect("the store should be readable")
                .is_none(),
            "a fully deleted VM must no longer be known to VMLord"
        );
    }

    #[test]
    fn removes_the_endpoint_of_a_vm_that_has_one() {
        let endpoint_id = Uuid::new_v4();
        let fixture = fixture_with("endpoint", Some(endpoint_id));

        pipeline(&fixture, false)
            .delete(&fixture.store, "dev", &fixture.vm_directory, true)
            .expect("deletion should succeed");

        assert_eq!(
            fixture.endpoint_teardowns.lock().unwrap().as_slice(),
            [endpoint_id],
            "an endpoint left in HNS holds an address of the network's subnet forever"
        );
    }

    #[test]
    fn a_vm_that_never_started_has_no_endpoint_to_remove() {
        let fixture = fixture("no-endpoint");

        pipeline(&fixture, false)
            .delete(&fixture.store, "dev", &fixture.vm_directory, true)
            .expect("deletion should succeed");

        assert!(fixture.endpoint_teardowns.lock().unwrap().is_empty());
    }

    #[test]
    fn a_failed_endpoint_removal_keeps_the_mapping_and_removes_the_rest() {
        // The endpoint is one resource among the VM's, under the same rules as
        // the others: the remaining steps still run, and the VM stays known so
        // the endpoint can be removed by deleting it again.
        let endpoint_id = Uuid::new_v4();
        let fixture = fixture_with("endpoint-failure", Some(endpoint_id));

        let error = pipeline_with(&fixture, false, true)
            .delete(&fixture.store, "dev", &fixture.vm_directory, true)
            .expect_err("a failed endpoint removal must be reported");

        assert!(error.to_string().contains("injected endpoint failure"));
        assert!(error.to_string().contains(&endpoint_id.to_string()));
        assert!(
            !fixture.vm_directory.exists(),
            "a failed endpoint removal must not stop the remaining cleanup"
        );
        assert!(
            fixture
                .store
                .find_by_vm_name("dev")
                .expect("the store should be readable")
                .is_some(),
            "a VM whose endpoint is still present must stay known to VMLord"
        );
    }

    #[test]
    fn the_endpoint_is_removed_after_the_compute_system_it_may_be_attached_to() {
        // HNS keeps an endpoint attached to the compute system it was handed
        // to, so deleting it before that system is torn down asks HNS to remove
        // something still in use.
        let endpoint_id = Uuid::new_v4();
        let fixture = fixture_with("endpoint-order", Some(endpoint_id));

        pipeline_with(&fixture, true, false)
            .delete(&fixture.store, "dev", &fixture.vm_directory, true)
            .expect_err("a failed teardown must be reported");

        assert_eq!(
            fixture.order.lock().unwrap().as_slice(),
            ["system", "endpoint"]
        );
        assert_eq!(
            fixture.endpoint_teardowns.lock().unwrap().as_slice(),
            [endpoint_id],
            "a failed compute system teardown must not stop the endpoint removal"
        );
    }

    #[test]
    fn keeping_the_disks_removes_the_configuration_but_not_the_disk() {
        let fixture = fixture("keep-disks");

        pipeline(&fixture, false)
            .delete(&fixture.store, "dev", &fixture.vm_directory, false)
            .expect("deletion should succeed");

        assert!(
            !fixture.vm_directory.join("config.json").exists(),
            "the configuration describes a VM that no longer exists"
        );
        assert!(
            fixture
                .vm_directory
                .join("disks")
                .join("system.vhdx")
                .exists(),
            "the disks must survive when the user asked to keep them"
        );
        assert!(
            fixture
                .store
                .find_by_vm_name("dev")
                .expect("the store should be readable")
                .is_none()
        );
    }

    #[test]
    fn keeping_the_disks_still_takes_the_ssh_identity_with_the_vm() {
        // The user asked to keep the disks, not to keep a private key for a VM
        // that no longer exists.
        let fixture = fixture("keep-disks-ssh");

        pipeline(&fixture, false)
            .delete(&fixture.store, "dev", &fixture.vm_directory, false)
            .expect("deletion should succeed");

        assert!(
            !crate::layout::ssh_keys_directory(&fixture.vm_directory).exists(),
            "the VM's key pair must not outlive the VM"
        );
        assert!(
            !crate::layout::ssh_known_hosts_path(&fixture.vm_directory).exists(),
            "the learned host keys pin a guest that can no longer be reached"
        );
        assert!(
            crate::layout::cloud_init_status_log_path(&fixture.vm_directory).exists(),
            "the readiness transcript says what the VM did, not how to log into it"
        );
        assert!(
            fixture
                .vm_directory
                .join("disks")
                .join("system.vhdx")
                .exists(),
            "removing the SSH identity must not touch the kept disks"
        );
    }

    /// The secret authenticates an agent as this VM. The VM is gone, so what
    /// the secret would authenticate is gone with it -- and unlike the disks,
    /// it is of no use to anyone who kept them.
    #[test]
    fn keeping_the_disks_still_takes_the_agent_secret_with_the_vm() {
        let fixture = fixture("keep-disks-agent-secret");

        pipeline(&fixture, false)
            .delete(&fixture.store, "dev", &fixture.vm_directory, false)
            .expect("deletion should succeed");

        assert!(!crate::layout::agent_secret_path(&fixture.vm_directory).exists());
        assert!(
            fixture
                .vm_directory
                .join("disks")
                .join("system.vhdx")
                .exists(),
            "removing the secret must not touch the kept disks"
        );
    }

    /// A VM created from local media never got a secret; having nothing to
    /// remove is not a failed deletion.
    #[test]
    fn a_vm_that_never_got_an_agent_secret_leaves_nothing_to_remove() {
        let fixture = fixture("no-agent-secret");
        fs::remove_file(crate::layout::agent_secret_path(&fixture.vm_directory))
            .expect("the secret should be removable");

        pipeline(&fixture, false)
            .delete(&fixture.store, "dev", &fixture.vm_directory, false)
            .expect("deletion should succeed");
    }

    #[test]
    fn a_full_deletion_takes_the_ssh_identity_with_everything_else() {
        let fixture = fixture("delete-disks-ssh");

        pipeline(&fixture, false)
            .delete(&fixture.store, "dev", &fixture.vm_directory, true)
            .expect("deletion should succeed");

        assert!(!crate::layout::ssh_keys_directory(&fixture.vm_directory).exists());
        assert!(!crate::layout::ssh_known_hosts_path(&fixture.vm_directory).exists());
        assert!(!fixture.vm_directory.exists());
    }

    #[test]
    fn a_vm_that_never_got_an_ssh_identity_leaves_nothing_to_remove() {
        // A VM created with SSH disabled has no key pair and never learned a
        // host key; that is a VM with nothing to clean up, not a failure.
        let fixture = fixture("no-ssh-identity");
        fs::remove_dir_all(crate::layout::ssh_keys_directory(&fixture.vm_directory))
            .expect("the keys directory should be removable");
        fs::remove_file(crate::layout::ssh_known_hosts_path(&fixture.vm_directory))
            .expect("the known hosts file should be removable");

        pipeline(&fixture, false)
            .delete(&fixture.store, "dev", &fixture.vm_directory, false)
            .expect("an absent SSH identity is not a failure");

        assert!(
            fixture
                .store
                .find_by_vm_name("dev")
                .expect("the store should be readable")
                .is_none()
        );
    }

    #[test]
    fn rejects_an_unknown_vm_without_touching_hcs_or_the_filesystem() {
        let fixture = fixture("unknown");

        let error = pipeline(&fixture, false)
            .delete(&fixture.store, "missing-vm", &fixture.vm_directory, true)
            .expect_err("an unknown VM must not be deleted");

        assert!(error.to_string().contains("missing-vm"));
        assert!(fixture.teardowns.lock().unwrap().is_empty());
        assert!(fixture.vm_directory.exists());
    }

    #[test]
    fn a_failed_teardown_keeps_the_mapping_so_the_deletion_can_be_retried() {
        let fixture = fixture("teardown-failure");

        let error = pipeline(&fixture, true)
            .delete(&fixture.store, "dev", &fixture.vm_directory, true)
            .expect_err("a failed teardown must be reported");

        assert!(error.to_string().contains("injected teardown failure"));
        assert!(
            fixture
                .store
                .find_by_vm_name("dev")
                .expect("the store should be readable")
                .is_some(),
            "a VM whose resources are still present must stay known to VMLord"
        );
    }

    #[test]
    fn removes_the_files_even_when_the_teardown_failed() {
        let fixture = fixture("keeps-going");

        pipeline(&fixture, true)
            .delete(&fixture.store, "dev", &fixture.vm_directory, true)
            .expect_err("a failed teardown must be reported");

        assert!(
            !fixture.vm_directory.exists(),
            "a failed teardown must not stop the remaining cleanup"
        );
    }

    #[test]
    fn an_already_removed_configuration_does_not_fail_a_kept_disks_deletion() {
        let fixture = fixture("no-config");
        fs::remove_file(fixture.vm_directory.join("config.json"))
            .expect("the configuration should be removable");

        pipeline(&fixture, false)
            .delete(&fixture.store, "dev", &fixture.vm_directory, false)
            .expect("an already-removed configuration is not a failure");

        assert!(
            fixture
                .store
                .find_by_vm_name("dev")
                .expect("the store should be readable")
                .is_none()
        );
    }

    #[test]
    fn production_pipeline_is_available_to_the_repository() {
        let _: fn() -> VmDeletionPipeline = VmDeletionPipeline::production;
        let _: fn(
            &VmDeletionPipeline,
            &MetadataStore,
            &str,
            &Path,
            bool,
        ) -> Result<(), RepositoryError> = VmDeletionPipeline::delete;
    }
}
