//! Starting an HCS-backed virtual machine.

use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use vmlord_core::RepositoryError;

use crate::{
    HcsClient, HcsSystem,
    hcs::HCS_ACCESS_ALL,
    layout,
    metadata::{MetadataStore, VmComputeSystemMapping},
};

/// A start operation completes once HCS has handed the VM to its worker
/// process, well before the guest OS has booted; the generous bound only
/// guards against a wedged Host Compute Service.
const START_TIMEOUT: Duration = Duration::from_secs(60);

/// Bounds the re-creation of a compute system HCS no longer knows; it is the
/// same operation `VmCreationPipeline` waits on.
const CREATE_TIMEOUT: Duration = Duration::from_secs(60);

type AccessGranter = Box<dyn Fn(&str, &Path) -> Result<(), RepositoryError>>;
type SystemStarter = Box<dyn Fn(&str, &str) -> Result<(), RepositoryError>>;

/// Starts VMs created by [`crate::VmCreationPipeline`].
pub struct VmStartPipeline {
    access_granter: AccessGranter,
    system_starter: SystemStarter,
}

impl VmStartPipeline {
    /// Creates a pipeline backed by the real HCS API.
    #[must_use]
    pub fn production() -> Self {
        Self {
            access_granter: Box::new(grant_vm_access),
            system_starter: Box::new(start_hcs_system),
        }
    }

    #[cfg(test)]
    fn for_test(
        access_granter: impl Fn(&str, &Path) -> Result<(), RepositoryError> + 'static,
        system_starter: impl Fn(&str, &str) -> Result<(), RepositoryError> + 'static,
    ) -> Self {
        Self {
            access_granter: Box::new(access_granter),
            system_starter: Box::new(system_starter),
        }
    }

    /// Starts the VM named `vm_name`, whose configuration lives under
    /// `vm_directory`.
    ///
    /// Every file the configuration attaches is re-granted to the VM's
    /// security principal first: Hyper-V opens those files as the VM itself,
    /// so a start without the grant fails with `ERROR_ACCESS_DENIED` even
    /// when the calling (elevated) process can read them.
    ///
    /// The stored `config.json` is also what a VM whose compute system HCS no
    /// longer knows is rebuilt from, so a start after a stop needs no other
    /// state than what creation persisted.
    pub fn start(
        &self,
        store: &MetadataStore,
        vm_name: &str,
        vm_directory: &Path,
    ) -> Result<(), RepositoryError> {
        let mapping = store.find_by_vm_name(vm_name)?.ok_or_else(|| {
            let error = RepositoryError::new(format!("no HCS mapping found for VM \"{vm_name}\""));
            log::error!("{error}");
            error
        })?;

        log::info!(
            "starting VM \"{}\" ({}) as HCS compute system \"{}\"",
            mapping.vm_name,
            mapping.vm_id,
            mapping.hcs_compute_system_id
        );

        let configuration = self.read_configuration(&mapping, vm_directory)?;
        self.grant_access_to_attachments(&mapping, &configuration)?;
        (self.system_starter)(&mapping.hcs_compute_system_id, &configuration).inspect_err(
            |error| {
                log::error!("failed to start VM \"{}\": {error}", mapping.vm_name);
            },
        )?;

        log::info!("started VM \"{}\" ({})", mapping.vm_name, mapping.vm_id);
        Ok(())
    }

    fn read_configuration(
        &self,
        mapping: &VmComputeSystemMapping,
        vm_directory: &Path,
    ) -> Result<String, RepositoryError> {
        let configuration_path = layout::configuration_path(vm_directory);
        fs::read_to_string(&configuration_path).map_err(|error| {
            let error = RepositoryError::new(format!(
                "failed to read the HCS configuration of VM \"{}\" from {}: {error}",
                mapping.vm_name,
                configuration_path.display()
            ));
            log::error!("{error}");
            error
        })
    }

    fn grant_access_to_attachments(
        &self,
        mapping: &VmComputeSystemMapping,
        document: &str,
    ) -> Result<(), RepositoryError> {
        let paths = attachment_paths(document)?;
        if paths.is_empty() {
            log::warn!(
                "the HCS configuration of VM \"{}\" attaches no files; \
                 starting without granting any VM access",
                mapping.vm_name
            );
        }
        for path in &paths {
            (self.access_granter)(&mapping.hcs_compute_system_id, path)?;
        }

        Ok(())
    }
}

impl Default for VmStartPipeline {
    fn default() -> Self {
        Self::production()
    }
}

/// Collects the path of every file attached by an HCS configuration document.
///
/// Attachment entries without a `Path` are skipped rather than treated as a
/// parse failure: HCS attachment kinds that carry no host file are valid, and
/// only the ones that do need an access grant.
fn attachment_paths(document: &str) -> Result<Vec<PathBuf>, RepositoryError> {
    let configuration: serde_json::Value = serde_json::from_str(document).map_err(|error| {
        let error = RepositoryError::new(format!(
            "the stored HCS configuration is not valid JSON: {error}"
        ));
        log::error!("{error}");
        error
    })?;

    let Some(attachments) = configuration
        .pointer("/VirtualMachine/Devices/Scsi/Primary/Attachments")
        .and_then(serde_json::Value::as_object)
    else {
        return Ok(Vec::new());
    };

    Ok(attachments
        .values()
        .filter_map(|attachment| {
            attachment
                .get("Path")
                .and_then(serde_json::Value::as_str)
                .map(PathBuf::from)
        })
        .collect())
}

fn grant_vm_access(id: &str, path: &Path) -> Result<(), RepositoryError> {
    HcsClient::new().grant_vm_access(id, path)
}

/// Starts the compute system `id`, re-creating it from `configuration` first
/// if HCS no longer knows it.
///
/// HCS destroys a compute system when it exits, so every VM that has been
/// stopped -- by its guest or by a forced stop -- has to be rebuilt before it
/// can run again. Re-creating from the stored configuration keeps the VM's id,
/// disks and metadata mapping unchanged, so a stop stays a stop rather than
/// becoming an implicit delete.
fn start_hcs_system(id: &str, configuration: &str) -> Result<(), RepositoryError> {
    // The system handle must outlive the start operation it issued.
    let system = match HcsSystem::open_if_present(id, HCS_ACCESS_ALL)? {
        Some(system) => system,
        None => {
            log::info!(
                "HCS no longer knows compute system \"{id}\"; \
                 re-creating it from the stored configuration before starting it"
            );
            let (system, creation) = HcsClient::new().create_system(id, configuration)?;
            creation.wait_for_completion(CREATE_TIMEOUT)?;
            system
        }
    };
    system
        .start()?
        .wait_for_completion(START_TIMEOUT)
        .map(|_document| ())
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
    use vmlord_core::RepositoryError;

    use super::{VmStartPipeline, attachment_paths};
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
            "vmlord-start-test-{label}-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("test root should be created");
        TempRoot(path)
    }

    fn configuration(disk: &str, iso: &str) -> String {
        serde_json::json!({
            "VirtualMachine": {
                "Devices": {
                    "Scsi": { "Primary": { "Attachments": {
                        "0": { "Type": "VirtualDisk", "Path": disk },
                        "1": { "Type": "Iso", "Path": iso }
                    }}}
                }
            }
        })
        .to_string()
    }

    #[derive(Clone, Default)]
    struct Calls {
        grant: Arc<Mutex<Vec<(String, PathBuf)>>>,
        start: Arc<Mutex<Vec<(String, String)>>>,
    }

    struct Fixture {
        _root: TempRoot,
        store: MetadataStore,
        vm_directory: PathBuf,
        mapping: VmComputeSystemMapping,
        calls: Calls,
    }

    fn fixture(label: &str) -> Fixture {
        let root = temp_root(label);
        let vm_directory = root.0.join("vm");
        fs::create_dir_all(&vm_directory).expect("VM directory should be created");
        fs::write(
            vm_directory.join("config.json"),
            configuration(
                "C:\\vms\\dev\\disks\\system.vhdx",
                "C:\\images\\installer.iso",
            ),
        )
        .expect("HCS configuration should be written");

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
            vm_directory,
            mapping,
            calls: Calls::default(),
            _root: root,
        }
    }

    fn pipeline(calls: &Calls, fail_start: bool) -> VmStartPipeline {
        VmStartPipeline::for_test(
            {
                let calls = calls.clone();
                move |id: &str, path: &Path| {
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
                move |id: &str, configuration: &str| {
                    calls
                        .start
                        .lock()
                        .unwrap()
                        .push((id.to_owned(), configuration.to_owned()));
                    if fail_start {
                        return Err(RepositoryError::new("injected start failure"));
                    }
                    Ok(())
                }
            },
        )
    }

    #[test]
    fn grants_access_to_every_attachment_before_starting() {
        let fixture = fixture("happy");
        let calls = fixture.calls.clone();

        pipeline(&calls, false)
            .start(&fixture.store, "dev", &fixture.vm_directory)
            .expect("start should succeed");

        let mut granted = calls.grant.lock().unwrap().clone();
        granted.sort();
        assert_eq!(
            granted,
            vec![
                (
                    fixture.mapping.hcs_compute_system_id.clone(),
                    PathBuf::from("C:\\images\\installer.iso")
                ),
                (
                    fixture.mapping.hcs_compute_system_id.clone(),
                    PathBuf::from("C:\\vms\\dev\\disks\\system.vhdx")
                ),
            ]
        );
        let started = calls.start.lock().unwrap().clone();
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].0, fixture.mapping.hcs_compute_system_id);
    }

    #[test]
    fn hands_the_stored_configuration_to_the_starter() {
        // The starter re-creates a compute system HCS no longer knows, so it
        // needs the very document creation persisted.
        let fixture = fixture("configuration");
        let calls = fixture.calls.clone();

        pipeline(&calls, false)
            .start(&fixture.store, "dev", &fixture.vm_directory)
            .expect("start should succeed");

        let started = calls.start.lock().unwrap().clone();
        assert_eq!(
            started[0].1,
            fs::read_to_string(fixture.vm_directory.join("config.json")).unwrap()
        );
    }

    #[test]
    fn rejects_an_unmapped_vm_without_touching_hcs() {
        let fixture = fixture("unmapped");
        let calls = fixture.calls.clone();

        let error = pipeline(&calls, false)
            .start(&fixture.store, "missing-vm", &fixture.vm_directory)
            .expect_err("an unmapped VM must not be started");

        assert!(error.to_string().contains("missing-vm"));
        assert!(calls.grant.lock().unwrap().is_empty());
        assert!(calls.start.lock().unwrap().is_empty());
    }

    #[test]
    fn a_missing_configuration_aborts_before_starting() {
        let fixture = fixture("no-config");
        let calls = fixture.calls.clone();
        fs::remove_file(fixture.vm_directory.join("config.json")).unwrap();

        let error = pipeline(&calls, false)
            .start(&fixture.store, "dev", &fixture.vm_directory)
            .expect_err("a missing configuration must abort the start");

        assert!(error.to_string().contains("HCS configuration"));
        assert!(calls.start.lock().unwrap().is_empty());
    }

    #[test]
    fn a_malformed_configuration_aborts_before_starting() {
        let fixture = fixture("bad-config");
        let calls = fixture.calls.clone();
        fs::write(fixture.vm_directory.join("config.json"), b"not json").unwrap();

        let error = pipeline(&calls, false)
            .start(&fixture.store, "dev", &fixture.vm_directory)
            .expect_err("a malformed configuration must abort the start");

        assert!(error.to_string().contains("not valid JSON"));
        assert!(calls.grant.lock().unwrap().is_empty());
        assert!(calls.start.lock().unwrap().is_empty());
    }

    #[test]
    fn propagates_a_start_failure() {
        let fixture = fixture("start-failure");
        let calls = fixture.calls.clone();

        let error = pipeline(&calls, true)
            .start(&fixture.store, "dev", &fixture.vm_directory)
            .expect_err("a failed start must be reported");

        assert!(error.to_string().contains("injected start failure"));
        assert_eq!(calls.grant.lock().unwrap().len(), 2);
    }

    #[test]
    fn attachment_paths_are_empty_for_a_configuration_without_attachments() {
        assert_eq!(
            attachment_paths(r#"{"VirtualMachine":{"Devices":{}}}"#).unwrap(),
            Vec::<PathBuf>::new()
        );
    }

    #[test]
    fn attachment_paths_skip_entries_without_a_path() {
        let document = serde_json::json!({
            "VirtualMachine": { "Devices": { "Scsi": { "Primary": { "Attachments": {
                "0": { "Type": "PassThru" },
                "1": { "Type": "Iso", "Path": "C:\\images\\installer.iso" }
            }}}}}
        })
        .to_string();

        assert_eq!(
            attachment_paths(&document).unwrap(),
            vec![PathBuf::from("C:\\images\\installer.iso")]
        );
    }

    #[test]
    fn attachment_paths_reject_malformed_json() {
        assert!(attachment_paths("not json").is_err());
    }
}
