//! Persistent mapping between VMLord VM identity and HCS compute-system IDs.
//!
//! HCS enumerates and reconnects to compute systems by an ID that is only
//! known once a VM has been created. This store lets `create`, `enumerate`,
//! `reconnect` and `delete` resolve a VMLord VM id/name to its HCS
//! compute-system id (and back) across VMLord restarts.

use std::{
    fs,
    io::{self},
    path::{Path, PathBuf},
    sync::Mutex,
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use vmlord_core::{NetworkMode, RepositoryError};

/// Serializes the read-modify-write of the mapping document.
///
/// Creating a VM runs on its own thread, so two builds finishing at the same
/// moment would both read the document, both add their own VM and both write
/// it back -- and one of the two VMs would be gone from a file that reported
/// success twice. The lock is process-wide because a `MetadataStore` is a path
/// and nothing else: two stores over the same file are the same document.
///
/// Two VMLord processes over one storage root are not covered, and are not a
/// case this task creates.
static DOCUMENT_LOCK: Mutex<()> = Mutex::new(());

/// A persisted link between a VMLord VM and the HCS compute system that backs it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmComputeSystemMapping {
    pub vm_id: Uuid,
    pub vm_name: String,
    pub hcs_compute_system_id: String,
    /// The size the VM's system disk presents to its guest, in GiB.
    ///
    /// It is recorded here because the disk itself cannot be asked while the
    /// VM runs: Hyper-V holds the VHDX open exclusively, so `OpenVirtualDisk`
    /// fails with `ERROR_ACCESS_DENIED` for exactly the VMs whose size the VM
    /// list is most often refreshing.
    ///
    /// Zero means "not recorded" -- a mapping written before this field
    /// existed -- and callers fall back to reading the disk.
    #[serde(default)]
    pub disk_gb: u32,
    /// The VM's endpoint in VMLord's shared NAT network, once it has one.
    ///
    /// `None` means the VM has never been started -- or was created before
    /// this field existed, which reads the same way and needs no migration.
    /// The endpoint is created on the first start and kept until the VM is
    /// deleted: re-creating it per start would hand the guest a new address
    /// every time and break everything that remembered the old one.
    #[serde(default)]
    pub endpoint_id: Option<Uuid>,
    /// How the VM was asked to be attached to the network.
    ///
    /// Recorded because a start has to decide whether to give the VM an
    /// endpoint, and the stored `config.json` cannot answer that: it describes
    /// the adapter a VM already has, not the mode it was created with.
    ///
    /// A mapping written before this field existed reads as
    /// [`NetworkMode::None`] -- which is what every VM created so far asked
    /// for, since the HCS backend still rejects every other mode.
    #[serde(default)]
    pub network_mode: NetworkMode,
}

impl VmComputeSystemMapping {
    fn validate(&self) -> Result<(), RepositoryError> {
        if self.vm_name.is_empty() {
            return Err(RepositoryError::new("VM name must not be empty"));
        }
        if self.hcs_compute_system_id.is_empty() {
            return Err(RepositoryError::new(
                "HCS compute system ID must not be empty",
            ));
        }
        Ok(())
    }
}

/// File-backed store for VM id/name to HCS compute-system id mappings.
#[derive(Clone, Debug)]
pub struct MetadataStore {
    mapping_file_path: PathBuf,
}

impl MetadataStore {
    /// Creates a store backed by a single JSON document at `mapping_file_path`.
    #[must_use]
    pub fn new(mapping_file_path: impl Into<PathBuf>) -> Self {
        Self {
            mapping_file_path: mapping_file_path.into(),
        }
    }

    /// Persists a mapping, replacing any existing entry for the same VM id.
    pub fn insert(&self, mapping: VmComputeSystemMapping) -> Result<(), RepositoryError> {
        mapping.validate()?;

        let _guard = DOCUMENT_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut mappings = self.load()?;
        if let Some(conflict) = mappings.iter().find(|existing| {
            existing.vm_id != mapping.vm_id
                && existing.hcs_compute_system_id == mapping.hcs_compute_system_id
        }) {
            let message = format!(
                "HCS compute system id \"{}\" is already mapped to VM \"{}\" ({})",
                mapping.hcs_compute_system_id, conflict.vm_name, conflict.vm_id
            );
            log::error!("{message}");
            return Err(RepositoryError::new(message));
        }

        mappings.retain(|existing| existing.vm_id != mapping.vm_id);
        log::debug!(
            "mapping VM \"{}\" ({}) to HCS compute system \"{}\"",
            mapping.vm_name,
            mapping.vm_id,
            mapping.hcs_compute_system_id
        );
        mappings.push(mapping);
        self.save(&mappings)
    }

    /// Removes the mapping for `vm_id`, if present.
    pub fn remove(&self, vm_id: Uuid) -> Result<(), RepositoryError> {
        let _guard = DOCUMENT_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut mappings = self.load()?;
        let original_len = mappings.len();
        mappings.retain(|existing| existing.vm_id != vm_id);
        if mappings.len() == original_len {
            log::warn!("no HCS compute system mapping found for VM {vm_id} to remove");
            return Ok(());
        }

        log::debug!("removed HCS compute system mapping for VM {vm_id}");
        self.save(&mappings)
    }

    /// Returns every persisted mapping.
    pub fn list(&self) -> Result<Vec<VmComputeSystemMapping>, RepositoryError> {
        self.load()
    }

    /// Finds the mapping for a VM by its stable id.
    pub fn find_by_vm_id(
        &self,
        vm_id: Uuid,
    ) -> Result<Option<VmComputeSystemMapping>, RepositoryError> {
        Ok(self
            .load()?
            .into_iter()
            .find(|mapping| mapping.vm_id == vm_id))
    }

    /// Finds the mapping for a VM by its display name.
    pub fn find_by_vm_name(
        &self,
        vm_name: &str,
    ) -> Result<Option<VmComputeSystemMapping>, RepositoryError> {
        Ok(self
            .load()?
            .into_iter()
            .find(|mapping| mapping.vm_name == vm_name))
    }

    /// Finds the mapping for a VM by its HCS compute-system id.
    pub fn find_by_hcs_id(
        &self,
        hcs_compute_system_id: &str,
    ) -> Result<Option<VmComputeSystemMapping>, RepositoryError> {
        Ok(self
            .load()?
            .into_iter()
            .find(|mapping| mapping.hcs_compute_system_id == hcs_compute_system_id))
    }

    fn load(&self) -> Result<Vec<VmComputeSystemMapping>, RepositoryError> {
        let contents = match fs::read_to_string(&self.mapping_file_path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                log::debug!(
                    "no HCS metadata mapping file at {}; starting empty",
                    self.mapping_file_path.display()
                );
                return Ok(Vec::new());
            }
            Err(source) => {
                let error =
                    filesystem_error("read metadata mapping", &self.mapping_file_path, source);
                log::error!("{error}");
                return Err(error);
            }
        };

        let mappings: Vec<VmComputeSystemMapping> =
            serde_json::from_str(&contents).map_err(|source| {
                let error = RepositoryError::new(format!(
                    "failed to parse metadata mapping at {}: {source}",
                    self.mapping_file_path.display()
                ));
                log::error!("{error}");
                error
            })?;

        for mapping in &mappings {
            mapping.validate().map_err(|source| {
                let error = RepositoryError::new(format!(
                    "invalid metadata mapping at {}: {source}",
                    self.mapping_file_path.display()
                ));
                log::error!("{error}");
                error
            })?;
        }

        Ok(mappings)
    }

    fn save(&self, mappings: &[VmComputeSystemMapping]) -> Result<(), RepositoryError> {
        if let Some(directory) = self.mapping_file_path.parent() {
            fs::create_dir_all(directory).map_err(|source| {
                let error =
                    filesystem_error("create metadata mapping directory", directory, source);
                log::error!("{error}");
                error
            })?;
        }

        let document = serde_json::to_string_pretty(mappings).map_err(|source| {
            RepositoryError::new(format!("failed to serialize metadata mapping: {source}"))
        })?;
        fs::write(&self.mapping_file_path, document).map_err(|source| {
            let error = filesystem_error("write metadata mapping", &self.mapping_file_path, source);
            log::error!("{error}");
            error
        })
    }
}

fn filesystem_error(operation: &str, path: &Path, source: io::Error) -> RepositoryError {
    RepositoryError::new(format!(
        "failed to {operation} at {}: {source}",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use uuid::Uuid;
    use vmlord_core::NetworkMode;

    use super::{MetadataStore, VmComputeSystemMapping};

    fn temporary_mapping_file() -> std::path::PathBuf {
        let unique_id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir()
            .join(format!("vmlord-metadata-test-{unique_id}"))
            .join("vm-mapping.json")
    }

    fn mapping(vm_id: Uuid, vm_name: &str, hcs_id: &str) -> VmComputeSystemMapping {
        VmComputeSystemMapping {
            vm_id,
            vm_name: vm_name.into(),
            hcs_compute_system_id: hcs_id.into(),
            disk_gb: 20,
            endpoint_id: None,
            network_mode: NetworkMode::None,
        }
    }

    #[test]
    fn list_is_empty_when_no_file_exists() {
        let store = MetadataStore::new(temporary_mapping_file());

        assert_eq!(store.list().unwrap(), Vec::new());
    }

    #[test]
    fn insert_then_find_round_trips_a_mapping() {
        let path = temporary_mapping_file();
        let store = MetadataStore::new(&path);
        let vm_id = Uuid::new_v4();
        let entry = mapping(vm_id, "dev-linux", "9C1A...compute-system-id");

        store.insert(entry.clone()).unwrap();

        assert_eq!(store.find_by_vm_id(vm_id).unwrap(), Some(entry.clone()));
        assert_eq!(
            store.find_by_vm_name("dev-linux").unwrap(),
            Some(entry.clone())
        );
        assert_eq!(
            store.find_by_hcs_id("9C1A...compute-system-id").unwrap(),
            Some(entry)
        );
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn insert_replaces_the_existing_entry_for_the_same_vm_id() {
        let path = temporary_mapping_file();
        let store = MetadataStore::new(&path);
        let vm_id = Uuid::new_v4();
        store
            .insert(mapping(vm_id, "dev-linux", "compute-system-1"))
            .unwrap();

        store
            .insert(mapping(vm_id, "dev-linux-renamed", "compute-system-2"))
            .unwrap();

        let mappings = store.list().unwrap();
        assert_eq!(mappings.len(), 1);
        assert_eq!(mappings[0].vm_name, "dev-linux-renamed");
        assert_eq!(mappings[0].hcs_compute_system_id, "compute-system-2");
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn insert_rejects_a_hcs_id_already_mapped_to_another_vm() {
        let path = temporary_mapping_file();
        let store = MetadataStore::new(&path);
        store
            .insert(mapping(Uuid::new_v4(), "dev-linux", "compute-system-1"))
            .unwrap();

        let error = store
            .insert(mapping(Uuid::new_v4(), "other-vm", "compute-system-1"))
            .unwrap_err();

        assert!(error.to_string().contains("already mapped"));
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn insert_rejects_empty_names_and_ids() {
        let store = MetadataStore::new(temporary_mapping_file());

        assert!(
            store
                .insert(mapping(Uuid::new_v4(), "", "compute-system-1"))
                .is_err()
        );
        assert!(
            store
                .insert(mapping(Uuid::new_v4(), "dev-linux", ""))
                .is_err()
        );
    }

    #[test]
    fn remove_deletes_only_the_matching_mapping() {
        let path = temporary_mapping_file();
        let store = MetadataStore::new(&path);
        let kept = Uuid::new_v4();
        let removed = Uuid::new_v4();
        store
            .insert(mapping(kept, "keep-vm", "compute-system-keep"))
            .unwrap();
        store
            .insert(mapping(removed, "remove-vm", "compute-system-remove"))
            .unwrap();

        store.remove(removed).unwrap();

        assert_eq!(store.find_by_vm_id(removed).unwrap(), None);
        assert!(store.find_by_vm_id(kept).unwrap().is_some());
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn remove_is_idempotent_for_an_unknown_vm_id() {
        let store = MetadataStore::new(temporary_mapping_file());

        assert!(store.remove(Uuid::new_v4()).is_ok());
    }

    #[test]
    fn a_mapping_written_before_disk_sizes_existed_still_loads() {
        let path = temporary_mapping_file();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let vm_id = Uuid::new_v4();
        fs::write(
            &path,
            format!(
                r#"[{{"vm_id":"{vm_id}","vm_name":"legacy","hcs_compute_system_id":"vmlord-1"}}]"#
            ),
        )
        .unwrap();

        let loaded = MetadataStore::new(&path).find_by_vm_id(vm_id).unwrap();

        assert_eq!(
            loaded,
            Some(mapping(vm_id, "legacy", "vmlord-1")).map(|mapping| VmComputeSystemMapping {
                disk_gb: 0,
                ..mapping
            })
        );
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_mapping_written_before_endpoints_existed_still_loads() {
        let path = temporary_mapping_file();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let vm_id = Uuid::new_v4();
        fs::write(
            &path,
            format!(
                r#"[{{"vm_id":"{vm_id}","vm_name":"legacy","hcs_compute_system_id":"vmlord-1","disk_gb":20}}]"#
            ),
        )
        .unwrap();

        let loaded = MetadataStore::new(&path).find_by_vm_id(vm_id).unwrap();

        // No endpoint recorded reads the same as never started: the next start
        // creates one, which is what a VM from before endpoints existed needs.
        // No network mode recorded reads as `None`, which is what every VM
        // created before the field existed asked for.
        assert_eq!(loaded, Some(mapping(vm_id, "legacy", "vmlord-1")));
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn the_network_mode_survives_being_written_and_read_back() {
        // The variant name is an on-disk format: renaming it in the domain
        // would silently change what already-stored VMs read back as.
        let path = temporary_mapping_file();
        let vm_id = Uuid::new_v4();
        let store = MetadataStore::new(&path);
        store
            .insert(VmComputeSystemMapping {
                network_mode: NetworkMode::Nat,
                ..mapping(vm_id, "nat", "vmlord-1")
            })
            .unwrap();

        assert!(fs::read_to_string(&path).unwrap().contains(r#""Nat""#));
        assert_eq!(
            store.find_by_vm_id(vm_id).unwrap().unwrap().network_mode,
            NetworkMode::Nat
        );
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn an_endpoint_id_survives_being_written_and_read_back() {
        let path = temporary_mapping_file();
        let store = MetadataStore::new(&path);
        let vm_id = Uuid::new_v4();
        let endpoint_id = Uuid::new_v4();

        store
            .insert(VmComputeSystemMapping {
                endpoint_id: Some(endpoint_id),
                ..mapping(vm_id, "dev-linux", "vmlord-1")
            })
            .unwrap();

        assert_eq!(
            store.find_by_vm_id(vm_id).unwrap().unwrap().endpoint_id,
            Some(endpoint_id)
        );
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    /// Parallel builds are the first thing to write metadata concurrently, and
    /// `insert` is a read-modify-write: two writers finishing together would
    /// otherwise drop one of the two VMs that had just been created.
    #[test]
    fn concurrent_inserts_keep_every_mapping() {
        let path = temporary_mapping_file();
        let store = MetadataStore::new(&path);

        let mut workers = Vec::new();
        for index in 0..8 {
            let store = store.clone();
            workers.push(std::thread::spawn(move || {
                store
                    .insert(mapping(
                        Uuid::new_v4(),
                        &format!("vm-{index}"),
                        &format!("vmlord-{index}"),
                    ))
                    .expect("each mapping should be stored");
            }));
        }
        for worker in workers {
            worker.join().expect("no writer should panic");
        }

        assert_eq!(store.list().unwrap().len(), 8);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn load_rejects_corrupt_json() {
        let path = temporary_mapping_file();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "not json").unwrap();
        let store = MetadataStore::new(&path);

        assert!(store.list().is_err());
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
