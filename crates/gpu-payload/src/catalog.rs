use crate::{PayloadError, Sha256Digest};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, ffi::OsStr, fs, path::Path};
const ENTRY_SCHEMA_VERSION: u32 = 2;
const PAYLOAD_ABI_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct GuestTarget {
    pub distribution: String,
    pub release: String,
    pub architecture: String,
    pub kernel_release: String,
    pub payload_abi: u32,
}
/// A guest as the host knows it before that guest has booted.
///
/// Three fields and not four: `kernel_release` is a property of a running
/// kernel, and the host chooses a payload before there is one. The guest
/// checks applicability itself and DKMS rebuilds the module for whatever
/// kernel it runs, so the catalog's kernel records what a payload was proven
/// on rather than what it requires.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GuestSelector<'a> {
    pub distribution: &'a str,
    pub release: &'a str,
    pub architecture: &'a str,
}
impl GuestTarget {
    pub fn ubuntu_26_04_amd64(kernel_release: impl Into<String>) -> Self {
        Self {
            distribution: "ubuntu".into(),
            release: "26.04".into(),
            architecture: "amd64".into(),
            kernel_release: kernel_release.into(),
            payload_abi: 1,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RendererCapability {
    D3d12Gallium,
    DznVulkan,
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MesaPolicy {
    Distro,
    Bundled,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Source {
    pub url: String,
    pub commit: String,
    pub version: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct License {
    pub spdx: String,
    pub path: String,
}
#[derive(Clone, Debug, Deserialize)]
struct CatalogEntryDocument {
    schema_version: u32,
    payload_id: String,
    target: GuestTarget,
    expanded_size_limit: u64,
    file_count_limit: u64,
    archive_sha256: Sha256Digest,
    payload_manifest_sha256: Sha256Digest,
    required_renderers: Vec<RendererCapability>,
    mesa_policy: MesaPolicy,
    sources: Vec<Source>,
    licenses: Vec<License>,
}
/// A catalog entry that has passed all catalog validation.
///
/// Entries cannot be deserialized directly because doing so would bypass
/// [`CatalogEntry::from_json`] and its validation boundary.
///
/// ```compile_fail
/// let _: vmlord_gpu_payload::CatalogEntry = serde_json::from_str("{}").unwrap();
/// ```
#[derive(Clone, Debug, Serialize)]
pub struct CatalogEntry {
    payload_id: String,
    target: GuestTarget,
    expanded_size_limit: u64,
    file_count_limit: u64,
    archive_sha256: Sha256Digest,
    payload_manifest_sha256: Sha256Digest,
    required_renderers: Vec<RendererCapability>,
    mesa_policy: MesaPolicy,
    sources: Vec<Source>,
    licenses: Vec<License>,
}
impl From<CatalogEntryDocument> for CatalogEntry {
    fn from(value: CatalogEntryDocument) -> Self {
        Self {
            payload_id: value.payload_id,
            target: value.target,
            expanded_size_limit: value.expanded_size_limit,
            file_count_limit: value.file_count_limit,
            archive_sha256: value.archive_sha256,
            payload_manifest_sha256: value.payload_manifest_sha256,
            required_renderers: value.required_renderers,
            mesa_policy: value.mesa_policy,
            sources: value.sources,
            licenses: value.licenses,
        }
    }
}
impl CatalogEntry {
    fn validate(&self) -> Result<(), PayloadError> {
        if self.payload_id.is_empty()
            || self.target.distribution.is_empty()
            || self.target.release.is_empty()
            || self.target.architecture.is_empty()
            || self.target.kernel_release.is_empty()
            || self.target.payload_abi != PAYLOAD_ABI_VERSION
            || self.expanded_size_limit == 0
            || self.file_count_limit == 0
            || self.required_renderers.is_empty()
        {
            return Err(PayloadError::InvalidCatalog(
                "missing or invalid required catalog field".into(),
            ));
        }
        if self.sources.is_empty()
            || self.sources.iter().any(|source| {
                source.url.is_empty()
                    || source.version.is_empty()
                    || source.commit.len() != 40
                    || !source.commit.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        {
            return Err(PayloadError::InvalidCatalog(
                "sources must carry immutable commits and provenance".into(),
            ));
        }
        if self
            .licenses
            .iter()
            .any(|license| license.spdx.is_empty() || license.path.is_empty())
            || self.licenses.is_empty()
        {
            return Err(PayloadError::InvalidCatalog(
                "licenses must carry SPDX expressions and paths".into(),
            ));
        }
        Ok(())
    }
    pub fn payload_id(&self) -> &str {
        &self.payload_id
    }
    pub fn target(&self) -> &GuestTarget {
        &self.target
    }
    pub fn expanded_size_limit(&self) -> u64 {
        self.expanded_size_limit
    }
    pub fn file_count_limit(&self) -> u64 {
        self.file_count_limit
    }
    pub fn archive_sha256(&self) -> &Sha256Digest {
        &self.archive_sha256
    }
    pub fn payload_manifest_sha256(&self) -> &Sha256Digest {
        &self.payload_manifest_sha256
    }
    pub fn required_renderers(&self) -> &[RendererCapability] {
        &self.required_renderers
    }
    pub fn mesa_policy(&self) -> &MesaPolicy {
        &self.mesa_policy
    }
    pub fn sources(&self) -> &[Source] {
        &self.sources
    }
    pub fn licenses(&self) -> &[License] {
        &self.licenses
    }
}
pub struct PayloadCatalog {
    entries: Vec<CatalogEntry>,
}
impl CatalogEntry {
    /// Reads one entry document, as `cargo xtask gpu-payload pack` writes it
    /// and as a release carries it beside its archive.
    ///
    /// The entry is a release artifact rather than a build artifact waiting to
    /// be pasted into a larger document, so it carries a schema version of its
    /// own instead of borrowing a catalog's.
    pub fn from_json(bytes: &[u8]) -> Result<Self, PayloadError> {
        let document: CatalogEntryDocument = serde_json::from_slice(bytes)
            .map_err(|error| PayloadError::InvalidCatalog(error.to_string()))?;
        if document.schema_version != ENTRY_SCHEMA_VERSION {
            return Err(PayloadError::InvalidCatalog(
                "unknown catalog entry schema version".into(),
            ));
        }
        let entry = Self::from(document);
        entry.validate()?;
        Ok(entry)
    }
}
impl PayloadCatalog {
    /// The catalog a release carries beside its executable.
    ///
    /// `directory` is the one holding the executable; naming its `gpu-payload`
    /// child is `release.rs`'s job. A child that is not there, cannot be
    /// listed, or holds no entry is an empty catalog rather than an error: a
    /// build without a payload is a build without GPU support, and GPU support
    /// is best effort.
    ///
    /// A file that *is* there and is wrong fails the whole catalog. That is a
    /// broken release, and a silent absence is the worst way to learn of one.
    /// An archive nothing claims is ignored, because failing over a leftover
    /// file would be a rule worse than the problem.
    pub fn from_release_directory(directory: &Path) -> Result<Self, PayloadError> {
        let payloads = crate::release::local_payload_directory(directory);
        let Ok(listing) = fs::read_dir(&payloads) else {
            return Self::from_entries(Vec::new());
        };
        let mut entries = Vec::new();
        for item in listing {
            let Ok(item) = item else {
                continue;
            };
            let path = item.path();
            if path.extension().and_then(OsStr::to_str) != Some("json") {
                continue;
            }
            let bytes = fs::read(&path)
                .map_err(|error| PayloadError::io("read GPU payload entry", path.clone(), error))?;
            let entry = CatalogEntry::from_json(&bytes)?;
            if path.file_stem().and_then(OsStr::to_str) != Some(entry.payload_id()) {
                return Err(PayloadError::InvalidCatalog(format!(
                    "{} does not name its payload ID {}",
                    path.display(),
                    entry.payload_id()
                )));
            }
            let archive = crate::local_archive_path(directory, entry.payload_id());
            if !archive.is_file() {
                return Err(PayloadError::InvalidCatalog(format!(
                    "payload {} has no archive at {}",
                    entry.payload_id(),
                    archive.display()
                )));
            }
            entries.push(entry);
        }
        Self::from_entries(entries)
    }
    /// The catalog a set of read entries forms.
    ///
    /// Uniqueness is checked once, here: two entries for one guest would make
    /// selection depend on the order a directory happened to list.
    fn from_entries(entries: Vec<CatalogEntry>) -> Result<Self, PayloadError> {
        let mut ids = HashSet::new();
        let mut targets = HashSet::new();
        for entry in &entries {
            if !ids.insert(entry.payload_id.clone()) || !targets.insert(entry.target.clone()) {
                return Err(PayloadError::InvalidCatalog(
                    "duplicate payload ID or target".into(),
                ));
            }
        }
        Ok(Self { entries })
    }
    pub fn entries(&self) -> &[CatalogEntry] {
        &self.entries
    }
    pub fn select(&self, target: &GuestTarget) -> Result<&CatalogEntry, PayloadError> {
        self.entries
            .iter()
            .find(|entry| entry.target == *target)
            .ok_or_else(|| PayloadError::UnsupportedTarget(target.clone()))
    }
    /// The entry for a guest, ignoring the kernel that guest runs.
    ///
    /// When a triple has several entries the newest proven kernel wins: it was
    /// built against the most recent headers, and an older one buys nothing.
    pub fn select_for_guest(
        &self,
        guest: &GuestSelector<'_>,
    ) -> Result<&CatalogEntry, PayloadError> {
        self.entries
            .iter()
            .filter(|entry| {
                entry
                    .target
                    .distribution
                    .eq_ignore_ascii_case(guest.distribution)
                    && entry.target.release == guest.release
                    && entry
                        .target
                        .architecture
                        .eq_ignore_ascii_case(guest.architecture)
            })
            .max_by_key(|entry| kernel_order(&entry.target.kernel_release))
            .ok_or_else(|| PayloadError::NoPayloadForGuest {
                distribution: guest.distribution.to_owned(),
                release: guest.release.to_owned(),
                architecture: guest.architecture.to_owned(),
            })
    }
}
/// A kernel release as numbers, so that 14 sorts above 9.
///
/// Every run of digits in order and nothing else: `7.0.0-14-generic` and
/// `7.0.0-14-lowlatency` are one kernel in two flavours, and a flavour must
/// not decide which payload is newer.
fn kernel_order(release: &str) -> Vec<u64> {
    release
        .split(|character: char| !character.is_ascii_digit())
        .filter_map(|part| part.parse().ok())
        .collect()
}

/// One entry as a test writes it: the schema version is this module's to know,
/// so a test states only what it is testing.
#[cfg(test)]
pub(crate) fn test_entry(mut value: serde_json::Value) -> CatalogEntry {
    value["schema_version"] = ENTRY_SCHEMA_VERSION.into();
    CatalogEntry::from_json(&serde_json::to_vec(&value).unwrap())
        .expect("the test entry must be a valid entry document")
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::kernel_order;
    use crate::{CatalogEntry, GuestSelector, GuestTarget, PayloadCatalog, PayloadError};

    static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

    struct TemporaryDirectory(PathBuf);

    impl TemporaryDirectory {
        fn new(label: &str) -> Self {
            let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "vmlord-gpu-payload-catalog-{label}-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TemporaryDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Writes the pair a release carries for one payload.
    fn write_pair(directory: &Path, payload_id: &str, entry: &str) {
        let payloads = directory.join("gpu-payload");
        fs::create_dir_all(&payloads).unwrap();
        fs::write(payloads.join(format!("{payload_id}.json")), entry).unwrap();
        fs::write(payloads.join(format!("{payload_id}.zip")), b"archive").unwrap();
    }
    const Z: &str = "0000000000000000000000000000000000000000000000000000000000000000";
    const C: &str = "14794180686c2fb6307fbe359c359bec765249f3";
    fn catalog() -> String {
        format!(
            r#"{{"schema_version":2,"payload_id":"ubuntu-26.04-amd64-7.0.0-14-v1","target":{{"distribution":"ubuntu","release":"26.04","architecture":"amd64","kernel_release":"7.0.0-14-generic","payload_abi":1}},"expanded_size_limit":2,"file_count_limit":3,"archive_sha256":"{Z}","payload_manifest_sha256":"{Z}","required_renderers":["d3d12-gallium","dzn-vulkan"],"mesa_policy":"bundled","sources":[{{"url":"https://github.com/microsoft/WSL2-Linux-Kernel","commit":"{C}","version":"1"}}],"licenses":[{{"spdx":"GPL-2.0","path":"licenses/GPL-2.0.txt"}}]}}"#
        )
    }
    #[test]
    fn a_catalog_selects_only_the_exact_kernel_tuple() {
        let c = catalog_with(&[catalog()]);
        assert_eq!(
            c.select(&GuestTarget::ubuntu_26_04_amd64("7.0.0-14-generic"))
                .unwrap()
                .payload_id(),
            "ubuntu-26.04-amd64-7.0.0-14-v1"
        );
        assert!(matches!(
            c.select(&GuestTarget::ubuntu_26_04_amd64("other")),
            Err(PayloadError::UnsupportedTarget(_))
        ));
    }
    fn entry_json(distribution: &str, release: &str, architecture: &str, kernel: &str) -> String {
        format!(
            r#"{{"schema_version":2,"payload_id":"{distribution}-{release}-{architecture}-{kernel}","target":{{"distribution":"{distribution}","release":"{release}","architecture":"{architecture}","kernel_release":"{kernel}","payload_abi":1}},"expanded_size_limit":2,"file_count_limit":3,"archive_sha256":"{Z}","payload_manifest_sha256":"{Z}","required_renderers":["d3d12-gallium"],"mesa_policy":"bundled","sources":[{{"url":"https://github.com/microsoft/WSL2-Linux-Kernel","commit":"{C}","version":"1"}}],"licenses":[{{"spdx":"GPL-2.0","path":"licenses/GPL-2.0.txt"}}]}}"#
        )
    }
    fn catalog_with(entries: &[String]) -> PayloadCatalog {
        PayloadCatalog::from_entries(
            entries
                .iter()
                .map(|entry| {
                    CatalogEntry::from_json(entry.as_bytes()).expect("the entry must parse")
                })
                .collect(),
        )
        .expect("the catalog must be well formed")
    }
    fn ubuntu_2604() -> GuestSelector<'static> {
        GuestSelector {
            distribution: "ubuntu",
            release: "26.04",
            architecture: "amd64",
        }
    }
    #[test]
    fn a_guest_selects_an_entry_whatever_kernel_it_runs() {
        let catalog = catalog_with(&[entry_json("ubuntu", "26.04", "amd64", "7.0.0-14-generic")]);
        assert_eq!(
            catalog
                .select_for_guest(&ubuntu_2604())
                .expect("the triple matches, so the kernel must not decide")
                .target()
                .kernel_release,
            "7.0.0-14-generic"
        );
    }
    #[test]
    fn the_newest_proven_kernel_wins_when_a_triple_has_several_entries() {
        let catalog = catalog_with(&[
            entry_json("ubuntu", "26.04", "amd64", "7.0.0-9-generic"),
            entry_json("ubuntu", "26.04", "amd64", "7.0.0-14-generic"),
        ]);
        assert_eq!(
            catalog
                .select_for_guest(&ubuntu_2604())
                .expect("one of the two entries must be chosen")
                .target()
                .kernel_release,
            "7.0.0-14-generic",
            "14 is newer than 9, which sorting the text would get wrong"
        );
    }
    #[test]
    fn a_guest_with_no_entry_is_told_which_guest_had_none() {
        let catalog = catalog_with(&[entry_json("ubuntu", "26.04", "amd64", "7.0.0-14-generic")]);
        let error = catalog
            .select_for_guest(&GuestSelector {
                release: "24.04",
                ..ubuntu_2604()
            })
            .expect_err("no entry matches this release");
        assert!(
            error.to_string().contains("24.04"),
            "the error has to name the guest it found nothing for: {error}"
        );
    }
    #[test]
    fn an_empty_catalog_has_nothing_for_anyone() {
        assert!(
            catalog_with(&[]).select_for_guest(&ubuntu_2604()).is_err(),
            "the shipped catalog is empty today, so this is the ordinary answer"
        );
    }
    #[test]
    fn kernel_order_reads_the_numbers_and_not_the_text() {
        assert!(kernel_order("7.0.0-14-generic") > kernel_order("7.0.0-9-generic"));
        assert!(kernel_order("7.1.0-1-generic") > kernel_order("7.0.0-99-generic"));
        assert_eq!(
            kernel_order("7.0.0-14-generic"),
            kernel_order("7.0.0-14-lowlatency"),
            "a flavour is not a newer kernel"
        );
    }
    #[test]
    fn target_dimensions_must_all_be_non_empty() {
        for dimension in ["distribution", "release", "architecture", "kernel_release"] {
            let mut document: serde_json::Value = serde_json::from_str(&catalog()).unwrap();
            document["target"][dimension] = "".into();
            assert!(
                matches!(
                    CatalogEntry::from_json(&serde_json::to_vec(&document).unwrap()),
                    Err(PayloadError::InvalidCatalog(_))
                ),
                "accepted an empty {dimension} target dimension"
            );
        }
    }
    #[test]
    fn a_catalog_entry_requires_at_least_one_source() {
        let empty_sources=catalog().replace(&format!(r#"[{{"url":"https://github.com/microsoft/WSL2-Linux-Kernel","commit":"{C}","version":"1"}}]"#), "[]");
        assert!(matches!(
            CatalogEntry::from_json(empty_sources.as_bytes()),
            Err(PayloadError::InvalidCatalog(_))
        ));
    }

    #[test]
    fn an_entry_at_another_schema_version_is_refused() {
        let mut document: serde_json::Value = serde_json::from_str(&catalog()).unwrap();
        document["schema_version"] = 1.into();

        assert!(matches!(
            CatalogEntry::from_json(&serde_json::to_vec(&document).unwrap()),
            Err(PayloadError::InvalidCatalog(_))
        ));
    }

    #[test]
    fn a_release_directory_is_read_as_the_catalog_it_holds() {
        let temporary = TemporaryDirectory::new("pair");
        write_pair(
            temporary.path(),
            "ubuntu-26.04-amd64-7.0.0-14-generic",
            &entry_json("ubuntu", "26.04", "amd64", "7.0.0-14-generic"),
        );

        let catalog = PayloadCatalog::from_release_directory(temporary.path())
            .expect("a valid pair must be read");

        assert_eq!(
            catalog
                .select_for_guest(&ubuntu_2604())
                .expect("the entry is for this guest")
                .payload_id(),
            "ubuntu-26.04-amd64-7.0.0-14-generic"
        );
    }

    #[test]
    fn a_build_that_ships_no_payload_has_an_empty_catalog_and_not_an_error() {
        let temporary = TemporaryDirectory::new("empty");
        // Three shapes of nothing: no directory at all, an empty one, and one
        // holding an archive no entry claims.
        let absent = PayloadCatalog::from_release_directory(&temporary.path().join("absent"));
        fs::create_dir(temporary.path().join("gpu-payload")).unwrap();
        let empty = PayloadCatalog::from_release_directory(temporary.path());
        fs::write(
            temporary.path().join("gpu-payload").join("stray.zip"),
            b"archive",
        )
        .unwrap();
        let stray = PayloadCatalog::from_release_directory(temporary.path());

        for catalog in [absent, empty, stray] {
            let catalog = catalog.expect("a release without a payload is a release without GPU");
            assert!(catalog.entries().is_empty());
            assert!(matches!(
                catalog.select_for_guest(&ubuntu_2604()),
                Err(PayloadError::NoPayloadForGuest { .. })
            ));
        }
    }

    #[test]
    fn an_entry_file_that_is_there_and_wrong_fails_the_catalog() {
        let valid = entry_json("ubuntu", "26.04", "amd64", "7.0.0-14-generic");

        let unreadable = TemporaryDirectory::new("broken-json");
        write_pair(unreadable.path(), "a", "{not json");
        assert!(PayloadCatalog::from_release_directory(unreadable.path()).is_err());

        let misnamed = TemporaryDirectory::new("broken-name");
        write_pair(misnamed.path(), "wrong-name", &valid);
        assert!(PayloadCatalog::from_release_directory(misnamed.path()).is_err());

        let archiveless = TemporaryDirectory::new("broken-pair");
        write_pair(
            archiveless.path(),
            "ubuntu-26.04-amd64-7.0.0-14-generic",
            &valid,
        );
        fs::remove_file(
            archiveless
                .path()
                .join("gpu-payload")
                .join("ubuntu-26.04-amd64-7.0.0-14-generic.zip"),
        )
        .unwrap();
        assert!(PayloadCatalog::from_release_directory(archiveless.path()).is_err());
    }

    #[test]
    fn two_entries_for_one_guest_fail_rather_than_depend_on_directory_order() {
        let temporary = TemporaryDirectory::new("duplicate");
        let first = entry_json("ubuntu", "26.04", "amd64", "7.0.0-14-generic");
        write_pair(
            temporary.path(),
            "ubuntu-26.04-amd64-7.0.0-14-generic",
            &first,
        );
        let mut second: serde_json::Value = serde_json::from_str(&first).unwrap();
        second["payload_id"] = "second".into();
        write_pair(
            temporary.path(),
            "second",
            &serde_json::to_string(&second).unwrap(),
        );

        assert!(matches!(
            PayloadCatalog::from_release_directory(temporary.path()),
            Err(PayloadError::InvalidCatalog(_))
        ));
    }
}
