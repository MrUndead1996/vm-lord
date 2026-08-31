use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
    sync::{Mutex, MutexGuard},
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use vmlord_core::{
    AppSandboxImportStage, AppSandboxSourceId, DesktopProfile, GpuMode, RepositoryError,
};
#[cfg(windows)]
use windows::{
    Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW},
    core::HSTRING,
};

use crate::layout::{import_journal_path, import_staging_directory, imports_root};

/// Serializes every access to the import journals.
///
/// A journal is written by the thread running its import and read by the UI
/// thread, which lists the unfinished imports on every refresh. Replacing the
/// journal is a rename over an existing file, and Windows refuses that with
/// `ERROR_ACCESS_DENIED` while a reader holds the file it is about to replace
/// -- so an import could be killed outright by nothing worse than the list
/// beside it being drawn. The lock is process-wide because a journal is a path
/// and nothing else: two `ImportJournal` values over one file are one document,
/// exactly as they are for [`crate::metadata::MetadataStore`].
///
/// Two VMLord processes over one storage root are not covered, and are not a
/// case this creates.
static JOURNAL_LOCK: Mutex<()> = Mutex::new(());

/// Recovers a poisoned lock rather than propagating the panic: an import thread
/// that panicked must not make every later import unrecoverable.
fn lock_journals() -> MutexGuard<'static, ()> {
    JOURNAL_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The import lifecycle retained on disk for recovery after an interrupted run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum JournalStage {
    Validating,
    Copying,
    Creating,
    BootstrapStarting,
    Converting,
    Restarting,
    Verifying,
    NeedsAttention,
    Complete,
}

impl JournalStage {
    pub(crate) const ALL: [Self; 9] = [
        Self::Validating,
        Self::Copying,
        Self::Creating,
        Self::BootstrapStarting,
        Self::Converting,
        Self::Restarting,
        Self::Verifying,
        Self::NeedsAttention,
        Self::Complete,
    ];

    pub(crate) const fn import_stage(self) -> AppSandboxImportStage {
        match self {
            Self::Validating => AppSandboxImportStage::Validating,
            Self::Copying => AppSandboxImportStage::Copying,
            Self::Creating => AppSandboxImportStage::Creating,
            Self::BootstrapStarting => AppSandboxImportStage::BootstrapStarting,
            Self::Converting => AppSandboxImportStage::Converting,
            Self::Restarting => AppSandboxImportStage::Restarting,
            Self::Verifying => AppSandboxImportStage::Verifying,
            Self::NeedsAttention => AppSandboxImportStage::NeedsAttention,
            Self::Complete => AppSandboxImportStage::Complete,
        }
    }
}

/// An idempotent guest-conversion step that has been confirmed on the copied guest.
///
/// Ordered, and declared in the order a conversion takes them: a resumed run
/// asks "is this step behind the last one confirmed?", and the answer has to be
/// the same one every time regardless of which module is asking. Renaming or
/// reordering a variant changes both what a stored journal reads back as and
/// where a resumption starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) enum ConversionStep {
    GuestObserved,
    /// A journal written before payload delivery moved to the host.
    ///
    /// This is a migration marker, not a current conversion stage, so it is
    /// deliberately absent from [`Self::ALL`]. The runner replaces the guest
    /// program, checks that replacement and then resumes at the agent boundary
    /// which both historical payload steps necessarily followed.
    #[serde(alias = "DisplayPayloadInstalled", alias = "GpuPayloadInstalled")]
    LegacyPayloadBundleRefreshRequired,
    BundleUploaded,
    VmlordSshKeyDeployed,
    AgentInstalled,
    AppSandboxUnitsDisabled,
    ReplacementsValidated,
    ObsoleteFilesRemoved,
    /// The guest asks for its address again, and the source application's
    /// network configuration is gone.
    ///
    /// After the removal, because it is the last thing done to the guest before
    /// it is asked to shut down: the configuration written here is applied by
    /// the next boot, and rewriting the network under the session issuing the
    /// commands would cut it.
    GuestNetworkHandedOver,
    ShutdownRequested,
}

impl ConversionStep {
    /// Every step, in the order a conversion confirms them.
    pub(crate) const ALL: [Self; 9] = [
        Self::GuestObserved,
        Self::BundleUploaded,
        Self::VmlordSshKeyDeployed,
        Self::AgentInstalled,
        Self::AppSandboxUnitsDisabled,
        Self::ReplacementsValidated,
        Self::ObsoleteFilesRemoved,
        Self::GuestNetworkHandedOver,
        Self::ShutdownRequested,
    ];
}

/// Stable source facts needed to reject a recovery request for a different VM.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SourceFingerprint {
    pub(crate) source_id: AppSandboxSourceId,
    pub(crate) disk_path: PathBuf,
    pub(crate) vm_ordinal: usize,
}

/// The VM resources retained while the imported guest is not ordinary metadata yet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ImportResources {
    pub(crate) ram_mb: u32,
    pub(crate) cpu_cores: u32,
    pub(crate) disk_gb: u32,
    pub(crate) desktop_profile: DesktopProfile,
}

/// The port an imported guest's sshd listens on.
///
/// Always 22, and deliberately not the `SshPort` the source application's
/// configuration carries: that number is a port on the *host*, where the source
/// application listens and relays over hv_socket into the guest. A copied
/// guest's own `sshd_config` has no `Port` directive at all, so its sshd is at
/// the default -- which is why this is a constant and not a discovered fact.
pub(crate) const GUEST_SSH_PORT: u16 = 22;

/// Facts needed to establish the temporary AppSandbox-key SSH connection.
///
/// The username alone, because the port is [`GUEST_SSH_PORT`] for every
/// imported guest. The AppSandbox key remains at its protected source path and
/// is never represented by this type, so neither its bytes nor an agent secret
/// can be serialized with a journal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BootstrapSshFacts {
    pub(crate) username: String,
}

/// Inputs captured when an import is first made durable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImportJournalDetails {
    pub(crate) import_id: Uuid,
    pub(crate) source_fingerprint: SourceFingerprint,
    pub(crate) destination: PathBuf,
    pub(crate) requested_resources: ImportResources,
    pub(crate) desired_gpu: GpuMode,
    pub(crate) bootstrap_ssh: BootstrapSshFacts,
}

/// Durable, platform-private state for a VMLord-owned AppSandbox import.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ImportJournal {
    import_id: Uuid,
    source_fingerprint: SourceFingerprint,
    destination: PathBuf,
    requested_resources: ImportResources,
    desired_gpu: GpuMode,
    bootstrap_ssh: BootstrapSshFacts,
    stage: JournalStage,
    last_confirmed_conversion_step: Option<ConversionStep>,
    #[serde(skip, default)]
    storage_root: PathBuf,
}

impl ImportJournal {
    /// Creates and atomically persists the first recovery marker for an import.
    pub(crate) fn create(
        storage_root: impl Into<PathBuf>,
        details: ImportJournalDetails,
    ) -> Result<Self, RepositoryError> {
        let storage_root = storage_root.into();
        let journal = Self {
            import_id: details.import_id,
            source_fingerprint: details.source_fingerprint,
            destination: details.destination,
            requested_resources: details.requested_resources,
            desired_gpu: details.desired_gpu,
            bootstrap_ssh: details.bootstrap_ssh,
            stage: JournalStage::Validating,
            last_confirmed_conversion_step: None,
            storage_root,
        };
        journal.validate_under(&journal.storage_root)?;
        journal.save()?;
        Ok(journal)
    }

    /// Loads one durable import marker and revalidates its containment.
    pub(crate) fn load(
        storage_root: impl Into<PathBuf>,
        import_id: Uuid,
    ) -> Result<Self, RepositoryError> {
        let _guard = lock_journals();
        Self::load_locked(storage_root, import_id)
    }

    /// The body of [`Self::load`], for callers already holding the lock.
    fn load_locked(
        storage_root: impl Into<PathBuf>,
        import_id: Uuid,
    ) -> Result<Self, RepositoryError> {
        let storage_root = storage_root.into();
        let path = import_journal_path(&import_staging_directory(&storage_root, import_id));
        let contents = fs::read_to_string(&path).map_err(|error| read_failure(&path, error))?;
        let mut journal = serde_json::from_str::<Self>(&contents).map_err(|error| {
            RepositoryError::new(format!(
                "import journal {} is corrupted: {error}",
                path.display()
            ))
        })?;
        if journal.import_id != import_id {
            return Err(RepositoryError::new(format!(
                "import journal {} does not match its staging directory",
                path.display()
            )));
        }
        journal.storage_root = storage_root;
        journal.validate_under(&journal.storage_root)?;
        Ok(journal)
    }

    /// Atomically replaces the journal with its latest confirmed state.
    pub(crate) fn save(&self) -> Result<(), RepositoryError> {
        let _guard = lock_journals();
        self.validate_under(&self.storage_root)?;
        let staging = import_staging_directory(&self.storage_root, self.import_id);
        fs::create_dir_all(&staging).map_err(|error| create_directory_failure(&staging, error))?;

        let journal_path = import_journal_path(&staging);
        let temporary_path = journal_path.with_file_name("journal.json.new");
        let encoded = serde_json::to_vec_pretty(self).map_err(|error| {
            RepositoryError::new(format!("failed to serialize import journal: {error}"))
        })?;

        write_and_flush(&temporary_path, &encoded)?;
        replace_journal(&temporary_path, &journal_path)?;
        Ok(())
    }

    /// Removes this import's staging directory once it is no longer needed.
    ///
    /// The whole directory and not the marker alone: the conversion bundle and
    /// the guest transcript are written beside it, and a marker removed on its
    /// own would leave both behind forever under a name nothing ever looks at
    /// again. The directory is named by this import's own UUID under VMLord's
    /// `imports` root, so nothing but this import can be inside it.
    pub(crate) fn remove(&self) -> Result<(), RepositoryError> {
        let _guard = lock_journals();
        self.validate_under(&self.storage_root)?;
        let staging = import_staging_directory(&self.storage_root, self.import_id);
        match fs::remove_dir_all(&staging) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(remove_failure(&staging, error)),
        }
    }

    /// Lists durable, incomplete import markers after a fresh process starts.
    pub(crate) fn list(storage_root: impl Into<PathBuf>) -> Result<Vec<Self>, RepositoryError> {
        let _guard = lock_journals();
        let storage_root = storage_root.into();
        let root = imports_root(&storage_root);
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(read_directory_failure(&root, error)),
        };

        let mut journals = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| read_directory_failure(&root, error))?;
            let file_type = entry
                .file_type()
                .map_err(|error| read_directory_failure(&root, error))?;
            if !file_type.is_dir() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(import_id) = Uuid::parse_str(&name) else {
                continue;
            };
            let journal_path = import_journal_path(&entry.path());
            if !journal_path.is_file() {
                continue;
            }
            let journal = Self::load_locked(storage_root.clone(), import_id)?;
            if journal.stage != JournalStage::Complete {
                journals.push(journal);
            }
        }
        journals.sort_by_key(Self::import_id);
        Ok(journals)
    }

    /// Ensures the destination is a VMLord-owned child of the configured root.
    pub(crate) fn validate_under(&self, storage_root: &Path) -> Result<(), RepositoryError> {
        let relative = self.destination.strip_prefix(storage_root).map_err(|_| {
            RepositoryError::new(format!(
                "import destination {} is outside VMLord storage root {}",
                self.destination.display(),
                storage_root.display()
            ))
        })?;
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(RepositoryError::new(format!(
                "import destination {} is not a VMLord-owned VM directory",
                self.destination.display()
            )));
        }
        Ok(())
    }

    pub(crate) const fn import_id(&self) -> Uuid {
        self.import_id
    }

    pub(crate) fn path(&self) -> PathBuf {
        import_journal_path(&import_staging_directory(
            &self.storage_root,
            self.import_id,
        ))
    }

    pub(crate) fn destination(&self) -> &Path {
        &self.destination
    }

    pub(crate) const fn stage(&self) -> JournalStage {
        self.stage
    }

    pub(crate) fn validate_destination(&self) -> Result<(), RepositoryError> {
        self.validate_under(&self.storage_root)?;
        let relative = self
            .destination
            .strip_prefix(&self.storage_root)
            .expect("validate_under accepted this prefix");
        let mut components = relative.components();
        let only = components.next();
        if components.next().is_some()
            || only.is_some_and(|component| {
                component
                    .as_os_str()
                    .to_string_lossy()
                    .eq_ignore_ascii_case("imports")
            })
        {
            return Err(RepositoryError::new(format!(
                "import cleanup target {} is not one exact VMLord VM directory",
                self.destination.display()
            )));
        }
        Ok(())
    }

    pub(crate) const fn source_fingerprint(&self) -> &SourceFingerprint {
        &self.source_fingerprint
    }

    /// Re-reads what another writer of the same journal has confirmed.
    ///
    /// The conversion runner advances the confirmed step through the file
    /// rather than through this value, so whoever holds a copy across a
    /// conversion has to take the file's word for it afterwards. Writing a
    /// stale copy back would silently undo a resumable step.
    pub(crate) fn reload(&mut self) -> Result<(), RepositoryError> {
        *self = Self::load(self.storage_root.clone(), self.import_id)?;
        Ok(())
    }

    pub(crate) const fn requested_resources(&self) -> &ImportResources {
        &self.requested_resources
    }

    pub(crate) const fn desired_gpu(&self) -> GpuMode {
        self.desired_gpu
    }

    pub(crate) const fn bootstrap_ssh(&self) -> &BootstrapSshFacts {
        &self.bootstrap_ssh
    }

    pub(crate) const fn last_confirmed_conversion_step(&self) -> Option<ConversionStep> {
        self.last_confirmed_conversion_step
    }

    pub(crate) fn set_stage(&mut self, stage: JournalStage) {
        self.stage = stage;
    }

    pub(crate) fn set_last_confirmed_conversion_step(&mut self, step: Option<ConversionStep>) {
        self.last_confirmed_conversion_step = step;
    }
}

fn write_and_flush(path: &Path, contents: &[u8]) -> Result<(), RepositoryError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(|error| write_failure(path, error))?;
    file.write_all(contents)
        .and_then(|()| file.sync_all())
        .map_err(|error| write_failure(path, error))
}

#[cfg(windows)]
fn replace_journal(temporary_path: &Path, journal_path: &Path) -> Result<(), RepositoryError> {
    let temporary = HSTRING::from(temporary_path.as_os_str().to_string_lossy().as_ref());
    let journal = HSTRING::from(journal_path.as_os_str().to_string_lossy().as_ref());
    // SAFETY: both `HSTRING`s remain alive for the complete replacement call.
    unsafe {
        MoveFileExW(
            &temporary,
            &journal,
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(|error| {
        RepositoryError::windows(
            "replace import journal",
            None,
            error.code().0 as u32,
            format!("failed to replace {}", journal_path.display()),
        )
    })
}

#[cfg(not(windows))]
fn replace_journal(temporary_path: &Path, journal_path: &Path) -> Result<(), RepositoryError> {
    fs::rename(temporary_path, journal_path).map_err(|error| {
        RepositoryError::new(format!(
            "failed to replace import journal {}: {error}",
            journal_path.display()
        ))
    })
}

fn read_failure(path: &Path, error: std::io::Error) -> RepositoryError {
    RepositoryError::new(format!(
        "failed to read import journal {}: {error}",
        path.display()
    ))
}

fn write_failure(path: &Path, error: std::io::Error) -> RepositoryError {
    RepositoryError::new(format!(
        "failed to write import journal {}: {error}",
        path.display()
    ))
}

fn remove_failure(path: &Path, error: std::io::Error) -> RepositoryError {
    RepositoryError::new(format!(
        "failed to remove import journal {}: {error}",
        path.display()
    ))
}

fn create_directory_failure(path: &Path, error: std::io::Error) -> RepositoryError {
    RepositoryError::new(format!(
        "failed to create import staging directory {}: {error}",
        path.display()
    ))
}

fn read_directory_failure(path: &Path, error: std::io::Error) -> RepositoryError {
    RepositoryError::new(format!(
        "failed to list import staging directory {}: {error}",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    use uuid::Uuid;
    use vmlord_core::{AppSandboxSourceId, DesktopProfile, GpuMode};

    use super::{
        BootstrapSshFacts, ConversionStep, ImportJournal, ImportJournalDetails, ImportResources,
        JournalStage, SourceFingerprint,
    };

    fn fixture_details(destination: PathBuf) -> ImportJournalDetails {
        ImportJournalDetails {
            import_id: Uuid::from_u128(7),
            source_fingerprint: SourceFingerprint {
                source_id: AppSandboxSourceId::from_stable_hash("source-7").unwrap(),
                disk_path: PathBuf::from(r"C:\ProgramData\AppSandbox\ubuntu\disk.vhdx"),
                vm_ordinal: 3,
            },
            destination,
            requested_resources: ImportResources {
                ram_mb: 4096,
                cpu_cores: 4,
                disk_gb: 80,
                desktop_profile: DesktopProfile::Gnome,
            },
            desired_gpu: GpuMode::Default,
            bootstrap_ssh: BootstrapSshFacts {
                username: "ubuntu".to_owned(),
            },
        }
    }

    fn fixture_journal(destination: PathBuf) -> ImportJournal {
        let details = fixture_details(destination);
        ImportJournal {
            import_id: details.import_id,
            source_fingerprint: details.source_fingerprint,
            destination: details.destination,
            requested_resources: details.requested_resources,
            desired_gpu: details.desired_gpu,
            bootstrap_ssh: details.bootstrap_ssh,
            stage: JournalStage::Validating,
            last_confirmed_conversion_step: None,
            storage_root: PathBuf::from(r"C:\VMLord\vms"),
        }
    }

    /// An import must survive the list beside it being drawn.
    ///
    /// The UI thread lists the unfinished imports on every refresh while the
    /// import thread is writing its journal, and replacing a journal is a
    /// rename over a file a reader may be holding. Windows answers that with
    /// `ERROR_ACCESS_DENIED`, which killed a real import seven milliseconds
    /// after it started. Without the journal lock this fails a few times in
    /// forty; with it, never.
    #[test]
    fn a_journal_can_be_saved_while_the_unfinished_imports_are_listed() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        };

        let root = temporary_root("concurrent-listing");
        let stop = Arc::new(AtomicBool::new(false));
        let reads = Arc::new(AtomicUsize::new(0));
        let listing = {
            let root = root.clone();
            let stop = Arc::clone(&stop);
            let reads = Arc::clone(&reads);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let _ = ImportJournal::list(&root);
                    reads.fetch_add(1, Ordering::Relaxed);
                }
            })
        };

        let mut failures = Vec::new();
        let mut ids = Vec::new();
        for attempt in 0..40 {
            let mut details = fixture_details(root.join("diagnostic-destination"));
            details.import_id = Uuid::new_v4();
            ids.push(details.import_id);
            match ImportJournal::create(&root, details) {
                Ok(mut journal) => {
                    journal.stage = JournalStage::Validating;
                    if let Err(error) = journal.save() {
                        failures.push(format!("attempt {attempt}: save: {error}"));
                    }
                }
                Err(error) => failures.push(format!("attempt {attempt}: create: {error}")),
            }
        }
        stop.store(true, Ordering::Relaxed);
        let _ = listing.join();
        for id in ids {
            let _ = fs::remove_dir_all(crate::layout::import_staging_directory(&root, id));
        }

        assert!(
            failures.is_empty(),
            "{} of 40 saves were refused while {} listings ran: {failures:?}",
            failures.len(),
            reads.load(Ordering::Relaxed)
        );
    }

    #[test]
    fn removing_a_journal_takes_the_whole_staging_directory_with_it() {
        // The conversion bundle and the guest transcript are written beside the
        // marker. Removing the marker alone would leave both behind forever
        // under a UUID nothing ever looks at again.
        let root = temporary_root("remove-staging");
        let journal = ImportJournal::create(&root, fixture_details(root.join("imported"))).unwrap();
        let staging = journal.path().parent().unwrap().to_path_buf();
        fs::create_dir_all(staging.join("bundle")).unwrap();
        fs::write(staging.join("bundle").join("payload"), b"uploaded").unwrap();
        fs::write(staging.join("transcript.log"), b"what the guest said").unwrap();

        journal.remove().unwrap();

        assert!(!staging.exists(), "{}", staging.display());
        assert!(root.exists(), "only this import's own directory goes");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn removing_a_journal_that_is_already_gone_is_not_a_failure() {
        let root = temporary_root("remove-twice");
        let journal = ImportJournal::create(&root, fixture_details(root.join("imported"))).unwrap();

        journal.remove().unwrap();
        journal
            .remove()
            .expect("a second removal has nothing to do");

        let _ = fs::remove_dir_all(&root);
    }

    fn temporary_root(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "vmlord-appsandbox-journal-{label}-{}",
            Uuid::new_v4()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn journal_refuses_a_destination_outside_storage_root() {
        let root = Path::new(r"C:\VMLord\vms");
        let journal = fixture_journal(PathBuf::from(r"C:\AppSandbox\ubuntu"));

        assert!(journal.validate_under(root).is_err());
    }

    #[test]
    fn every_stage_round_trips_without_key_or_agent_secret_contents() {
        let root = temporary_root("stages");
        let destination = root.join("ubuntu");

        for stage in JournalStage::ALL {
            let mut journal =
                ImportJournal::create(&root, fixture_details(destination.clone())).unwrap();
            journal.set_stage(stage);
            journal.set_last_confirmed_conversion_step(Some(ConversionStep::BundleUploaded));
            journal.save().unwrap();

            let journal_path = journal.path();
            let saved = fs::read_to_string(&journal_path).unwrap();
            assert!(!saved.contains("PRIVATE KEY"));
            assert!(!saved.contains("agent-secret-value"));
            assert!(!saved.contains("id_appsandbox"));

            let loaded = ImportJournal::load(&root, journal.import_id()).unwrap();
            assert_eq!(loaded.stage(), stage);
            assert_eq!(
                loaded.last_confirmed_conversion_step(),
                Some(ConversionStep::BundleUploaded)
            );
        }

        fs::remove_dir_all(root).unwrap();
    }

    /// Payload delivery moved to the second boot's host-side Plan9 shares.
    /// Journals written by the first Task 7 conversion still name the two
    /// removed guest payload stages, though, and a recovery must mark either
    /// for a fresh conversion-program delivery rather than calling an
    /// otherwise recoverable import corrupted.
    #[test]
    fn journals_from_the_removed_guest_payload_stages_resume_after_agent_installation() {
        for old_step in ["DisplayPayloadInstalled", "GpuPayloadInstalled"] {
            let root = temporary_root("legacy-payload-step");
            let mut journal =
                ImportJournal::create(&root, fixture_details(root.join("ubuntu"))).unwrap();
            journal.set_last_confirmed_conversion_step(Some(ConversionStep::AgentInstalled));
            journal.save().unwrap();

            let contents = fs::read_to_string(journal.path()).unwrap();
            fs::write(journal.path(), contents.replace("AgentInstalled", old_step)).unwrap();

            let loaded = ImportJournal::load(&root, journal.import_id()).unwrap();
            assert_eq!(
                loaded.last_confirmed_conversion_step(),
                Some(ConversionStep::LegacyPayloadBundleRefreshRequired),
                "a journal at the removed {old_step} stage must refresh its program first"
            );

            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn save_replaces_the_existing_journal_through_a_sibling_temporary_file() {
        let root = temporary_root("replacement");
        let mut journal =
            ImportJournal::create(&root, fixture_details(root.join("ubuntu"))).unwrap();
        let journal_path = journal.path();
        let first = fs::read_to_string(&journal_path).unwrap();

        journal.set_stage(JournalStage::Converting);
        journal.save().unwrap();

        assert!(!journal_path.with_file_name("journal.json.new").exists());
        let second = fs::read_to_string(&journal_path).unwrap();
        assert_ne!(first, second);
        assert_eq!(
            ImportJournal::load(&root, journal.import_id())
                .unwrap()
                .stage(),
            JournalStage::Converting
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn corrupted_journal_reports_an_error() {
        let root = temporary_root("corrupt");
        let journal = ImportJournal::create(&root, fixture_details(root.join("ubuntu"))).unwrap();
        fs::write(journal.path(), "not JSON").unwrap();

        assert!(ImportJournal::load(&root, journal.import_id()).is_err());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fresh_listing_classifies_interrupted_imports() {
        let root = temporary_root("listing");
        let mut journal =
            ImportJournal::create(&root, fixture_details(root.join("ubuntu"))).unwrap();
        journal.set_stage(JournalStage::NeedsAttention);
        journal.save().unwrap();

        let found = ImportJournal::list(&root).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].stage(), JournalStage::NeedsAttention);
        assert_eq!(found[0].destination(), root.join("ubuntu"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fresh_listing_does_not_classify_a_completed_import_as_interrupted() {
        let root = temporary_root("complete");
        let mut journal =
            ImportJournal::create(&root, fixture_details(root.join("ubuntu"))).unwrap();
        journal.set_stage(JournalStage::Complete);
        journal.save().unwrap();

        assert!(ImportJournal::list(&root).unwrap().is_empty());

        fs::remove_dir_all(root).unwrap();
    }
}
