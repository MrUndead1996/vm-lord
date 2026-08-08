//! Transactional creation of an HCS-backed virtual machine.

use std::{fs, path::Path, time::Duration};

use uuid::Uuid;
use vmlord_core::{RepositoryError, VmCreateRequest};

use crate::{
    HcsClient,
    cleanup::{self, SystemTeardown},
    hcs_config::HcsVmConfigBuilder,
    layout,
    metadata::{MetadataStore, VmComputeSystemMapping},
    vhd::create_dynamic_vhdx,
};

const CREATE_TIMEOUT: Duration = Duration::from_secs(30);
const BYTES_PER_GIB: u64 = 1024 * 1024 * 1024;

type VhdCreator = Box<dyn Fn(&Path, u64) -> Result<(), RepositoryError>>;
type AccessGranter = Box<dyn Fn(&str, &Path) -> Result<(), RepositoryError>>;
type SystemCreator = Box<dyn Fn(&str, &str) -> Result<(), RepositoryError>>;

/// Orchestrates the multi-step, transactional creation of an HCS-backed VM.
///
/// Every step after the VM directory is created is wrapped so that any
/// failure rolls back: the compute system (if created) is torn down and the
/// VM directory is removed.
pub struct VmCreationPipeline {
    vhd_creator: VhdCreator,
    access_granter: AccessGranter,
    system_creator: SystemCreator,
    system_teardown: SystemTeardown,
}

impl VmCreationPipeline {
    /// Creates a pipeline backed by the real VHDX and HCS APIs.
    #[must_use]
    pub fn production() -> Self {
        Self {
            vhd_creator: Box::new(create_dynamic_vhdx),
            access_granter: Box::new(grant_vm_access),
            system_creator: Box::new(create_hcs_system),
            system_teardown: Box::new(cleanup::teardown_compute_system),
        }
    }

    #[cfg(test)]
    fn for_test(
        vhd_creator: impl Fn(&Path, u64) -> Result<(), RepositoryError> + 'static,
        access_granter: impl Fn(&str, &Path) -> Result<(), RepositoryError> + 'static,
        system_creator: impl Fn(&str, &str) -> Result<(), RepositoryError> + 'static,
        system_teardown: impl Fn(&str) -> Result<(), RepositoryError> + 'static,
    ) -> Self {
        Self {
            vhd_creator: Box::new(vhd_creator),
            access_granter: Box::new(access_granter),
            system_creator: Box::new(system_creator),
            system_teardown: Box::new(system_teardown),
        }
    }

    /// Creates a new VM under `vm_directory`, registering it in `store` once
    /// every step has succeeded. Rolls back all partial state on failure.
    pub fn create(
        &self,
        store: &MetadataStore,
        request: &VmCreateRequest,
        vm_directory: &Path,
    ) -> Result<VmComputeSystemMapping, RepositoryError> {
        request.validate()?;

        if store.find_by_vm_name(&request.name)?.is_some() {
            return Err(RepositoryError::new(format!(
                "VM \"{}\" already exists",
                request.name
            )));
        }
        if vm_directory.exists() {
            return Err(RepositoryError::new(format!(
                "VM directory already exists: {}",
                vm_directory.display()
            )));
        }

        let vm_id = Uuid::new_v4();
        let hcs_compute_system_id = format!("vmlord-{}", vm_id.as_simple());
        let system_disk_path = layout::system_disk_path(vm_directory);
        // Rejects an unsupported request (name, GPU/network mode, ...) before
        // any filesystem or HCS side effect.
        let configuration = HcsVmConfigBuilder::build(request, &system_disk_path)?;

        log::info!(
            "creating VM \"{}\" ({vm_id}) as HCS compute system \"{hcs_compute_system_id}\"",
            request.name
        );

        let disk_directory = system_disk_path
            .parent()
            .expect("system disk path always has a parent under vm_directory");
        fs::create_dir_all(disk_directory).map_err(|error| {
            let error = RepositoryError::new(format!(
                "failed to create VM directory {}: {error}",
                vm_directory.display()
            ));
            log::error!("{error}");
            error
        })?;

        let mapping = VmComputeSystemMapping {
            vm_id,
            vm_name: request.name.clone(),
            hcs_compute_system_id: hcs_compute_system_id.clone(),
            disk_gb: request.disk_gb,
            // No endpoint yet: it is created on the first start, so that a VM
            // that is never started never takes an address.
            endpoint_id: None,
            network_mode: request.network_mode,
        };

        let mut system_created = false;
        let result = (|| {
            (self.vhd_creator)(
                &system_disk_path,
                u64::from(request.disk_gb) * BYTES_PER_GIB,
            )?;

            if !Path::new(&request.image_path).is_file() {
                return Err(RepositoryError::new(format!(
                    "VM image no longer exists: {}",
                    request.image_path
                )));
            }

            fs::write(layout::configuration_path(vm_directory), &configuration).map_err(|error| {
                RepositoryError::new(format!("failed to write HCS configuration: {error}"))
            })?;

            // Hyper-V opens VM-owned files under the VM's own security
            // principal, not the creating user's token: without this, start
            // fails with access denied even though both files exist and are
            // readable by this (elevated) process.
            (self.access_granter)(&hcs_compute_system_id, &system_disk_path)?;
            (self.access_granter)(&hcs_compute_system_id, Path::new(&request.image_path))?;

            (self.system_creator)(&hcs_compute_system_id, &configuration)?;
            system_created = true;

            store.insert(mapping.clone())?;
            Ok(())
        })();

        match result {
            Ok(()) => {
                log::info!("created VM \"{}\" ({vm_id})", request.name);
                Ok(mapping)
            }
            Err(error) => Err(self.rollback(vm_directory, &mapping, system_created, error)),
        }
    }

    fn rollback(
        &self,
        vm_directory: &Path,
        mapping: &VmComputeSystemMapping,
        system_created: bool,
        error: RepositoryError,
    ) -> RepositoryError {
        let mut failures = vec![error.to_string()];

        if system_created
            && let Err(teardown_error) = (self.system_teardown)(&mapping.hcs_compute_system_id)
        {
            failures.push(format!("rollback teardown also failed: {teardown_error}"));
        }
        if let Err(remove_error) = cleanup::remove_vm_directory(vm_directory) {
            failures.push(format!("rollback also failed: {remove_error}"));
        }

        cleanup::combine_failures(
            &format!("creation of VM \"{}\" failed", mapping.vm_name),
            failures,
        )
    }
}

impl Default for VmCreationPipeline {
    fn default() -> Self {
        Self::production()
    }
}

fn grant_vm_access(id: &str, path: &Path) -> Result<(), RepositoryError> {
    HcsClient::new().grant_vm_access(id, path)
}

fn create_hcs_system(id: &str, configuration: &str) -> Result<(), RepositoryError> {
    let (system, operation) = HcsClient::new().create_system(id, configuration)?;
    let result = operation.wait_for_completion(CREATE_TIMEOUT);
    // Persisting past this handle close relies on the configuration setting
    // `ShouldTerminateOnLastHandleClosed: false` (see `hcs_config`); with
    // `true`, HCS discards even a never-started system as soon as the
    // creating handle closes.
    drop(system);
    match result {
        Ok(_document) => Ok(()),
        Err(error) => {
            // The outcome is ambiguous: `HcsCreateComputeSystem` may already
            // have succeeded, so the system could persist in HCS without a
            // metadata entry. It belongs to this operation; tear it down
            // best-effort before reporting the failure.
            let mut message = error.to_string();
            if let Err(teardown_error) = cleanup::teardown_compute_system(id) {
                message.push_str(&format!(
                    "; cleanup of the ambiguously-created compute system also failed: {teardown_error}"
                ));
            }
            Err(RepositoryError::new(message))
        }
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
        },
    };

    use vmlord_core::{GpuMode, NetworkMode, VmCreateRequest};

    use super::VmCreationPipeline;
    use crate::MetadataStore;

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn path(&self) -> &PathBuf {
            &self.0
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temp_root(label: &str) -> TempRoot {
        static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "vmlord-create-test-{label}-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("test root should be created");
        TempRoot(path)
    }

    #[derive(Clone, Default)]
    struct Calls {
        vhd: Arc<Mutex<Vec<(PathBuf, u64)>>>,
        grant: Arc<Mutex<Vec<(String, PathBuf)>>>,
        create: Arc<Mutex<Vec<(String, String)>>>,
        teardown: Arc<Mutex<Vec<String>>>,
    }

    struct Fixture {
        _root: TempRoot,
        store: MetadataStore,
        vm_directory: PathBuf,
        request: VmCreateRequest,
        image_path: PathBuf,
        calls: Calls,
    }

    fn fixture(label: &str) -> Fixture {
        let root = temp_root(label);
        let image_path = root.path().join("installer.iso");
        fs::write(&image_path, b"iso").expect("test image should be written");
        let request = VmCreateRequest {
            name: "test-vm".into(),
            image_path: image_path.to_string_lossy().into_owned(),
            ram_mb: 512,
            disk_gb: 1,
            cpu_cores: 1,
            gpu_mode: GpuMode::None,
            network_mode: NetworkMode::None,
            username: "admin".into(),
            password: "secret".into(),
            ssh_enabled: false,
            ssh_deploy_key: false,
        };
        Fixture {
            store: MetadataStore::new(root.path().join("vm-mapping.json")),
            vm_directory: root.path().join("vm"),
            request,
            image_path,
            calls: Calls::default(),
            _root: root,
        }
    }

    fn pipeline(calls: &Calls, fail_vhd: bool, fail_create: bool) -> VmCreationPipeline {
        VmCreationPipeline::for_test(
            {
                let calls = calls.clone();
                move |path, size| {
                    calls.vhd.lock().unwrap().push((path.to_path_buf(), size));
                    if fail_vhd {
                        return Err(vmlord_core::RepositoryError::new("injected disk failure"));
                    }
                    fs::write(path, b"vhdx")
                        .map_err(|error| vmlord_core::RepositoryError::new(format!("vhd: {error}")))
                }
            },
            {
                let calls = calls.clone();
                move |id, path| {
                    calls
                        .grant
                        .lock()
                        .unwrap()
                        .push((id.to_owned(), path.to_path_buf()));
                    Ok(())
                }
            },
            {
                let calls = calls.clone();
                move |id, config| {
                    calls
                        .create
                        .lock()
                        .unwrap()
                        .push((id.to_owned(), config.to_owned()));
                    if fail_create {
                        // Simulate the production contract: an ambiguous
                        // create failure tears the system down inside the
                        // creator itself before returning the error.
                        calls.teardown.lock().unwrap().push(id.to_owned());
                        return Err(vmlord_core::RepositoryError::new(
                            "create compute system timed out after 30s",
                        ));
                    }
                    Ok(())
                }
            },
            {
                let calls = calls.clone();
                move |id| {
                    calls.teardown.lock().unwrap().push(id.to_owned());
                    Ok(())
                }
            },
        )
    }

    #[test]
    fn creates_and_registers_a_vm_through_all_stages() {
        let fixture = fixture("happy");
        let calls = fixture.calls.clone();
        let pipeline = pipeline(&calls, false, false);

        let mapping = pipeline
            .create(&fixture.store, &fixture.request, &fixture.vm_directory)
            .expect("creation should succeed");

        assert_eq!(mapping.vm_name, "test-vm");
        assert_eq!(
            mapping.hcs_compute_system_id,
            format!("vmlord-{}", mapping.vm_id.as_simple())
        );
        assert!(fixture.vm_directory.join("config.json").is_file());
        assert!(
            fixture
                .vm_directory
                .join("disks")
                .join("system.vhdx")
                .is_file()
        );
        assert_eq!(fixture.store.list().unwrap(), vec![mapping.clone()]);

        let create_calls = calls.create.lock().unwrap();
        assert_eq!(create_calls.len(), 1);
        assert_eq!(create_calls[0].0, mapping.hcs_compute_system_id);
        let config: serde_json::Value =
            serde_json::from_str(&create_calls[0].1).expect("HCS config should be valid JSON");
        assert_eq!(
            config.pointer("/VirtualMachine/Devices/Scsi/Primary/Attachments/0/Path"),
            Some(&serde_json::json!(
                fixture.vm_directory.join("disks").join("system.vhdx")
            ))
        );
        assert!(!create_calls[0].1.contains("secret"));
        assert!(calls.teardown.lock().unwrap().is_empty());

        let grant_calls = calls.grant.lock().unwrap();
        assert_eq!(
            grant_calls.as_slice(),
            &[
                (
                    mapping.hcs_compute_system_id.clone(),
                    fixture.vm_directory.join("disks").join("system.vhdx")
                ),
                (
                    mapping.hcs_compute_system_id.clone(),
                    fixture.image_path.clone()
                ),
            ],
            "the VM must be granted access to its disk and installer image before HCS create"
        );
    }

    #[test]
    fn rejects_a_duplicate_vm_name_before_any_side_effect() {
        let fixture = fixture("duplicate");
        let calls = fixture.calls.clone();
        let pipeline = pipeline(&calls, false, false);
        pipeline
            .create(&fixture.store, &fixture.request, &fixture.vm_directory)
            .expect("the first creation should succeed");

        let other_directory = fixture.vm_directory.with_file_name("vm-2");
        let error = pipeline
            .create(&fixture.store, &fixture.request, &other_directory)
            .expect_err("a duplicate VM name must be rejected");

        assert!(error.to_string().contains("test-vm"));
        assert_eq!(calls.vhd.lock().unwrap().len(), 1);
        assert_eq!(calls.create.lock().unwrap().len(), 1);
        assert_eq!(fixture.store.list().unwrap().len(), 1);
    }

    #[test]
    fn rejects_when_the_vm_directory_already_exists() {
        let fixture = fixture("existing-dir");
        let calls = fixture.calls.clone();
        fs::create_dir_all(&fixture.vm_directory).unwrap();
        let pipeline = pipeline(&calls, false, false);

        let error = pipeline
            .create(&fixture.store, &fixture.request, &fixture.vm_directory)
            .expect_err("an existing VM directory must be rejected");

        assert!(error.to_string().contains("already exists"));
        assert!(calls.vhd.lock().unwrap().is_empty());
    }

    #[test]
    fn a_disk_creation_failure_removes_the_vm_directory_without_hcs() {
        let fixture = fixture("disk-failure");
        let calls = fixture.calls.clone();
        let pipeline = pipeline(&calls, true, false);

        let error = pipeline
            .create(&fixture.store, &fixture.request, &fixture.vm_directory)
            .expect_err("disk failure must abort creation");

        assert!(error.to_string().contains("injected disk failure"));
        assert!(calls.create.lock().unwrap().is_empty());
        assert!(calls.teardown.lock().unwrap().is_empty());
        assert!(!fixture.vm_directory.exists());
        assert!(fixture.store.list().unwrap().is_empty());
    }

    #[test]
    fn a_missing_image_at_create_time_aborts_before_hcs() {
        let fixture = fixture("image-gone");
        let calls = fixture.calls.clone();
        let pipeline = pipeline(&calls, false, false);
        fs::remove_file(&fixture.image_path).unwrap();

        let error = pipeline
            .create(&fixture.store, &fixture.request, &fixture.vm_directory)
            .expect_err("a vanished image must abort creation");

        assert!(error.to_string().contains("image"));
        assert!(calls.create.lock().unwrap().is_empty());
        assert!(!fixture.vm_directory.exists());
    }

    #[test]
    fn an_hcs_create_failure_tears_down_the_ambiguous_system() {
        let fixture = fixture("create-failure");
        let calls = fixture.calls.clone();
        let pipeline = pipeline(&calls, false, true);

        let error = pipeline
            .create(&fixture.store, &fixture.request, &fixture.vm_directory)
            .expect_err("an HCS create failure must abort creation");

        assert!(error.to_string().contains("timed out"));
        let create_calls = calls.create.lock().unwrap();
        assert_eq!(create_calls.len(), 1);
        // The ambiguous system is torn down by the creator itself (recorded
        // by the injected closure); the pipeline's own rollback must not
        // tear it down a second time because `system_created` stays false.
        assert_eq!(
            calls.teardown.lock().unwrap().as_slice(),
            &[create_calls[0].0.clone()]
        );
        drop(create_calls);
        assert!(!fixture.vm_directory.exists());
        assert!(fixture.store.list().unwrap().is_empty());
    }

    #[test]
    fn a_metadata_registration_failure_tears_down_the_created_system() {
        let fixture = fixture("metadata-failure");
        let calls = fixture.calls.clone();
        // Pointing the mapping file at a path whose parent cannot be created
        // (it collides with an existing file) makes the final `store.insert`
        // step fail with a real filesystem error, without adding a metadata
        // injection seam just for this test.
        let blocked_parent = fixture.vm_directory.with_file_name("blocked");
        fs::write(&blocked_parent, b"file").unwrap();
        let blocked_store = MetadataStore::new(blocked_parent.join("vm-mapping.json"));
        let pipeline = pipeline(&calls, false, false);

        let error = pipeline
            .create(&blocked_store, &fixture.request, &fixture.vm_directory)
            .expect_err("a metadata registration failure must abort creation");

        assert!(error.to_string().contains("creation of VM"));
        let create_calls = calls.create.lock().unwrap();
        let teardown_calls = calls.teardown.lock().unwrap();
        assert_eq!(create_calls.len(), 1);
        assert_eq!(teardown_calls.as_slice(), &[create_calls[0].0.clone()]);
        drop(teardown_calls);
        drop(create_calls);
        assert!(!fixture.vm_directory.exists());
    }

    #[test]
    fn rollback_never_touches_the_source_image() {
        let fixture = fixture("image-safe");
        let calls = fixture.calls.clone();
        let pipeline = pipeline(&calls, true, false);

        let _ = pipeline.create(&fixture.store, &fixture.request, &fixture.vm_directory);

        assert_eq!(fs::read(&fixture.image_path).unwrap(), b"iso");
    }
}
