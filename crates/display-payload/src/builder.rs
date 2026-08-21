//! Turning a prepared display tree into the pair a release ships.
//!
//! The mechanics -- what a prepared tree may contain, how it is hashed, how
//! the archive is written deterministically -- are `vmlord_payload::builder`'s.
//! What is here is the display payload's own: the recipe that says what cannot
//! be derived from the files, and the entry that describes the archive once it
//! has been written and measured.

use std::{fs, path::Path};

use serde::{Deserialize, Serialize};
use vmlord_payload::{
    PayloadError, Sha256Digest,
    builder::{
        BuiltArtifact, PAYLOAD_MANIFEST_LIMIT, PackPaths, PreparedInput, collect_files,
        validate_paths, write_archive,
    },
};

use crate::{
    DisplayCatalogEntry, DisplayManifest, DisplaySources, DisplayTarget, License, MODULE_DIRECTORY,
    PayloadVersion, ProtocolRange, Source,
};

/// Where a display payload pack reads from and writes to.
pub struct PackRequest<'a> {
    pub prepared_directory: &'a Path,
    pub recipe_path: &'a Path,
    pub archive_path: &'a Path,
    pub catalog_entry_path: &'a Path,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PackRecipe {
    schema_version: u32,
    version: PayloadVersion,
    target: DisplayTarget,
    proven_on: String,
    protocol: ProtocolRange,
    sources: Vec<Source>,
    licenses: Vec<License>,
}

impl PackRecipe {
    /// The payload's ID, derived rather than given.
    ///
    /// A recipe that could name its own ID could name one that does not match
    /// its version, and the catalog would then refuse what the pack produced.
    fn payload_id(&self) -> String {
        format!(
            "display-{}-{}-{}-{}",
            self.target.distribution, self.target.release, self.target.architecture, self.version
        )
    }
}

#[derive(Serialize)]
struct DisplayManifestDocument<'a> {
    schema_version: u32,
    payload_id: &'a str,
    version: &'a PayloadVersion,
    target: &'a DisplayTarget,
    files: Vec<PreparedFileDocument<'a>>,
}

#[derive(Serialize)]
struct PreparedFileDocument<'a> {
    path: &'a str,
    size: u64,
    sha256: &'a Sha256Digest,
}

/// Packs `prepared_directory` into an archive and the entry describing it.
///
/// The entry is written last and from the archive's measured digest, so it can
/// never claim a digest of something that includes the claim.
///
/// # Errors
///
/// [`PayloadError`] for a recipe at another schema version, a tree with no
/// module in it, a tree that cannot be packed, or an archive that cannot be
/// written.
pub fn pack(request: PackRequest<'_>) -> Result<BuiltArtifact, PayloadError> {
    validate_paths(&PackPaths {
        prepared_directory: request.prepared_directory,
        recipe_path: request.recipe_path,
        archive_path: request.archive_path,
        catalog_entry_path: request.catalog_entry_path,
    })?;

    let recipe_bytes = fs::read(request.recipe_path).map_err(|error| {
        PayloadError::io(
            "read display payload recipe",
            request.recipe_path.into(),
            error,
        )
    })?;
    let recipe: PackRecipe = serde_json::from_slice(&recipe_bytes)
        .map_err(|error| PayloadError::InvalidCatalog(error.to_string()))?;
    if recipe.schema_version != 1 {
        return Err(PayloadError::InvalidCatalog(
            "unknown display payload recipe schema version".into(),
        ));
    }
    if recipe.target.distribution.is_empty()
        || recipe.target.release.is_empty()
        || recipe.target.architecture.is_empty()
        || recipe.proven_on.is_empty()
    {
        return Err(PayloadError::InvalidCatalog(
            "a display payload recipe must name its guest and the kernel it was proven on".into(),
        ));
    }

    let files = collect_files(request.prepared_directory)?;
    if !files
        .iter()
        .any(|file| file.archive_path.starts_with(MODULE_DIRECTORY))
    {
        return Err(PayloadError::InvalidManifest(format!(
            "a display payload must carry a module under {MODULE_DIRECTORY}"
        )));
    }

    let payload_id = recipe.payload_id();
    let manifest = DisplayManifestDocument {
        schema_version: 1,
        payload_id: &payload_id,
        version: &recipe.version,
        target: &recipe.target,
        files: files
            .iter()
            .map(|file| PreparedFileDocument {
                path: &file.archive_path,
                size: file.size,
                sha256: &file.sha256,
            })
            .collect(),
    };
    let mut manifest_bytes = serde_json::to_vec(&manifest)
        .map_err(|error| PayloadError::InvalidManifest(error.to_string()))?;
    manifest_bytes.push(b'\n');
    let manifest_size = u64::try_from(manifest_bytes.len()).unwrap_or(u64::MAX);
    if manifest_size > PAYLOAD_MANIFEST_LIMIT {
        return Err(PayloadError::LimitExceeded {
            subject: "payload.json",
            limit: PAYLOAD_MANIFEST_LIMIT,
            actual: manifest_size,
        });
    }
    let payload_manifest_sha256 = Sha256Digest::hash_reader(manifest_bytes.as_slice())?;

    write_archive(request.archive_path, &files, &manifest_bytes)?;
    let archive_size = fs::metadata(request.archive_path)
        .map_err(|error| {
            PayloadError::io(
                "measure payload archive",
                request.archive_path.into(),
                error,
            )
        })?
        .len();
    let archive_sha256 =
        Sha256Digest::hash_reader(fs::File::open(request.archive_path).map_err(|error| {
            PayloadError::io("read payload archive", request.archive_path.into(), error)
        })?)?;

    let expanded_bytes = files
        .iter()
        .try_fold(manifest_bytes.len() as u64, |total, file| {
            total.checked_add(file.size).ok_or_else(|| {
                PayloadError::InvalidManifest("expanded payload size overflow".into())
            })
        })?;
    // The archive's own length is the floor under this ceiling: a small ZIP can
    // be larger than its expanded members because of headers, and a limit below
    // that would refuse the payload this very run just built.
    let expanded_size_limit = expanded_bytes.max(archive_size);
    let file_count = u64::try_from(files.len()).unwrap_or(u64::MAX);

    let entry = entry_document(
        &recipe,
        &payload_id,
        expanded_size_limit,
        file_count,
        &archive_sha256,
        &payload_manifest_sha256,
    );
    let entry_bytes = serde_json::to_vec_pretty(&entry)
        .map_err(|error| PayloadError::InvalidCatalog(error.to_string()))?;
    // Validated from the exact bytes that are about to be written, so what the
    // file says and what was checked cannot differ.
    let validated_entry = DisplayCatalogEntry::from_json(&entry_bytes)?;
    DisplayManifest::parse_and_validate(&manifest_bytes, &validated_entry)?;
    let sources_bytes = read_prepared_file(&files, "sources.json")?;
    DisplaySources::parse_and_validate(&sources_bytes, &validated_entry)?;

    fs::write(request.catalog_entry_path, entry_bytes).map_err(|error| {
        PayloadError::io(
            "write catalog entry",
            request.catalog_entry_path.into(),
            error,
        )
    })?;

    Ok(BuiltArtifact {
        archive_size,
        expanded_size: expanded_bytes,
        file_count,
        archive_sha256,
        payload_manifest_sha256,
    })
}

fn entry_document(
    recipe: &PackRecipe,
    payload_id: &str,
    expanded_size_limit: u64,
    file_count: u64,
    archive_sha256: &Sha256Digest,
    payload_manifest_sha256: &Sha256Digest,
) -> serde_json::Value {
    serde_json::json!({
        "schema_version": 1,
        "payload_id": payload_id,
        "version": recipe.version,
        "target": recipe.target,
        "proven_on": recipe.proven_on,
        "protocol": recipe.protocol,
        "archive_sha256": archive_sha256,
        "payload_manifest_sha256": payload_manifest_sha256,
        "expanded_size_limit": expanded_size_limit,
        "file_count_limit": file_count,
        "sources": recipe.sources,
        "licenses": recipe.licenses,
    })
}

fn read_prepared_file(files: &[PreparedInput], path: &str) -> Result<Vec<u8>, PayloadError> {
    let file = files
        .iter()
        .find(|file| file.archive_path == path)
        .ok_or_else(|| {
            PayloadError::InvalidManifest(format!("prepared directory has no {path}"))
        })?;
    fs::read(&file.host_path)
        .map_err(|error| PayloadError::io("read prepared file", file.host_path.clone(), error))
}
