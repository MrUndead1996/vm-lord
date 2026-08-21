use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::{CatalogEntry, GuestTarget, MesaPolicy, PayloadError, PreparedFile, Sha256Digest};
use vmlord_payload::validate_path;

const D3DKMTHK_PATH: &str = "include/uapi/misc/d3dkmthk.h";
const D3DKMTHK_LICENSE: &str = "GPL-2.0 WITH Linux-syscall-note";

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

        Ok(Self { files: value.files })
    }

    pub fn files(&self) -> &[PreparedFile] {
        &self.files
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SourceManifestDocument {
    schema_version: u32,
    target: GuestTarget,
    mesa_policy: MesaPolicy,
    sources: Vec<SourceRecord>,
    overlays: Vec<OverlayRecord>,
}

/// One upstream a payload owes something to.
///
/// Untagged rather than `#[serde(tag = "kind")]` on purpose: serde does not honour
/// `deny_unknown_fields` on an internally tagged enum, and refusing a field nobody
/// meant to write is worth a less specific error message when both variants fail.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
enum SourceRecord {
    Checkout(CheckoutRecord),
    Built(BuiltRecord),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum CheckoutKind {
    Checkout,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum BuiltKind {
    Built,
}

/// Upstream files that travelled into the payload byte for byte.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CheckoutRecord {
    kind: CheckoutKind,
    url: String,
    commit: String,
    version: String,
    paths: Vec<String>,
    licenses: Vec<SourceLicenseRecord>,
    sha256: Sha256Digest,
}

/// A tree that was compiled, whose members correspond to no upstream file.
///
/// `output` is a path in the payload rather than upstream, `licenses` are bare SPDX
/// identifiers because attributing one shared object to one upstream file is not
/// meaningful, and `sha256` covers what shipped -- which makes it the one digest here
/// that can be checked rather than believed.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BuiltRecord {
    kind: BuiltKind,
    url: String,
    commit: String,
    version: String,
    output: String,
    licenses: Vec<String>,
    inputs: Vec<SourceInputRecord>,
    sha256: Sha256Digest,
}

/// An upstream that ended up inside a built tree's binaries.
///
/// No digest of its own: its bytes are not separable from the output's. The commit is
/// what makes it auditable, and it reaches the catalog as an ordinary source row.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SourceInputRecord {
    url: String,
    commit: String,
    version: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
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
        if doc.schema_version != 2
            || doc.target != *entry.target()
            || doc.mesa_policy != *entry.mesa_policy()
        {
            return Err(PayloadError::InvalidManifest(
                "sources.json does not exactly match catalog provenance".into(),
            ));
        }

        let rows = catalog_rows(&doc.sources);
        if rows.len() != entry.sources().len()
            || rows.iter().zip(entry.sources()).any(|(row, expected)| {
                row.0 != expected.url || row.1 != expected.commit || row.2 != expected.version
            })
        {
            return Err(PayloadError::InvalidManifest(
                "sources.json does not exactly match catalog provenance".into(),
            ));
        }

        for source in &doc.sources {
            match source {
                SourceRecord::Checkout(checkout) => validate_checkout(checkout, entry)?,
                SourceRecord::Built(built) => validate_built(built, entry)?,
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

    pub(crate) fn validate_overlays_against(
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

impl vmlord_payload::PayloadFiles for PayloadManifest {
    fn files(&self) -> &[PreparedFile] {
        self.files()
    }
}

impl serde::Serialize for SourceManifest {
    /// The document as it was read: the wrapper exists to prove the document
    /// was validated, and nothing about that is worth writing out.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.document.serialize(serializer)
    }
}

impl vmlord_payload::PayloadSources<PayloadManifest> for SourceManifest {
    fn validate_prepared_files(&self, manifest: &PayloadManifest) -> Result<(), PayloadError> {
        self.validate_overlays_against(manifest)
    }
}

/// The `url`/`commit`/`version` rows one source contributes to a catalog entry.
///
/// A built record contributes itself and every input, in that order: what is inside the
/// binaries is part of what the payload carries, and the catalog is where a person looks
/// for that.
fn catalog_rows(sources: &[SourceRecord]) -> Vec<(&str, &str, &str)> {
    let mut rows = Vec::new();
    for source in sources {
        match source {
            SourceRecord::Checkout(checkout) => {
                rows.push((
                    checkout.url.as_str(),
                    checkout.commit.as_str(),
                    checkout.version.as_str(),
                ));
            }
            SourceRecord::Built(built) => {
                rows.push((
                    built.url.as_str(),
                    built.commit.as_str(),
                    built.version.as_str(),
                ));
                for input in &built.inputs {
                    rows.push((
                        input.url.as_str(),
                        input.commit.as_str(),
                        input.version.as_str(),
                    ));
                }
            }
        }
    }
    rows
}

fn validate_checkout(checkout: &CheckoutRecord, entry: &CatalogEntry) -> Result<(), PayloadError> {
    if checkout.paths.is_empty() || checkout.licenses.len() != checkout.paths.len() {
        return Err(PayloadError::InvalidManifest(
            "sources.json does not exactly match catalog provenance".into(),
        ));
    }
    let mut previous = "";
    for path in &checkout.paths {
        validate_path(path)?;
        if !previous.is_empty() && previous >= path.as_str() {
            return Err(PayloadError::InvalidManifest(
                "selected source paths must be unique and sorted".into(),
            ));
        }
        previous = path;
    }
    for (path, license) in checkout.paths.iter().zip(&checkout.licenses) {
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
    Ok(())
}

fn validate_built(built: &BuiltRecord, entry: &CatalogEntry) -> Result<(), PayloadError> {
    validate_path(&built.output)?;
    if built.licenses.is_empty()
        || !built
            .licenses
            .iter()
            .all(|spdx| license_expression_is_declared(spdx, entry))
    {
        return Err(PayloadError::InvalidManifest(
            "a built source must declare licences the catalog knows".into(),
        ));
    }
    Ok(())
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
    #[derive(Serialize)]
    struct CacheProvenance<'a> {
        schema_version: u32,
        payload_id: &'a str,
        archive_sha256: &'a Sha256Digest,
        payload_manifest_sha256: &'a Sha256Digest,
        catalog_entry: &'a CatalogEntry,
        sources: &'a SourceManifestDocument,
    }

    let value = CacheProvenance {
        schema_version: 1,
        payload_id: entry.payload_id(),
        archive_sha256: entry.archive_sha256(),
        payload_manifest_sha256: entry.payload_manifest_sha256(),
        catalog_entry: entry,
        sources: &sources.document,
    };
    let mut bytes =
        serde_json::to_vec(&value).map_err(|e| PayloadError::InvalidManifest(e.to_string()))?;
    bytes.push(b'\n');
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::{CatalogEntry, PayloadError, PayloadManifest, SourceManifest};

    use super::cache_provenance;

    const ZERO: &str = "0000000000000000000000000000000000000000000000000000000000000000";
    const COMMIT: &str = "14794180686c2fb6307fbe359c359bec765249f3";

    fn entry() -> CatalogEntry {
        let entry_document = json!({
                "payload_id": "p",
                "target": {
                    "distribution": "ubuntu",
                    "release": "26.04",
                    "architecture": "amd64",
                    "kernel_release": "k",
                    "payload_abi": 1
                },
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
        });
        crate::test_entry(entry_document)
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
            "schema_version": 2,
            "target": {
                "distribution": "ubuntu",
                "release": "26.04",
                "architecture": "amd64",
                "kernel_release": "k",
                "payload_abi": 1
            },
            "mesa_policy": "bundled",
            "sources": [{
                "kind": "checkout",
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

    fn built_sources() -> Value {
        json!({
            "schema_version": 2,
            "target": {
                "distribution": "ubuntu",
                "release": "26.04",
                "architecture": "amd64",
                "kernel_release": "k",
                "payload_abi": 1
            },
            "mesa_policy": "bundled",
            "sources": [{
                "kind": "built",
                "url": "https://gitlab.freedesktop.org/mesa/mesa",
                "commit": COMMIT,
                "version": "26.1.2",
                "output": "content/mesa",
                "licenses": ["GPL-2.0"],
                "inputs": [{
                    "url": "https://github.com/microsoft/DirectX-Headers",
                    "commit": COMMIT,
                    "version": "v1.615.0"
                }],
                "sha256": ZERO
            }],
            "overlays": []
        })
    }

    /// A built record's inputs are sources in their own right: they are in the
    /// binaries, and the catalog is where a person looks for what is in a payload.
    fn built_entry() -> CatalogEntry {
        CatalogEntry::from_json(
            &serde_json::to_vec(&json!({
                "schema_version": 2,
                "payload_id": "p",
                "target": {
                    "distribution": "ubuntu",
                    "release": "26.04",
                    "architecture": "amd64",
                    "kernel_release": "k",
                    "payload_abi": 1
                },
                "expanded_size_limit": 2,
                "file_count_limit": 4,
                "archive_sha256": ZERO,
                "payload_manifest_sha256": ZERO,
                "required_renderers": ["d3d12-gallium", "dzn-vulkan"],
                "mesa_policy": "bundled",
                "sources": [
                    {
                        "url": "https://gitlab.freedesktop.org/mesa/mesa",
                        "commit": COMMIT,
                        "version": "26.1.2"
                    },
                    {
                        "url": "https://github.com/microsoft/DirectX-Headers",
                        "commit": COMMIT,
                        "version": "v1.615.0"
                    }
                ],
                "licenses": [{"spdx": "GPL-2.0", "path": "licenses/GPL-2.0.txt"}]
            }))
            .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn a_built_source_contributes_itself_and_its_inputs_to_the_catalog() {
        let entry = built_entry();
        let document = serde_json::to_vec(&built_sources()).unwrap();

        SourceManifest::parse_and_validate(&document, &entry)
            .expect("a built record and its inputs are the catalog's two source rows");
    }

    #[test]
    fn a_built_source_that_hides_an_input_from_the_catalog_is_refused() {
        let entry = built_entry();
        let mut document = built_sources();
        document["sources"][0]["inputs"] = json!([]);

        let error =
            SourceManifest::parse_and_validate(&serde_json::to_vec(&document).unwrap(), &entry)
                .unwrap_err();

        assert!(matches!(error, PayloadError::InvalidManifest(_)));
    }

    #[test]
    fn a_built_source_must_declare_licences_the_catalog_knows() {
        let entry = built_entry();
        let mut document = built_sources();
        document["sources"][0]["licenses"] = json!(["MIT"]);

        let error =
            SourceManifest::parse_and_validate(&serde_json::to_vec(&document).unwrap(), &entry)
                .unwrap_err();

        assert!(matches!(error, PayloadError::InvalidManifest(_)));
    }

    #[test]
    fn a_built_source_needs_an_output_a_licence_and_a_digest() {
        let entry = built_entry();
        for (field, value) in [
            ("output", json!("")),
            ("licenses", json!([])),
            ("output", json!("../escape")),
        ] {
            let mut document = built_sources();
            document["sources"][0][field] = value;

            let error =
                SourceManifest::parse_and_validate(&serde_json::to_vec(&document).unwrap(), &entry)
                    .unwrap_err();

            assert!(matches!(error, PayloadError::InvalidManifest(_)));
        }
    }

    /// `kind` is the only thing separating the two variants, so a record that declares
    /// one kind and carries the other's fields must be refused rather than quietly read
    /// as the variant it happens to fit.
    #[test]
    fn a_source_record_whose_kind_disagrees_with_its_shape_is_refused() {
        let entry = built_entry();
        let mut document = built_sources();
        document["sources"][0]["kind"] = json!("checkout");

        let error =
            SourceManifest::parse_and_validate(&serde_json::to_vec(&document).unwrap(), &entry)
                .unwrap_err();

        let PayloadError::InvalidManifest(message) = error else {
            panic!("a record must be refused, not read as the kind it does not declare");
        };
        assert!(message.contains("untagged enum SourceRecord"), "{message}");
    }

    #[test]
    fn a_sources_document_at_version_one_is_no_longer_understood() {
        let entry = entry();
        let mut document = sources();
        document["schema_version"] = json!(1);

        let error =
            SourceManifest::parse_and_validate(&serde_json::to_vec(&document).unwrap(), &entry)
                .unwrap_err();

        assert!(matches!(error, PayloadError::InvalidManifest(_)));
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
    fn sources_must_match_the_catalog_target_and_mesa_policy() {
        for mutation in ["target", "mesa"] {
            let mut document = sources();
            match mutation {
                "target" => document["target"]["kernel_release"] = "other".into(),
                "mesa" => document["mesa_policy"] = "distro".into(),
                _ => unreachable!(),
            }

            assert!(
                matches!(
                    SourceManifest::parse_and_validate(
                        &serde_json::to_vec(&document).unwrap(),
                        &entry()
                    ),
                    Err(PayloadError::InvalidManifest(_))
                ),
                "accepted mismatched {mutation} provenance"
            );
        }
    }

    #[test]
    fn cache_provenance_contains_the_validated_catalog_entry_and_source_manifest() {
        let entry = entry();
        let source_document = sources();
        let sources = SourceManifest::parse_and_validate(
            &serde_json::to_vec(&source_document).unwrap(),
            &entry,
        )
        .unwrap();

        let first = cache_provenance(&entry, &sources).unwrap();
        let second = cache_provenance(&entry, &sources).unwrap();
        let value: Value = serde_json::from_slice(&first).unwrap();

        assert_eq!(first, second);
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["payload_id"], "p");
        assert_eq!(value["archive_sha256"], entry.archive_sha256().as_hex());
        assert_eq!(
            value["payload_manifest_sha256"],
            entry.payload_manifest_sha256().as_hex()
        );
        assert_eq!(value["catalog_entry"]["payload_id"], "p");
        assert_eq!(value["catalog_entry"]["target"]["kernel_release"], "k");
        assert_eq!(value["catalog_entry"]["mesa_policy"], "bundled");
        assert_eq!(value["sources"], source_document);
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
                sources.validate_overlays_against(&payload),
                Err(PayloadError::InvalidManifest(_))
            ));
        }
    }
}
