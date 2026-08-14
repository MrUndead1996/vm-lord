use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

use sha2::{Digest, Sha256};
use zip::ZipArchive;

use crate::{
    CatalogEntry, PayloadError, PayloadManifest, PayloadProgress, Sha256Digest, SourceManifest,
};

const PAYLOAD_MANIFEST_LIMIT: u64 = 1024 * 1024;
const COPY_BUFFER_SIZE: usize = 64 * 1024;

struct InspectedEntry {
    index: usize,
    name: String,
    path: PathBuf,
    size: u64,
}

pub(crate) fn extract(
    entry: &CatalogEntry,
    archive: &Path,
    destination: &Path,
    progress: &dyn Fn(PayloadProgress),
    cancel: &AtomicBool,
) -> Result<(PayloadManifest, SourceManifest), PayloadError> {
    let archive_file = File::open(archive)
        .map_err(|error| PayloadError::io("open archive", archive.into(), error))?;
    let mut zip =
        ZipArchive::new(archive_file).map_err(|error| PayloadError::Archive(error.to_string()))?;
    let central_names = read_central_names(archive, zip.central_directory_start())?;
    if central_names.len() != zip.len() {
        return Err(PayloadError::Archive(
            "ZIP reader and central directory disagree on entry count".into(),
        ));
    }
    let payload_index = central_names
        .iter()
        .position(|name| name.as_slice() == b"payload.json")
        .ok_or_else(|| PayloadError::UnsafeArchive("archive has no payload.json".into()))?;
    let payload_bytes = read_payload_manifest(&mut zip, payload_index)?;
    let actual = Sha256Digest::hash_reader(payload_bytes.as_slice())?;
    if actual != *entry.payload_manifest_sha256() {
        return Err(PayloadError::DigestMismatch {
            subject: "payload.json".into(),
            expected: entry.payload_manifest_sha256().clone(),
            actual,
        });
    }
    let manifest = PayloadManifest::parse_and_validate(&payload_bytes, entry)?;
    validate_manifest_limits(&manifest, entry)?;
    let inspected = inspect_entries(&mut zip, entry)?;

    let declared: HashMap<_, _> = manifest
        .files()
        .iter()
        .map(|file| (file.path(), file))
        .collect();
    let mut consumed = HashSet::new();
    for metadata in &inspected {
        let expected = declared.get(metadata.name.as_str()).ok_or_else(|| {
            PayloadError::UnsafeArchive(format!("undeclared archive entry {}", metadata.name))
        })?;
        if metadata.size != expected.size() {
            return Err(PayloadError::InvalidManifest(format!(
                "size mismatch for {}",
                metadata.name
            )));
        }
        if !consumed.insert(metadata.name.as_str()) {
            return Err(PayloadError::UnsafeArchive(metadata.name.clone()));
        }
    }
    if consumed.len() != declared.len() {
        return Err(PayloadError::InvalidManifest(
            "archive does not contain every declared file".into(),
        ));
    }

    write_new_file(
        &destination.join("payload.json"),
        &payload_bytes,
        "write extracted payload.json",
    )?;
    for (completed, metadata) in inspected.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err(PayloadError::Cancelled);
        }
        let expected = declared[metadata.name.as_str()];
        let target = destination.join(&metadata.path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                PayloadError::io("create extraction directory", parent.into(), error)
            })?;
        }
        let mut member = zip
            .by_index(metadata.index)
            .map_err(|error| PayloadError::Archive(error.to_string()))?;
        stream_member(
            &mut member,
            &target,
            &metadata.name,
            expected.size(),
            expected.sha256(),
            cancel,
        )?;
        progress(PayloadProgress::Extracting {
            files: (completed + 1) as u64,
            total: manifest.files().len() as u64,
        });
    }

    let sources_path = destination.join("sources.json");
    let sources = SourceManifest::parse_and_validate(
        &fs::read(&sources_path).map_err(|error| {
            PayloadError::io("read extracted sources.json", sources_path, error)
        })?,
        entry,
    )?;
    sources.validate_prepared_files(&manifest)?;
    Ok((manifest, sources))
}

fn read_central_names(archive: &Path, directory_start: u64) -> Result<Vec<Vec<u8>>, PayloadError> {
    const CENTRAL_DIRECTORY_HEADER: u32 = 0x0201_4b50;
    const FIXED_HEADER_AFTER_SIGNATURE: usize = 42;
    let mut file = File::open(archive)
        .map_err(|error| PayloadError::io("inspect archive directory", archive.into(), error))?;
    file.seek(SeekFrom::Start(directory_start))
        .map_err(|error| PayloadError::io("inspect archive directory", archive.into(), error))?;
    let mut names = Vec::new();
    let mut seen = HashSet::new();
    loop {
        let mut signature = [0_u8; 4];
        file.read_exact(&mut signature).map_err(|error| {
            PayloadError::io("inspect archive directory", archive.into(), error)
        })?;
        if u32::from_le_bytes(signature) != CENTRAL_DIRECTORY_HEADER {
            break;
        }
        let mut header = [0_u8; FIXED_HEADER_AFTER_SIGNATURE];
        file.read_exact(&mut header).map_err(|error| {
            PayloadError::io("inspect archive directory", archive.into(), error)
        })?;
        let name_length = u16::from_le_bytes([header[24], header[25]]) as u64;
        let extra_length = u16::from_le_bytes([header[26], header[27]]) as u64;
        let comment_length = u16::from_le_bytes([header[28], header[29]]) as u64;
        let mut name = vec![0_u8; name_length as usize];
        file.read_exact(&mut name).map_err(|error| {
            PayloadError::io("inspect archive directory", archive.into(), error)
        })?;
        if !seen.insert(name.clone()) {
            return Err(PayloadError::UnsafeArchive(
                "archive contains duplicate entry names".into(),
            ));
        }
        names.push(name);
        let remaining_length = extra_length
            .checked_add(comment_length)
            .ok_or_else(|| PayloadError::Archive("central-directory length overflow".into()))?;
        file.seek(SeekFrom::Current(remaining_length as i64))
            .map_err(|error| {
                PayloadError::io("inspect archive directory", archive.into(), error)
            })?;
    }
    Ok(names)
}

fn inspect_entries(
    zip: &mut ZipArchive<File>,
    entry: &CatalogEntry,
) -> Result<Vec<InspectedEntry>, PayloadError> {
    let mut seen = HashSet::new();
    let mut prepared_count = 0_u64;
    let mut compressed_size = 0_u64;
    let mut inspected = Vec::new();

    for index in 0..zip.len() {
        let member = zip
            .by_index(index)
            .map_err(|error| PayloadError::Archive(error.to_string()))?;
        let raw_name = std::str::from_utf8(member.name_raw())
            .map_err(|_| PayloadError::UnsafeArchive("non-UTF-8 entry name".into()))?;
        validate_raw_name(raw_name)?;
        let enclosed = member
            .enclosed_name()
            .ok_or_else(|| PayloadError::UnsafeArchive(raw_name.to_owned()))?;
        if !seen.insert(enclosed.clone()) {
            return Err(PayloadError::UnsafeArchive(raw_name.to_owned()));
        }
        validate_member_type(&member, raw_name)?;

        compressed_size = checked_limit_add(
            compressed_size,
            member.compressed_size(),
            "compressed size",
            entry.archive_size(),
        )?;

        if raw_name == "payload.json" {
            continue;
        }

        prepared_count = checked_limit_add(
            prepared_count,
            1,
            "archive file count",
            entry.file_count_limit(),
        )?;
        inspected.push(InspectedEntry {
            index,
            name: raw_name.to_owned(),
            path: enclosed,
            size: member.size(),
        });
    }

    Ok(inspected)
}

fn validate_raw_name(raw_name: &str) -> Result<(), PayloadError> {
    let bytes = raw_name.as_bytes();
    let has_windows_prefix = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    let ordinary_components = !raw_name.is_empty()
        && !raw_name.contains('\\')
        && !raw_name.contains('\0')
        && !raw_name.starts_with('/')
        && !has_windows_prefix
        && raw_name.split('/').all(is_safe_component);
    let ordinary_path = Path::new(raw_name)
        .components()
        .all(|component| matches!(component, Component::Normal(_)));
    if !ordinary_components || !ordinary_path {
        return Err(PayloadError::UnsafeArchive(raw_name.to_owned()));
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
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
            | "CONIN$"
            | "CONOUT$"
    )
}

fn validate_member_type<R: Read>(
    member: &zip::read::ZipFile<'_, R>,
    raw_name: &str,
) -> Result<(), PayloadError> {
    if member.is_dir() {
        return Err(PayloadError::UnsafeArchive(raw_name.to_owned()));
    }
    if let Some(mode) = member.unix_mode() {
        let file_type = mode & 0o170000;
        if file_type != 0 && file_type != 0o100000 {
            return Err(PayloadError::UnsafeArchive(raw_name.to_owned()));
        }
    }
    Ok(())
}

fn read_payload_manifest(
    zip: &mut ZipArchive<File>,
    payload_index: usize,
) -> Result<Vec<u8>, PayloadError> {
    let payload = zip
        .by_index(payload_index)
        .map_err(|error| PayloadError::Archive(error.to_string()))?;
    let raw_name = std::str::from_utf8(payload.name_raw())
        .map_err(|_| PayloadError::UnsafeArchive("non-UTF-8 entry name".into()))?;
    if raw_name != "payload.json" {
        return Err(PayloadError::UnsafeArchive(raw_name.to_owned()));
    }
    validate_raw_name(raw_name)?;
    validate_member_type(&payload, raw_name)?;
    if payload.size() > PAYLOAD_MANIFEST_LIMIT {
        return Err(PayloadError::LimitExceeded {
            subject: "payload.json",
            limit: PAYLOAD_MANIFEST_LIMIT,
            actual: payload.size(),
        });
    }
    let mut bytes = Vec::with_capacity(payload.size() as usize);
    payload
        .take(PAYLOAD_MANIFEST_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| PayloadError::Archive(error.to_string()))?;
    if bytes.len() as u64 > PAYLOAD_MANIFEST_LIMIT {
        return Err(PayloadError::LimitExceeded {
            subject: "payload.json",
            limit: PAYLOAD_MANIFEST_LIMIT,
            actual: bytes.len() as u64,
        });
    }
    Ok(bytes)
}

fn validate_manifest_limits(
    manifest: &PayloadManifest,
    entry: &CatalogEntry,
) -> Result<(), PayloadError> {
    let mut count = 0_u64;
    let mut expanded = 0_u64;
    for file in manifest.files() {
        count = checked_limit_add(count, 1, "archive file count", entry.file_count_limit())?;
        expanded = checked_limit_add(
            expanded,
            file.size(),
            "expanded size",
            entry.expanded_size_limit(),
        )?;
    }
    Ok(())
}

fn checked_limit_add(
    current: u64,
    amount: u64,
    subject: &'static str,
    limit: u64,
) -> Result<u64, PayloadError> {
    let actual = current
        .checked_add(amount)
        .ok_or(PayloadError::LimitExceeded {
            subject,
            limit,
            actual: u64::MAX,
        })?;
    if actual > limit {
        return Err(PayloadError::LimitExceeded {
            subject,
            limit,
            actual,
        });
    }
    Ok(actual)
}

fn stream_member(
    member: &mut impl Read,
    target: &Path,
    name: &str,
    expected_size: u64,
    expected_digest: &Sha256Digest,
    cancel: &AtomicBool,
) -> Result<(), PayloadError> {
    let mut output = File::options()
        .write(true)
        .create_new(true)
        .open(target)
        .map_err(|error| PayloadError::io("create extracted file", target.into(), error))?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; COPY_BUFFER_SIZE];
    let mut written = 0_u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(PayloadError::Cancelled);
        }
        let remaining = expected_size - written;
        if remaining == 0 {
            let mut extra = [0_u8; 1];
            if member
                .read(&mut extra)
                .map_err(|error| PayloadError::Archive(error.to_string()))?
                != 0
            {
                return Err(PayloadError::InvalidManifest(format!(
                    "size mismatch for {name}"
                )));
            }
            break;
        }
        let capacity = remaining.min(buffer.len() as u64) as usize;
        let count = member
            .read(&mut buffer[..capacity])
            .map_err(|error| PayloadError::Archive(error.to_string()))?;
        if count == 0 {
            return Err(PayloadError::InvalidManifest(format!(
                "size mismatch for {name}"
            )));
        }
        written = written
            .checked_add(count as u64)
            .ok_or_else(|| PayloadError::InvalidManifest(format!("size overflow for {name}")))?;
        output
            .write_all(&buffer[..count])
            .map_err(|error| PayloadError::io("write extracted file", target.into(), error))?;
        hash.update(&buffer[..count]);
    }
    output
        .sync_all()
        .map_err(|error| PayloadError::io("flush extracted file", target.into(), error))?;
    let actual = Sha256Digest::from_bytes(hash.finalize().into())?;
    if actual != *expected_digest {
        return Err(PayloadError::DigestMismatch {
            subject: name.into(),
            expected: expected_digest.clone(),
            actual,
        });
    }
    Ok(())
}

fn write_new_file(path: &Path, bytes: &[u8], operation: &'static str) -> Result<(), PayloadError> {
    let mut file = File::options()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| PayloadError::io(operation, path.into(), error))?;
    file.write_all(bytes)
        .map_err(|error| PayloadError::io(operation, path.into(), error))?;
    file.sync_all()
        .map_err(|error| PayloadError::io("flush extracted file", path.into(), error))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::{Cursor, Read, Write},
        path::{Path, PathBuf},
        sync::atomic::{AtomicBool, AtomicU64, Ordering},
    };

    use zip::{CompressionMethod, System, ZipWriter, write::SimpleFileOptions};

    use crate::{PayloadCatalog, PayloadError, Sha256Digest};

    use super::{extract, stream_member};

    const SOURCE_URL: &str = "https://github.com/example/source";
    const SOURCE_COMMIT: &str = "14794180686c2fb6307fbe359c359bec765249f3";
    static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

    #[derive(Clone, Copy)]
    enum MemberKind {
        File,
        Directory,
        Symlink,
        Device,
    }

    #[derive(Clone, Copy)]
    struct Member<'a> {
        name: &'a str,
        bytes: &'a [u8],
        kind: MemberKind,
    }

    struct TemporaryDirectory(PathBuf);

    impl TemporaryDirectory {
        fn new(label: &str) -> Self {
            let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "vmlord-gpu-payload-archive-{label}-{}-{sequence}",
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

    fn digest(bytes: &[u8]) -> String {
        Sha256Digest::hash_reader(bytes)
            .unwrap()
            .as_hex()
            .to_owned()
    }

    fn source_manifest() -> Vec<u8> {
        let source = b"source material";
        serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "sources": [{
                "url": SOURCE_URL,
                "commit": SOURCE_COMMIT,
                "version": "1",
                "paths": ["content/file"],
                "sha256": digest(source)
            }],
            "overlays": []
        }))
        .unwrap()
    }

    fn payload_manifest(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut files = files
            .iter()
            .map(|(path, bytes)| {
                serde_json::json!({
                    "path": path,
                    "size": bytes.len(),
                    "sha256": digest(bytes)
                })
            })
            .collect::<Vec<_>>();
        let license = b"MIT license text\n";
        files.push(serde_json::json!({
            "path": "licenses/MIT.txt",
            "size": license.len(),
            "sha256": digest(license)
        }));
        files.sort_by(|left, right| left["path"].as_str().cmp(&right["path"].as_str()));
        serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "payload_id": "test",
            "target": {
                "distribution": "ubuntu",
                "release": "26.04",
                "architecture": "amd64",
                "kernel_release": "test",
                "payload_abi": 1
            },
            "files": files
        }))
        .unwrap()
    }

    fn archive_bytes(members: &[Member<'_>]) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        let file_options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .unix_permissions(0o644);
        for member in members {
            match member.kind {
                MemberKind::File => {
                    writer.start_file(member.name, file_options).unwrap();
                    writer.write_all(member.bytes).unwrap();
                }
                MemberKind::Directory => {
                    writer.add_directory(member.name, file_options).unwrap();
                }
                MemberKind::Symlink => {
                    writer
                        .add_symlink(member.name, "target", file_options)
                        .unwrap();
                }
                MemberKind::Device => {
                    let options = file_options
                        .system(System::Unix)
                        .external_attributes((0o020666_u32) << 16);
                    writer.start_file(member.name, options).unwrap();
                    writer.write_all(member.bytes).unwrap();
                }
            }
        }
        if members.iter().any(|member| member.name == "payload.json")
            && !members
                .iter()
                .any(|member| member.name == "licenses/MIT.txt")
        {
            writer.start_file("licenses/MIT.txt", file_options).unwrap();
            writer.write_all(b"MIT license text\n").unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn rename_member(mut archive: Vec<u8>, from: &[u8], to: &[u8]) -> Vec<u8> {
        assert_eq!(from.len(), to.len());
        let mut replacements = 0;
        for index in 0..=archive.len() - from.len() {
            if archive[index..index + from.len()] == *from {
                archive[index..index + to.len()].copy_from_slice(to);
                replacements += 1;
            }
        }
        assert_eq!(
            replacements, 2,
            "local and central names must both be patched"
        );
        archive
    }

    fn entry(
        archive: &[u8],
        payload: &[u8],
        archive_size_limit: u64,
        expanded_size_limit: u64,
        file_count_limit: u64,
    ) -> crate::CatalogEntry {
        let json = serde_json::json!({
            "schema_version": 1,
            "entries": [{
                "payload_id": "test",
                "target": {
                    "distribution": "ubuntu",
                    "release": "26.04",
                    "architecture": "amd64",
                    "kernel_release": "test",
                    "payload_abi": 1
                },
                "archive_url": "https://offline.invalid/payload.zip",
                "archive_size": archive_size_limit,
                "expanded_size_limit": expanded_size_limit,
                "file_count_limit": file_count_limit,
                "archive_sha256": digest(archive),
                "payload_manifest_sha256": digest(payload),
                "required_renderers": ["d3d12-gallium"],
                "mesa_policy": "bundled",
                "sources": [{
                    "url": SOURCE_URL,
                    "commit": SOURCE_COMMIT,
                    "version": "1"
                }],
                "licenses": [{"spdx": "MIT", "path": "licenses/MIT.txt"}]
            }]
        });
        PayloadCatalog::from_json(&serde_json::to_vec(&json).unwrap())
            .unwrap()
            .entries()[0]
            .clone()
    }

    fn extract_fixture(
        archive: &[u8],
        payload: &[u8],
        archive_size_limit: u64,
        expanded_size_limit: u64,
        file_count_limit: u64,
        cancelled: bool,
    ) -> Result<(), PayloadError> {
        let temporary = TemporaryDirectory::new("extract");
        let archive_path = temporary.path().join("archive.zip");
        let destination = temporary.path().join("files");
        fs::write(&archive_path, archive).unwrap();
        fs::create_dir(&destination).unwrap();
        let entry = entry(
            archive,
            payload,
            archive_size_limit,
            expanded_size_limit,
            file_count_limit,
        );
        extract(
            &entry,
            &archive_path,
            &destination,
            &|_| {},
            &AtomicBool::new(cancelled),
        )
        .map(|_| ())
    }

    fn valid_archive(content: &[u8]) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let sources = source_manifest();
        let payload = payload_manifest(&[("content/file", content), ("sources.json", &sources)]);
        let archive = archive_bytes(&[
            Member {
                name: "payload.json",
                bytes: &payload,
                kind: MemberKind::File,
            },
            Member {
                name: "content/file",
                bytes: content,
                kind: MemberKind::File,
            },
            Member {
                name: "sources.json",
                bytes: &sources,
                kind: MemberKind::File,
            },
        ]);
        (archive, payload, sources)
    }

    #[test]
    fn extraction_rejects_an_overlay_with_a_digest_not_declared_by_payload() {
        let content = b"content";
        let sources = serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "sources": [{
                "url": SOURCE_URL,
                "commit": SOURCE_COMMIT,
                "version": "1",
                "paths": ["content/file"],
                "sha256": digest(b"source material")
            }],
            "overlays": [{
                "path": "content/file",
                "sha256": digest(b"different content"),
                "license": "MIT",
                "author": "VMLord contributors"
            }]
        }))
        .unwrap();
        let payload = payload_manifest(&[("content/file", content), ("sources.json", &sources)]);
        let archive = archive_bytes(&[
            Member {
                name: "payload.json",
                bytes: &payload,
                kind: MemberKind::File,
            },
            Member {
                name: "content/file",
                bytes: content,
                kind: MemberKind::File,
            },
            Member {
                name: "sources.json",
                bytes: &sources,
                kind: MemberKind::File,
            },
        ]);

        assert!(matches!(
            extract_fixture(
                &archive,
                &payload,
                archive.len() as u64,
                archive.len() as u64,
                3,
                false,
            ),
            Err(PayloadError::InvalidManifest(message)) if message.contains("overlay digest")
        ));
    }

    #[test]
    fn file_count_limit_counts_declared_files_not_payload_json() {
        let (archive, payload, _) = valid_archive(b"content");

        extract_fixture(
            &archive,
            &payload,
            archive.len() as u64,
            archive.len() as u64,
            3,
            false,
        )
        .unwrap();

        let error = extract_fixture(
            &archive,
            &payload,
            archive.len() as u64,
            archive.len() as u64,
            2,
            false,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            PayloadError::LimitExceeded {
                subject: "archive file count",
                limit: 2,
                actual: 3
            }
        ));
    }

    #[test]
    fn raw_dot_components_and_windows_prefixes_are_rejected_before_normalization() {
        let sources = source_manifest();
        for (declared, raw) in [
            ("content/file", "./content/file"),
            ("C:/escape", "C:/escape"),
            ("content/file:stream", "content/file:stream"),
            ("content/file.", "content/file."),
            ("NUL", "NUL"),
        ] {
            let payload = payload_manifest(&[(declared, b"content"), ("sources.json", &sources)]);
            let archive = archive_bytes(&[
                Member {
                    name: "payload.json",
                    bytes: &payload,
                    kind: MemberKind::File,
                },
                Member {
                    name: raw,
                    bytes: b"content",
                    kind: MemberKind::File,
                },
                Member {
                    name: "sources.json",
                    bytes: &sources,
                    kind: MemberKind::File,
                },
            ]);

            let error = extract_fixture(
                &archive,
                &payload,
                archive.len() as u64,
                archive.len() as u64,
                3,
                false,
            )
            .unwrap_err();
            assert!(
                matches!(error, PayloadError::UnsafeArchive(_)),
                "{raw}: {error}"
            );
        }
    }

    #[test]
    fn duplicate_payload_manifest_is_rejected() {
        let sources = source_manifest();
        let payload = payload_manifest(&[("content/file", b"content"), ("sources.json", &sources)]);
        let archive = archive_bytes(&[
            Member {
                name: "payload.json",
                bytes: &payload,
                kind: MemberKind::File,
            },
            Member {
                name: "payload.jsox",
                bytes: &payload,
                kind: MemberKind::File,
            },
            Member {
                name: "content/file",
                bytes: b"content",
                kind: MemberKind::File,
            },
            Member {
                name: "sources.json",
                bytes: &sources,
                kind: MemberKind::File,
            },
        ]);
        let archive = rename_member(archive, b"payload.jsox", b"payload.json");

        let error = extract_fixture(
            &archive,
            &payload,
            archive.len() as u64,
            archive.len() as u64,
            4,
            false,
        )
        .unwrap_err();
        assert!(matches!(error, PayloadError::UnsafeArchive(_)));
    }

    #[test]
    fn hostile_entry_types_names_duplicates_and_undeclared_files_are_rejected() {
        let sources = source_manifest();
        let payload = payload_manifest(&[("content/file", b"content"), ("sources.json", &sources)]);
        let cases = [
            Member {
                name: "../escape",
                bytes: b"content",
                kind: MemberKind::File,
            },
            Member {
                name: "/absolute",
                bytes: b"content",
                kind: MemberKind::File,
            },
            Member {
                name: r"content\windows",
                bytes: b"content",
                kind: MemberKind::File,
            },
            Member {
                name: "content/directory/",
                bytes: b"",
                kind: MemberKind::Directory,
            },
            Member {
                name: "content/link",
                bytes: b"",
                kind: MemberKind::Symlink,
            },
            Member {
                name: "content/device",
                bytes: b"",
                kind: MemberKind::Device,
            },
            Member {
                name: "content/extra",
                bytes: b"extra",
                kind: MemberKind::File,
            },
        ];

        for hostile in cases {
            let archive = archive_bytes(&[
                Member {
                    name: "payload.json",
                    bytes: &payload,
                    kind: MemberKind::File,
                },
                Member {
                    name: "content/file",
                    bytes: b"content",
                    kind: MemberKind::File,
                },
                hostile,
                Member {
                    name: "sources.json",
                    bytes: &sources,
                    kind: MemberKind::File,
                },
            ]);
            let error = extract_fixture(
                &archive,
                &payload,
                archive.len() as u64,
                archive.len() as u64,
                4,
                false,
            )
            .unwrap_err();
            assert!(
                matches!(
                    error,
                    PayloadError::UnsafeArchive(_) | PayloadError::InvalidManifest(_)
                ),
                "{}: {error}",
                hostile.name
            );
        }

        let archive = archive_bytes(&[
            Member {
                name: "payload.json",
                bytes: &payload,
                kind: MemberKind::File,
            },
            Member {
                name: "content/file",
                bytes: b"content",
                kind: MemberKind::File,
            },
            Member {
                name: "content/fild",
                bytes: b"content",
                kind: MemberKind::File,
            },
            Member {
                name: "sources.json",
                bytes: &sources,
                kind: MemberKind::File,
            },
        ]);
        let archive = rename_member(archive, b"content/fild", b"content/file");
        assert!(matches!(
            extract_fixture(
                &archive,
                &payload,
                archive.len() as u64,
                archive.len() as u64,
                4,
                false
            ),
            Err(PayloadError::UnsafeArchive(_))
        ));
    }

    #[test]
    fn compressed_and_expanded_limits_are_enforced() {
        let large = vec![b'a'; 64 * 1024];
        let (archive, payload, sources) = valid_archive(&large);

        let compressed_error = extract_fixture(
            &archive,
            &payload,
            1,
            (large.len() + b"MIT license text\n".len() + sources.len()) as u64,
            3,
            false,
        )
        .unwrap_err();
        assert!(matches!(
            compressed_error,
            PayloadError::LimitExceeded {
                subject: "compressed size",
                ..
            }
        ));

        let expanded_limit = archive.len() as u64;
        assert!(
            expanded_limit < (large.len() + b"MIT license text\n".len() + sources.len()) as u64
        );
        let expanded_error = extract_fixture(
            &archive,
            &payload,
            archive.len() as u64,
            expanded_limit,
            3,
            false,
        )
        .unwrap_err();
        assert!(matches!(
            expanded_error,
            PayloadError::LimitExceeded {
                subject: "expanded size",
                ..
            }
        ));
    }

    #[test]
    fn wrong_payload_digest_declared_size_and_cancellation_are_rejected() {
        let (archive, payload, sources) = valid_archive(b"content");
        let wrong_digest_entry = entry(
            &archive,
            b"different payload bytes",
            archive.len() as u64,
            archive.len() as u64,
            3,
        );
        let temporary = TemporaryDirectory::new("wrong-digest");
        let archive_path = temporary.path().join("archive.zip");
        let destination = temporary.path().join("files");
        fs::write(&archive_path, &archive).unwrap();
        fs::create_dir(&destination).unwrap();
        assert!(matches!(
            extract(
                &wrong_digest_entry,
                &archive_path,
                &destination,
                &|_| {},
                &AtomicBool::new(false)
            ),
            Err(PayloadError::DigestMismatch { .. })
        ));

        let wrong_size_payload =
            payload_manifest(&[("content/file", b"content!"), ("sources.json", &sources)]);
        let wrong_size_archive = archive_bytes(&[
            Member {
                name: "payload.json",
                bytes: &wrong_size_payload,
                kind: MemberKind::File,
            },
            Member {
                name: "content/file",
                bytes: b"content",
                kind: MemberKind::File,
            },
            Member {
                name: "sources.json",
                bytes: &sources,
                kind: MemberKind::File,
            },
        ]);
        assert!(matches!(
            extract_fixture(
                &wrong_size_archive,
                &wrong_size_payload,
                wrong_size_archive.len() as u64,
                wrong_size_archive.len() as u64,
                3,
                false
            ),
            Err(PayloadError::InvalidManifest(_))
        ));

        assert!(matches!(
            extract_fixture(
                &archive,
                &payload,
                archive.len() as u64,
                archive.len() as u64,
                3,
                true
            ),
            Err(PayloadError::Cancelled)
        ));
    }

    #[test]
    fn streamed_members_reject_extra_bytes_and_mid_stream_cancellation() {
        let temporary = TemporaryDirectory::new("streaming");
        let expected = Sha256Digest::hash_reader(b"abcdef".as_slice()).unwrap();
        let mut extra = Cursor::new(b"abcdefX".as_slice());
        let extra_error = stream_member(
            &mut extra,
            &temporary.path().join("extra"),
            "content/file",
            6,
            &expected,
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(matches!(extra_error, PayloadError::InvalidManifest(_)));

        struct CancelAfterFirstRead<'a> {
            inner: Cursor<&'a [u8]>,
            cancel: &'a AtomicBool,
            first_read: bool,
        }

        impl Read for CancelAfterFirstRead<'_> {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                let count = self.inner.read(buffer)?;
                if self.first_read {
                    self.cancel.store(true, Ordering::Relaxed);
                    self.first_read = false;
                }
                Ok(count)
            }
        }

        let bytes = vec![b'a'; 128 * 1024];
        let expected = Sha256Digest::hash_reader(bytes.as_slice()).unwrap();
        let cancel = AtomicBool::new(false);
        let mut reader = CancelAfterFirstRead {
            inner: Cursor::new(bytes.as_slice()),
            cancel: &cancel,
            first_read: true,
        };
        let cancel_error = stream_member(
            &mut reader,
            &temporary.path().join("cancelled"),
            "content/large",
            bytes.len() as u64,
            &expected,
            &cancel,
        )
        .unwrap_err();
        assert!(matches!(cancel_error, PayloadError::Cancelled));
        assert!(
            fs::metadata(temporary.path().join("cancelled"))
                .unwrap()
                .len()
                < bytes.len() as u64
        );
    }
}
