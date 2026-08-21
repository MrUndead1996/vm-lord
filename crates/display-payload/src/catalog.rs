//! What a release says about one version of the display payload, and which of
//! several versions a guest gets.

use serde::{Deserialize, Serialize};
use vmlord_payload::{PayloadError, Sha256Digest};

use crate::{PayloadVersion, ProtocolRange, ProtocolVersionParts};

const ENTRY_SCHEMA_VERSION: u32 = 1;
const PAYLOAD_ABI_VERSION: u32 = 1;

/// The guest a payload was built for, as the host knows it before boot.
///
/// Three dimensions and no kernel: the host chooses a payload before the guest
/// that will run it has booted, so there is no running kernel to match on. What
/// the payload was proven against is recorded beside this as `proven_on`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DisplayTarget {
    pub distribution: String,
    pub release: String,
    pub architecture: String,
    pub payload_abi: u32,
}

/// A guest asking for a payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GuestSelector<'a> {
    pub distribution: &'a str,
    pub release: &'a str,
    pub architecture: &'a str,
}

impl DisplayTarget {
    #[must_use]
    pub fn matches(&self, guest: &GuestSelector<'_>) -> bool {
        self.distribution.eq_ignore_ascii_case(guest.distribution)
            && self.release == guest.release
            && self.architecture.eq_ignore_ascii_case(guest.architecture)
    }
}

/// One upstream a payload owes something to.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub url: String,
    pub commit: String,
    pub version: String,
}

/// One license text a payload carries, and what it covers.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct License {
    pub spdx: String,
    pub path: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DisplayCatalogEntryDocument {
    schema_version: u32,
    payload_id: String,
    version: PayloadVersion,
    target: DisplayTarget,
    proven_on: String,
    protocol: ProtocolRange,
    archive_sha256: Sha256Digest,
    payload_manifest_sha256: Sha256Digest,
    expanded_size_limit: u64,
    file_count_limit: u64,
    sources: Vec<Source>,
    licenses: Vec<License>,
}

/// A display payload entry that has passed validation.
///
/// Entries cannot be deserialized directly, because doing so would bypass
/// [`DisplayCatalogEntry::from_json`] and its validation boundary.
///
/// ```compile_fail
/// let _: vmlord_display_payload::DisplayCatalogEntry = serde_json::from_str("{}").unwrap();
/// ```
#[derive(Clone, Debug, Serialize)]
pub struct DisplayCatalogEntry {
    payload_id: String,
    version: PayloadVersion,
    target: DisplayTarget,
    proven_on: String,
    protocol: ProtocolRange,
    archive_sha256: Sha256Digest,
    payload_manifest_sha256: Sha256Digest,
    expanded_size_limit: u64,
    file_count_limit: u64,
    sources: Vec<Source>,
    licenses: Vec<License>,
}

impl From<DisplayCatalogEntryDocument> for DisplayCatalogEntry {
    fn from(value: DisplayCatalogEntryDocument) -> Self {
        Self {
            payload_id: value.payload_id,
            version: value.version,
            target: value.target,
            proven_on: value.proven_on,
            protocol: value.protocol,
            archive_sha256: value.archive_sha256,
            payload_manifest_sha256: value.payload_manifest_sha256,
            expanded_size_limit: value.expanded_size_limit,
            file_count_limit: value.file_count_limit,
            sources: value.sources,
            licenses: value.licenses,
        }
    }
}

impl DisplayCatalogEntry {
    /// Reads one entry document, as `cargo xtask display-payload pack` writes
    /// it and as a release carries it beside its archive.
    ///
    /// # Errors
    ///
    /// [`PayloadError::InvalidCatalog`] for a document at another schema
    /// version, or one that fails validation.
    pub fn from_json(bytes: &[u8]) -> Result<Self, PayloadError> {
        let document: DisplayCatalogEntryDocument = serde_json::from_slice(bytes)
            .map_err(|error| PayloadError::InvalidCatalog(error.to_string()))?;
        if document.schema_version != ENTRY_SCHEMA_VERSION {
            return Err(PayloadError::InvalidCatalog(
                "unknown display payload entry schema version".into(),
            ));
        }
        let entry = Self::from(document);
        entry.validate()?;
        Ok(entry)
    }

    fn validate(&self) -> Result<(), PayloadError> {
        if self.payload_id.is_empty()
            || self.target.distribution.is_empty()
            || self.target.release.is_empty()
            || self.target.architecture.is_empty()
            || self.target.payload_abi != PAYLOAD_ABI_VERSION
            || self.proven_on.is_empty()
            || self.expanded_size_limit == 0
            || self.file_count_limit == 0
        {
            return Err(PayloadError::InvalidCatalog(
                "missing or invalid required display payload field".into(),
            ));
        }
        if !self.protocol.is_valid() {
            return Err(PayloadError::InvalidCatalog(
                "a payload's protocol range must not be inverted".into(),
            ));
        }
        // The version is in the ID because an update is a second file in one
        // directory: two versions for one guest have to be able to sit beside
        // each other, and a listing has to say which is which.
        if !self.payload_id.ends_with(&format!("-{}", self.version)) {
            return Err(PayloadError::InvalidCatalog(format!(
                "payload ID {} does not end in its version {}",
                self.payload_id, self.version
            )));
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
        if self.licenses.is_empty()
            || self
                .licenses
                .iter()
                .any(|license| license.spdx.is_empty() || license.path.is_empty())
        {
            return Err(PayloadError::InvalidCatalog(
                "licenses must carry SPDX expressions and paths".into(),
            ));
        }
        Ok(())
    }

    #[must_use]
    pub fn payload_id(&self) -> &str {
        &self.payload_id
    }

    #[must_use]
    pub fn version(&self) -> &PayloadVersion {
        &self.version
    }

    #[must_use]
    pub fn target(&self) -> &DisplayTarget {
        &self.target
    }

    /// The kernel this payload was built and proven against.
    ///
    /// A record and never a condition: DKMS builds against the headers of
    /// whatever kernel the guest is running, and Ubuntu upgrades kernels
    /// unattended.
    #[must_use]
    pub fn proven_on(&self) -> &str {
        &self.proven_on
    }

    #[must_use]
    pub fn protocol(&self) -> &ProtocolRange {
        &self.protocol
    }

    #[must_use]
    pub fn archive_sha256(&self) -> &Sha256Digest {
        &self.archive_sha256
    }

    #[must_use]
    pub fn payload_manifest_sha256(&self) -> &Sha256Digest {
        &self.payload_manifest_sha256
    }

    #[must_use]
    pub fn expanded_size_limit(&self) -> u64 {
        self.expanded_size_limit
    }

    #[must_use]
    pub fn file_count_limit(&self) -> u64 {
        self.file_count_limit
    }

    #[must_use]
    pub fn sources(&self) -> &[Source] {
        &self.sources
    }

    #[must_use]
    pub fn licenses(&self) -> &[License] {
        &self.licenses
    }
}

/// Every display payload a release carries.
pub struct DisplayPayloadCatalog {
    entries: Vec<DisplayCatalogEntry>,
}

impl DisplayPayloadCatalog {
    /// The catalog a release carries beside its executable.
    ///
    /// # Errors
    ///
    /// [`PayloadError::InvalidCatalog`] for a broken release: an entry that
    /// does not validate, one not named for its payload ID, one with no
    /// archive, or two entries claiming the same guest and version.
    pub fn from_release_directory(directory: &std::path::Path) -> Result<Self, PayloadError> {
        Self::from_entries(vmlord_payload::catalog::read_release_directory(
            directory,
            crate::LOCAL_ARCHIVE_DIRECTORY,
        )?)
    }

    /// The catalog a set of read entries forms.
    ///
    /// Several versions for one guest is the ordinary state of a catalog that
    /// can be updated, so what is refused is the *same* version twice: that
    /// would make selection depend on the order a directory happened to list
    /// two identical candidates.
    ///
    /// # Errors
    ///
    /// [`PayloadError::InvalidCatalog`] for a duplicate payload ID or a
    /// duplicate guest-and-version pair.
    pub fn from_entries(entries: Vec<DisplayCatalogEntry>) -> Result<Self, PayloadError> {
        let mut ids = std::collections::HashSet::new();
        let mut versions = std::collections::HashSet::new();
        for entry in &entries {
            if !ids.insert(entry.payload_id.clone())
                || !versions.insert((entry.target.clone(), entry.version))
            {
                return Err(PayloadError::InvalidCatalog(
                    "duplicate display payload ID, or one version twice for one guest".into(),
                ));
            }
        }
        Ok(Self { entries })
    }

    #[must_use]
    pub fn entries(&self) -> &[DisplayCatalogEntry] {
        &self.entries
    }

    /// The best entry for a guest this build can actually talk to.
    ///
    /// The triple is the hard gate and is decided before the guest has booted,
    /// which is why no kernel appears in it. Of what is left, an entry whose
    /// protocol range this build is outside of is passed over rather than
    /// failed: a payload may legitimately be built for a newer or an older
    /// VMLord, and a release carrying one is not broken. The greatest version
    /// wins, because a payload is only published when it is meant to be used.
    ///
    /// # Errors
    ///
    /// [`PayloadError::NoPayloadForGuest`] when nothing applies, which is an
    /// ordinary answer: the display goes degraded and the VM starts.
    pub fn select_for_guest(
        &self,
        guest: &GuestSelector<'_>,
        protocol: ProtocolVersionParts,
    ) -> Result<&DisplayCatalogEntry, PayloadError> {
        self.entries
            .iter()
            .filter(|entry| entry.target.matches(guest))
            .filter(|entry| entry.protocol.covers(protocol.major, protocol.minor))
            .max_by_key(|entry| entry.version)
            .ok_or_else(|| PayloadError::NoPayloadForGuest {
                distribution: guest.distribution.to_owned(),
                release: guest.release.to_owned(),
                architecture: guest.architecture.to_owned(),
            })
    }
}

#[cfg(test)]
pub(crate) fn test_entry(mut value: serde_json::Value) -> DisplayCatalogEntry {
    value["schema_version"] = ENTRY_SCHEMA_VERSION.into();
    DisplayCatalogEntry::from_json(&serde_json::to_vec(&value).unwrap())
        .expect("the test entry must be a valid entry document")
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use vmlord_payload::PayloadError;

    use super::{DisplayCatalogEntry, DisplayPayloadCatalog, GuestSelector};
    use crate::ProtocolVersionParts;

    const ZERO: &str = "0000000000000000000000000000000000000000000000000000000000000000";
    const COMMIT: &str = "14794180686c2fb6307fbe359c359bec765249f3";
    const SPEAKS_1_0: ProtocolVersionParts = ProtocolVersionParts { major: 1, minor: 0 };

    fn entry_document(release: &str, version: &str) -> serde_json::Value {
        json!({
            "schema_version": 1,
            "payload_id": format!("display-ubuntu-{release}-amd64-{version}"),
            "version": version,
            "target": {
                "distribution": "ubuntu",
                "release": release,
                "architecture": "amd64",
                "payload_abi": 1
            },
            "proven_on": "6.8.0-137-generic",
            "protocol": { "major": 1, "min_minor": 0, "max_minor": 0 },
            "archive_sha256": ZERO,
            "payload_manifest_sha256": ZERO,
            "expanded_size_limit": 33_554_432,
            "file_count_limit": 512,
            "sources": [{
                "url": "https://vmlord.invalid/display",
                "commit": COMMIT,
                "version": version
            }],
            "licenses": [{ "spdx": "GPL-2.0", "path": "licenses/GPL-2.0.txt" }]
        })
    }

    fn parse(document: &serde_json::Value) -> DisplayCatalogEntry {
        DisplayCatalogEntry::from_json(&serde_json::to_vec(document).unwrap())
            .expect("the fixture must be a valid entry")
    }

    fn catalog_with(documents: &[serde_json::Value]) -> DisplayPayloadCatalog {
        DisplayPayloadCatalog::from_entries(documents.iter().map(parse).collect())
            .expect("the fixtures must form a catalog")
    }

    fn ubuntu_2404() -> GuestSelector<'static> {
        GuestSelector {
            distribution: "ubuntu",
            release: "24.04",
            architecture: "amd64",
        }
    }

    #[test]
    fn an_entry_carries_its_version_its_proof_and_its_protocol_range() {
        let entry = parse(&entry_document("24.04", "0.1.0"));

        assert_eq!(entry.payload_id(), "display-ubuntu-24.04-amd64-0.1.0");
        assert_eq!(entry.version().to_string(), "0.1.0");
        assert_eq!(
            entry.proven_on(),
            "6.8.0-137-generic",
            "a proof, never a selector"
        );
        assert!(entry.protocol().covers(1, 0));
    }

    #[test]
    fn an_entry_whose_id_does_not_end_in_its_version_is_refused() {
        let mut document = entry_document("24.04", "0.1.0");
        document["payload_id"] = "display-ubuntu-24.04-amd64-0.2.0".into();

        assert!(matches!(
            DisplayCatalogEntry::from_json(&serde_json::to_vec(&document).unwrap()),
            Err(PayloadError::InvalidCatalog(_))
        ));
    }

    #[test]
    fn an_entry_missing_any_required_field_is_refused() {
        for field in ["payload_id", "proven_on"] {
            let mut document = entry_document("24.04", "0.1.0");
            document[field] = "".into();
            assert!(
                DisplayCatalogEntry::from_json(&serde_json::to_vec(&document).unwrap()).is_err(),
                "accepted an empty {field}"
            );
        }

        for dimension in ["distribution", "release", "architecture"] {
            let mut document = entry_document("24.04", "0.1.0");
            document["target"][dimension] = "".into();
            assert!(
                DisplayCatalogEntry::from_json(&serde_json::to_vec(&document).unwrap()).is_err(),
                "accepted an empty {dimension}"
            );
        }
    }

    #[test]
    fn an_entry_at_another_schema_version_is_refused() {
        let mut document = entry_document("24.04", "0.1.0");
        document["schema_version"] = 2.into();

        assert!(matches!(
            DisplayCatalogEntry::from_json(&serde_json::to_vec(&document).unwrap()),
            Err(PayloadError::InvalidCatalog(_))
        ));
    }

    #[test]
    fn an_entry_needs_provenance_and_a_license() {
        for field in ["sources", "licenses"] {
            let mut document = entry_document("24.04", "0.1.0");
            document[field] = json!([]);
            assert!(
                DisplayCatalogEntry::from_json(&serde_json::to_vec(&document).unwrap()).is_err(),
                "accepted an empty {field}"
            );
        }
    }

    #[test]
    fn the_newest_version_for_the_guest_wins() {
        let catalog = catalog_with(&[
            entry_document("24.04", "0.1.0"),
            entry_document("24.04", "0.10.0"),
        ]);

        assert_eq!(
            catalog
                .select_for_guest(&ubuntu_2404(), SPEAKS_1_0)
                .unwrap()
                .version()
                .to_string(),
            "0.10.0",
            "10 is newer than 1, which sorting the text would get wrong"
        );
    }

    #[test]
    fn a_payload_this_build_cannot_speak_to_is_passed_over_and_not_an_error() {
        let mut future = entry_document("24.04", "0.2.0");
        future["protocol"] = json!({ "major": 2, "min_minor": 0, "max_minor": 0 });
        let catalog = catalog_with(&[entry_document("24.04", "0.1.0"), future]);

        assert_eq!(
            catalog
                .select_for_guest(&ubuntu_2404(), SPEAKS_1_0)
                .unwrap()
                .version()
                .to_string(),
            "0.1.0",
            "a payload built for a VMLord this is not is a candidate that does not apply"
        );
    }

    #[test]
    fn a_guest_with_no_entry_is_told_which_guest_had_none() {
        let catalog = catalog_with(&[entry_document("24.04", "0.1.0")]);

        let error = catalog
            .select_for_guest(
                &GuestSelector {
                    release: "22.04",
                    ..ubuntu_2404()
                },
                SPEAKS_1_0,
            )
            .expect_err("no entry matches this release");

        assert!(matches!(error, PayloadError::NoPayloadForGuest { .. }));
        assert!(error.to_string().contains("22.04"));
    }

    #[test]
    fn several_versions_for_one_guest_are_a_catalog_and_not_a_conflict() {
        assert!(
            DisplayPayloadCatalog::from_entries(vec![
                parse(&entry_document("24.04", "0.1.0")),
                parse(&entry_document("24.04", "0.2.0")),
            ])
            .is_ok(),
            "holding two versions at once is what an update is made of"
        );
    }

    #[test]
    fn one_version_twice_for_one_guest_is_a_broken_release() {
        let mut second = entry_document("24.04", "0.1.0");
        second["payload_id"] = "display-ubuntu-24.04-amd64-again-0.1.0".into();

        assert!(
            DisplayPayloadCatalog::from_entries(vec![
                parse(&entry_document("24.04", "0.1.0")),
                parse(&second),
            ])
            .is_err(),
            "selection must not depend on the order a directory listed two identical candidates"
        );
    }

    #[test]
    fn an_empty_catalog_has_nothing_for_anyone() {
        assert!(
            catalog_with(&[])
                .select_for_guest(&ubuntu_2404(), SPEAKS_1_0)
                .is_err()
        );
    }
}
