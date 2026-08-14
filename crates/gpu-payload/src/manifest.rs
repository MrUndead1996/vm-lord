use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::{CatalogEntry, GuestTarget, PayloadError, Sha256Digest};

const D3DKMTHK_PATH: &str = "include/uapi/misc/d3dkmthk.h";
const D3DKMTHK_LICENSE: &str = "GPL-2.0 WITH Linux-syscall-note";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedFile {
    path: String,
    size: u64,
    sha256: Sha256Digest,
}
impl PreparedFile {
    pub fn path(&self) -> &str {
        &self.path
    }
    pub fn size(&self) -> u64 {
        self.size
    }
    pub fn sha256(&self) -> &Sha256Digest {
        &self.sha256
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PayloadManifestDocument {
    schema_version: u32,
    payload_id: String,
    target: ManifestTarget,
    files: Vec<PreparedFile>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestTarget {
    distribution: String,
    release: String,
    architecture: String,
    kernel_release: String,
    payload_abi: u32,
}

impl ManifestTarget {
    fn matches(&self, target: &GuestTarget) -> bool {
        self.distribution == target.distribution
            && self.release == target.release
            && self.architecture == target.architecture
            && self.kernel_release == target.kernel_release
            && self.payload_abi == target.payload_abi
    }
}

#[derive(Clone, Debug)]
pub struct PayloadManifest {
    files: Vec<PreparedFile>,
}
impl PayloadManifest {
    pub fn parse_and_validate(bytes: &[u8], entry: &CatalogEntry) -> Result<Self, PayloadError> {
        let value: PayloadManifestDocument = serde_json::from_slice(bytes)
            .map_err(|error| PayloadError::InvalidManifest(error.to_string()))?;
        if value.schema_version != 1
            || value.payload_id != entry.payload_id()
            || !value.target.matches(entry.target())
        {
            return Err(PayloadError::InvalidManifest(
                "manifest identity does not match catalog".into(),
            ));
        }

        let mut paths = HashSet::new();
        let mut last = "";
        for file in &value.files {
            validate_path(&file.path)?;
            if file.size == 0
                || !paths.insert(file.path.as_str())
                || (!last.is_empty() && last >= file.path.as_str())
            {
                return Err(PayloadError::InvalidManifest(
                    "prepared file paths must be unique, sorted, and non-empty".into(),
                ));
            }
            last = &file.path;
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

        Ok(Self { files: value.files })
    }

    pub fn files(&self) -> &[PreparedFile] {
        &self.files
    }
}

fn validate_path(path: &str) -> Result<(), PayloadError> {
    if path.is_empty()
        || path.contains('\\')
        || path.contains('\0')
        || path.starts_with('/')
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || path == "payload.json"
    {
        return Err(PayloadError::InvalidManifest(format!(
            "unsafe prepared-file path: {path}"
        )));
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SourceManifestDocument {
    schema_version: u32,
    sources: Vec<SourceRecord>,
    overlays: Vec<OverlayRecord>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SourceRecord {
    url: String,
    commit: String,
    version: String,
    paths: Vec<String>,
    licenses: Vec<SourceLicenseRecord>,
    sha256: Sha256Digest,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SourceLicenseRecord {
    path: String,
    spdx: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct OverlayRecord {
    path: String,
    sha256: Sha256Digest,
    license: String,
    author: String,
}

#[derive(Clone, Debug)]
pub struct SourceManifest {
    document: SourceManifestDocument,
}
impl SourceManifest {
    pub fn parse_and_validate(bytes: &[u8], entry: &CatalogEntry) -> Result<Self, PayloadError> {
        let doc: SourceManifestDocument = serde_json::from_slice(bytes)
            .map_err(|error| PayloadError::InvalidManifest(error.to_string()))?;
        if doc.schema_version != 1 || doc.sources.len() != entry.sources().len() {
            return Err(PayloadError::InvalidManifest(
                "sources.json does not exactly match catalog provenance".into(),
            ));
        }

        for (source, expected) in doc.sources.iter().zip(entry.sources()) {
            if source.url != expected.url
                || source.commit != expected.commit
                || source.version != expected.version
                || source.paths.is_empty()
                || source.licenses.len() != source.paths.len()
            {
                return Err(PayloadError::InvalidManifest(
                    "sources.json does not exactly match catalog provenance".into(),
                ));
            }
            let mut previous = "";
            for path in &source.paths {
                validate_path(path)?;
                if !previous.is_empty() && previous >= path.as_str() {
                    return Err(PayloadError::InvalidManifest(
                        "selected source paths must be unique and sorted".into(),
                    ));
                }
                previous = path;
            }
            for (path, license) in source.paths.iter().zip(&source.licenses) {
                validate_path(&license.path)?;
                if license.path != *path
                    || !license_expression_is_declared(&license.spdx, entry)
                    || (license.path == D3DKMTHK_PATH && license.spdx != D3DKMTHK_LICENSE)
                {
                    return Err(PayloadError::InvalidManifest(
                        "selected source paths must carry their declared licenses".into(),
                    ));
                }
            }
        }

        let mut overlay_paths = HashSet::new();
        for overlay in &doc.overlays {
            validate_path(&overlay.path)?;
            if !overlay_paths.insert(overlay.path.as_str())
                || overlay.license.is_empty()
                || overlay.author.is_empty()
                || overlay.author.eq_ignore_ascii_case("microsoft")
                || !entry
                    .licenses()
                    .iter()
                    .any(|license| license.spdx == overlay.license)
            {
                return Err(PayloadError::InvalidManifest(
                    "invalid VMLord overlay provenance".into(),
                ));
            }
        }

        Ok(Self { document: doc })
    }

    pub(crate) fn validate_prepared_files(
        &self,
        manifest: &PayloadManifest,
    ) -> Result<(), PayloadError> {
        for overlay in &self.document.overlays {
            let Some(file) = manifest
                .files()
                .iter()
                .find(|file| file.path() == overlay.path)
            else {
                return Err(PayloadError::InvalidManifest(format!(
                    "overlay is not declared by payload.json: {}",
                    overlay.path
                )));
            };
            if file.sha256() != &overlay.sha256 {
                return Err(PayloadError::InvalidManifest(format!(
                    "overlay digest does not match payload.json: {}",
                    overlay.path
                )));
            }
        }
        Ok(())
    }
}

fn license_expression_is_declared(expression: &str, entry: &CatalogEntry) -> bool {
    let declared = |identifier: &str| {
        !identifier.is_empty()
            && entry
                .licenses()
                .iter()
                .any(|license| license.spdx == identifier)
    };
    match expression.split_once(" WITH ") {
        Some((license, exception)) => {
            !exception.contains(" WITH ") && declared(license) && declared(exception)
        }
        None => declared(expression),
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReadyMarker {
    schema_version: u32,
    payload_id: String,
    generation: Sha256Digest,
    payload_manifest_sha256: Sha256Digest,
}
impl ReadyMarker {
    pub fn new(entry: &CatalogEntry) -> Self {
        Self {
            schema_version: 1,
            payload_id: entry.payload_id().into(),
            generation: entry.archive_sha256().clone(),
            payload_manifest_sha256: entry.payload_manifest_sha256().clone(),
        }
    }
    pub(crate) fn new_for(payload: &crate::ReadyGpuPayload) -> Self {
        Self {
            schema_version: 1,
            payload_id: payload.payload_id().into(),
            generation: payload.generation().clone(),
            payload_manifest_sha256: payload.payload_manifest_sha256().clone(),
        }
    }
    pub fn to_json_bytes(&self) -> Result<Vec<u8>, PayloadError> {
        let mut bytes =
            serde_json::to_vec(self).map_err(|e| PayloadError::InvalidManifest(e.to_string()))?;
        bytes.push(b'\n');
        Ok(bytes)
    }
}
pub(crate) fn cache_provenance(
    entry: &CatalogEntry,
    sources: &SourceManifest,
) -> Result<Vec<u8>, PayloadError> {
    let mut value = serde_json::json!({"archive_sha256":entry.archive_sha256(),"payload_id":entry.payload_id(),"payload_manifest_sha256":entry.payload_manifest_sha256(),"sources":sources.document});
    let mut bytes =
        serde_json::to_vec(&mut value).map_err(|e| PayloadError::InvalidManifest(e.to_string()))?;
    bytes.push(b'\n');
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::{CatalogEntry, PayloadCatalog, PayloadError, PayloadManifest, SourceManifest};

    const ZERO: &str = "0000000000000000000000000000000000000000000000000000000000000000";
    const COMMIT: &str = "14794180686c2fb6307fbe359c359bec765249f3";

    fn entry() -> CatalogEntry {
        let catalog = json!({
            "schema_version": 1,
            "entries": [{
                "payload_id": "p",
                "target": {
                    "distribution": "ubuntu",
                    "release": "26.04",
                    "architecture": "amd64",
                    "kernel_release": "k",
                    "payload_abi": 1
                },
                "archive_url": "https://example.test/p.zip",
                "archive_size": 1,
                "expanded_size_limit": 2,
                "file_count_limit": 4,
                "archive_sha256": ZERO,
                "payload_manifest_sha256": ZERO,
                "required_renderers": ["d3d12-gallium"],
                "mesa_policy": "bundled",
                "sources": [{
                    "url": "https://github.com/x/y",
                    "commit": COMMIT,
                    "version": "1"
                }],
                "licenses": [{
                    "spdx": "GPL-2.0",
                    "path": "licenses/GPL-2.0.txt"
                }, {
                    "spdx": "Linux-syscall-note",
                    "path": "licenses/Linux-syscall-note.txt"
                }]
            }]
        });
        PayloadCatalog::from_json(&serde_json::to_vec(&catalog).unwrap())
            .unwrap()
            .entries()[0]
            .clone()
    }

    fn file(path: &str) -> Value {
        json!({"path": path, "size": 1, "sha256": ZERO})
    }

    fn payload(files: Vec<Value>) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "payload_id": "p",
            "target": {
                "distribution": "ubuntu",
                "release": "26.04",
                "architecture": "amd64",
                "kernel_release": "k",
                "payload_abi": 1
            },
            "files": files
        }))
        .unwrap()
    }

    fn sources() -> Value {
        json!({
            "schema_version": 1,
            "sources": [{
                "url": "https://github.com/x/y",
                "commit": COMMIT,
                "version": "1",
                "paths": [
                    "drivers/hv/dxgkrnl",
                    "include/uapi/misc/d3dkmthk.h"
                ],
                "licenses": [{
                    "path": "drivers/hv/dxgkrnl",
                    "spdx": "GPL-2.0"
                }, {
                    "path": "include/uapi/misc/d3dkmthk.h",
                    "spdx": "GPL-2.0 WITH Linux-syscall-note"
                }],
                "sha256": ZERO
            }],
            "overlays": []
        })
    }

    #[test]
    fn unsafe_duplicate_and_self_referential_paths_are_rejected() {
        for path in [
            "/absolute",
            "../escape",
            r"content\escape",
            "payload.json",
            "a/../../b",
        ] {
            let data = payload(vec![file(path), file("sources.json")]);
            assert!(matches!(
                PayloadManifest::parse_and_validate(&data, &entry()),
                Err(PayloadError::InvalidManifest(_))
            ));
        }
    }

    #[test]
    fn every_catalog_license_must_be_declared_by_payload_json() {
        let data = payload(vec![file("content/file"), file("sources.json")]);

        assert!(matches!(
            PayloadManifest::parse_and_validate(&data, &entry()),
            Err(PayloadError::InvalidManifest(_))
        ));
    }

    #[test]
    fn payload_manifest_schema_rejects_unknown_fields() {
        let mut document: Value = serde_json::from_slice(&payload(vec![
            file("licenses/GPL-2.0.txt"),
            file("licenses/Linux-syscall-note.txt"),
            file("sources.json"),
        ]))
        .unwrap();
        document["unexpected"] = json!(true);

        assert!(matches!(
            PayloadManifest::parse_and_validate(&serde_json::to_vec(&document).unwrap(), &entry()),
            Err(PayloadError::InvalidManifest(_))
        ));
    }

    #[test]
    fn sources_must_correspond_exactly_to_catalog_sources() {
        let mut document = sources();
        document["sources"].as_array_mut().unwrap().push(json!({
            "url": "https://github.com/attacker/extra",
            "commit": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "version": "extra",
            "paths": ["extra"],
            "licenses": [{"path": "extra", "spdx": "GPL-2.0"}],
            "sha256": ZERO
        }));

        assert!(matches!(
            SourceManifest::parse_and_validate(&serde_json::to_vec(&document).unwrap(), &entry()),
            Err(PayloadError::InvalidManifest(_))
        ));
    }

    #[test]
    fn every_source_requires_selected_paths_licenses_and_a_digest() {
        for missing in ["paths", "licenses", "sha256"] {
            let mut document = sources();
            document["sources"][0]
                .as_object_mut()
                .unwrap()
                .remove(missing);
            assert!(matches!(
                SourceManifest::parse_and_validate(
                    &serde_json::to_vec(&document).unwrap(),
                    &entry()
                ),
                Err(PayloadError::InvalidManifest(_))
            ));
        }
    }

    #[test]
    fn every_selected_source_path_has_an_associated_declared_license() {
        SourceManifest::parse_and_validate(&serde_json::to_vec(&sources()).unwrap(), &entry())
            .unwrap();

        for mutation in ["missing", "misassociated", "undeclared"] {
            let mut document = sources();
            match mutation {
                "missing" => {
                    document["sources"][0]["licenses"]
                        .as_array_mut()
                        .unwrap()
                        .pop();
                }
                "misassociated" => {
                    document["sources"][0]["licenses"][0]["path"] =
                        "include/uapi/misc/d3dkmthk.h".into();
                }
                "undeclared" => {
                    document["sources"][0]["licenses"][0]["spdx"] = "Proprietary".into();
                }
                _ => unreachable!(),
            }
            assert!(matches!(
                SourceManifest::parse_and_validate(
                    &serde_json::to_vec(&document).unwrap(),
                    &entry()
                ),
                Err(PayloadError::InvalidManifest(_))
            ));
        }
    }

    #[test]
    fn d3dkmthk_requires_the_linux_syscall_license_exception() {
        let mut document = sources();
        document["sources"][0]["licenses"][1]["spdx"] = "GPL-2.0".into();

        assert!(matches!(
            SourceManifest::parse_and_validate(&serde_json::to_vec(&document).unwrap(), &entry()),
            Err(PayloadError::InvalidManifest(_))
        ));
    }

    #[test]
    fn source_manifest_schema_rejects_unknown_fields() {
        let mut document = sources();
        document["sources"][0]["unexpected"] = json!(true);

        assert!(matches!(
            SourceManifest::parse_and_validate(&serde_json::to_vec(&document).unwrap(), &entry()),
            Err(PayloadError::InvalidManifest(_))
        ));
    }

    #[test]
    fn overlay_licenses_must_be_declared_by_the_catalog() {
        let mut document = sources();
        document["overlays"] = json!([{
            "path": "content/overlay",
            "sha256": ZERO,
            "license": "Proprietary",
            "author": "VMLord contributors"
        }]);

        assert!(matches!(
            SourceManifest::parse_and_validate(&serde_json::to_vec(&document).unwrap(), &entry()),
            Err(PayloadError::InvalidManifest(_))
        ));
    }

    #[test]
    fn every_overlay_path_and_digest_must_match_a_prepared_file() {
        let payload = PayloadManifest::parse_and_validate(
            &payload(vec![
                file("content/overlay"),
                file("licenses/GPL-2.0.txt"),
                file("licenses/Linux-syscall-note.txt"),
                file("sources.json"),
            ]),
            &entry(),
        )
        .unwrap();

        for (path, sha256) in [
            ("content/missing", ZERO),
            (
                "content/overlay",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ),
        ] {
            let mut document = sources();
            document["overlays"] = json!([{
                "path": path,
                "sha256": sha256,
                "license": "GPL-2.0",
                "author": "VMLord contributors"
            }]);
            let sources = SourceManifest::parse_and_validate(
                &serde_json::to_vec(&document).unwrap(),
                &entry(),
            )
            .unwrap();

            assert!(matches!(
                sources.validate_prepared_files(&payload),
                Err(PayloadError::InvalidManifest(_))
            ));
        }
    }
}
