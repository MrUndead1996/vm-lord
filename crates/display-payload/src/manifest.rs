//! What a display payload says about itself, and what its provenance says.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use vmlord_payload::{
    PayloadEntry, PayloadError, PayloadFiles, PayloadSources, PreparedFile, Sha256Digest,
    validate_path,
};

use crate::{DisplayCatalogEntry, DisplayTarget, PayloadVersion};

/// The directory the DKMS sources live under, inside a payload.
pub const MODULE_DIRECTORY: &str = "content/drm/";

/// The directory the guest display services live under.
///
/// Empty until task #115 fills it; declared here from the first version so
/// that adding it later is not a second schema.
pub const SERVICES_DIRECTORY: &str = "content/services/";

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DisplayManifestDocument {
    schema_version: u32,
    payload_id: String,
    version: PayloadVersion,
    target: DisplayTarget,
    files: Vec<PreparedFile>,
}

/// A payload's own account of the files it contains.
#[derive(Clone, Debug)]
pub struct DisplayManifest {
    version: PayloadVersion,
    files: Vec<PreparedFile>,
}

impl DisplayManifest {
    /// Reads `payload.json` and checks it describes the entry that claims it.
    ///
    /// # Errors
    ///
    /// [`PayloadError::InvalidManifest`] for a document at another schema
    /// version, one whose identity is not the entry's, one whose file list is
    /// unsorted, duplicated, empty or unsafe, one that fails to declare
    /// `sources.json` or a license the entry claims, or one that declares no
    /// module at all.
    pub fn parse_and_validate(
        bytes: &[u8],
        entry: &DisplayCatalogEntry,
    ) -> Result<Self, PayloadError> {
        let value: DisplayManifestDocument = serde_json::from_slice(bytes)
            .map_err(|error| PayloadError::InvalidManifest(error.to_string()))?;
        if value.schema_version != 1
            || value.payload_id != entry.payload_id()
            || value.version != *entry.version()
            || value.target != *entry.target()
        {
            return Err(PayloadError::InvalidManifest(
                "display manifest identity does not match catalog".into(),
            ));
        }

        let mut paths = HashSet::new();
        let mut last = "";
        for file in &value.files {
            validate_path(file.path())?;
            if file.size() == 0
                || !paths.insert(file.path())
                || (!last.is_empty() && last >= file.path())
            {
                return Err(PayloadError::InvalidManifest(
                    "prepared file paths must be unique, sorted, and non-empty".into(),
                ));
            }
            last = file.path();
        }

        if !paths.contains("sources.json") {
            return Err(PayloadError::InvalidManifest(
                "payload.json must declare sources.json".into(),
            ));
        }
        for license in entry.licenses() {
            if !paths.contains(license.path.as_str()) {
                return Err(PayloadError::InvalidManifest(format!(
                    "payload.json does not declare catalog license text: {}",
                    license.path
                )));
            }
        }
        // The one rule beyond what any payload owes: a display payload with no
        // module is a payload that cannot do the thing it exists for, and that
        // is worth refusing here rather than discovering as an empty
        // `/usr/src` tree inside a guest.
        if !paths.iter().any(|path| path.starts_with(MODULE_DIRECTORY)) {
            return Err(PayloadError::InvalidManifest(format!(
                "payload.json declares nothing under {MODULE_DIRECTORY}"
            )));
        }

        Ok(Self {
            version: value.version,
            files: value.files,
        })
    }

    /// The version the mounted payload says it is.
    ///
    /// The guest reads this rather than the catalog: the host chose an entry,
    /// but what is on the share is what will be installed.
    #[must_use]
    pub fn version(&self) -> &PayloadVersion {
        &self.version
    }

    #[must_use]
    pub fn files(&self) -> &[PreparedFile] {
        &self.files
    }
}

impl PayloadFiles for DisplayManifest {
    fn files(&self) -> &[PreparedFile] {
        &self.files
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DisplaySourcesDocument {
    schema_version: u32,
    sources: Vec<SourceRecord>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SourceRecord {
    url: String,
    commit: String,
    version: String,
}

/// Where a display payload came from, as the payload itself records it.
#[derive(Clone, Debug)]
pub struct DisplaySources {
    document: DisplaySourcesDocument,
}

impl DisplaySources {
    /// Reads `sources.json` and checks it says what the entry says.
    ///
    /// Exactly the same rows, in the same order: the catalog is what a person
    /// reads provenance out of, and a payload whose own record disagrees with
    /// it is one of the two lying.
    ///
    /// # Errors
    ///
    /// [`PayloadError::InvalidManifest`] for a document at another schema
    /// version or one whose rows are not the entry's.
    pub fn parse_and_validate(
        bytes: &[u8],
        entry: &DisplayCatalogEntry,
    ) -> Result<Self, PayloadError> {
        let document: DisplaySourcesDocument = serde_json::from_slice(bytes)
            .map_err(|error| PayloadError::InvalidManifest(error.to_string()))?;
        if document.schema_version != 1 {
            return Err(PayloadError::InvalidManifest(
                "unknown display sources schema version".into(),
            ));
        }
        let declared: Vec<_> = entry
            .sources()
            .iter()
            .map(|source| {
                (
                    source.url.as_str(),
                    source.commit.as_str(),
                    source.version.as_str(),
                )
            })
            .collect();
        let recorded: Vec<_> = document
            .sources
            .iter()
            .map(|source| {
                (
                    source.url.as_str(),
                    source.commit.as_str(),
                    source.version.as_str(),
                )
            })
            .collect();
        if declared != recorded {
            return Err(PayloadError::InvalidManifest(
                "sources.json does not match the catalog entry's provenance".into(),
            ));
        }
        Ok(Self { document })
    }
}

impl Serialize for DisplaySources {
    /// The document as it was read: the wrapper exists to prove the document
    /// was validated, and nothing about that is worth writing out.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.document.serialize(serializer)
    }
}

impl PayloadSources<DisplayManifest> for DisplaySources {
    /// Nothing. A display payload's provenance names upstreams rather than
    /// individual files, so it has no claim about the tree to cross-check --
    /// unlike the GPU payload's overlays.
    fn validate_prepared_files(&self, _manifest: &DisplayManifest) -> Result<(), PayloadError> {
        Ok(())
    }
}

impl PayloadEntry for DisplayCatalogEntry {
    type Manifest = DisplayManifest;
    type Sources = DisplaySources;
    const NAMESPACE: &'static str = crate::LOCAL_ARCHIVE_DIRECTORY;

    fn from_json(bytes: &[u8]) -> Result<Self, PayloadError> {
        Self::from_json(bytes)
    }

    fn payload_id(&self) -> &str {
        self.payload_id()
    }

    fn archive_sha256(&self) -> &Sha256Digest {
        self.archive_sha256()
    }

    fn payload_manifest_sha256(&self) -> &Sha256Digest {
        self.payload_manifest_sha256()
    }

    fn expanded_size_limit(&self) -> u64 {
        self.expanded_size_limit()
    }

    fn file_count_limit(&self) -> u64 {
        self.file_count_limit()
    }

    fn parse_manifest(&self, bytes: &[u8]) -> Result<Self::Manifest, PayloadError> {
        DisplayManifest::parse_and_validate(bytes, self)
    }

    fn parse_sources(&self, bytes: &[u8]) -> Result<Self::Sources, PayloadError> {
        DisplaySources::parse_and_validate(bytes, self)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use vmlord_payload::PayloadError;

    use super::{DisplayManifest, DisplaySources};
    use crate::{DisplayCatalogEntry, catalog::test_entry};

    const ZERO: &str = "0000000000000000000000000000000000000000000000000000000000000000";
    const COMMIT: &str = "14794180686c2fb6307fbe359c359bec765249f3";

    fn entry() -> DisplayCatalogEntry {
        test_entry(json!({
            "payload_id": "display-ubuntu-24.04-amd64-0.1.0",
            "version": "0.1.0",
            "target": {
                "distribution": "ubuntu",
                "release": "24.04",
                "architecture": "amd64",
                "payload_abi": 1
            },
            "proven_on": "6.8.0-137-generic",
            "protocol": { "major": 1, "min_minor": 0, "max_minor": 0 },
            "archive_sha256": ZERO,
            "payload_manifest_sha256": ZERO,
            "expanded_size_limit": 1024,
            "file_count_limit": 16,
            "sources": [{
                "url": "https://vmlord.invalid/display",
                "commit": COMMIT,
                "version": "0.1.0"
            }],
            "licenses": [{ "spdx": "GPL-2.0", "path": "licenses/GPL-2.0.txt" }]
        }))
    }

    fn manifest_json(version: &str, files: &[&str]) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "payload_id": "display-ubuntu-24.04-amd64-0.1.0",
            "version": version,
            "target": {
                "distribution": "ubuntu",
                "release": "24.04",
                "architecture": "amd64",
                "payload_abi": 1
            },
            "files": files
                .iter()
                .map(|path| json!({ "path": path, "size": 1, "sha256": ZERO }))
                .collect::<Vec<_>>()
        }))
        .unwrap()
    }

    const COMPLETE: [&str; 4] = [
        "content/drm/Kbuild",
        "content/drm/vmlord_drm.c",
        "licenses/GPL-2.0.txt",
        "sources.json",
    ];

    #[test]
    fn a_manifest_must_match_the_entry_that_claims_it() {
        assert!(
            DisplayManifest::parse_and_validate(&manifest_json("0.1.0", &COMPLETE), &entry())
                .is_ok()
        );

        assert!(matches!(
            DisplayManifest::parse_and_validate(&manifest_json("0.2.0", &COMPLETE), &entry()),
            Err(PayloadError::InvalidManifest(_))
        ));
    }

    #[test]
    fn a_payload_with_no_module_is_not_a_display_payload() {
        let error = DisplayManifest::parse_and_validate(
            &manifest_json(
                "0.1.0",
                &[
                    "content/services/README",
                    "licenses/GPL-2.0.txt",
                    "sources.json",
                ],
            ),
            &entry(),
        )
        .expect_err("nothing under content/drm means nothing to build");

        assert!(error.to_string().contains("content/drm"));
    }

    #[test]
    fn a_manifest_must_declare_its_provenance_and_its_licenses() {
        for files in [
            vec!["content/drm/Kbuild", "licenses/GPL-2.0.txt"],
            vec!["content/drm/Kbuild", "sources.json"],
        ] {
            assert!(
                DisplayManifest::parse_and_validate(&manifest_json("0.1.0", &files), &entry())
                    .is_err(),
                "accepted a manifest missing one of the two documents it owes"
            );
        }
    }

    #[test]
    fn file_paths_must_be_sorted_unique_and_safe() {
        for files in [
            vec!["sources.json", "content/drm/Kbuild", "licenses/GPL-2.0.txt"],
            vec![
                "content/drm/Kbuild",
                "content/drm/Kbuild",
                "licenses/GPL-2.0.txt",
                "sources.json",
            ],
            vec![
                "content/drm/../../escape",
                "licenses/GPL-2.0.txt",
                "sources.json",
            ],
        ] {
            assert!(
                DisplayManifest::parse_and_validate(&manifest_json("0.1.0", &files), &entry())
                    .is_err(),
                "accepted {files:?}"
            );
        }
    }

    #[test]
    fn sources_must_say_exactly_what_the_entry_says() {
        let matching = json!({
            "schema_version": 1,
            "sources": [{
                "url": "https://vmlord.invalid/display",
                "commit": COMMIT,
                "version": "0.1.0"
            }]
        });
        assert!(
            DisplaySources::parse_and_validate(&serde_json::to_vec(&matching).unwrap(), &entry())
                .is_ok()
        );

        let mut divergent = matching;
        divergent["sources"][0]["version"] = "0.2.0".into();
        assert!(matches!(
            DisplaySources::parse_and_validate(&serde_json::to_vec(&divergent).unwrap(), &entry()),
            Err(PayloadError::InvalidManifest(_))
        ));
    }
}
