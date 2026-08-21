//! Turning a prepared directory into the pair a release ships.
//!
//! Everything here is true of packing any payload: what a prepared tree may
//! contain, how it is hashed, how the archive is written so that two runs over
//! the same tree produce the same bytes, and that an entry is written only
//! after the archive it describes has been closed and measured. What goes
//! *into* the manifest and the entry is the payload kind's own, and stays in
//! its crate.

use std::{
    collections::HashSet,
    fs::{self, File, Metadata},
    io::{self, Write},
    path::{Component, Path, PathBuf},
};

use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

use crate::{PayloadError, Sha256Digest};

/// The ceiling on `payload.json` itself, which is read before anything about
/// the payload is known and so cannot be bounded by the payload's own limits.
pub const PAYLOAD_MANIFEST_LIMIT: u64 = 1024 * 1024;

/// Where a pack reads from and writes to.
pub struct PackPaths<'a> {
    pub prepared_directory: &'a Path,
    pub recipe_path: &'a Path,
    pub archive_path: &'a Path,
    pub catalog_entry_path: &'a Path,
}

#[derive(Debug)]
pub struct BuiltArtifact {
    pub archive_size: u64,
    pub expanded_size: u64,
    pub file_count: u64,
    pub archive_sha256: Sha256Digest,
    pub payload_manifest_sha256: Sha256Digest,
}

impl BuiltArtifact {
    pub fn archive_size(&self) -> u64 {
        self.archive_size
    }

    pub fn expanded_size(&self) -> u64 {
        self.expanded_size
    }

    pub fn file_count(&self) -> u64 {
        self.file_count
    }

    pub fn archive_sha256(&self) -> &Sha256Digest {
        &self.archive_sha256
    }

    pub fn payload_manifest_sha256(&self) -> &Sha256Digest {
        &self.payload_manifest_sha256
    }
}

/// One file of a prepared tree, as it will appear in the archive.
pub struct PreparedInput {
    // Public to the crate that composes the manifest out of them.
    pub archive_path: String,
    pub host_path: PathBuf,
    pub size: u64,
    pub sha256: Sha256Digest,
}

/// Refuses inputs and outputs that are the same file, or outputs that
/// would be written inside the tree being packed.
///
/// # Errors
///
/// [`PayloadError::InvalidManifest`] naming the collision, or
/// [`PayloadError::Io`] for a path that cannot be resolved.
pub fn validate_paths(request: &PackPaths<'_>) -> Result<(), PayloadError> {
    let prepared_directory = fs::canonicalize(request.prepared_directory).map_err(|error| {
        PayloadError::io(
            "resolve prepared directory",
            request.prepared_directory.into(),
            error,
        )
    })?;
    let recipe = fs::canonicalize(request.recipe_path).map_err(|error| {
        PayloadError::io("resolve payload recipe", request.recipe_path.into(), error)
    })?;
    let archive = resolve_output_path(request.archive_path)?;
    let catalog_entry = resolve_output_path(request.catalog_entry_path)?;
    if paths_equal(&archive, &catalog_entry)
        || paths_equal(&archive, &recipe)
        || paths_equal(&catalog_entry, &recipe)
        || archive.starts_with(&prepared_directory)
        || catalog_entry.starts_with(&prepared_directory)
    {
        return Err(PayloadError::InvalidManifest(
            "payload inputs and outputs must use distinct paths".into(),
        ));
    }
    Ok(())
}

fn resolve_output_path(path: &Path) -> Result<PathBuf, PayloadError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(PayloadError::InvalidManifest(format!(
            "payload output already exists: {}",
            path.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            let parent = fs::canonicalize(parent).map_err(|error| {
                PayloadError::io("resolve output directory", parent.into(), error)
            })?;
            let file_name = path.file_name().ok_or_else(|| {
                PayloadError::InvalidManifest("output path must name a file".into())
            })?;
            Ok(parent.join(file_name))
        }
        Err(error) => Err(PayloadError::io("inspect output path", path.into(), error)),
    }
}

#[cfg(windows)]
fn paths_equal(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

#[cfg(not(windows))]
fn paths_equal(left: &Path, right: &Path) -> bool {
    left == right
}

/// Writes the archive: every prepared file plus `payload.json`, in sorted
/// order with fixed timestamps, so that the same tree produces the same bytes.
///
/// # Errors
///
/// [`PayloadError::Archive`] or [`PayloadError::Io`] for a tree that cannot be
/// read or an archive that cannot be written.
pub fn write_archive(
    archive_path: &Path,
    files: &[PreparedInput],
    manifest_bytes: &[u8],
) -> Result<(), PayloadError> {
    let output = File::create(archive_path)
        .map_err(|error| PayloadError::io("create payload archive", archive_path.into(), error))?;
    let mut writer = ZipWriter::new(output);
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .compression_level(Some(6))
        .last_modified_time(zip::DateTime::default())
        .unix_permissions(0o644);
    let mut paths = files
        .iter()
        .map(|file| file.archive_path.as_str())
        .chain(std::iter::once("payload.json"))
        .collect::<Vec<_>>();
    paths.sort_unstable();

    for path in paths {
        writer
            .start_file(path, options)
            .map_err(|error| PayloadError::Archive(error.to_string()))?;
        if path == "payload.json" {
            writer
                .write_all(manifest_bytes)
                .map_err(|error| PayloadError::Archive(error.to_string()))?;
        } else {
            let file = files
                .iter()
                .find(|file| file.archive_path == path)
                .expect("archive paths came from prepared files");
            let mut input = File::open(&file.host_path).map_err(|error| {
                PayloadError::io("open prepared file", file.host_path.clone(), error)
            })?;
            io::copy(&mut input, &mut writer)
                .map_err(|error| PayloadError::Archive(error.to_string()))?;
        }
    }

    writer
        .finish()
        .map_err(|error| PayloadError::Archive(error.to_string()))?
        .sync_all()
        .map_err(|error| PayloadError::io("flush payload archive", archive_path.into(), error))
}

/// Every file below `root`, hashed, with the paths an archive may carry.
///
/// Symlinks, reparse points, devices, empty files and a `payload.json` in the
/// tree are all refused: the first three because an archive that carries them
/// is an archive that can escape its destination, the fourth because a
/// zero-length member says nothing, and the last because `payload.json` is
/// written by the pack rather than found by it.
///
/// # Errors
///
/// [`PayloadError::UnsafeArchive`] or [`PayloadError::InvalidManifest`] for a
/// tree that cannot be packed; [`PayloadError::Io`] for one that cannot be
/// read.
pub fn collect_files(root: &Path) -> Result<Vec<PreparedInput>, PayloadError> {
    fn walk(
        root: &Path,
        current: &Path,
        output: &mut Vec<PreparedInput>,
    ) -> Result<(), PayloadError> {
        for item in fs::read_dir(current)
            .map_err(|error| PayloadError::io("read prepared directory", current.into(), error))?
        {
            let item = item.map_err(|error| {
                PayloadError::io("read prepared directory entry", current.into(), error)
            })?;
            let path = item.path();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|error| PayloadError::io("read prepared metadata", path.clone(), error))?;
            if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
                return Err(PayloadError::UnsafeArchive(path.display().to_string()));
            }
            if metadata.is_dir() {
                walk(root, &path, output)?;
            } else if metadata.is_file() {
                let relative = path
                    .strip_prefix(root)
                    .expect("walk stays below root")
                    .to_str()
                    .ok_or_else(|| PayloadError::UnsafeArchive("non-UTF-8 prepared path".into()))?
                    .replace('\\', "/");
                validate_archive_path(&relative)?;
                if relative == "payload.json" {
                    return Err(PayloadError::InvalidManifest(
                        "prepared directory must not contain payload.json".into(),
                    ));
                }
                if metadata.len() == 0 {
                    return Err(PayloadError::InvalidManifest(format!(
                        "prepared file must not be empty: {relative}"
                    )));
                }
                output.push(PreparedInput {
                    archive_path: relative,
                    host_path: path.clone(),
                    size: metadata.len(),
                    sha256: Sha256Digest::hash_reader(File::open(&path).map_err(|error| {
                        PayloadError::io("read prepared file", path.clone(), error)
                    })?)?,
                });
            } else {
                return Err(PayloadError::UnsafeArchive(path.display().to_string()));
            }
        }
        Ok(())
    }

    let root_metadata = fs::symlink_metadata(root)
        .map_err(|error| PayloadError::io("read prepared directory", root.into(), error))?;
    if !root_metadata.is_dir()
        || root_metadata.file_type().is_symlink()
        || is_reparse_point(&root_metadata)
    {
        return Err(PayloadError::UnsafeArchive(root.display().to_string()));
    }
    let mut files = Vec::new();
    walk(root, root, &mut files)?;
    files.sort_by(|left, right| left.archive_path.cmp(&right.archive_path));
    let mut windows_paths = HashSet::from([windows_path_key("payload.json")]);
    for file in &files {
        if !windows_paths.insert(windows_path_key(&file.archive_path)) {
            return Err(PayloadError::InvalidManifest(format!(
                "prepared paths collide on Windows: {}",
                file.archive_path
            )));
        }
    }
    Ok(files)
}

fn windows_path_key(path: &str) -> String {
    path.to_lowercase()
}

/// Whether a path may be carried inside an archive at all.
///
/// # Errors
///
/// [`PayloadError::UnsafeArchive`] naming the path.
pub fn validate_archive_path(path: &str) -> Result<(), PayloadError> {
    let bytes = path.as_bytes();
    let has_windows_prefix = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    let ordinary_components = !path.is_empty()
        && !path.contains('\\')
        && !path.contains('\0')
        && !path.starts_with('/')
        && !has_windows_prefix
        && path.split('/').all(is_safe_component);
    let ordinary_path = Path::new(path)
        .components()
        .all(|component| matches!(component, Component::Normal(_)));
    if !ordinary_components || !ordinary_path {
        return Err(PayloadError::UnsafeArchive(path.into()));
    }
    Ok(())
}

fn is_safe_component(component: &str) -> bool {
    if component.is_empty()
        || component == "."
        || component == ".."
        || component.ends_with('.')
        || component.ends_with(' ')
        || component.chars().any(|character| {
            character < ' ' || matches!(character, '<' | '>' | ':' | '"' | '|' | '?' | '*')
        })
    {
        return false;
    }
    let device_stem = component
        .split_once('.')
        .map_or(component, |(stem, _)| stem)
        .to_ascii_uppercase();
    !matches!(
        device_stem.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "COM¹"
            | "COM²"
            | "COM³"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
            | "LPT¹"
            | "LPT²"
            | "LPT³"
            | "CONIN$"
            | "CONOUT$"
    )
}

#[cfg(windows)]
fn is_reparse_point(metadata: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    metadata.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_: &Metadata) -> bool {
    false
}
