use crate::{PayloadError, Sha256Digest};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
const CATALOG_SCHEMA_VERSION: u32 = 1;
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
struct CatalogDocument {
    schema_version: u32,
    entries: Vec<CatalogEntryDocument>,
}
#[derive(Clone, Debug, Deserialize)]
struct CatalogEntryDocument {
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
/// Entries cannot be deserialized independently because doing so would bypass
/// [`PayloadCatalog::from_json`] and its validation boundary.
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
impl PayloadCatalog {
    pub fn from_json(bytes: &[u8]) -> Result<Self, PayloadError> {
        let doc: CatalogDocument = serde_json::from_slice(bytes)
            .map_err(|error| PayloadError::InvalidCatalog(error.to_string()))?;
        if doc.schema_version != CATALOG_SCHEMA_VERSION {
            return Err(PayloadError::InvalidCatalog(
                "unknown catalog schema version".into(),
            ));
        }
        let entries = doc
            .entries
            .into_iter()
            .map(CatalogEntry::from)
            .collect::<Vec<_>>();
        let mut ids = HashSet::new();
        let mut targets = HashSet::new();
        for entry in &entries {
            entry.validate()?;
            if !ids.insert(entry.payload_id.clone()) || !targets.insert(entry.target.clone()) {
                return Err(PayloadError::InvalidCatalog(
                    "duplicate payload ID or target".into(),
                ));
            }
        }
        Ok(Self { entries })
    }
    /// Reads one entry as `cargo xtask gpu-payload pack` writes it.
    ///
    /// `pack` emits a bare entry object rather than a catalog document, and
    /// what makes an entry trustworthy is [`Self::from_json`]'s validation --
    /// so the entry is wrapped in the document it belongs to and read through
    /// exactly that. Both the builder and the release build use this, so the
    /// file has one reading rather than two that can drift apart.
    pub fn from_entry_json(bytes: &[u8]) -> Result<CatalogEntry, PayloadError> {
        let entry: serde_json::Value = serde_json::from_slice(bytes)
            .map_err(|error| PayloadError::InvalidCatalog(error.to_string()))?;
        let document = serde_json::to_vec(&serde_json::json!({
            "schema_version": CATALOG_SCHEMA_VERSION,
            "entries": [entry],
        }))
        .map_err(|error| PayloadError::InvalidCatalog(error.to_string()))?;
        Self::from_json(&document)?
            .entries
            .into_iter()
            .next()
            .ok_or_else(|| PayloadError::InvalidCatalog("empty catalog entry".into()))
    }
    pub fn embedded() -> Result<Self, PayloadError> {
        Self::from_json(include_bytes!("../catalog/catalog.json"))
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

#[cfg(test)]
mod tests {
    use super::kernel_order;
    use crate::{GuestSelector, GuestTarget, PayloadCatalog, PayloadError};
    const Z: &str = "0000000000000000000000000000000000000000000000000000000000000000";
    const C: &str = "14794180686c2fb6307fbe359c359bec765249f3";
    fn catalog() -> String {
        format!(
            r#"{{"schema_version":1,"entries":[{{"payload_id":"ubuntu-26.04-amd64-7.0.0-14-v1","target":{{"distribution":"ubuntu","release":"26.04","architecture":"amd64","kernel_release":"7.0.0-14-generic","payload_abi":1}},"expanded_size_limit":2,"file_count_limit":3,"archive_sha256":"{Z}","payload_manifest_sha256":"{Z}","required_renderers":["d3d12-gallium","dzn-vulkan"],"mesa_policy":"bundled","sources":[{{"url":"https://github.com/microsoft/WSL2-Linux-Kernel","commit":"{C}","version":"1"}}],"licenses":[{{"spdx":"GPL-2.0","path":"licenses/GPL-2.0.txt"}}]}}]}}"#
        )
    }
    #[test]
    fn a_catalog_selects_only_the_exact_kernel_tuple() {
        let c = PayloadCatalog::from_json(catalog().as_bytes()).unwrap();
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
            r#"{{"payload_id":"{distribution}-{release}-{architecture}-{kernel}","target":{{"distribution":"{distribution}","release":"{release}","architecture":"{architecture}","kernel_release":"{kernel}","payload_abi":1}},"expanded_size_limit":2,"file_count_limit":3,"archive_sha256":"{Z}","payload_manifest_sha256":"{Z}","required_renderers":["d3d12-gallium"],"mesa_policy":"bundled","sources":[{{"url":"https://github.com/microsoft/WSL2-Linux-Kernel","commit":"{C}","version":"1"}}],"licenses":[{{"spdx":"GPL-2.0","path":"licenses/GPL-2.0.txt"}}]}}"#
        )
    }
    fn catalog_with(entries: &[String]) -> PayloadCatalog {
        let document = format!(
            r#"{{"schema_version":1,"entries":[{}]}}"#,
            entries.join(",")
        );
        PayloadCatalog::from_json(document.as_bytes()).expect("the catalog must parse")
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
            document["entries"][0]["target"][dimension] = "".into();
            assert!(
                matches!(
                    PayloadCatalog::from_json(&serde_json::to_vec(&document).unwrap()),
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
            PayloadCatalog::from_json(empty_sources.as_bytes()),
            Err(PayloadError::InvalidCatalog(_))
        ));
    }

    #[test]
    fn the_embedded_catalog_is_valid_and_offers_the_guests_it_names() {
        // Validation is the point: every entry goes through the same checks a
        // packed one does, and a catalog compiled into the application is the
        // only one it trusts, so a malformed entry has to fail the build
        // rather than a host.
        let catalog = PayloadCatalog::embedded().expect("the shipped catalog must parse");

        for entry in catalog.entries() {
            let target = entry.target();
            assert!(
                catalog
                    .select_for_guest(&GuestSelector {
                        distribution: &target.distribution,
                        release: &target.release,
                        architecture: &target.architecture,
                    })
                    .is_ok(),
                "an entry that cannot be selected for its own guest is unreachable: {}",
                entry.payload_id()
            );
        }
    }

    #[test]
    fn a_packed_entry_is_read_through_the_same_validation_as_the_catalog() {
        let document: serde_json::Value = serde_json::from_str(&catalog()).unwrap();
        let entry = serde_json::to_vec(&document["entries"][0]).unwrap();

        assert_eq!(
            PayloadCatalog::from_entry_json(&entry)
                .unwrap()
                .payload_id(),
            "ubuntu-26.04-amd64-7.0.0-14-v1"
        );
    }

    #[test]
    fn a_packed_entry_that_fails_catalog_validation_is_refused() {
        let mut document: serde_json::Value = serde_json::from_str(&catalog()).unwrap();
        document["entries"][0]["target"]["payload_abi"] = 2.into();
        let entry = serde_json::to_vec(&document["entries"][0]).unwrap();

        assert!(matches!(
            PayloadCatalog::from_entry_json(&entry),
            Err(PayloadError::InvalidCatalog(_))
        ));
    }

    #[test]
    fn a_whole_catalog_document_is_not_an_entry() {
        assert!(matches!(
            PayloadCatalog::from_entry_json(catalog().as_bytes()),
            Err(PayloadError::InvalidCatalog(_))
        ));
    }
}
