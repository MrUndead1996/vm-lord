use std::{
    fs::{self, File},
    io,
    path::Path,
};

use serde::{Deserialize, Serialize};
use vmlord_payload::builder::{
    BuiltArtifact, PAYLOAD_MANIFEST_LIMIT, PackPaths, PreparedInput, collect_files, validate_paths,
    write_archive,
};

use crate::{
    CatalogEntry, GuestTarget, MesaPolicy, PayloadError, PayloadManifest, RendererCapability,
    Sha256Digest, SourceManifest,
};

pub use vmlord_payload::builder::BuiltArtifact as BuiltGpuArtifact;

/// Where a GPU payload pack reads from and writes to.
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
    payload_id: String,
    target: GuestTarget,
    required_renderers: Vec<RendererCapability>,
    mesa_policy: MesaPolicy,
    sources: Vec<RecipeSource>,
    overlays: Vec<RecipeOverlay>,
    licenses: Vec<RecipeLicense>,
}

/// One upstream a payload owes something to.
///
/// Untagged rather than `#[serde(tag = "kind")]` on purpose: serde does not honour
/// `deny_unknown_fields` on an internally tagged enum, and refusing a field nobody
/// meant to write is worth a less specific error message when both variants fail.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
enum RecipeSource {
    Checkout(RecipeCheckout),
    Built(RecipeBuilt),
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
struct RecipeCheckout {
    kind: CheckoutKind,
    url: String,
    commit: String,
    version: String,
    paths: Vec<String>,
    licenses: Vec<RecipeSourceLicense>,
    sha256: Sha256Digest,
}

/// A tree that was compiled, whose members correspond to no upstream file.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RecipeBuilt {
    kind: BuiltKind,
    url: String,
    commit: String,
    version: String,
    output: String,
    licenses: Vec<String>,
    inputs: Vec<RecipeSourceInput>,
    /// Absent in a payload prepared before patches existed, which is the same
    /// statement as an empty list: nothing was changed before compiling.
    #[serde(default)]
    patches: Vec<RecipeSourcePatch>,
    sha256: Sha256Digest,
}

/// An upstream that ended up inside a built tree's binaries.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RecipeSourceInput {
    url: String,
    commit: String,
    version: String,
}

/// A change this repository made to an upstream tree before compiling it.
///
/// It contributes no catalog row: the upstream it patches is already a row, and a patch
/// is not a source anyone can fetch. What it carries is the digest, which is the only
/// way to tell one revision of a patch from another after the binaries exist.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RecipeSourcePatch {
    file: String,
    sha256: Sha256Digest,
    author: String,
    reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RecipeSourceLicense {
    path: String,
    spdx: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RecipeOverlay {
    path: String,
    sha256: Sha256Digest,
    license: String,
    author: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RecipeLicense {
    spdx: String,
    path: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparedSources {
    schema_version: u32,
    target: GuestTarget,
    mesa_policy: MesaPolicy,
    sources: Vec<RecipeSource>,
    overlays: Vec<RecipeOverlay>,
}

#[derive(Serialize)]
struct PayloadManifestDocument<'a> {
    schema_version: u32,
    payload_id: &'a str,
    target: &'a GuestTarget,
    files: Vec<PreparedFileDocument<'a>>,
}

#[derive(Serialize)]
struct PreparedFileDocument<'a> {
    path: &'a str,
    size: u64,
    sha256: &'a Sha256Digest,
}

pub fn pack(request: PackRequest<'_>) -> Result<BuiltArtifact, PayloadError> {
    validate_paths(&PackPaths {
        prepared_directory: request.prepared_directory,
        recipe_path: request.recipe_path,
        archive_path: request.archive_path,
        catalog_entry_path: request.catalog_entry_path,
    })?;
    let recipe_bytes = fs::read(request.recipe_path).map_err(|error| {
        PayloadError::io("read GPU payload recipe", request.recipe_path.into(), error)
    })?;
    let recipe: PackRecipe = serde_json::from_slice(&recipe_bytes)
        .map_err(|error| PayloadError::InvalidCatalog(error.to_string()))?;
    if recipe.schema_version != 2 {
        return Err(PayloadError::InvalidCatalog(
            "unknown GPU payload recipe schema version".into(),
        ));
    }
    if recipe.target.distribution.is_empty()
        || recipe.target.release.is_empty()
        || recipe.target.architecture.is_empty()
        || recipe.target.kernel_release.is_empty()
    {
        return Err(PayloadError::InvalidCatalog(
            "GPU payload recipe target must name every guest dimension".into(),
        ));
    }

    let files = collect_files(request.prepared_directory)?;
    validate_prepared_provenance(&recipe, &files)?;

    let manifest = PayloadManifestDocument {
        schema_version: 1,
        payload_id: &recipe.payload_id,
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
        Sha256Digest::hash_reader(File::open(request.archive_path).map_err(|error| {
            PayloadError::io("read payload archive", request.archive_path.into(), error)
        })?)?;
    let expanded_bytes = files
        .iter()
        .try_fold(manifest_bytes.len() as u64, |total, file| {
            total.checked_add(file.size).ok_or_else(|| {
                PayloadError::InvalidManifest("expanded payload size overflow".into())
            })
        })?;
    // Catalog limits are defensive ceilings, and the archive's own length is
    // the floor under this one: a tiny ZIP can be larger than its expanded
    // members because of headers, and a ceiling below that would refuse the
    // payload this very run just built.
    let expanded_size_limit = expanded_bytes.max(archive_size);
    let file_count = u64::try_from(files.len()).unwrap_or(u64::MAX);
    let entry = catalog_entry(
        &recipe,
        expanded_size_limit,
        file_count,
        &archive_sha256,
        &payload_manifest_sha256,
    );
    let entry_bytes = serde_json::to_vec_pretty(&entry)
        .map_err(|error| PayloadError::InvalidCatalog(error.to_string()))?;
    // Validated from the exact bytes that are about to be written, so what
    // the file says and what was checked cannot differ.
    let validated_entry = CatalogEntry::from_json(&entry_bytes)?;
    PayloadManifest::parse_and_validate(&manifest_bytes, &validated_entry)?;
    let sources_bytes = read_prepared_file(&files, "sources.json")?;
    SourceManifest::parse_and_validate(&sources_bytes, &validated_entry)?;

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

fn catalog_entry(
    recipe: &PackRecipe,
    expanded_size: u64,
    file_count: u64,
    archive_sha256: &Sha256Digest,
    payload_manifest_sha256: &Sha256Digest,
) -> serde_json::Value {
    let mut sources = Vec::new();
    for source in &recipe.sources {
        let (url, commit, version) = match source {
            RecipeSource::Checkout(checkout) => {
                (&checkout.url, &checkout.commit, &checkout.version)
            }
            RecipeSource::Built(built) => (&built.url, &built.commit, &built.version),
        };
        sources.push(serde_json::json!({
            "url": url, "commit": commit, "version": version,
        }));
        if let RecipeSource::Built(built) = source {
            for input in &built.inputs {
                sources.push(serde_json::json!({
                    "url": input.url,
                    "commit": input.commit,
                    "version": input.version,
                }));
            }
        }
    }
    serde_json::json!({
        "schema_version": 2,
        "payload_id": recipe.payload_id,
        "target": recipe.target,
        "expanded_size_limit": expanded_size,
        "file_count_limit": file_count,
        "archive_sha256": archive_sha256,
        "payload_manifest_sha256": payload_manifest_sha256,
        "required_renderers": recipe.required_renderers,
        "mesa_policy": recipe.mesa_policy,
        "sources": sources,
        "licenses": recipe.licenses,
    })
}

fn validate_prepared_provenance(
    recipe: &PackRecipe,
    files: &[PreparedInput],
) -> Result<(), PayloadError> {
    let sources_bytes = read_prepared_file(files, "sources.json")?;
    let prepared: PreparedSources = serde_json::from_slice(&sources_bytes)
        .map_err(|error| PayloadError::InvalidManifest(error.to_string()))?;
    if prepared.schema_version != 2
        || prepared.target != recipe.target
        || prepared.mesa_policy != recipe.mesa_policy
        || prepared.sources != recipe.sources
        || prepared.overlays != recipe.overlays
    {
        return Err(PayloadError::InvalidManifest(
            "prepared sources.json does not exactly match recipe provenance".into(),
        ));
    }
    for source in &recipe.sources {
        let RecipeSource::Built(built) = source else {
            continue;
        };
        let digest = built_output_digest(files, &built.output)?;
        if digest != built.sha256 {
            return Err(PayloadError::InvalidManifest(format!(
                "the built tree at {} is not what the recipe recorded",
                built.output
            )));
        }
    }
    for overlay in &recipe.overlays {
        if !recipe
            .licenses
            .iter()
            .any(|license| license.spdx == overlay.license)
        {
            return Err(PayloadError::InvalidManifest(format!(
                "overlay license is not declared by the recipe: {}",
                overlay.license
            )));
        }
        let file = find_prepared_file(files, &overlay.path)?;
        if file.sha256 != overlay.sha256 {
            return Err(PayloadError::InvalidManifest(format!(
                "prepared overlay digest does not match recipe: {}",
                overlay.path
            )));
        }
    }
    for license in &recipe.licenses {
        let file = find_prepared_file(files, &license.path)?;
        if file.size == 0 {
            return Err(PayloadError::InvalidManifest(format!(
                "prepared license is empty: {}",
                license.path
            )));
        }
    }
    Ok(())
}

/// Digests the tree a built source produced, by the rule its record claims.
///
/// The same shape as the upstream digest -- each file, sorted by path, contributed as
/// path, NUL, contents -- but over files that are in this builder's hands, which makes
/// this the one digest in the document it can verify rather than record.
fn built_output_digest(
    files: &[PreparedInput],
    output: &str,
) -> Result<Sha256Digest, PayloadError> {
    let prefix = format!("{output}/");
    let mut members = files
        .iter()
        .filter(|file| file.archive_path.starts_with(&prefix))
        .peekable();
    if members.peek().is_none() {
        return Err(PayloadError::InvalidManifest(format!(
            "the built tree at {output} holds nothing"
        )));
    }

    let mut hasher = Sha256Digest::hasher();
    for file in members {
        hasher.update(file.archive_path.as_bytes());
        hasher.update(b"\0");
        let mut input = File::open(&file.host_path).map_err(|error| {
            PayloadError::io("read prepared file", file.host_path.clone(), error)
        })?;
        io::copy(&mut input, &mut hasher).map_err(|error| {
            PayloadError::io("read prepared file", file.host_path.clone(), error)
        })?;
    }
    Ok(hasher.finish())
}

fn read_prepared_file(
    files: &[PreparedInput],
    archive_path: &str,
) -> Result<Vec<u8>, PayloadError> {
    let file = find_prepared_file(files, archive_path)?;
    fs::read(&file.host_path)
        .map_err(|error| PayloadError::io("read prepared file", file.host_path.clone(), error))
}

fn find_prepared_file<'a>(
    files: &'a [PreparedInput],
    archive_path: &str,
) -> Result<&'a PreparedInput, PayloadError> {
    files
        .iter()
        .find(|file| file.archive_path == archive_path)
        .ok_or_else(|| {
            PayloadError::InvalidManifest(format!(
                "recipe references missing prepared file: {archive_path}"
            ))
        })
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicBool, AtomicU64, Ordering},
    };

    use zip::{CompressionMethod, ZipArchive};

    use crate::{CatalogEntry, PayloadError, Sha256Digest};

    use vmlord_payload::builder::validate_archive_path;

    use super::{PackRequest, pack};

    static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

    struct PreparedFixture {
        root: PathBuf,
        prepared: PathBuf,
        recipe: PathBuf,
    }

    impl PreparedFixture {
        fn new(label: &str) -> Self {
            let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "vmlord-gpu-payload-builder-{label}-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&root).unwrap();
            let prepared = root.join("prepared");
            copy_directory(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/prepared"),
                &prepared,
            );
            let recipe = root.join("recipe.json");
            fs::copy(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/recipe.json"),
                &recipe,
            )
            .unwrap();
            Self {
                root,
                prepared,
                recipe,
            }
        }

        fn request<'a>(&'a self, archive: &'a Path, catalog_entry: &'a Path) -> PackRequest<'a> {
            PackRequest {
                prepared_directory: &self.prepared,
                recipe_path: &self.recipe,
                archive_path: archive,
                catalog_entry_path: catalog_entry,
            }
        }

        fn rewrite_recipe(&self, update: impl FnOnce(&mut serde_json::Value)) {
            rewrite_json(&self.recipe, update);
        }
    }

    impl Drop for PreparedFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn copy_directory(source: impl AsRef<Path>, destination: &Path) {
        fs::create_dir(destination).unwrap();
        for entry in fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            let destination = destination.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_directory(entry.path(), &destination);
            } else {
                fs::copy(entry.path(), destination).unwrap();
            }
        }
    }

    fn rewrite_json(path: &Path, update: impl FnOnce(&mut serde_json::Value)) {
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        update(&mut value);
        fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    }

    fn collect_files(root: &Path) -> BTreeMap<String, Vec<u8>> {
        fn visit(root: &Path, directory: &Path, files: &mut BTreeMap<String, Vec<u8>>) {
            for entry in fs::read_dir(directory).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_dir() {
                    visit(root, &entry.path(), files);
                } else {
                    let relative = entry
                        .path()
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/");
                    files.insert(relative, fs::read(entry.path()).unwrap());
                }
            }
        }

        let mut files = BTreeMap::new();
        visit(root, root, &mut files);
        files
    }

    fn emitted_entry(path: &Path) -> crate::CatalogEntry {
        CatalogEntry::from_json(&fs::read(path).unwrap()).expect("pack must write a valid entry")
    }

    #[test]
    fn identical_inputs_produce_identical_zip_bytes() {
        let fixture = PreparedFixture::new("deterministic");
        let one = fixture.root.join("one.zip");
        let two = fixture.root.join("two.zip");
        let one_entry = fixture.root.join("one.json");
        let two_entry = fixture.root.join("two.json");

        let first = pack(fixture.request(&one, &one_entry)).unwrap();
        let second = pack(fixture.request(&two, &two_entry)).unwrap();

        assert_eq!(fs::read(&one).unwrap(), fs::read(&two).unwrap());
        assert_eq!(first.archive_sha256(), second.archive_sha256());
        assert_eq!(fs::read(one_entry).unwrap(), fs::read(two_entry).unwrap());
    }

    #[test]
    fn archive_members_are_lexically_sorted_with_fixed_metadata() {
        let fixture = PreparedFixture::new("archive-metadata");
        let archive = fixture.root.join("payload.zip");
        pack(fixture.request(&archive, &fixture.root.join("entry.json"))).unwrap();
        let mut zip = ZipArchive::new(fs::File::open(archive).unwrap()).unwrap();
        let expected_names = [
            "content/dxgkrnl/dxgmodule.c",
            "content/mesa/lib/x86_64-linux-gnu/marker.so",
            "licenses/GPL-2.0.txt",
            "licenses/Linux-syscall-note.txt",
            "payload.json",
            "sources.json",
        ];

        for (index, expected_name) in expected_names.iter().enumerate() {
            let member = zip.by_index(index).unwrap();
            assert_eq!(member.name().unwrap().as_ref(), *expected_name);
            assert_eq!(member.compression(), CompressionMethod::Deflated);
            assert_eq!(member.unix_mode().unwrap() & 0o777, 0o644);
            assert_eq!(member.last_modified().unwrap(), zip::DateTime::default());
        }
    }

    #[test]
    fn changing_one_overlay_byte_changes_archive_identity() {
        let fixture = PreparedFixture::new("changed-overlay");
        let before = pack(fixture.request(
            &fixture.root.join("before.zip"),
            &fixture.root.join("before.json"),
        ))
        .unwrap();
        let overlay = fixture.prepared.join("content/dxgkrnl/dxgmodule.c");
        let mut changed = fs::read(&overlay).unwrap();
        changed.push(b'!');
        fs::write(&overlay, &changed).unwrap();
        let digest = Sha256Digest::hash_reader(changed.as_slice())
            .unwrap()
            .as_hex()
            .to_owned();
        rewrite_json(&fixture.prepared.join("sources.json"), |sources| {
            sources["overlays"][0]["sha256"] = digest.clone().into();
        });
        fixture.rewrite_recipe(|recipe| {
            recipe["overlays"][0]["sha256"] = digest.into();
        });
        let after = pack(fixture.request(
            &fixture.root.join("after.zip"),
            &fixture.root.join("after.json"),
        ))
        .unwrap();

        assert_ne!(before.archive_sha256(), after.archive_sha256());
    }

    #[test]
    fn incomplete_or_unknown_recipe_fields_are_rejected() {
        let fixture = PreparedFixture::new("recipe-schema");
        fs::write(&fixture.recipe, br#"{"payload_id":"test"}"#).unwrap();
        assert!(matches!(
            pack(fixture.request(
                &fixture.root.join("incomplete.zip"),
                &fixture.root.join("incomplete.json")
            )),
            Err(PayloadError::InvalidCatalog(_))
        ));

        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/recipe.json"),
            &fixture.recipe,
        )
        .unwrap();
        fixture.rewrite_recipe(|recipe| recipe["unexpected"] = true.into());
        assert!(matches!(
            pack(fixture.request(
                &fixture.root.join("unknown.zip"),
                &fixture.root.join("unknown.json")
            )),
            Err(PayloadError::InvalidCatalog(_))
        ));
    }

    #[test]
    fn recipe_target_must_name_every_exact_guest_dimension() {
        let fixture = PreparedFixture::new("exact-target");
        fixture.rewrite_recipe(|recipe| recipe["target"]["kernel_release"] = "".into());

        assert!(matches!(
            pack(fixture.request(
                &fixture.root.join("payload.zip"),
                &fixture.root.join("entry.json")
            )),
            Err(PayloadError::InvalidCatalog(_))
        ));
    }

    #[test]
    fn prepared_sources_must_match_entire_recipe_provenance() {
        for mutation in ["source", "target", "mesa"] {
            let fixture = PreparedFixture::new(&format!("source-provenance-{mutation}"));
            rewrite_json(
                &fixture.prepared.join("sources.json"),
                |sources| match mutation {
                    "source" => {
                        sources["sources"][0]["commit"] =
                            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into()
                    }
                    "target" => sources["target"]["kernel_release"] = "other".into(),
                    "mesa" => sources["mesa_policy"] = "distro".into(),
                    _ => unreachable!(),
                },
            );

            assert!(
                matches!(
                    pack(fixture.request(
                        &fixture.root.join("payload.zip"),
                        &fixture.root.join("entry.json")
                    )),
                    Err(PayloadError::InvalidManifest(_))
                ),
                "accepted mismatched {mutation} provenance"
            );
        }
    }

    #[test]
    fn prepared_overlay_digest_must_match_the_overlay_bytes() {
        let fixture = PreparedFixture::new("overlay-provenance");
        fs::write(
            fixture.prepared.join("content/dxgkrnl/dxgmodule.c"),
            b"changed without provenance",
        )
        .unwrap();

        assert!(matches!(
            pack(fixture.request(
                &fixture.root.join("payload.zip"),
                &fixture.root.join("entry.json")
            )),
            Err(PayloadError::InvalidManifest(_))
        ));
    }

    #[test]
    fn every_recipe_license_must_be_a_prepared_file() {
        let fixture = PreparedFixture::new("license-provenance");
        fs::remove_file(fixture.prepared.join("licenses/GPL-2.0.txt")).unwrap();

        assert!(matches!(
            pack(fixture.request(
                &fixture.root.join("payload.zip"),
                &fixture.root.join("entry.json")
            )),
            Err(PayloadError::InvalidManifest(_))
        ));
    }

    #[test]
    fn every_overlay_license_must_be_declared_by_the_recipe() {
        let fixture = PreparedFixture::new("overlay-license");
        rewrite_json(&fixture.prepared.join("sources.json"), |sources| {
            sources["overlays"][0]["license"] = "Proprietary".into();
        });
        fixture.rewrite_recipe(|recipe| {
            recipe["overlays"][0]["license"] = "Proprietary".into();
        });

        assert!(matches!(
            pack(fixture.request(
                &fixture.root.join("payload.zip"),
                &fixture.root.join("entry.json")
            )),
            Err(PayloadError::InvalidManifest(_))
        ));
    }

    #[test]
    fn invalid_standalone_catalog_entry_is_rejected_before_success() {
        let fixture = PreparedFixture::new("catalog-validation");
        fixture.rewrite_recipe(|recipe| recipe["required_renderers"] = serde_json::json!([]));

        assert!(matches!(
            pack(fixture.request(
                &fixture.root.join("payload.zip"),
                &fixture.root.join("entry.json")
            )),
            Err(PayloadError::InvalidCatalog(_))
        ));
    }

    #[test]
    fn input_and_output_paths_must_not_overlap() {
        let fixture = PreparedFixture::new("path-overlap");
        let same_output = fixture.root.join("same-output");
        assert!(matches!(
            pack(fixture.request(&same_output, &same_output)),
            Err(PayloadError::InvalidManifest(_))
        ));

        assert!(matches!(
            pack(fixture.request(
                &fixture.prepared.join("archive.zip"),
                &fixture.root.join("entry.json")
            )),
            Err(PayloadError::InvalidManifest(_))
        ));

        assert!(matches!(
            pack(PackRequest {
                prepared_directory: &fixture.prepared,
                recipe_path: &fixture.recipe,
                archive_path: &fixture.recipe,
                catalog_entry_path: &fixture.root.join("recipe-entry.json"),
            }),
            Err(PayloadError::InvalidManifest(_))
        ));
    }

    #[test]
    fn preexisting_hard_link_output_aliases_are_rejected() {
        let fixture = PreparedFixture::new("hard-link-output-alias");
        let archive = fixture.root.join("archive.zip");
        let catalog_entry = fixture.root.join("entry.json");
        fs::write(&archive, b"existing output").unwrap();
        fs::hard_link(&archive, &catalog_entry).unwrap();

        assert!(matches!(
            pack(fixture.request(&archive, &catalog_entry)),
            Err(PayloadError::InvalidManifest(_))
        ));
        assert_eq!(fs::read(&archive).unwrap(), b"existing output");
        assert_eq!(fs::read(&catalog_entry).unwrap(), b"existing output");
    }

    #[test]
    fn generated_payload_manifest_must_fit_the_runtime_limit() {
        let fixture = PreparedFixture::new("manifest-limit");
        let many_files = fixture.prepared.join("content/many");
        fs::create_dir_all(&many_files).unwrap();
        for index in 0..9_000 {
            fs::write(many_files.join(format!("file-{index:04}")), b"x").unwrap();
        }

        assert!(matches!(
            pack(fixture.request(
                &fixture.root.join("payload.zip"),
                &fixture.root.join("entry.json")
            )),
            Err(PayloadError::LimitExceeded {
                subject: "payload.json",
                limit: 1_048_576,
                actual: _
            })
        ));
    }

    #[test]
    fn prepared_paths_cannot_collide_on_windows() {
        let fixture = PreparedFixture::new("windows-path-collision");
        fs::write(fixture.prepared.join("content/CaseSensitive"), b"one").unwrap();
        fs::write(fixture.prepared.join("content/casesensitive"), b"two").unwrap();

        assert!(matches!(
            pack(fixture.request(
                &fixture.root.join("payload.zip"),
                &fixture.root.join("entry.json")
            )),
            Err(PayloadError::InvalidManifest(_))
        ));
    }

    #[test]
    fn prepared_paths_reject_windows_superscript_device_aliases() {
        for name in ["COM¹.txt", "LPT²", "com³.log"] {
            assert!(
                matches!(
                    validate_archive_path(name),
                    Err(PayloadError::UnsafeArchive(_))
                ),
                "accepted reserved Windows device alias {name}"
            );
        }
    }

    #[test]
    fn built_artifact_reports_true_expanded_member_bytes() {
        let fixture = PreparedFixture::new("expanded-size");
        let archive = fixture.root.join("payload.zip");
        let catalog_entry = fixture.root.join("entry.json");
        let built = pack(fixture.request(&archive, &catalog_entry)).unwrap();
        let mut zip = ZipArchive::new(fs::File::open(archive).unwrap()).unwrap();
        let actual_expanded = (0..zip.len())
            .map(|index| zip.by_index(index).unwrap().size())
            .sum::<u64>();

        assert_eq!(built.expanded_size(), actual_expanded);
        assert!(emitted_entry(&catalog_entry).expanded_size_limit() >= actual_expanded);
    }

    #[test]
    fn built_artifact_round_trips_through_the_runtime_cache_path() {
        let fixture = PreparedFixture::new("runtime-round-trip");
        let archive = fixture.root.join("payload.zip");
        let catalog_entry = fixture.root.join("entry.json");
        let built = pack(fixture.request(&archive, &catalog_entry)).unwrap();
        let entry = emitted_entry(&catalog_entry);
        let zip = ZipArchive::new(fs::File::open(&archive).unwrap()).unwrap();

        assert_eq!(zip.len() as u64, built.file_count() + 1);
        assert_eq!(entry.file_count_limit(), built.file_count());

        let cancel = AtomicBool::new(false);
        let ready = vmlord_payload::prepare_verified_archive(
            &entry,
            &archive,
            &fixture.root.join("cache"),
            &|_| {},
            &cancel,
        )
        .unwrap();
        let expected = collect_files(&fixture.prepared);
        let mut extracted = collect_files(ready.files_directory());
        assert!(extracted.remove("payload.json").is_some());
        assert_eq!(extracted, expected);
        assert_eq!(ready.manifest().files().len() as u64, built.file_count());
        assert!(ready.provenance_path().is_file());
    }
    #[test]
    fn a_built_tree_that_does_not_match_its_recorded_digest_is_refused() {
        let fixture = PreparedFixture::new("built-digest");
        fs::write(
            fixture
                .prepared
                .join("content/mesa/lib/x86_64-linux-gnu/marker.so"),
            b"different bytes entirely\n",
        )
        .unwrap();

        let error = pack(fixture.request(
            &fixture.root.join("payload.zip"),
            &fixture.root.join("entry.json"),
        ))
        .unwrap_err();

        assert!(matches!(error, PayloadError::InvalidManifest(_)));
    }

    /// The digest of the adversarial tree below, and the tie between this rule and the
    /// one `payloads/ubuntu-26.04-amd64/prepare.py` writes into every recipe.
    ///
    /// The same literal appears in `prepare_test.py` beside that script, over a tree with
    /// the same three members and the same bytes. The two implementations are one rule in
    /// two languages -- each file under the built output, sorted by its payload-relative
    /// POSIX path *string*, contributed as path, NUL, contents -- and a disagreement
    /// between them surfaces only as `pack` refusing a tree it just built, naming neither
    /// the rule nor the file. Changing either side turns exactly one of the two red.
    const EXPECTED_ADVERSARIAL_DIGEST: &str =
        "033ae129cd90239804e4a42ba7b80063e4fdcf85d5b2a634eb9dc32acdc3a034";

    /// Members whose two plausible orders genuinely differ: as joined strings `-` < `.`
    /// < `/`, so `lib-extra` and `lib.conf` both precede anything under `lib/`; sorted as
    /// path components instead, `lib` precedes both and `lib/dri.so` comes first.
    const ADVERSARIAL_TREE: [(&str, &[u8]); 3] = [
        ("content/mesa/lib/dri.so", b"dri\n"),
        ("content/mesa/lib-extra", b"extra\n"),
        ("content/mesa/lib.conf", b"conf\n"),
    ];

    #[test]
    fn the_built_output_digest_matches_the_python_golden_vector() {
        let fixture = PreparedFixture::new("golden-digest");
        let root = fixture.root.join("adversarial");
        for (relative, contents) in ADVERSARIAL_TREE {
            let path = root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }

        let files = super::collect_files(&root).unwrap();
        let digest = super::built_output_digest(&files, "content/mesa").unwrap();

        assert_eq!(
            digest.as_hex(),
            EXPECTED_ADVERSARIAL_DIGEST,
            "built_output_digest no longer computes the rule prepare.py writes into the \
             recipe; see prepare_test.py, which asserts this same literal"
        );
    }

    #[test]
    fn the_golden_vector_would_catch_a_component_wise_sort() {
        let fixture = PreparedFixture::new("golden-divergence");
        let root = fixture.root.join("adversarial");
        for (relative, contents) in ADVERSARIAL_TREE {
            let path = root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }

        let mut wrong: Vec<_> = super::collect_files(&root).unwrap();
        wrong.sort_by(|left, right| {
            let left: Vec<_> = left.archive_path.split('/').collect();
            let right: Vec<_> = right.archive_path.split('/').collect();
            left.cmp(&right)
        });
        let digest = super::built_output_digest(&wrong, "content/mesa").unwrap();

        assert_ne!(
            digest.as_hex(),
            EXPECTED_ADVERSARIAL_DIGEST,
            "the adversarial tree no longer distinguishes the two orders, so the golden \
             vector proves nothing"
        );
    }

    #[test]
    fn a_built_record_whose_output_holds_nothing_is_refused() {
        let fixture = PreparedFixture::new("built-empty");
        fixture.rewrite_recipe(|recipe| {
            recipe["sources"][1]["output"] = serde_json::json!("content/absent");
        });
        rewrite_json(&fixture.prepared.join("sources.json"), |sources| {
            sources["sources"][1]["output"] = serde_json::json!("content/absent");
        });

        let error = pack(fixture.request(
            &fixture.root.join("payload.zip"),
            &fixture.root.join("entry.json"),
        ))
        .unwrap_err();

        assert!(matches!(error, PayloadError::InvalidManifest(_)));
    }

    #[test]
    fn a_built_records_inputs_reach_the_catalog_beside_it() {
        let fixture = PreparedFixture::new("built-inputs");
        let entry_path = fixture.root.join("entry.json");
        pack(fixture.request(&fixture.root.join("payload.zip"), &entry_path)).unwrap();

        let document: serde_json::Value =
            serde_json::from_slice(&fs::read(&entry_path).unwrap()).unwrap();
        let urls: Vec<&str> = document["sources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|source| source["url"].as_str().unwrap())
            .collect();

        assert_eq!(
            urls,
            [
                "https://github.com/microsoft/WSL2-Linux-Kernel",
                "https://gitlab.freedesktop.org/mesa/mesa",
                "https://github.com/microsoft/DirectX-Headers",
            ]
        );
    }

    /// `kind` is the only thing separating the two variants, so a record that declares
    /// one kind and carries the other's fields must be refused rather than quietly read
    /// as the variant it happens to fit.
    #[test]
    fn a_source_record_whose_kind_disagrees_with_its_shape_is_refused() {
        let fixture = PreparedFixture::new("kind-mismatch");
        fixture.rewrite_recipe(|recipe| {
            recipe["sources"][0]["kind"] = serde_json::json!("built");
        });

        let error = pack(fixture.request(
            &fixture.root.join("payload.zip"),
            &fixture.root.join("entry.json"),
        ))
        .unwrap_err();

        let PayloadError::InvalidCatalog(message) = error else {
            panic!("a record must be refused, not read as the kind it does not declare");
        };
        assert!(message.contains("untagged enum RecipeSource"), "{message}");
    }

    #[test]
    fn a_recipe_at_version_one_is_no_longer_understood() {
        let fixture = PreparedFixture::new("old-recipe");
        fixture.rewrite_recipe(|recipe| {
            recipe["schema_version"] = serde_json::json!(1);
        });

        let error = pack(fixture.request(
            &fixture.root.join("payload.zip"),
            &fixture.root.join("entry.json"),
        ))
        .unwrap_err();

        assert!(matches!(error, PayloadError::InvalidCatalog(_)));
    }
}
