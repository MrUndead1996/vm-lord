use std::{
    collections::HashSet,
    fs::{self, File, TryLockError},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

use sha2::{Digest, Sha256};

use crate::{
    CatalogEntry, PayloadError, PayloadManifest, PayloadProgress, Sha256Digest, SourceManifest,
    archive, download::LockedArchive, manifest::cache_provenance,
};

const PAYLOAD_MANIFEST_LIMIT: u64 = 1024 * 1024;
const HASH_BUFFER_SIZE: usize = 64 * 1024;
static NEXT_OPERATION: AtomicU64 = AtomicU64::new(0);

pub struct PrepareRequest<'a> {
    pub entry: &'a CatalogEntry,
    pub cache_root: &'a Path,
    pub progress: &'a dyn Fn(PayloadProgress),
    pub cancel: &'a AtomicBool,
}

pub struct ReadyGpuPayload {
    payload_id: String,
    generation: Sha256Digest,
    payload_manifest_sha256: Sha256Digest,
    files_directory: PathBuf,
    manifest: PayloadManifest,
    provenance_path: PathBuf,
}

impl ReadyGpuPayload {
    pub fn payload_id(&self) -> &str {
        &self.payload_id
    }

    pub fn generation(&self) -> &Sha256Digest {
        &self.generation
    }

    pub fn files_directory(&self) -> &Path {
        &self.files_directory
    }

    pub fn manifest(&self) -> &PayloadManifest {
        &self.manifest
    }

    pub fn provenance_path(&self) -> &Path {
        &self.provenance_path
    }

    pub(crate) fn payload_manifest_sha256(&self) -> &Sha256Digest {
        &self.payload_manifest_sha256
    }
}

pub fn prepare(request: PrepareRequest<'_>) -> Result<ReadyGpuPayload, PayloadError> {
    let root = request.cache_root.join("gpu-payload").join("v1");
    let _digest_lock = DigestLock::acquire(&root, request.entry)?;
    let final_directory = root.join(request.entry.archive_sha256().as_hex());
    let mut quarantines = Vec::new();
    if path_exists(&final_directory) {
        match load_ready(
            request.entry,
            &final_directory,
            request.progress,
            request.cancel,
        ) {
            Ok(ready) => return Ok(ready),
            Err(PayloadError::Cancelled) => return Err(PayloadError::Cancelled),
            Err(_) => {
                if let Some(quarantine) = quarantine(&final_directory, request.entry)? {
                    quarantines.push(OperationPath::new(quarantine));
                }
            }
        }
    }

    let mut locked = LockedArchive::acquire(&root, request.entry)?;
    locked.download(request.progress, request.cancel)?;
    locked.verify(request.progress, request.cancel)?;
    let archive_path = locked.path().to_owned();
    drop(locked);
    let ready = prepare_verified_archive(
        request.entry,
        &archive_path,
        &root,
        request.progress,
        request.cancel,
    );
    drop(quarantines);
    ready
}

pub(crate) fn prepare_verified_archive(
    entry: &CatalogEntry,
    archive: &Path,
    root: &Path,
    progress: &dyn Fn(PayloadProgress),
    cancel: &AtomicBool,
) -> Result<ReadyGpuPayload, PayloadError> {
    fs::create_dir_all(root)
        .map_err(|error| PayloadError::io("create GPU payload cache", root.into(), error))?;
    let final_directory = root.join(entry.archive_sha256().as_hex());
    let mut quarantines = Vec::new();
    if path_exists(&final_directory) {
        match load_ready(entry, &final_directory, progress, cancel) {
            Ok(ready) => return Ok(ready),
            Err(PayloadError::Cancelled) => return Err(PayloadError::Cancelled),
            Err(_) => {
                if let Some(path) = quarantine(&final_directory, entry)? {
                    quarantines.push(OperationPath::new(path));
                }
            }
        }
    }

    let mut temporary = OperationPath::new(create_temporary_directory(root, entry)?);
    let files_directory = temporary.path().join("files");
    fs::create_dir(&files_directory).map_err(|error| {
        PayloadError::io(
            "create cache temporary files directory",
            files_directory.clone(),
            error,
        )
    })?;
    let cached_archive = temporary.path().join("archive.zip");
    copy_and_flush(archive, &cached_archive, entry.archive_size(), cancel)?;
    verify_digest(
        &cached_archive,
        entry.archive_sha256(),
        format!("payload {} archive", entry.payload_id()),
        cancel,
    )?;
    let (_, sources) =
        archive::extract(entry, &cached_archive, &files_directory, progress, cancel)?;
    write_and_flush(
        &temporary.path().join("provenance.json"),
        &cache_provenance(entry, &sources)?,
        "write provenance",
    )?;

    loop {
        match rename_noreplace(temporary.path(), &final_directory) {
            Ok(()) => {
                temporary.disarm();
                let ready = load_ready(entry, &final_directory, progress, cancel);
                if ready.is_err() && path_exists(&final_directory) {
                    if let Some(path) = quarantine(&final_directory, entry)? {
                        quarantines.push(OperationPath::new(path));
                    }
                }
                return ready;
            }
            Err(_) if path_exists(&final_directory) => {
                match load_ready(entry, &final_directory, progress, cancel) {
                    Ok(ready) => return Ok(ready),
                    Err(PayloadError::Cancelled) => return Err(PayloadError::Cancelled),
                    Err(_) => {
                        if let Some(path) = quarantine(&final_directory, entry)? {
                            quarantines.push(OperationPath::new(path));
                        }
                    }
                }
            }
            Err(error) => {
                return Err(PayloadError::io(
                    "publish cache entry",
                    final_directory,
                    error,
                ));
            }
        }
    }
}

fn load_ready(
    entry: &CatalogEntry,
    root: &Path,
    progress: &dyn Fn(PayloadProgress),
    cancel: &AtomicBool,
) -> Result<ReadyGpuPayload, PayloadError> {
    require_directory(root, "verify cached payload directory")?;
    if cancel.load(Ordering::Relaxed) {
        return Err(PayloadError::Cancelled);
    }

    let archive_path = root.join("archive.zip");
    let archive_size = require_regular_file(&archive_path, "verify cached archive")?;
    if archive_size != entry.archive_size() {
        return Err(PayloadError::ArchiveSizeMismatch {
            expected: entry.archive_size(),
            actual: archive_size,
        });
    }
    progress(PayloadProgress::Verifying {
        hashed: 0,
        total: entry.archive_size(),
    });
    verify_digest(
        &archive_path,
        entry.archive_sha256(),
        format!("payload {} archive", entry.payload_id()),
        cancel,
    )?;

    let files_directory = root.join("files");
    require_directory(&files_directory, "verify cached files directory")?;
    let payload_path = files_directory.join("payload.json");
    let payload_bytes = read_bounded_regular_file(
        &payload_path,
        PAYLOAD_MANIFEST_LIMIT,
        "read cached payload manifest",
    )?;
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
    verify_cached_tree(&files_directory, &manifest)?;

    for file in manifest.files() {
        if cancel.load(Ordering::Relaxed) {
            return Err(PayloadError::Cancelled);
        }
        let path = files_directory.join(file.path());
        let actual_size = require_regular_file(&path, "verify cached file")?;
        if actual_size != file.size() {
            return Err(PayloadError::InvalidManifest(format!(
                "size mismatch for {}",
                file.path()
            )));
        }
        verify_digest(&path, file.sha256(), file.path().into(), cancel)?;
    }

    let sources_path = files_directory.join("sources.json");
    let source_size = manifest
        .files()
        .iter()
        .find(|file| file.path() == "sources.json")
        .expect("validated manifests declare sources.json")
        .size();
    let source_bytes =
        read_bounded_regular_file(&sources_path, source_size, "read cached sources manifest")?;
    let sources = SourceManifest::parse_and_validate(&source_bytes, entry)?;
    sources.validate_prepared_files(&manifest)?;
    let provenance_path = root.join("provenance.json");
    let expected_provenance = cache_provenance(entry, &sources)?;
    let actual_provenance = read_bounded_regular_file(
        &provenance_path,
        expected_provenance.len() as u64,
        "read cached provenance",
    )?;
    if actual_provenance != expected_provenance {
        return Err(PayloadError::InvalidManifest(
            "cached provenance does not match payload".into(),
        ));
    }

    Ok(ReadyGpuPayload {
        payload_id: entry.payload_id().into(),
        generation: entry.archive_sha256().clone(),
        payload_manifest_sha256: entry.payload_manifest_sha256().clone(),
        files_directory,
        manifest,
        provenance_path,
    })
}

fn validate_manifest_limits(
    manifest: &PayloadManifest,
    entry: &CatalogEntry,
) -> Result<(), PayloadError> {
    let count = u64::try_from(manifest.files().len()).unwrap_or(u64::MAX);
    if count > entry.file_count_limit() {
        return Err(PayloadError::LimitExceeded {
            subject: "archive file count",
            limit: entry.file_count_limit(),
            actual: count,
        });
    }
    let mut expanded = 0_u64;
    for file in manifest.files() {
        expanded = expanded
            .checked_add(file.size())
            .ok_or(PayloadError::LimitExceeded {
                subject: "expanded size",
                limit: entry.expanded_size_limit(),
                actual: u64::MAX,
            })?;
        if expanded > entry.expanded_size_limit() {
            return Err(PayloadError::LimitExceeded {
                subject: "expanded size",
                limit: entry.expanded_size_limit(),
                actual: expanded,
            });
        }
    }
    Ok(())
}

fn verify_cached_tree(root: &Path, manifest: &PayloadManifest) -> Result<(), PayloadError> {
    let mut expected = manifest
        .files()
        .iter()
        .map(|file| PathBuf::from(file.path()))
        .collect::<HashSet<_>>();
    expected.insert(PathBuf::from("payload.json"));
    let mut actual = HashSet::new();
    collect_regular_files(root, root, &mut actual)?;
    if actual != expected {
        return Err(PayloadError::InvalidManifest(
            "cached files do not exactly match payload.json".into(),
        ));
    }
    Ok(())
}

fn collect_regular_files(
    root: &Path,
    directory: &Path,
    files: &mut HashSet<PathBuf>,
) -> Result<(), PayloadError> {
    for entry in fs::read_dir(directory)
        .map_err(|error| PayloadError::io("inspect cached files", directory.into(), error))?
    {
        let entry = entry
            .map_err(|error| PayloadError::io("inspect cached files", directory.into(), error))?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| PayloadError::io("inspect cached file", path.clone(), error))?;
        if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
            return Err(PayloadError::UnsafeArchive(path.display().to_string()));
        }
        if metadata.is_dir() {
            collect_regular_files(root, &path, files)?;
        } else if metadata.is_file() {
            files.insert(
                path.strip_prefix(root)
                    .expect("walked cache paths remain below their root")
                    .to_owned(),
            );
        } else {
            return Err(PayloadError::UnsafeArchive(path.display().to_string()));
        }
    }
    Ok(())
}

fn verify_digest(
    path: &Path,
    expected: &Sha256Digest,
    subject: String,
    cancel: &AtomicBool,
) -> Result<(), PayloadError> {
    let mut file = File::open(path)
        .map_err(|error| PayloadError::io("read cached file", path.into(), error))?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; HASH_BUFFER_SIZE];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(PayloadError::Cancelled);
        }
        let count = file
            .read(&mut buffer)
            .map_err(|error| PayloadError::io("read cached file", path.into(), error))?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    let actual = Sha256Digest::from_bytes(hash.finalize().into())?;
    if actual != *expected {
        return Err(PayloadError::DigestMismatch {
            subject,
            expected: expected.clone(),
            actual,
        });
    }
    Ok(())
}

fn require_regular_file(path: &Path, operation: &'static str) -> Result<u64, PayloadError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| PayloadError::io(operation, path.into(), error))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
        return Err(PayloadError::UnsafeArchive(path.display().to_string()));
    }
    Ok(metadata.len())
}

fn require_directory(path: &Path, operation: &'static str) -> Result<(), PayloadError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| PayloadError::io(operation, path.into(), error))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
        return Err(PayloadError::UnsafeArchive(path.display().to_string()));
    }
    Ok(())
}

#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

fn read_bounded_regular_file(
    path: &Path,
    limit: u64,
    operation: &'static str,
) -> Result<Vec<u8>, PayloadError> {
    let size = require_regular_file(path, operation)?;
    if size > limit {
        return Err(PayloadError::LimitExceeded {
            subject: "cached metadata",
            limit,
            actual: size,
        });
    }
    let mut bytes = Vec::with_capacity(size as usize);
    File::open(path)
        .map_err(|error| PayloadError::io(operation, path.into(), error))?
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| PayloadError::io(operation, path.into(), error))?;
    if bytes.len() as u64 > limit {
        return Err(PayloadError::LimitExceeded {
            subject: "cached metadata",
            limit,
            actual: bytes.len() as u64,
        });
    }
    Ok(bytes)
}

fn copy_and_flush(
    source: &Path,
    destination: &Path,
    expected_size: u64,
    cancel: &AtomicBool,
) -> Result<(), PayloadError> {
    let mut input = File::open(source)
        .map_err(|error| PayloadError::io("open verified archive", source.into(), error))?;
    let mut output = File::options()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|error| PayloadError::io("copy verified archive", destination.into(), error))?;
    let mut buffer = [0_u8; HASH_BUFFER_SIZE];
    let mut copied = 0_u64;
    while copied < expected_size {
        if cancel.load(Ordering::Relaxed) {
            return Err(PayloadError::Cancelled);
        }
        let capacity = (expected_size - copied).min(buffer.len() as u64) as usize;
        let count = input
            .read(&mut buffer[..capacity])
            .map_err(|error| PayloadError::io("copy verified archive", source.into(), error))?;
        if count == 0 {
            return Err(PayloadError::ArchiveSizeMismatch {
                expected: expected_size,
                actual: copied,
            });
        }
        output.write_all(&buffer[..count]).map_err(|error| {
            PayloadError::io("copy verified archive", destination.into(), error)
        })?;
        copied = copied
            .checked_add(count as u64)
            .ok_or(PayloadError::ArchiveSizeMismatch {
                expected: expected_size,
                actual: u64::MAX,
            })?;
    }
    let mut extra = [0_u8; 1];
    if input
        .read(&mut extra)
        .map_err(|error| PayloadError::io("copy verified archive", source.into(), error))?
        != 0
    {
        return Err(PayloadError::ArchiveSizeMismatch {
            expected: expected_size,
            actual: expected_size.saturating_add(1),
        });
    }
    output
        .sync_all()
        .map_err(|error| PayloadError::io("flush verified archive", destination.into(), error))
}

fn write_and_flush(path: &Path, bytes: &[u8], operation: &'static str) -> Result<(), PayloadError> {
    let mut file = File::options()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| PayloadError::io(operation, path.into(), error))?;
    file.write_all(bytes)
        .map_err(|error| PayloadError::io(operation, path.into(), error))?;
    file.sync_all()
        .map_err(|error| PayloadError::io("flush cache metadata", path.into(), error))
}

fn create_temporary_directory(root: &Path, entry: &CatalogEntry) -> Result<PathBuf, PayloadError> {
    loop {
        let path = unique_operation_path(root, entry, "tmp");
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(PayloadError::io(
                    "create cache temporary directory",
                    path,
                    error,
                ));
            }
        }
    }
}

fn quarantine(source: &Path, entry: &CatalogEntry) -> Result<Option<PathBuf>, PayloadError> {
    let root = source.parent().expect("cache entries have a parent");
    loop {
        let destination = unique_operation_path(root, entry, "corrupt");
        match rename_noreplace(source, &destination) {
            Ok(()) => return Ok(Some(destination)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(PayloadError::io(
                    "quarantine corrupt cache entry",
                    source.into(),
                    error,
                ));
            }
        }
    }
}

fn unique_operation_path(root: &Path, entry: &CatalogEntry, kind: &str) -> PathBuf {
    let sequence = NEXT_OPERATION.fetch_add(1, Ordering::Relaxed);
    root.join(format!(
        "{}.{kind}-{}-{sequence}",
        entry.archive_sha256(),
        std::process::id()
    ))
}

fn path_exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn rename_noreplace(source: &Path, destination: &Path) -> io::Result<()> {
    match platform::rename_noreplace(source, destination) {
        Err(error) if fs::symlink_metadata(destination).is_ok() => {
            Err(io::Error::new(io::ErrorKind::AlreadyExists, error))
        }
        result => result,
    }
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod platform {
    use std::{
        ffi::{CString, c_char, c_int, c_uint},
        io,
        os::unix::ffi::OsStrExt,
        path::Path,
    };

    const AT_FDCWD: c_int = -100;
    const RENAME_NOREPLACE: c_uint = 1;

    unsafe extern "C" {
        fn renameat2(
            old_directory: c_int,
            old_path: *const c_char,
            new_directory: c_int,
            new_path: *const c_char,
            flags: c_uint,
        ) -> c_int;
    }

    pub(super) fn rename_noreplace(source: &Path, destination: &Path) -> io::Result<()> {
        let source = CString::new(source.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source path contains NUL"))?;
        let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "destination path contains NUL")
        })?;
        // SAFETY: Both paths are live NUL-terminated C strings for the duration of the call.
        let result = unsafe {
            renameat2(
                AT_FDCWD,
                source.as_ptr(),
                AT_FDCWD,
                destination.as_ptr(),
                RENAME_NOREPLACE,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod platform {
    use std::{
        io,
        os::windows::ffi::OsStrExt,
        path::{Path, absolute},
    };

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(source: *const u16, destination: *const u16, flags: u32) -> i32;
    }

    pub(super) fn rename_noreplace(source: &Path, destination: &Path) -> io::Result<()> {
        let source = verbatim_path(source)?;
        let destination = verbatim_path(destination)?;
        // SAFETY: Both vectors are NUL-terminated UTF-16 strings and remain live for the call.
        let result = unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), 0) };
        if result != 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn verbatim_path(path: &Path) -> io::Result<Vec<u16>> {
        let absolute = absolute(path)?;
        let encoded = absolute.as_os_str().encode_wide().collect::<Vec<_>>();
        if encoded.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path contains NUL",
            ));
        }
        let mut verbatim = Vec::with_capacity(encoded.len() + 8);
        if encoded.starts_with(&[b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16])
            || encoded.starts_with(&[b'\\' as u16, b'\\' as u16, b'.' as u16, b'\\' as u16])
        {
            verbatim.extend(encoded);
        } else if encoded.starts_with(&[b'\\' as u16, b'\\' as u16]) {
            verbatim.extend("\\\\?\\UNC\\".encode_utf16());
            verbatim.extend_from_slice(&encoded[2..]);
        } else {
            verbatim.extend("\\\\?\\".encode_utf16());
            verbatim.extend(encoded);
        }
        verbatim.push(0);
        Ok(verbatim)
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod platform {
    use std::{io, path::Path};

    pub(super) fn rename_noreplace(_source: &Path, _destination: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "atomic no-replace rename is unsupported on this platform",
        ))
    }
}

struct DigestLock {
    _file: File,
}

impl DigestLock {
    fn acquire(root: &Path, entry: &CatalogEntry) -> Result<Self, PayloadError> {
        fs::create_dir_all(root)
            .map_err(|error| PayloadError::io("create GPU payload cache", root.into(), error))?;
        let path = root.join(format!("{}.lock", entry.archive_sha256()));
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| PayloadError::io("open payload digest lock", path.clone(), error))?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(TryLockError::WouldBlock) => Err(PayloadError::AlreadyInProgress { path }),
            Err(TryLockError::Error(error)) => {
                Err(PayloadError::io("lock payload digest", path, error))
            }
        }
    }
}

struct OperationPath {
    path: PathBuf,
    armed: bool,
}

impl OperationPath {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for OperationPath {
    fn drop(&mut self) {
        if self.armed {
            if let Ok(metadata) = fs::symlink_metadata(&self.path) {
                if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
                    if fs::remove_dir(&self.path).is_err() {
                        let _ = fs::remove_file(&self.path);
                    }
                } else if metadata.is_dir() {
                    let _ = fs::remove_dir_all(&self.path);
                } else {
                    let _ = fs::remove_file(&self.path);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs::{self, File},
        io::{Cursor, Write},
        path::{Path, PathBuf},
        sync::{
            Arc, Barrier,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
        thread,
    };

    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    use crate::{PayloadCatalog, PayloadError, PayloadProgress, Sha256Digest};

    use super::{
        OperationPath, PrepareRequest, prepare, prepare_verified_archive, rename_noreplace,
    };

    const SOURCE_URL: &str = "https://github.com/example/source";
    const SOURCE_COMMIT: &str = "14794180686c2fb6307fbe359c359bec765249f3";
    static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

    struct TemporaryDirectory(PathBuf);

    impl TemporaryDirectory {
        fn new(label: &str) -> Self {
            let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "vmlord-gpu-payload-cache-{label}-{}-{sequence}",
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

    struct Fixture {
        temporary: TemporaryDirectory,
        archive: Vec<u8>,
        payload: Vec<u8>,
        entry: crate::CatalogEntry,
        archive_path: PathBuf,
    }

    impl Fixture {
        fn new(label: &str) -> Self {
            let temporary = TemporaryDirectory::new(label);
            let content = b"original content";
            let source = serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "sources": [{
                    "url": SOURCE_URL,
                    "commit": SOURCE_COMMIT,
                    "version": "1",
                    "paths": ["content/file"],
                    "sha256": digest(content)
                }],
                "overlays": []
            }))
            .unwrap();
            let license = b"MIT license text\n";
            let payload = serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "payload_id": "test",
                "target": {
                    "distribution": "ubuntu",
                    "release": "26.04",
                    "architecture": "amd64",
                    "kernel_release": "test",
                    "payload_abi": 1
                },
                "files": [
                    {
                        "path": "content/file",
                        "size": content.len(),
                        "sha256": digest(content)
                    },
                    {
                        "path": "licenses/MIT.txt",
                        "size": license.len(),
                        "sha256": digest(license)
                    },
                    {
                        "path": "sources.json",
                        "size": source.len(),
                        "sha256": digest(&source)
                    }
                ]
            }))
            .unwrap();
            let archive = build_archive(&payload, content, license, &source);
            let catalog = serde_json::json!({
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
                    "archive_size": archive.len(),
                    "expanded_size_limit": archive.len(),
                    "file_count_limit": 3,
                    "archive_sha256": digest(&archive),
                    "payload_manifest_sha256": digest(&payload),
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
            let entry = PayloadCatalog::from_json(&serde_json::to_vec(&catalog).unwrap())
                .unwrap()
                .entries()[0]
                .clone();
            let archive_path = temporary.path().join("fixture.zip");
            fs::write(&archive_path, &archive).unwrap();
            Self {
                temporary,
                archive,
                payload,
                entry,
                archive_path,
            }
        }

        fn cache_root(&self) -> PathBuf {
            self.temporary.path().join("cache")
        }

        fn version_root(&self) -> PathBuf {
            self.cache_root().join("gpu-payload/v1")
        }

        fn final_directory(&self) -> PathBuf {
            self.version_root()
                .join(self.entry.archive_sha256().as_hex())
        }

        fn prepare_local(&self) -> Result<super::ReadyGpuPayload, PayloadError> {
            prepare_verified_archive(
                &self.entry,
                &self.archive_path,
                &self.version_root(),
                &|_| {},
                &AtomicBool::new(false),
            )
        }
    }

    fn digest(bytes: &[u8]) -> String {
        Sha256Digest::hash_reader(bytes)
            .unwrap()
            .as_hex()
            .to_owned()
    }

    fn build_archive(payload: &[u8], content: &[u8], license: &[u8], source: &[u8]) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .unix_permissions(0o644);
        for (name, bytes) in [
            ("payload.json", payload),
            ("content/file", content),
            ("licenses/MIT.txt", license),
            ("sources.json", source),
        ] {
            writer.start_file(name, options).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn assert_no_operation_directories(fixture: &Fixture) {
        let prefix = fixture.entry.archive_sha256().as_hex();
        for entry in fs::read_dir(fixture.version_root()).unwrap() {
            let name = entry.unwrap().file_name().into_string().unwrap();
            assert!(
                !name.starts_with(&format!("{prefix}.tmp-"))
                    && !name.starts_with(&format!("{prefix}.corrupt-")),
                "operation directory was left behind: {name}"
            );
        }
    }

    #[test]
    fn warm_cache_is_verified_under_digest_lock_without_network_access() {
        let fixture = Fixture::new("offline-hit");
        fixture.prepare_local().unwrap();
        let connected = AtomicBool::new(false);
        let progress = |event| {
            if event == PayloadProgress::Connecting {
                connected.store(true, Ordering::Relaxed);
            }
        };

        let ready = prepare(PrepareRequest {
            entry: &fixture.entry,
            cache_root: &fixture.cache_root(),
            progress: &progress,
            cancel: &AtomicBool::new(false),
        })
        .unwrap();

        assert_eq!(ready.generation(), fixture.entry.archive_sha256());
        assert!(!connected.load(Ordering::Relaxed));
        assert!(
            fixture
                .version_root()
                .join(format!("{}.lock", fixture.entry.archive_sha256()))
                .is_file()
        );
    }

    #[test]
    fn corrupt_archive_payload_and_prepared_file_are_quarantined_and_rebuilt() {
        let fixture = Fixture::new("corruption");
        fixture.prepare_local().unwrap();

        let mut corrupt_archive = fixture.archive.clone();
        corrupt_archive[0] ^= 0xff;
        fs::write(
            fixture.final_directory().join("archive.zip"),
            corrupt_archive,
        )
        .unwrap();
        fixture.prepare_local().unwrap();
        assert_eq!(
            fs::read(fixture.final_directory().join("archive.zip")).unwrap(),
            fixture.archive
        );

        for (relative, corrupt, expected) in [
            (
                "files/payload.json",
                b"{}".as_slice(),
                fixture.payload.as_slice(),
            ),
            (
                "files/content/file",
                b"tampered content".as_slice(),
                b"original content".as_slice(),
            ),
        ] {
            fs::write(fixture.final_directory().join(relative), corrupt).unwrap();

            fixture.prepare_local().unwrap();

            assert_eq!(
                fs::read(fixture.final_directory().join(relative)).unwrap(),
                expected
            );
            assert_no_operation_directories(&fixture);
        }
    }

    #[test]
    fn payload_manifest_digest_is_rechecked_even_when_json_still_parses() {
        let fixture = Fixture::new("payload-digest");
        fixture.prepare_local().unwrap();
        let mut changed = fixture.payload.clone();
        changed.push(b'\n');
        fs::write(
            fixture.final_directory().join("files/payload.json"),
            changed,
        )
        .unwrap();

        fixture.prepare_local().unwrap();

        assert_eq!(
            fs::read(fixture.final_directory().join("files/payload.json")).unwrap(),
            fixture.payload
        );
    }

    #[test]
    fn digest_lock_contention_is_reported_before_cache_or_network_work() {
        let fixture = Fixture::new("lock-order");
        fixture.prepare_local().unwrap();
        let lock_path = fixture
            .version_root()
            .join(format!("{}.lock", fixture.entry.archive_sha256()));
        let lock = File::options()
            .read(true)
            .write(true)
            .create(true)
            .open(&lock_path)
            .unwrap();
        lock.try_lock().unwrap();
        let connected = AtomicBool::new(false);
        let progress = |event| {
            if event == PayloadProgress::Connecting {
                connected.store(true, Ordering::Relaxed);
            }
        };

        let result = prepare(PrepareRequest {
            entry: &fixture.entry,
            cache_root: &fixture.cache_root(),
            progress: &progress,
            cancel: &AtomicBool::new(false),
        });

        assert!(
            matches!(result, Err(PayloadError::AlreadyInProgress { path }) if path == lock_path)
        );
        assert!(!connected.load(Ordering::Relaxed));
    }

    #[test]
    fn concurrent_preparers_adopt_one_verified_winner_without_leftovers() {
        let fixture = Fixture::new("race");
        let entry = Arc::new(fixture.entry.clone());
        let archive_path = Arc::new(fixture.archive_path.clone());
        let version_root = Arc::new(fixture.version_root());
        let barrier = Arc::new(Barrier::new(4));
        let mut workers = Vec::new();
        for _ in 0..4 {
            let entry = Arc::clone(&entry);
            let archive_path = Arc::clone(&archive_path);
            let version_root = Arc::clone(&version_root);
            let barrier = Arc::clone(&barrier);
            workers.push(thread::spawn(move || {
                barrier.wait();
                prepare_verified_archive(
                    &entry,
                    &archive_path,
                    &version_root,
                    &|_| {},
                    &AtomicBool::new(false),
                )
                .map(|ready| ready.generation().clone())
            }));
        }

        for worker in workers {
            assert_eq!(
                worker.join().unwrap().unwrap(),
                *fixture.entry.archive_sha256()
            );
        }
        assert!(fixture.final_directory().is_dir());
        assert_no_operation_directories(&fixture);
    }

    #[test]
    fn no_replace_rename_preserves_an_existing_empty_directory() {
        let temporary = TemporaryDirectory::new("rename-no-replace");
        let source = temporary.path().join("source");
        let destination = temporary.path().join("destination");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("marker"), b"source").unwrap();
        fs::create_dir(&destination).unwrap();

        let error = rename_noreplace(&source, &destination).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(source.join("marker")).unwrap(), b"source");
        assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
    }

    #[test]
    fn operation_cleanup_removes_a_quarantined_regular_file() {
        let temporary = TemporaryDirectory::new("regular-file-cleanup");
        let quarantine = temporary.path().join("digest.corrupt-1-1");
        fs::write(&quarantine, b"corrupt").unwrap();

        drop(OperationPath::new(quarantine.clone()));

        assert!(fs::symlink_metadata(quarantine).is_err());
    }
}
