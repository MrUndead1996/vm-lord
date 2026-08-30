//! The VMLord compute system a copied AppSandbox guest first boots in.
//!
//! This is the point where an import stops being a file copy and becomes a VM:
//! the copied VHDX gets a compute system of VMLord's own making, registered in
//! VMLord's own metadata, so that everything after it -- starting the guest,
//! reaching it over SSH, converting it -- is ordinary VMLord work on an
//! ordinary VMLord VM.
//!
//! What it deliberately does not do is finish the import. The guest inside the
//! copy is still an AppSandbox one: it answers as the AppSandbox user, on the
//! AppSandbox port, with no VMLord agent, no display and no GPU. So the VM is
//! registered as exactly that -- a NAT machine with nothing claimed of it --
//! and what the import asked for stays in its journal until the conversion has
//! put it inside the guest.

use std::{fs, path::Path};

use uuid::Uuid;
use vmlord_core::{RepositoryError, SshPort};

use crate::{
    create::{self, AccessGranter, StateFileCreator, SystemCreator},
    hcs_config::{HcsVmConfigBuilder, ImportBootstrap, StateFilePaths, VmTopology},
    layout,
    metadata::{CompletedImport, MetadataStore, VmComputeSystemMapping},
};

use super::journal::{BootstrapSshFacts, ImportResources};

/// The compute system a copied guest's first boot runs in, as the import needs
/// to know it.
///
/// The mapping comes back with it rather than being looked up again: the
/// caller has just registered this VM and is about to start it, and a second
/// read of the store could only disagree with what was written.
#[derive(Clone, Debug)]
pub(crate) struct BootstrapVm {
    pub(crate) vm_id: Uuid,
    pub(crate) hcs_compute_system_id: String,
    pub(crate) mapping: VmComputeSystemMapping,
}

/// What one bootstrap creation is built from.
///
/// Every field is something the import already knows: the name the user chose,
/// the directory the copy was written into, the resources read out of the
/// source configuration and the SSH facts the journal kept for the first
/// session. Nothing here names the source VM.
pub(crate) struct BootstrapRequest<'a> {
    pub(crate) vm_name: &'a str,
    /// The VMLord VM directory the copy was written into -- the journal's
    /// destination, which already holds `disks/system.vhdx`.
    pub(crate) vm_directory: &'a Path,
    pub(crate) resources: &'a ImportResources,
    pub(crate) ssh: &'a BootstrapSshFacts,
}

/// Generates the VM's own SSH key pair and returns its public half.
type KeyGenerator = Box<dyn Fn(&Path, &str) -> Result<String, RepositoryError> + Send + Sync>;

/// Creates the compute system around a copied disk.
///
/// Seamed the way [`crate::create::VmCreationPipeline`] is, and through the
/// same helpers: HCS state files, the VM's access grants, the compute system
/// itself and the per-VM key pair are all one implementation shared with
/// creation, so an imported VM is protected exactly as a created one is.
pub(crate) struct ImportBootstrapPipeline {
    access_granter: AccessGranter,
    state_file_creator: StateFileCreator,
    system_creator: SystemCreator,
    key_generator: KeyGenerator,
}

impl ImportBootstrapPipeline {
    /// A pipeline backed by the real HCS and key APIs.
    pub(crate) fn production() -> Self {
        Self {
            access_granter: Box::new(create::grant_vm_access),
            state_file_creator: Box::new(create::create_state_files),
            system_creator: Box::new(create::create_hcs_system),
            key_generator: Box::new(create::generate_vm_key_pair),
        }
    }

    #[cfg(test)]
    fn for_test(
        access_granter: impl Fn(&str, &Path) -> Result<(), RepositoryError> + Send + Sync + 'static,
        state_file_creator: impl Fn(&Path, &Path) -> Result<(), RepositoryError> + Send + Sync + 'static,
        system_creator: impl Fn(&str, &str) -> Result<(), RepositoryError> + Send + Sync + 'static,
        key_generator: impl Fn(&Path, &str) -> Result<String, RepositoryError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            access_granter: Box::new(access_granter),
            state_file_creator: Box::new(state_file_creator),
            system_creator: Box::new(system_creator),
            key_generator: Box::new(key_generator),
        }
    }

    /// Builds the compute system for the copied disk under
    /// `request.vm_directory` and registers it in `store`.
    ///
    /// Nothing is rolled back when a step fails, and that is the point: the
    /// copied disk is the expensive half of an import and the only thing that
    /// cannot be made again cheaply, so a failure here leaves the destination
    /// exactly as it was for the import journal to resume from. What a failed
    /// compute-system creation might have left behind inside HCS is torn down
    /// by [`crate::create::create_hcs_system`] itself.
    pub(crate) fn create(
        &self,
        store: &MetadataStore,
        request: &BootstrapRequest<'_>,
    ) -> Result<BootstrapVm, RepositoryError> {
        let ssh_port = SshPort::new(request.ssh.port)?;
        let system_disk_path = layout::system_disk_path(request.vm_directory);
        if !system_disk_path.is_file() {
            let error = RepositoryError::new(format!(
                "the copied system disk is missing at {}",
                system_disk_path.display()
            ));
            tracing::error!("{error}");
            return Err(error);
        }
        if store.find_by_vm_name(request.vm_name)?.is_some() {
            return Err(RepositoryError::new(format!(
                "VM \"{}\" already exists",
                request.vm_name
            )));
        }

        let vm_id = Uuid::new_v4();
        let hcs_compute_system_id = format!("vmlord-{}", vm_id.as_simple());
        let guest_state_path = layout::guest_state_path(request.vm_directory);
        let runtime_state_path = layout::runtime_state_path(request.vm_directory);
        let configuration = HcsVmConfigBuilder::build_import_bootstrap(&ImportBootstrap {
            system_disk: &system_disk_path,
            state: StateFilePaths {
                guest_state: &guest_state_path,
                runtime_state: &runtime_state_path,
            },
            topology: VmTopology {
                ram_mb: request.resources.ram_mb,
                cpu_cores: request.resources.cpu_cores,
            },
            vm_id,
        })?;

        tracing::info!(
            "bootstrapping imported VM \"{}\" ({vm_id}) as HCS compute system \
             \"{hcs_compute_system_id}\"",
            request.vm_name
        );

        // Written before the guest is ever reached: the conversion deploys the
        // public half over the bootstrap session, and a VM whose key appeared
        // only after a successful login would have no identity to fall back on
        // when that login is what failed.
        (self.key_generator)(request.vm_directory, request.vm_name)?;

        fs::write(
            layout::configuration_path(request.vm_directory),
            &configuration,
        )
        .map_err(|error| {
            let error = RepositoryError::new(format!(
                "failed to write the imported VM's HCS configuration: {error}"
            ));
            tracing::error!("{error}");
            error
        })?;
        (self.state_file_creator)(&guest_state_path, &runtime_state_path)?;

        // The worker opens all three under the VM's own principal, and the
        // copy is no more readable to it than a freshly created disk would be.
        (self.access_granter)(&hcs_compute_system_id, &system_disk_path)?;
        (self.access_granter)(&hcs_compute_system_id, &guest_state_path)?;
        (self.access_granter)(&hcs_compute_system_id, &runtime_state_path)?;

        (self.system_creator)(&hcs_compute_system_id, &configuration)?;

        let mapping = VmComputeSystemMapping::from_completed_import(&CompletedImport {
            vm_id,
            vm_name: request.vm_name,
            hcs_compute_system_id: &hcs_compute_system_id,
            disk_gb: request.resources.disk_gb,
            ssh_username: &request.ssh.username,
            ssh_port,
        });
        store.insert(mapping.clone())?;

        Ok(BootstrapVm {
            vm_id,
            hcs_compute_system_id,
            mapping,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
    };

    use serde_json::Value;
    use uuid::Uuid;
    use vmlord_core::{DesktopProfile, GpuMode, NetworkMode, RepositoryError, SshAuthentication};

    use super::{BootstrapRequest, ImportBootstrapPipeline};
    use crate::{
        MetadataStore,
        appsandbox::journal::{BootstrapSshFacts, ImportResources},
        layout,
    };

    /// Every side effect the pipeline has outside the VM's own directory.
    #[derive(Clone, Default)]
    struct Calls {
        grants: Arc<Mutex<Vec<(String, PathBuf)>>>,
        state_files: Arc<Mutex<Vec<(PathBuf, PathBuf)>>>,
        systems: Arc<Mutex<Vec<(String, String)>>>,
        keys: Arc<Mutex<Vec<(PathBuf, String)>>>,
    }

    struct TempRoot(PathBuf);

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temporary_root(label: &str) -> TempRoot {
        let path = std::env::temp_dir().join(format!(
            "vmlord-appsandbox-bootstrap-{label}-{}",
            Uuid::new_v4()
        ));
        fs::create_dir_all(&path).unwrap();
        TempRoot(path)
    }

    /// The destination as the copy stage leaves it: a VM directory holding the
    /// copied system VHDX and nothing else.
    fn copied_destination(root: &Path) -> PathBuf {
        let vm_directory = root.join("imported");
        let disk = layout::system_disk_path(&vm_directory);
        fs::create_dir_all(disk.parent().unwrap()).unwrap();
        fs::write(&disk, b"copied disk").unwrap();
        vm_directory
    }

    fn resources() -> ImportResources {
        ImportResources {
            ram_mb: 4096,
            cpu_cores: 4,
            disk_gb: 80,
            // What the import asked for, which this boot must not claim.
            desktop_profile: DesktopProfile::Gnome,
        }
    }

    fn ssh() -> BootstrapSshFacts {
        BootstrapSshFacts {
            username: "sandbox".to_owned(),
            port: 2222,
        }
    }

    fn pipeline(calls: &Calls) -> ImportBootstrapPipeline {
        let grants = Arc::clone(&calls.grants);
        let state_files = Arc::clone(&calls.state_files);
        let systems = Arc::clone(&calls.systems);
        let keys = Arc::clone(&calls.keys);
        ImportBootstrapPipeline::for_test(
            move |id, path| {
                grants
                    .lock()
                    .unwrap()
                    .push((id.to_owned(), path.to_owned()));
                Ok(())
            },
            move |guest_state, runtime_state| {
                state_files
                    .lock()
                    .unwrap()
                    .push((guest_state.to_owned(), runtime_state.to_owned()));
                fs::write(guest_state, b"vmgs").unwrap();
                fs::write(runtime_state, b"vmrs").unwrap();
                Ok(())
            },
            move |id, configuration| {
                systems
                    .lock()
                    .unwrap()
                    .push((id.to_owned(), configuration.to_owned()));
                Ok(())
            },
            move |vm_directory, vm_name| {
                keys.lock()
                    .unwrap()
                    .push((vm_directory.to_owned(), vm_name.to_owned()));
                Ok("ssh-ed25519 AAAA test".to_owned())
            },
        )
    }

    #[test]
    fn a_bootstrap_registers_a_nat_vm_with_the_copied_guests_ssh_facts() {
        let root = temporary_root("registers");
        let store = MetadataStore::new(root.0.join("metadata.json"));
        let vm_directory = copied_destination(&root.0);

        let bootstrap = pipeline(&Calls::default())
            .create(
                &store,
                &BootstrapRequest {
                    vm_name: "imported",
                    vm_directory: &vm_directory,
                    resources: &resources(),
                    ssh: &ssh(),
                },
            )
            .expect("a copied disk should be bootstrappable");

        let stored = store
            .find_by_vm_name("imported")
            .unwrap()
            .expect("the bootstrap VM must be registered before it is started");
        assert_eq!(stored, bootstrap.mapping);
        assert_eq!(stored.vm_id, bootstrap.vm_id);
        assert_eq!(
            stored.hcs_compute_system_id,
            bootstrap.hcs_compute_system_id
        );
        assert_eq!(stored.network_mode, NetworkMode::Nat);
        assert_eq!(stored.disk_gb, 80);
        let ssh = stored.ssh.expect("the copied guest answers SSH already");
        assert_eq!(ssh.username, "sandbox");
        assert_eq!(ssh.port.get(), 2222);
        assert_eq!(ssh.authentication, SshAuthentication::VmlordKey);
    }

    #[test]
    fn a_bootstrap_claims_no_gpu_or_desktop_the_first_boot_has_none_of() {
        let root = temporary_root("claims-nothing");
        let store = MetadataStore::new(root.0.join("metadata.json"));
        let vm_directory = copied_destination(&root.0);

        let bootstrap = pipeline(&Calls::default())
            .create(
                &store,
                &BootstrapRequest {
                    vm_name: "imported",
                    vm_directory: &vm_directory,
                    resources: &resources(),
                    ssh: &ssh(),
                },
            )
            .unwrap();

        assert_eq!(bootstrap.mapping.gpu_mode, GpuMode::None);
        assert_eq!(
            bootstrap.mapping.desktop_profile,
            DesktopProfile::Headless,
            "the desktop the import asked for stays in its journal"
        );
        assert_eq!(bootstrap.mapping.guest_target, None);
    }

    #[test]
    fn a_bootstrap_builds_a_compute_system_around_the_copied_disk_alone() {
        let root = temporary_root("attachments");
        let store = MetadataStore::new(root.0.join("metadata.json"));
        let vm_directory = copied_destination(&root.0);
        let calls = Calls::default();

        let bootstrap = pipeline(&calls)
            .create(
                &store,
                &BootstrapRequest {
                    vm_name: "imported",
                    vm_directory: &vm_directory,
                    resources: &resources(),
                    ssh: &ssh(),
                },
            )
            .unwrap();

        let systems = calls.systems.lock().unwrap();
        let (id, configuration) = systems.first().expect("a compute system must be created");
        assert_eq!(id, &bootstrap.hcs_compute_system_id);
        assert_eq!(
            configuration,
            &fs::read_to_string(layout::configuration_path(&vm_directory)).unwrap(),
            "a start rebuilds the system from the stored document, so the two must agree"
        );

        let json: Value = serde_json::from_str(configuration).unwrap();
        let attachments = json
            .pointer("/VirtualMachine/Devices/Scsi/Primary/Attachments")
            .and_then(Value::as_object)
            .expect("the VM must have attachments");
        assert_eq!(attachments.len(), 1, "got {attachments:?}");
        assert_eq!(
            attachments["0"].pointer("/Path").unwrap().as_str().unwrap(),
            layout::system_disk_path(&vm_directory).to_str().unwrap()
        );
        assert!(!configuration.contains("Plan9"), "got {configuration}");
    }

    #[test]
    fn a_bootstrap_gives_the_vm_its_files_and_its_own_key_pair() {
        let root = temporary_root("grants");
        let store = MetadataStore::new(root.0.join("metadata.json"));
        let vm_directory = copied_destination(&root.0);
        let calls = Calls::default();

        let bootstrap = pipeline(&calls)
            .create(
                &store,
                &BootstrapRequest {
                    vm_name: "imported",
                    vm_directory: &vm_directory,
                    resources: &resources(),
                    ssh: &ssh(),
                },
            )
            .unwrap();

        let guest_state = layout::guest_state_path(&vm_directory);
        let runtime_state = layout::runtime_state_path(&vm_directory);
        assert_eq!(
            *calls.state_files.lock().unwrap(),
            vec![(guest_state.clone(), runtime_state.clone())],
            "the state files are made by HCS beside the copied disk"
        );
        assert_eq!(
            *calls.grants.lock().unwrap(),
            vec![
                (
                    bootstrap.hcs_compute_system_id.clone(),
                    layout::system_disk_path(&vm_directory)
                ),
                (bootstrap.hcs_compute_system_id.clone(), guest_state),
                (bootstrap.hcs_compute_system_id, runtime_state),
            ],
            "the worker opens all three under the VM's own principal"
        );
        assert_eq!(
            *calls.keys.lock().unwrap(),
            vec![(vm_directory, "imported".to_owned())],
            "the key the conversion deploys exists before the first login is tried"
        );
    }

    #[test]
    fn a_bootstrap_refuses_a_destination_the_copy_never_filled() {
        let root = temporary_root("no-disk");
        let store = MetadataStore::new(root.0.join("metadata.json"));
        let vm_directory = root.0.join("imported");
        fs::create_dir_all(&vm_directory).unwrap();
        let calls = Calls::default();

        let error = pipeline(&calls)
            .create(
                &store,
                &BootstrapRequest {
                    vm_name: "imported",
                    vm_directory: &vm_directory,
                    resources: &resources(),
                    ssh: &ssh(),
                },
            )
            .expect_err("a VM cannot be built around a disk that is not there");

        assert!(error.to_string().contains("copied system disk"), "{error}");
        assert!(calls.systems.lock().unwrap().is_empty());
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn a_bootstrap_refuses_a_name_another_vm_already_has() {
        let root = temporary_root("duplicate");
        let store = MetadataStore::new(root.0.join("metadata.json"));
        let vm_directory = copied_destination(&root.0);
        let calls = Calls::default();
        pipeline(&calls)
            .create(
                &store,
                &BootstrapRequest {
                    vm_name: "imported",
                    vm_directory: &vm_directory,
                    resources: &resources(),
                    ssh: &ssh(),
                },
            )
            .unwrap();

        let error = pipeline(&calls)
            .create(
                &store,
                &BootstrapRequest {
                    vm_name: "imported",
                    vm_directory: &vm_directory,
                    resources: &resources(),
                    ssh: &ssh(),
                },
            )
            .expect_err("two VMs must not answer to one name");

        assert!(error.to_string().contains("already exists"), "{error}");
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[test]
    fn a_bootstrap_refuses_a_port_no_session_can_reach() {
        let root = temporary_root("port");
        let store = MetadataStore::new(root.0.join("metadata.json"));
        let vm_directory = copied_destination(&root.0);
        let calls = Calls::default();

        let error = pipeline(&calls)
            .create(
                &store,
                &BootstrapRequest {
                    vm_name: "imported",
                    vm_directory: &vm_directory,
                    resources: &resources(),
                    ssh: &BootstrapSshFacts {
                        username: "sandbox".to_owned(),
                        port: 0,
                    },
                },
            )
            .expect_err("port 0 means nothing to a client");

        assert!(error.to_string().contains("SSH port"), "{error}");
        assert!(calls.systems.lock().unwrap().is_empty());
    }

    #[test]
    fn a_failed_compute_system_leaves_the_copied_disk_where_it_is() {
        let root = temporary_root("no-rollback");
        let store = MetadataStore::new(root.0.join("metadata.json"));
        let vm_directory = copied_destination(&root.0);
        let failing = ImportBootstrapPipeline::for_test(
            |_, _| Ok(()),
            |guest_state, runtime_state| {
                fs::write(guest_state, b"vmgs").unwrap();
                fs::write(runtime_state, b"vmrs").unwrap();
                Ok(())
            },
            |_, _| Err(RepositoryError::new("HCS refused the compute system")),
            |_, _| Ok("ssh-ed25519 AAAA test".to_owned()),
        );

        let error = failing
            .create(
                &store,
                &BootstrapRequest {
                    vm_name: "imported",
                    vm_directory: &vm_directory,
                    resources: &resources(),
                    ssh: &ssh(),
                },
            )
            .expect_err("the failure must reach the import");

        assert!(error.to_string().contains("HCS refused"), "{error}");
        assert!(
            layout::system_disk_path(&vm_directory).is_file(),
            "the copy an import cannot cheaply make again is what recovery resumes from"
        );
        assert!(
            store.list().unwrap().is_empty(),
            "a VM whose compute system was refused is not a VM"
        );
    }
}
