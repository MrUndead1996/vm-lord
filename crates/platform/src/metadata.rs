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
use vmlord_core::{
    DesktopProfile, DisplayMode, DisplayProvisioning, GpuMode, NetworkMode, RepositoryError,
    SshAuthentication, SshConfig, SshDaemon, SshPort, VmSource,
};
use vmlord_gpu_payload::GuestSelector;

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
    /// How to log into this VM over SSH, if it has an SSH server at all.
    ///
    /// `None` is a VM created with SSH switched off -- there are no VMs from
    /// before this field existed, so absence needs no second meaning. What is
    /// recorded is what a person chose at creation and what cloud-init was
    /// asked to apply; the address is not, because the guest takes a new one
    /// from HNS on every start, and the password is not, because a password on
    /// disk is a password leaked.
    #[serde(default)]
    pub ssh: Option<SshConfig>,
    /// How this guest's distribution runs and configures its SSH daemon.
    ///
    /// Recorded at creation because moving the port of a VM that already
    /// exists has to write the same drop-ins and poke the same units the seed
    /// did, and by then the image profile the VM was built from is no longer
    /// in hand -- the seed was consumed on the first boot and nothing on the
    /// host remembers which distribution answered.
    ///
    /// `None` is a VM whose guest VMLord did not configure: one installed by
    /// hand from local media. Such a guest's SSH daemon is its owner's, and
    /// VMLord has nothing to say about where its files are.
    #[serde(default)]
    pub ssh_daemon: Option<SshDaemon>,
    /// What the VM asks of the host's GPU.
    ///
    /// Recorded because a start has to know what to attach, and the stored HCS
    /// configuration cannot answer it: that document describes the shares a VM
    /// was last started with, not the mode it was created with.
    ///
    /// A mapping written before this field existed reads as [`GpuMode::None`],
    /// which is what every VM created so far asked for.
    #[serde(default)]
    pub gpu_mode: GpuMode,
    /// The desktop this VM was created with.
    ///
    /// Recorded because a start and a Connect both have to know whether there
    /// is a desktop at all, and nothing else on the host can be asked: the
    /// seed that installed it was consumed on the first boot.
    ///
    /// A mapping written before this field existed reads as
    /// [`DesktopProfile::Headless`] -- which is what those VMs are, since
    /// nothing installed a desktop into them. That is deliberately not the
    /// type's own default: a create form starts from a desktop, and a VM
    /// built before desktops existed did not.
    #[serde(default = "no_desktop")]
    pub desktop_profile: DesktopProfile,
    /// How far installing that desktop got, as it was last recorded.
    ///
    /// Stored rather than derived because the installation happens once,
    /// during the build, and every later run of VMLord has to be able to read
    /// its outcome -- including to offer a retry for it.
    ///
    /// A mapping written before this field existed reads as
    /// [`DisplayProvisioning::NotRequested`], which matches the profile such a
    /// mapping reads back with.
    #[serde(default)]
    pub display_provisioning: DisplayProvisioning,
    /// The mode this VM's output comes up at, when one has been saved.
    ///
    /// `None` is every VM today: nothing writes this field until task #120
    /// saves the size somebody resized a viewer to. The guest answers `None`
    /// with 1920x1080, which is what every VM has come up at so far, so an
    /// absent field and a mapping written before this field existed read the
    /// same and neither needs a migration.
    #[serde(default, deserialize_with = "forgiving_display_mode")]
    pub display_mode: Option<DisplayMode>,
    /// The guest a GPU payload would have to suit, as far as VMLord knows it.
    ///
    /// `None` is a VM built from installation media: VMLord promises nothing
    /// about the system inside it, so there is nothing to select a payload
    /// from. Deliberately not a guess.
    #[serde(default)]
    pub guest_target: Option<GuestTargetKey>,
}

/// What a mapping written before desktops existed asks for.
fn no_desktop() -> DesktopProfile {
    DesktopProfile::Headless
}

/// Reads a stored display mode, and reads an unusable one as no mode at all.
///
/// A mode outside what the module drives cannot be honoured, and the fallback
/// is a working desktop. A mapping that refuses to parse, on the other hand,
/// is a VM VMLord loses entirely -- so one bad field is worth the fallback and
/// is not worth the VM.
fn forgiving_display_mode<'de, D>(deserializer: D) -> Result<Option<DisplayMode>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    struct Stored {
        width: u32,
        height: u32,
    }

    let Some(stored) = Option::<Stored>::deserialize(deserializer)? else {
        return Ok(None);
    };
    Ok(DisplayMode::new(stored.width, stored.height))
}

/// The three facts that pick a GPU payload out of the catalog.
///
/// Not a [`vmlord_gpu_payload::GuestTarget`]: that type carries the kernel a
/// payload was proven on, which is a property of a booted guest and not of a
/// VM that has never run.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestTargetKey {
    pub distribution: String,
    pub release: String,
    pub architecture: String,
}

impl GuestTargetKey {
    pub(crate) fn selector(&self) -> GuestSelector<'_> {
        GuestSelector {
            distribution: &self.distribution,
            release: &self.release,
            architecture: &self.architecture,
        }
    }

    /// The same three facts, as the display catalog's own selector.
    ///
    /// Two methods and not one generic one: the two catalogs own their selector
    /// types, and a shared one would be the first thing tying their lifecycles
    /// together.
    pub(crate) fn display_selector(&self) -> vmlord_display_payload::GuestSelector<'_> {
        vmlord_display_payload::GuestSelector {
            distribution: &self.distribution,
            release: &self.release,
            architecture: &self.architecture,
        }
    }
}

/// Every VM VMLord builds is amd64. The field exists because the catalog has
/// one, and a literal in three places would be three places to correct.
const GUEST_ARCHITECTURE: &str = "amd64";

/// What a source says about the guest it will produce.
pub(crate) fn guest_target_key(source: &VmSource) -> Option<GuestTargetKey> {
    match source {
        VmSource::LocalMedia { .. } => None,
        VmSource::CloudImage { image, .. } => Some(GuestTargetKey {
            // The catalog spells a distribution the way the guest's
            // `/etc/os-release` does, which is lowercase; the profile spells
            // the name the way a person reads it.
            distribution: image.profile.name.to_ascii_lowercase(),
            release: image.release.clone(),
            architecture: GUEST_ARCHITECTURE.to_owned(),
        }),
    }
}

/// What VMLord knows about a copied AppSandbox guest once its disk is in place.
///
/// The source VM is not part of it. By the time this is built the copy is
/// finished and the import owns a disk of its own, so nothing that follows has
/// any reason to name the machine it came from -- and nothing that reads a
/// mapping can be handed a path into the source application's storage.
pub(crate) struct CompletedImport<'a> {
    pub(crate) vm_id: Uuid,
    pub(crate) vm_name: &'a str,
    pub(crate) hcs_compute_system_id: &'a str,
    pub(crate) disk_gb: u32,
    /// The guest user the source VM was provisioned with, which is also who
    /// the conversion connects as.
    pub(crate) ssh_username: &'a str,
    /// The port that guest's own SSH daemon already listens on.
    pub(crate) ssh_port: SshPort,
}

impl VmComputeSystemMapping {
    /// The mapping a copied AppSandbox guest is registered under for its first
    /// VMLord boot.
    ///
    /// Deliberately a plain VMLord VM with nothing claimed of it: NAT, no GPU,
    /// no desktop and no known guest. What the import asked for -- the GPU
    /// mode, the desktop profile -- stays in the import journal until the
    /// conversion has actually put it inside the guest, because a start reads
    /// this mapping to decide what to attach, and a mapping that promised a
    /// desktop the first boot has none of would have it export shares the
    /// guest cannot mount.
    ///
    /// The SSH facts are the exception, and are the source guest's own: it
    /// already answers as that user on that port, which is how the conversion
    /// reaches it at all. The recorded authentication is
    /// [`SshAuthentication::VmlordKey`] because that is the key the conversion
    /// deploys and the one every later connection uses; the AppSandbox key
    /// that opens the first session is never recorded anywhere.
    pub(crate) fn from_completed_import(import: &CompletedImport<'_>) -> Self {
        Self {
            vm_id: import.vm_id,
            vm_name: import.vm_name.to_owned(),
            hcs_compute_system_id: import.hcs_compute_system_id.to_owned(),
            disk_gb: import.disk_gb,
            // Taken on the first start, like any other VM's.
            endpoint_id: None,
            network_mode: NetworkMode::Nat,
            ssh: Some(SshConfig {
                username: import.ssh_username.to_owned(),
                port: import.ssh_port,
                authentication: SshAuthentication::VmlordKey,
            }),
            // VMLord did not configure this guest's SSH daemon, so it has
            // nothing to say about where that daemon's files are: a port move
            // would be guessing which distribution answered.
            ssh_daemon: None,
            gpu_mode: GpuMode::None,
            desktop_profile: DesktopProfile::Headless,
            display_provisioning: DisplayProvisioning::NotRequested,
            display_mode: None,
            // Learned from the guest during conversion, never guessed from the
            // source application's configuration.
            guest_target: None,
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if self.vm_name.is_empty() {
            return Err(RepositoryError::new("VM name must not be empty"));
        }
        if self.hcs_compute_system_id.is_empty() {
            return Err(RepositoryError::new(
                "HCS compute system ID must not be empty",
            ));
        }
        // Checked on the way in and on the way back out: a document edited by
        // hand between the two is refused while it is still data, rather than
        // when its user name has already become an `ssh -l` argument.
        if let Some(ssh) = &self.ssh {
            ssh.validate()?;
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
            tracing::error!("{message}");
            return Err(RepositoryError::new(message));
        }

        mappings.retain(|existing| existing.vm_id != mapping.vm_id);
        tracing::debug!(
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
            tracing::warn!("no HCS compute system mapping found for VM {vm_id} to remove");
            return Ok(());
        }

        tracing::debug!("removed HCS compute system mapping for VM {vm_id}");
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
                tracing::debug!(
                    "no HCS metadata mapping file at {}; starting empty",
                    self.mapping_file_path.display()
                );
                return Ok(Vec::new());
            }
            Err(source) => {
                let error =
                    filesystem_error("read metadata mapping", &self.mapping_file_path, source);
                tracing::error!("{error}");
                return Err(error);
            }
        };

        let mappings: Vec<VmComputeSystemMapping> =
            serde_json::from_str(&contents).map_err(|source| {
                let error = RepositoryError::new(format!(
                    "failed to parse metadata mapping at {}: {source}",
                    self.mapping_file_path.display()
                ));
                tracing::error!("{error}");
                error
            })?;

        for mapping in &mappings {
            mapping.validate().map_err(|source| {
                let error = RepositoryError::new(format!(
                    "invalid metadata mapping at {}: {source}",
                    self.mapping_file_path.display()
                ));
                tracing::error!("{error}");
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
                tracing::error!("{error}");
                error
            })?;
        }

        let document = serde_json::to_string_pretty(mappings).map_err(|source| {
            RepositoryError::new(format!("failed to serialize metadata mapping: {source}"))
        })?;
        fs::write(&self.mapping_file_path, document).map_err(|source| {
            let error = filesystem_error("write metadata mapping", &self.mapping_file_path, source);
            tracing::error!("{error}");
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
    use vmlord_core::{
        CloudImage, DesktopProfile, DisplayFailure, DisplayMode, DisplayProvisioning, DisplayStage,
        DisplayStatusCode, GpuMode, NetworkMode, Provisioning, SshAccess, SshAuthentication,
        SshConfig, SshPort, VmSource, distro,
    };

    use super::{
        CompletedImport, GuestTargetKey, MetadataStore, VmComputeSystemMapping, guest_target_key,
    };

    #[test]
    fn a_stored_display_mode_survives_a_round_trip() {
        let mut written = mapping(Uuid::nil(), "vm", "hcs");
        written.display_mode = DisplayMode::new(2560, 1440);

        let json = serde_json::to_string(&written).unwrap();
        let read: VmComputeSystemMapping = serde_json::from_str(&json).unwrap();

        assert_eq!(read.display_mode, DisplayMode::new(2560, 1440));
    }

    #[test]
    fn a_mapping_with_no_display_mode_reads_as_no_mode() {
        let json = serde_json::to_string(&mapping(Uuid::nil(), "vm", "hcs")).unwrap();
        let stripped = json.replace(r#""display_mode":null,"#, "");
        let read: VmComputeSystemMapping = serde_json::from_str(&stripped).unwrap();

        assert_eq!(
            read.display_mode, None,
            "every VM today, and every VM written before this field existed"
        );
    }

    #[test]
    fn a_stored_mode_the_module_will_not_drive_reads_as_no_mode() {
        let json = serde_json::to_string(&mapping(Uuid::nil(), "vm", "hcs"))
            .unwrap()
            .replace(
                r#""display_mode":null"#,
                r#""display_mode":{"width":7680,"height":4320}"#,
            );

        let read: VmComputeSystemMapping = serde_json::from_str(&json)
            .expect("one unusable field must not cost VMLord the whole VM");

        assert_eq!(read.display_mode, None);
    }

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
            ssh_daemon: None,
            gpu_mode: GpuMode::None,
            desktop_profile: vmlord_core::DesktopProfile::Headless,
            display_provisioning: vmlord_core::DisplayProvisioning::NotRequested,
            display_mode: None,
            guest_target: None,
            ssh: None,
        }
    }

    fn ssh_config() -> SshConfig {
        SshConfig {
            username: "ubuntu".into(),
            port: SshPort::new(2222).unwrap(),
            authentication: SshAuthentication::VmlordKey,
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

    #[test]
    fn an_ssh_configuration_survives_being_written_and_read_back() {
        let path = temporary_mapping_file();
        let store = MetadataStore::new(&path);
        let vm_id = Uuid::new_v4();

        store
            .insert(VmComputeSystemMapping {
                ssh: Some(ssh_config()),
                ..mapping(vm_id, "dev-linux", "vmlord-1")
            })
            .unwrap();

        let document = fs::read_to_string(&path).unwrap();
        assert!(document.contains("2222"), "got {document}");
        assert!(document.contains("VmlordKey"), "got {document}");
        assert_eq!(
            store.find_by_vm_id(vm_id).unwrap().unwrap().ssh,
            Some(ssh_config())
        );
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    /// Moving the port of a VM that already exists writes the files named
    /// here, so what a mapping reads back has to be what the seed was printed
    /// from -- unit names, drop-in paths and the shape of the two together.
    #[test]
    fn the_ssh_daemon_of_a_guest_survives_being_written_and_read_back() {
        let path = temporary_mapping_file();
        let store = MetadataStore::new(&path);
        let vm_id = Uuid::new_v4();

        store
            .insert(VmComputeSystemMapping {
                ssh_daemon: Some(distro::ubuntu().ssh),
                ..mapping(vm_id, "dev-linux", "vmlord-1")
            })
            .unwrap();

        let document = fs::read_to_string(&path).unwrap();
        assert!(document.contains("SocketActivated"), "got {document}");
        assert!(document.contains("ssh.socket"), "got {document}");
        assert_eq!(
            store.find_by_vm_id(vm_id).unwrap().unwrap().ssh_daemon,
            Some(distro::ubuntu().ssh)
        );
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    /// A VM created with SSH switched off. There are no VMs from before the
    /// field existed, so this is the only thing its absence can mean.
    #[test]
    fn a_vm_without_ssh_reads_back_as_having_none() {
        let path = temporary_mapping_file();
        let store = MetadataStore::new(&path);
        let vm_id = Uuid::new_v4();

        store.insert(mapping(vm_id, "no-ssh", "vmlord-1")).unwrap();

        assert_eq!(store.find_by_vm_id(vm_id).unwrap().unwrap().ssh, None);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn insert_rejects_an_ssh_user_name_the_guest_would_refuse() {
        let store = MetadataStore::new(temporary_mapping_file());

        let error = store
            .insert(VmComputeSystemMapping {
                ssh: Some(SshConfig {
                    username: "root -oProxyCommand=calc".into(),
                    ..ssh_config()
                }),
                ..mapping(Uuid::new_v4(), "dev-linux", "vmlord-1")
            })
            .unwrap_err();

        assert!(error.to_string().contains("user name"), "got {error}");
    }

    /// Hand-edited metadata is refused as a document, long before a user name
    /// or a port could reach the arguments of an `ssh.exe`.
    #[test]
    fn a_stored_ssh_section_that_cannot_be_connected_with_does_not_load() {
        for stored_ssh in [
            r#"{"username":"ubuntu","port":0,"authentication":"VmlordKey"}"#,
            r#"{"username":"Ubuntu Admin","port":22,"authentication":"VmlordKey"}"#,
            r#"{"username":"ubuntu","port":22,"authentication":"Agent"}"#,
        ] {
            let path = temporary_mapping_file();
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let vm_id = Uuid::new_v4();
            fs::write(
                &path,
                format!(
                    r#"[{{"vm_id":"{vm_id}","vm_name":"dev-linux",
                        "hcs_compute_system_id":"vmlord-1","disk_gb":20,
                        "ssh":{stored_ssh}}}]"#
                ),
            )
            .unwrap();

            assert!(
                MetadataStore::new(&path).find_by_vm_id(vm_id).is_err(),
                "{stored_ssh} must not load"
            );
            fs::remove_dir_all(path.parent().unwrap()).unwrap();
        }
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
    #[test]
    fn a_mapping_written_before_gpu_existed_reads_back_without_one() {
        let document = r#"{"vm_id":"00000000-0000-0000-0000-000000000001",
            "vm_name":"dev","hcs_compute_system_id":"vmlord-dev"}"#;

        let mapping: VmComputeSystemMapping =
            serde_json::from_str(document).expect("an older mapping must still read");

        assert_eq!(mapping.gpu_mode, GpuMode::None);
        assert_eq!(mapping.guest_target, None);
        // A VM built before desktops existed has none, whatever a create form
        // starts from today.
        assert_eq!(mapping.desktop_profile, DesktopProfile::Headless);
        assert_eq!(
            mapping.display_provisioning,
            DisplayProvisioning::NotRequested
        );
    }

    /// The desired desktop and what installing it came to are two fields, and
    /// both have to survive a restart: one is what the VM asked for and the
    /// other is what a retry is offered from.
    #[test]
    fn a_desktop_and_a_failed_installation_both_survive_a_round_trip() {
        let mapping = VmComputeSystemMapping {
            desktop_profile: DesktopProfile::Gnome,
            display_provisioning: DisplayProvisioning::Degraded(DisplayFailure::new(
                DisplayStage::Provisioning,
                DisplayStatusCode::PackageDownloadFailed,
                "archive.ubuntu.com did not answer",
            )),
            ..mapping(Uuid::from_u128(1), "dev", "vmlord-dev")
        };

        let encoded = serde_json::to_string(&mapping).expect("a mapping must serialize");
        let decoded: VmComputeSystemMapping =
            serde_json::from_str(&encoded).expect("a mapping must deserialize");

        assert_eq!(decoded.desktop_profile, DesktopProfile::Gnome);
        assert_eq!(decoded.display_provisioning, mapping.display_provisioning);
        assert!(decoded.display_provisioning.can_retry());
    }

    #[test]
    fn a_recorded_gpu_mode_survives_a_round_trip() {
        let mapping = VmComputeSystemMapping {
            ssh_daemon: None,
            gpu_mode: GpuMode::Mirror,
            desktop_profile: vmlord_core::DesktopProfile::Headless,
            display_provisioning: vmlord_core::DisplayProvisioning::NotRequested,
            guest_target: Some(GuestTargetKey {
                distribution: "ubuntu".into(),
                release: "26.04".into(),
                architecture: "amd64".into(),
            }),
            ..mapping(Uuid::from_u128(1), "dev", "vmlord-dev")
        };

        let encoded = serde_json::to_string(&mapping).expect("a mapping must serialize");
        let decoded: VmComputeSystemMapping =
            serde_json::from_str(&encoded).expect("a mapping must deserialize");

        assert_eq!(decoded.gpu_mode, GpuMode::Mirror);
        assert_eq!(decoded.guest_target.expect("recorded").release, "26.04");
    }

    #[test]
    fn a_cloud_image_names_the_guest_it_provisions() {
        let key = guest_target_key(&VmSource::CloudImage {
            image: CloudImage {
                profile: distro::ubuntu(),
                release: "26.04".into(),
            },
            provisioning: Provisioning {
                username: "ubuntu".into(),
                password: None,
                ssh: SshAccess::Disabled,
                locale: "en_US.UTF-8".into(),
                keyboard: "us".into(),
                timezone: "UTC".into(),
                desktop: vmlord_core::DesktopProfile::Headless,
            },
        })
        .expect("a cloud image knows what it boots");

        assert_eq!(
            key.distribution, "ubuntu",
            "the catalog spells it lowercase"
        );
        assert_eq!(key.release, "26.04");
        assert_eq!(key.architecture, "amd64");
    }

    #[test]
    fn installation_media_names_no_guest() {
        assert_eq!(
            guest_target_key(&VmSource::LocalMedia {
                path: "C:\\images\\ubuntu.iso".into()
            }),
            None,
            "VMLord does not know what system is inside installation media"
        );
    }

    /// A copied AppSandbox guest as VMLord first registers it: the source
    /// VM's identity is gone, and what is left is a VM of VMLord's own.
    fn completed_import() -> CompletedImport<'static> {
        CompletedImport {
            vm_id: Uuid::from_u128(11),
            vm_name: "imported",
            hcs_compute_system_id: "vmlord-imported",
            disk_gb: 80,
            ssh_username: "sandbox",
            ssh_port: SshPort::new(2222).unwrap(),
        }
    }

    #[test]
    fn completed_import_takes_nat_and_the_source_guests_ssh_facts() {
        // The copied guest already answers SSH as the AppSandbox user on the
        // port its own daemon was configured with, and it reaches the network
        // the way every VMLord VM does.
        let mapping = VmComputeSystemMapping::from_completed_import(&completed_import());

        assert_eq!(mapping.network_mode, NetworkMode::Nat);
        assert_eq!(
            mapping.endpoint_id, None,
            "the endpoint is taken on the first start, like any other VM's"
        );
        assert_eq!(
            mapping.ssh,
            Some(SshConfig {
                username: "sandbox".to_owned(),
                port: SshPort::new(2222).unwrap(),
                authentication: SshAuthentication::VmlordKey,
            })
        );
        assert_eq!(mapping.disk_gb, 80);
    }

    #[test]
    fn completed_import_claims_no_gpu_desktop_or_guest_before_conversion() {
        // What the import asked for stays in its journal until the conversion
        // has put it inside the guest: a mapping that claimed a GPU or a
        // desktop the first boot has none of would have the next start attach
        // shares nothing can mount.
        let mapping = VmComputeSystemMapping::from_completed_import(&completed_import());

        assert_eq!(mapping.gpu_mode, GpuMode::None);
        assert_eq!(mapping.desktop_profile, DesktopProfile::Headless);
        assert_eq!(
            mapping.display_provisioning,
            DisplayProvisioning::NotRequested
        );
        assert_eq!(mapping.display_mode, None);
        assert_eq!(
            mapping.guest_target, None,
            "which distribution answered is learned from the guest, not guessed"
        );
        assert_eq!(
            mapping.ssh_daemon, None,
            "VMLord did not configure this guest's SSH daemon"
        );
    }

    #[test]
    fn completed_import_is_a_mapping_the_store_accepts() {
        let mapping = VmComputeSystemMapping::from_completed_import(&completed_import());

        assert!(mapping.validate().is_ok());
    }
}
