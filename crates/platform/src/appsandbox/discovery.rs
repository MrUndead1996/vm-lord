use std::{
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
};

use vmlord_core::{
    AppSandboxCompatibility, AppSandboxIncompatibility, AppSandboxSourceId, AppSandboxVmCandidate,
    GpuMode, NetworkMode, RepositoryError,
};

use super::{ValidatedSource, config::parse_vms_cfg};

type SourceIdFactory =
    Arc<dyn Fn(&Path, usize) -> Result<AppSandboxSourceId, RepositoryError> + Send + Sync>;

/// Filesystem operations discovery replaces in tests.
pub(crate) trait FileSystem: Send + Sync {
    fn read_to_string(&self, path: &Path) -> io::Result<String>;
    fn is_file(&self, path: &Path) -> bool;
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf>;
}

struct WindowsFileSystem;

impl FileSystem for WindowsFileSystem {
    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        fs::read_to_string(path)
    }

    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        fs::canonicalize(path)
    }
}

/// One atomic discovery observation: public candidates beside their private
/// source resolution table.
pub(crate) struct DiscoveryResult {
    pub(crate) candidates: Vec<AppSandboxVmCandidate>,
    pub(crate) sources: HashMap<AppSandboxSourceId, ValidatedSource>,
}

/// Locates AppSandbox configuration and evaluates every parsed VM.
pub(crate) struct Discovery {
    appsandbox_root: PathBuf,
    config_path: PathBuf,
    private_key_path: PathBuf,
    files: Arc<dyn FileSystem>,
    source_id_factory: SourceIdFactory,
}

impl Discovery {
    /// Uses the AppSandbox locations under the Windows ProgramData directory.
    #[must_use]
    pub(crate) fn default_windows() -> Self {
        let program_data = std::env::var_os("ProgramData")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
        Self::with_file_system(program_data.join("AppSandbox"), Arc::new(WindowsFileSystem))
    }

    pub(crate) fn with_file_system(appsandbox_root: PathBuf, files: Arc<dyn FileSystem>) -> Self {
        Self {
            config_path: appsandbox_root.join("vms.cfg"),
            private_key_path: appsandbox_root.join("ssh").join("id_appsandbox"),
            appsandbox_root,
            files,
            source_id_factory: Arc::new(stable_source_id),
        }
    }

    #[cfg(test)]
    fn with_source_id_factory(mut self, factory: SourceIdFactory) -> Self {
        self.source_id_factory = factory;
        self
    }

    pub(crate) fn discover(&self) -> Result<DiscoveryResult, RepositoryError> {
        let input = self
            .files
            .read_to_string(&self.config_path)
            .map_err(|error| {
                RepositoryError::new(format!(
                    "failed to read AppSandbox VM configuration {}: {error}",
                    self.config_path.display()
                ))
            })?;
        let parsed = parse_vms_cfg(&input)?;
        let private_key = self.resolve_private_key()?;
        let mut drafts = Vec::with_capacity(parsed.len());

        for vm in parsed {
            let expected_disk = self.appsandbox_root.join(vm.name()).join("disk.vhdx");
            let source_disk = self.resolve_source_disk(vm.vhdx_path(), &expected_disk)?;
            let source_id = (self.source_id_factory)(&source_disk.canonical_path, vm.ordinal())?;
            let (network_mode, network_supported) = network_mode(vm.network_mode());
            let (gpu_mode, gpu_supported) = gpu_mode(vm.gpu_mode());
            let mut incompatibilities = Vec::new();

            if vm.os_type().eq_ignore_ascii_case("Template") {
                incompatibilities.push(AppSandboxIncompatibility::Template);
            } else if !vm.os_type().eq_ignore_ascii_case("Linux") {
                incompatibilities.push(AppSandboxIncompatibility::NotLinux);
            }
            if vm.install_complete() != 1 {
                incompatibilities.push(AppSandboxIncompatibility::InstallationIncomplete);
            }
            if vm.ssh_enabled() != 1 {
                incompatibilities.push(AppSandboxIncompatibility::SshDisabled);
            }
            if vm.ssh_deploy_key() != 1 || private_key.is_none() {
                incompatibilities.push(AppSandboxIncompatibility::SshKeyNotDeployed);
            }
            if !source_disk.exists {
                incompatibilities.push(AppSandboxIncompatibility::SourceDiskMissing);
            } else if !source_disk.matches_expected {
                incompatibilities.push(AppSandboxIncompatibility::SourceDiskMismatch);
            }
            if !network_supported {
                incompatibilities.push(AppSandboxIncompatibility::UnsupportedNetworkMode);
            }
            if !gpu_supported {
                incompatibilities.push(AppSandboxIncompatibility::UnsupportedGpuMode);
            }
            if vm.ssh_port() == 0 {
                incompatibilities.push(AppSandboxIncompatibility::InvalidSshPort);
            }

            let validated = match private_key.clone() {
                Some(private_key) if incompatibilities.is_empty() => Some(ValidatedSource {
                    config_path: self.config_path.clone(),
                    vm_ordinal: vm.ordinal(),
                    source_disk: source_disk.canonical_path,
                    private_key,
                }),
                _ => None,
            };
            drafts.push(DraftCandidate {
                candidate: AppSandboxVmCandidate {
                    source_id,
                    name: vm.name().to_owned(),
                    ram_mb: vm.ram_mb(),
                    disk_gb: vm.hdd_gb(),
                    cpu_cores: vm.cpu_cores(),
                    network_mode,
                    gpu_mode,
                    ssh_user: vm.admin_user().to_owned(),
                    ssh_port: vm.ssh_port(),
                    compatibility: AppSandboxCompatibility::Compatible,
                },
                incompatibilities,
                validated,
            });
        }

        mark_duplicate_sources(&mut drafts);

        let mut candidates = Vec::with_capacity(drafts.len());
        let mut sources = HashMap::with_capacity(drafts.len());
        for mut draft in drafts {
            if draft.incompatibilities.is_empty() {
                if let Some(source) = draft.validated {
                    sources.insert(draft.candidate.source_id.clone(), source);
                }
            } else {
                draft.candidate.compatibility =
                    AppSandboxCompatibility::Incompatible(draft.incompatibilities);
            }
            candidates.push(draft.candidate);
        }

        Ok(DiscoveryResult {
            candidates,
            sources,
        })
    }

    fn resolve_private_key(&self) -> Result<Option<PathBuf>, RepositoryError> {
        if !self.files.is_file(&self.private_key_path) {
            return Ok(None);
        }
        self.files
            .canonicalize(&self.private_key_path)
            .map(Some)
            .map_err(|error| {
                RepositoryError::new(format!(
                    "failed to resolve the AppSandbox private key: {error}"
                ))
            })
    }

    fn resolve_source_disk(
        &self,
        configured: &Path,
        expected: &Path,
    ) -> Result<SourceDisk, RepositoryError> {
        if !self.files.is_file(configured) {
            return Ok(SourceDisk {
                exists: false,
                matches_expected: paths_equal(configured, expected),
                canonical_path: self.canonicalize_allow_missing(configured)?,
            });
        }

        let canonical = self.files.canonicalize(configured).map_err(|error| {
            RepositoryError::new(format!(
                "failed to resolve AppSandbox source disk {}: {error}",
                configured.display()
            ))
        })?;
        let matches_expected = if paths_equal(configured, expected) {
            true
        } else if self.files.is_file(expected) {
            let canonical_expected = self.files.canonicalize(expected).map_err(|error| {
                RepositoryError::new(format!(
                    "failed to resolve expected AppSandbox source disk {}: {error}",
                    expected.display()
                ))
            })?;
            paths_equal(&canonical, &canonical_expected)
        } else {
            false
        };

        Ok(SourceDisk {
            exists: true,
            matches_expected,
            canonical_path: canonical,
        })
    }

    /// Resolves an absent leaf through the nearest ancestor the filesystem can
    /// canonicalize. Discovery still needs a stable identity to show the
    /// incompatible record, but it never puts that record in the validated
    /// source snapshot.
    fn canonicalize_allow_missing(&self, path: &Path) -> Result<PathBuf, RepositoryError> {
        let mut ancestor = path.to_path_buf();
        let mut missing = Vec::new();
        loop {
            match self.files.canonicalize(&ancestor) {
                Ok(mut canonical) => {
                    for component in missing.iter().rev() {
                        canonical.push(component);
                    }
                    return Ok(canonical);
                }
                Err(error) => {
                    let Some(component) = ancestor.file_name().map(ToOwned::to_owned) else {
                        return Err(RepositoryError::new(format!(
                            "failed to resolve AppSandbox source disk {}: {error}",
                            path.display()
                        )));
                    };
                    missing.push(component);
                    if !ancestor.pop() {
                        return Err(RepositoryError::new(format!(
                            "failed to resolve AppSandbox source disk {}: {error}",
                            path.display()
                        )));
                    }
                }
            }
        }
    }
}

struct SourceDisk {
    exists: bool,
    matches_expected: bool,
    canonical_path: PathBuf,
}

struct DraftCandidate {
    candidate: AppSandboxVmCandidate,
    incompatibilities: Vec<AppSandboxIncompatibility>,
    validated: Option<ValidatedSource>,
}

fn mark_duplicate_sources(drafts: &mut [DraftCandidate]) {
    let mut counts = HashMap::new();
    for draft in drafts.iter() {
        *counts
            .entry(draft.candidate.source_id.clone())
            .or_insert(0_usize) += 1;
    }
    for draft in drafts {
        if counts[&draft.candidate.source_id] > 1 {
            draft
                .incompatibilities
                .push(AppSandboxIncompatibility::DuplicateSource);
            draft.validated = None;
        }
    }
}

fn network_mode(raw: u32) -> (NetworkMode, bool) {
    if raw == 1 {
        (NetworkMode::Nat, true)
    } else {
        (NetworkMode::Unknown(unknown_mode(raw)), false)
    }
}

fn gpu_mode(raw: u32) -> (GpuMode, bool) {
    if raw == 1 {
        (GpuMode::Default, true)
    } else {
        (GpuMode::Unknown(unknown_mode(raw)), false)
    }
}

fn unknown_mode(raw: u32) -> i32 {
    i32::try_from(raw).unwrap_or(i32::MAX)
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

fn stable_source_id(
    canonical_disk: &Path,
    vm_ordinal: usize,
) -> Result<AppSandboxSourceId, RepositoryError> {
    // FNV-1a is intentionally written out: unlike `DefaultHasher`, its output
    // is specified and remains stable across Rust releases. This identity is
    // opaque, not a security digest; collisions are rejected above.
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in canonical_disk
        .to_string_lossy()
        .to_ascii_lowercase()
        .bytes()
        .chain([0])
        .chain(vm_ordinal.to_le_bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    AppSandboxSourceId::from_stable_hash(format!("{hash:016x}"))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        io,
        path::{Path, PathBuf},
        sync::Arc,
    };

    use vmlord_core::{
        AppSandboxCompatibility, AppSandboxIncompatibility, AppSandboxSourceId, GpuMode,
        NetworkMode,
    };

    use super::{Discovery, FileSystem};

    // Forward slashes keep the injected tests portable while still naming a
    // valid absolute Windows path in the production target.
    const ROOT: &str = "Z:/ProgramData/AppSandbox";
    const DISK: &str = "Z:/ProgramData/AppSandbox/ubuntu/disk.vhdx";
    const KEY: &str = "Z:/ProgramData/AppSandbox/ssh/id_appsandbox";

    #[derive(Default)]
    struct FakeFileSystem {
        config: String,
        canonical: HashMap<PathBuf, PathBuf>,
        files: Vec<PathBuf>,
    }

    impl FakeFileSystem {
        fn with_config(config: impl Into<String>) -> Self {
            let mut canonical = HashMap::new();
            canonical.insert(PathBuf::from(ROOT), PathBuf::from(ROOT));
            Self {
                config: config.into(),
                canonical,
                ..Self::default()
            }
        }

        fn file(mut self, path: impl Into<PathBuf>, canonical: impl Into<PathBuf>) -> Self {
            let path = path.into();
            self.files.push(path.clone());
            self.canonical.insert(path, canonical.into());
            self
        }

        fn canonical_path(
            mut self,
            path: impl Into<PathBuf>,
            canonical: impl Into<PathBuf>,
        ) -> Self {
            self.canonical.insert(path.into(), canonical.into());
            self
        }
    }

    impl FileSystem for FakeFileSystem {
        fn read_to_string(&self, _path: &Path) -> io::Result<String> {
            Ok(self.config.clone())
        }

        fn is_file(&self, path: &Path) -> bool {
            self.files.iter().any(|file| file == path)
        }

        fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
            self.canonical.get(path).cloned().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("{} is absent", path.display()),
                )
            })
        }
    }

    fn linux_config() -> String {
        [
            "[VM]",
            "Name=ubuntu",
            "OsType=Linux",
            "RamMB=4096",
            "CpuCores=4",
            "HddGB=64",
            "NetworkMode=1",
            "GpuMode=1",
            "AdminUser=ubuntu",
            "SshEnabled=1",
            "SshPort=22",
            "SshDeployKey=1",
            "InstallComplete=1",
            &format!("VhdxPath={DISK}"),
        ]
        .join("\n")
    }

    fn available_files(config: impl Into<String>) -> FakeFileSystem {
        FakeFileSystem::with_config(config)
            .file(DISK, DISK)
            .file(KEY, KEY)
    }

    fn discovery_with(files: FakeFileSystem) -> Discovery {
        Discovery::with_file_system(PathBuf::from(ROOT), Arc::new(files))
    }

    fn reasons(discovery: Discovery) -> Vec<AppSandboxIncompatibility> {
        let result = discovery.discover().expect("the configuration parses");
        assert!(
            result.sources.is_empty(),
            "an incompatible candidate must not resolve to a validated source"
        );
        match &result.candidates[0].compatibility {
            AppSandboxCompatibility::Compatible => panic!("candidate was unexpectedly compatible"),
            AppSandboxCompatibility::Incompatible(reasons) => reasons.clone(),
        }
    }

    #[test]
    fn completed_linux_vm_with_available_source_is_compatible() {
        let result = discovery_with(available_files(linux_config()))
            .discover()
            .expect("the VM is discoverable");

        assert_eq!(result.candidates.len(), 1);
        let candidate = &result.candidates[0];
        assert_eq!(candidate.name, "ubuntu");
        assert_eq!(candidate.ram_mb, 4096);
        assert_eq!(candidate.disk_gb, 64);
        assert_eq!(candidate.cpu_cores, 4);
        assert_eq!(candidate.network_mode, NetworkMode::Nat);
        assert_eq!(candidate.gpu_mode, GpuMode::Default);
        assert_eq!(candidate.ssh_user, "ubuntu");
        assert_eq!(candidate.ssh_port, 22);
        assert_eq!(candidate.compatibility, AppSandboxCompatibility::Compatible);
        assert!(result.sources.contains_key(&candidate.source_id));
    }

    #[test]
    fn windows_vm_is_visible_with_an_incompatibility_reason() {
        let config = linux_config().replace("OsType=Linux", "OsType=Windows");

        assert_eq!(
            reasons(discovery_with(available_files(config))),
            [AppSandboxIncompatibility::NotLinux]
        );
    }

    #[test]
    fn incomplete_install_is_reported() {
        let config = linux_config().replace("InstallComplete=1", "InstallComplete=0");

        assert_eq!(
            reasons(discovery_with(available_files(config))),
            [AppSandboxIncompatibility::InstallationIncomplete]
        );
    }

    #[test]
    fn disabled_ssh_is_reported() {
        let config = linux_config().replace("SshEnabled=1", "SshEnabled=0");

        assert_eq!(
            reasons(discovery_with(available_files(config))),
            [AppSandboxIncompatibility::SshDisabled]
        );
    }

    #[test]
    fn undeployed_ssh_key_is_reported() {
        let config = linux_config().replace("SshDeployKey=1", "SshDeployKey=0");

        assert_eq!(
            reasons(discovery_with(available_files(config))),
            [AppSandboxIncompatibility::SshKeyNotDeployed]
        );
    }

    #[test]
    fn unavailable_private_key_is_reported_as_not_deployed() {
        let files = FakeFileSystem::with_config(linux_config()).file(DISK, DISK);

        assert_eq!(
            reasons(discovery_with(files)),
            [AppSandboxIncompatibility::SshKeyNotDeployed]
        );
    }

    #[test]
    fn missing_source_disk_is_reported() {
        let files = FakeFileSystem::with_config(linux_config()).file(KEY, KEY);

        assert_eq!(
            reasons(discovery_with(files)),
            [AppSandboxIncompatibility::SourceDiskMissing]
        );
    }

    #[test]
    fn missing_disk_id_uses_its_canonical_path_and_section_ordinal() {
        let alias = "Z:/ProgramData/AppSandbox/ubuntu/./disk.vhdx";
        let first_files = FakeFileSystem::with_config(linux_config())
            .canonical_path(DISK, DISK)
            .file(KEY, KEY);
        let alias_files = FakeFileSystem::with_config(linux_config().replace(DISK, alias))
            .canonical_path(alias, DISK)
            .file(KEY, KEY);

        let first = discovery_with(first_files).discover().unwrap();
        let aliased = discovery_with(alias_files).discover().unwrap();

        assert_eq!(
            first.candidates[0].source_id,
            aliased.candidates[0].source_id
        );
    }

    #[test]
    fn disk_outside_the_named_vm_directory_is_reported() {
        let other = "Z:/ProgramData/AppSandbox/other/disk.vhdx";
        let config = linux_config().replace(DISK, other);
        let files = FakeFileSystem::with_config(config)
            .file(other, other)
            .file(DISK, DISK)
            .file(KEY, KEY);

        assert_eq!(
            reasons(discovery_with(files)),
            [AppSandboxIncompatibility::SourceDiskMismatch]
        );
    }

    #[test]
    fn unsupported_network_mode_is_preserved_and_reported() {
        let config = linux_config().replace("NetworkMode=1", "NetworkMode=17");
        let result = discovery_with(available_files(config)).discover().unwrap();

        assert_eq!(result.candidates[0].network_mode, NetworkMode::Unknown(17));
        assert_eq!(
            result.candidates[0].compatibility,
            AppSandboxCompatibility::Incompatible(vec![
                AppSandboxIncompatibility::UnsupportedNetworkMode,
            ])
        );
    }

    #[test]
    fn unsupported_gpu_mode_is_preserved_and_reported() {
        let config = linux_config().replace("GpuMode=1", "GpuMode=23");
        let result = discovery_with(available_files(config)).discover().unwrap();

        assert_eq!(result.candidates[0].gpu_mode, GpuMode::Unknown(23));
        assert_eq!(
            result.candidates[0].compatibility,
            AppSandboxCompatibility::Incompatible(vec![
                AppSandboxIncompatibility::UnsupportedGpuMode,
            ])
        );
    }

    #[test]
    fn zero_ssh_port_is_reported() {
        let config = linux_config().replace("SshPort=22", "SshPort=0");

        assert_eq!(
            reasons(discovery_with(available_files(config))),
            [AppSandboxIncompatibility::InvalidSshPort]
        );
    }

    #[test]
    fn all_observed_incompatibilities_are_returned_together() {
        let config = linux_config()
            .replace("OsType=Linux", "OsType=Windows")
            .replace("InstallComplete=1", "InstallComplete=0")
            .replace("SshEnabled=1", "SshEnabled=0")
            .replace("SshDeployKey=1", "SshDeployKey=0")
            .replace("SshPort=22", "SshPort=0")
            .replace("NetworkMode=1", "NetworkMode=17")
            .replace("GpuMode=1", "GpuMode=23");
        let files = FakeFileSystem::with_config(config).file(KEY, KEY);

        assert_eq!(
            reasons(discovery_with(files)),
            [
                AppSandboxIncompatibility::NotLinux,
                AppSandboxIncompatibility::InstallationIncomplete,
                AppSandboxIncompatibility::SshDisabled,
                AppSandboxIncompatibility::SshKeyNotDeployed,
                AppSandboxIncompatibility::SourceDiskMissing,
                AppSandboxIncompatibility::UnsupportedNetworkMode,
                AppSandboxIncompatibility::UnsupportedGpuMode,
                AppSandboxIncompatibility::InvalidSshPort,
            ]
        );
    }

    #[test]
    fn source_id_is_stable_for_a_canonical_disk_and_ordinal() {
        let alias = "Z:/ProgramData/AppSandbox/ubuntu/./disk.vhdx";
        let first = discovery_with(available_files(linux_config()))
            .discover()
            .unwrap();
        let aliased = linux_config().replace(DISK, alias);
        let files = FakeFileSystem::with_config(aliased)
            .file(alias, DISK)
            .file(DISK, DISK)
            .file(KEY, KEY);
        let second = discovery_with(files).discover().unwrap();

        assert_eq!(
            first.candidates[0].source_id,
            second.candidates[0].source_id
        );
    }

    #[test]
    fn vm_section_ordinal_distinguishes_two_records_for_the_same_disk() {
        let files = available_files(format!("{}\n{}", linux_config(), linux_config()));

        let result = discovery_with(files).discover().unwrap();

        assert_eq!(result.candidates.len(), 2);
        assert_ne!(
            result.candidates[0].source_id,
            result.candidates[1].source_id
        );
        assert_eq!(result.sources.len(), 2);
    }

    #[test]
    fn duplicate_source_ids_make_every_colliding_candidate_incompatible() {
        let second_disk = "Z:/ProgramData/AppSandbox/fedora/disk.vhdx";
        let second = linux_config()
            .replace("Name=ubuntu", "Name=fedora")
            .replace(DISK, second_disk);
        let files = FakeFileSystem::with_config(format!("{}\n{}", linux_config(), second))
            .file(DISK, DISK)
            .file(second_disk, second_disk)
            .file(KEY, KEY);
        let duplicate = AppSandboxSourceId::from_stable_hash("collision").unwrap();
        let discovery = discovery_with(files)
            .with_source_id_factory(Arc::new(move |_, _| Ok(duplicate.clone())));

        let result = discovery.discover().unwrap();

        assert_eq!(result.candidates.len(), 2);
        for candidate in &result.candidates {
            assert_eq!(
                candidate.compatibility,
                AppSandboxCompatibility::Incompatible(vec![
                    AppSandboxIncompatibility::DuplicateSource,
                ])
            );
        }
        assert!(result.sources.is_empty());
    }
}
