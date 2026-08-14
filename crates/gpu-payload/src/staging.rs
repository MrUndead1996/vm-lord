use std::{
    collections::HashSet,
    fs::{self, File, TryLockError},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    thread,
    time::Duration,
};

use sha2::{Digest, Sha256};

use crate::{PayloadError, PayloadProgress, ReadyGpuPayload, ReadyMarker, Sha256Digest};

const HASH_BUFFER_SIZE: usize = 64 * 1024;
const READY_MARKER_SIZE_LIMIT: u64 = 64 * 1024;
static NEXT_OPERATION: AtomicU64 = AtomicU64::new(0);

pub struct StagedGpuPayload {
    payload_id: String,
    generation: Sha256Digest,
    generation_directory: PathBuf,
    ready_marker_path: PathBuf,
}

impl StagedGpuPayload {
    pub fn payload_id(&self) -> &str {
        &self.payload_id
    }

    pub fn generation(&self) -> &Sha256Digest {
        &self.generation
    }

    pub fn generation_directory(&self) -> &Path {
        &self.generation_directory
    }

    pub fn ready_marker_path(&self) -> &Path {
        &self.ready_marker_path
    }
}

pub fn ensure_staging_root(path: &Path) -> Result<(), PayloadError> {
    for child in ["generations", "ready"] {
        let directory = path.join(child);
        fs::create_dir_all(&directory).map_err(|error| {
            PayloadError::io(
                "create GPU payload staging directory",
                directory.clone(),
                error,
            )
        })?;
        require_directory(&directory, "verify GPU payload staging directory")?;
    }
    Ok(())
}

pub fn stage_payload(
    payload: &ReadyGpuPayload,
    root: &Path,
    progress: &dyn Fn(PayloadProgress),
    cancel: &AtomicBool,
) -> Result<StagedGpuPayload, PayloadError> {
    stage_with(
        payload,
        root,
        &|source, target| fs::hard_link(source, target),
        progress,
        cancel,
    )
}

pub(crate) fn stage_with(
    payload: &ReadyGpuPayload,
    root: &Path,
    hard_link: &dyn Fn(&Path, &Path) -> io::Result<()>,
    progress: &dyn Fn(PayloadProgress),
    cancel: &AtomicBool,
) -> Result<StagedGpuPayload, PayloadError> {
    ensure_staging_root(root)?;
    check_cancelled(cancel)?;
    let digest = payload.generation().as_hex();
    let generations_root = root.join("generations");
    let ready_root = root.join("ready");
    let generation = generations_root.join(digest);
    let marker = ready_root.join(format!("{digest}.json"));
    let mut quarantines = Vec::new();
    let _digest_lock = DigestLock::acquire(&generations_root, digest, cancel)?;
    let expected = expected_files(payload, cancel)?;

    verify_or_quarantine_marker(payload, &marker, &ready_root, &mut quarantines, cancel)?;
    ensure_generation(
        payload,
        &expected,
        &generation,
        &generations_root,
        &marker,
        &ready_root,
        hard_link,
        progress,
        cancel,
        &mut quarantines,
    )?;
    ensure_ready_marker(payload, &marker, &ready_root, cancel, &mut quarantines)?;

    progress(PayloadProgress::Ready);
    Ok(StagedGpuPayload {
        payload_id: payload.payload_id().into(),
        generation: payload.generation().clone(),
        generation_directory: generation,
        ready_marker_path: marker,
    })
}

fn ensure_generation(
    payload: &ReadyGpuPayload,
    expected: &[ExpectedFile],
    generation: &Path,
    generations_root: &Path,
    marker: &Path,
    ready_root: &Path,
    hard_link: &dyn Fn(&Path, &Path) -> io::Result<()>,
    progress: &dyn Fn(PayloadProgress),
    cancel: &AtomicBool,
    quarantines: &mut Vec<OperationPath>,
) -> Result<(), PayloadError> {
    'prepare: loop {
        match verify_generation(expected, generation, cancel) {
            Ok(()) => return Ok(()),
            Err(PayloadError::Cancelled) => return Err(PayloadError::Cancelled),
            Err(_) => {
                deactivate_ready_marker(payload, marker, ready_root, quarantines, cancel)?;
                match verify_generation(expected, generation, cancel) {
                    Ok(()) => return Ok(()),
                    Err(PayloadError::Cancelled) => return Err(PayloadError::Cancelled),
                    Err(_) if path_exists(generation) => {
                        if let Some(quarantine) =
                            quarantine(generation, generations_root, payload.generation().as_hex())?
                        {
                            quarantines.push(quarantine);
                        }
                        continue;
                    }
                    Err(_) => {}
                }
            }
        }

        let mut temporary =
            create_temporary_generation(generations_root, payload.generation().as_hex())?;
        materialize_generation(
            payload,
            expected,
            temporary.path(),
            hard_link,
            progress,
            cancel,
        )?;
        verify_generation(expected, temporary.path(), cancel)?;

        loop {
            check_cancelled(cancel)?;
            match rename_noreplace(temporary.path(), generation) {
                Ok(()) => {
                    temporary.disarm();
                    match verify_generation(expected, generation, cancel) {
                        Ok(()) => return Ok(()),
                        Err(PayloadError::Cancelled) => return Err(PayloadError::Cancelled),
                        Err(_) if path_exists(generation) => {
                            deactivate_ready_marker(
                                payload,
                                marker,
                                ready_root,
                                quarantines,
                                cancel,
                            )?;
                            if let Some(quarantine) = quarantine(
                                generation,
                                generations_root,
                                payload.generation().as_hex(),
                            )? {
                                quarantines.push(quarantine);
                            }
                            continue 'prepare;
                        }
                        Err(error) => return Err(error),
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    match verify_generation(expected, generation, cancel) {
                        Ok(()) => return Ok(()),
                        Err(PayloadError::Cancelled) => return Err(PayloadError::Cancelled),
                        Err(_) if path_exists(generation) => {
                            deactivate_ready_marker(
                                payload,
                                marker,
                                ready_root,
                                quarantines,
                                cancel,
                            )?;
                            if let Some(quarantine) = quarantine(
                                generation,
                                generations_root,
                                payload.generation().as_hex(),
                            )? {
                                quarantines.push(quarantine);
                            }
                        }
                        Err(_) => {}
                    }
                }
                Err(error) => {
                    return Err(PayloadError::io(
                        "publish GPU payload generation",
                        generation.into(),
                        error,
                    ));
                }
            }
        }
    }
}

fn materialize_generation(
    payload: &ReadyGpuPayload,
    files: &[ExpectedFile],
    temporary: &Path,
    hard_link: &dyn Fn(&Path, &Path) -> io::Result<()>,
    progress: &dyn Fn(PayloadProgress),
    cancel: &AtomicBool,
) -> Result<(), PayloadError> {
    let total = files.len() as u64;
    for (index, expected) in files.iter().enumerate() {
        check_cancelled(cancel)?;
        let source = payload.files_directory().join(&expected.relative);
        require_regular_file(&source, "verify GPU payload staging source")?;
        let target = temporary.join(&expected.relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                PayloadError::io("create staging subdirectory", parent.into(), error)
            })?;
        }
        if hard_link(&source, &target).is_err() {
            fs::copy(&source, &target).map_err(|error| {
                PayloadError::io("copy GPU payload staging file", target.clone(), error)
            })?;
        }
        progress(PayloadProgress::Staging {
            files: index as u64 + 1,
            total,
        });
    }
    Ok(())
}

struct ExpectedFile {
    relative: PathBuf,
    size: u64,
    digest: Sha256Digest,
}

fn expected_files(
    payload: &ReadyGpuPayload,
    cancel: &AtomicBool,
) -> Result<Vec<ExpectedFile>, PayloadError> {
    let payload_path = payload.files_directory().join("payload.json");
    let payload_size = require_regular_file(&payload_path, "verify payload manifest source")?;
    verify_staged_file(
        &payload_path,
        Path::new("payload.json source"),
        payload_size,
        payload.payload_manifest_sha256(),
        cancel,
    )?;
    let mut files = Vec::with_capacity(payload.manifest().files().len() + 1);
    files.push(ExpectedFile {
        relative: PathBuf::from("payload.json"),
        size: payload_size,
        digest: payload.payload_manifest_sha256().clone(),
    });
    files.extend(payload.manifest().files().iter().map(|file| ExpectedFile {
        relative: PathBuf::from(file.path()),
        size: file.size(),
        digest: file.sha256().clone(),
    }));
    Ok(files)
}

fn verify_generation(
    expected: &[ExpectedFile],
    generation: &Path,
    cancel: &AtomicBool,
) -> Result<(), PayloadError> {
    require_directory(generation, "verify staged GPU payload generation")?;
    let expected_files = expected
        .iter()
        .map(|file| file.relative.clone())
        .collect::<HashSet<_>>();
    let mut expected_directories = HashSet::new();
    for file in expected {
        let mut parent = file.relative.parent();
        while let Some(relative) = parent {
            if relative.as_os_str().is_empty() {
                break;
            }
            expected_directories.insert(relative.to_owned());
            parent = relative.parent();
        }
    }
    verify_generation_tree(
        generation,
        Path::new(""),
        &expected_files,
        &expected_directories,
    )?;
    for file in expected {
        check_cancelled(cancel)?;
        verify_staged_file(
            &generation.join(&file.relative),
            &file.relative,
            file.size,
            &file.digest,
            cancel,
        )?;
    }
    Ok(())
}

fn verify_generation_tree(
    root: &Path,
    relative_directory: &Path,
    expected_files: &HashSet<PathBuf>,
    expected_directories: &HashSet<PathBuf>,
) -> Result<(), PayloadError> {
    let directory = root.join(relative_directory);
    let entries = fs::read_dir(&directory).map_err(|error| {
        PayloadError::io("inspect staged GPU payload generation", directory, error)
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            PayloadError::io("inspect staged GPU payload generation", root.into(), error)
        })?;
        let relative = relative_directory.join(entry.file_name());
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            PayloadError::io("inspect staged GPU payload entry", path.clone(), error)
        })?;
        if metadata.file_type().is_file() && !is_reparse_point(&metadata) {
            if !expected_files.contains(&relative) {
                return Err(PayloadError::InvalidManifest(format!(
                    "unexpected staged GPU payload file: {}",
                    relative.display()
                )));
            }
        } else if metadata.file_type().is_dir() && !is_reparse_point(&metadata) {
            if !expected_directories.contains(&relative) {
                return Err(PayloadError::InvalidManifest(format!(
                    "unexpected staged GPU payload directory: {}",
                    relative.display()
                )));
            }
            verify_generation_tree(root, &relative, expected_files, expected_directories)?;
        } else {
            return Err(PayloadError::InvalidManifest(format!(
                "staged GPU payload entry is not a regular file or directory: {}",
                relative.display()
            )));
        }
    }
    Ok(())
}

fn verify_staged_file(
    path: &Path,
    relative: &Path,
    expected_size: u64,
    expected_digest: &Sha256Digest,
    cancel: &AtomicBool,
) -> Result<(), PayloadError> {
    let actual_size = require_regular_file(path, "verify staged GPU payload file")?;
    if actual_size != expected_size {
        return Err(PayloadError::InvalidManifest(format!(
            "size mismatch for staged {}: expected {expected_size}, got {actual_size}",
            relative.display()
        )));
    }
    let mut file = File::open(path)
        .map_err(|error| PayloadError::io("open staged GPU payload file", path.into(), error))?;
    let actual = hash_reader(&mut file, path, cancel)?;
    if actual != *expected_digest {
        return Err(PayloadError::DigestMismatch {
            subject: format!("staged {}", relative.display()),
            expected: expected_digest.clone(),
            actual,
        });
    }
    Ok(())
}

fn hash_reader(
    reader: &mut impl Read,
    path: &Path,
    cancel: &AtomicBool,
) -> Result<Sha256Digest, PayloadError> {
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; HASH_BUFFER_SIZE];
    loop {
        check_cancelled(cancel)?;
        let count = reader.read(&mut buffer).map_err(|error| {
            PayloadError::io("hash staged GPU payload file", path.into(), error)
        })?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Sha256Digest::from_bytes(hash.finalize().into())
}

fn verify_or_quarantine_marker(
    payload: &ReadyGpuPayload,
    marker: &Path,
    ready_root: &Path,
    quarantines: &mut Vec<OperationPath>,
    cancel: &AtomicBool,
) -> Result<(), PayloadError> {
    loop {
        match inspect_ready_marker(payload, marker, cancel)? {
            MarkerState::Missing | MarkerState::Matching => return Ok(()),
            MarkerState::Corrupt => {
                if let Some(quarantine) =
                    quarantine(marker, ready_root, payload.generation().as_hex())?
                {
                    quarantines.push(quarantine);
                }
            }
        }
    }
}

fn deactivate_ready_marker(
    payload: &ReadyGpuPayload,
    marker: &Path,
    ready_root: &Path,
    quarantines: &mut Vec<OperationPath>,
    cancel: &AtomicBool,
) -> Result<(), PayloadError> {
    loop {
        match inspect_ready_marker(payload, marker, cancel)? {
            MarkerState::Missing => return Ok(()),
            MarkerState::Matching | MarkerState::Corrupt => {
                if let Some(quarantine) =
                    quarantine(marker, ready_root, payload.generation().as_hex())?
                {
                    quarantines.push(quarantine);
                }
            }
        }
    }
}

fn ensure_ready_marker(
    payload: &ReadyGpuPayload,
    marker: &Path,
    ready_root: &Path,
    cancel: &AtomicBool,
    quarantines: &mut Vec<OperationPath>,
) -> Result<(), PayloadError> {
    ensure_ready_marker_with(payload, marker, ready_root, cancel, quarantines, &|| {})
}

fn ensure_ready_marker_with(
    payload: &ReadyGpuPayload,
    marker: &Path,
    ready_root: &Path,
    cancel: &AtomicBool,
    quarantines: &mut Vec<OperationPath>,
    before_publish: &dyn Fn(),
) -> Result<(), PayloadError> {
    loop {
        match inspect_ready_marker(payload, marker, cancel)? {
            MarkerState::Matching => return Ok(()),
            MarkerState::Corrupt => {
                if let Some(quarantine) =
                    quarantine(marker, ready_root, payload.generation().as_hex())?
                {
                    quarantines.push(quarantine);
                }
                continue;
            }
            MarkerState::Missing => {}
        }

        let bytes = ReadyMarker::new_for(payload).to_json_bytes()?;
        let (mut partial, mut file) =
            create_partial_marker(ready_root, payload.generation().as_hex())?;
        file.write_all(&bytes).map_err(|error| {
            PayloadError::io("write ready marker", partial.path().into(), error)
        })?;
        file.sync_all().map_err(|error| {
            PayloadError::io("flush ready marker", partial.path().into(), error)
        })?;
        before_publish();
        if cancel.load(Ordering::Relaxed) {
            drop(file);
            return Err(PayloadError::Cancelled);
        }
        let publication = rename_noreplace(partial.path(), marker);
        drop(file);
        match publication {
            Ok(()) => partial.disarm(),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(PayloadError::io(
                    "publish ready marker",
                    marker.into(),
                    error,
                ));
            }
        }
    }
}

enum MarkerState {
    Missing,
    Matching,
    Corrupt,
}

#[derive(serde::Deserialize)]
struct ReadyMarkerDocument {
    schema_version: u32,
    payload_id: String,
    generation: Sha256Digest,
    payload_manifest_sha256: Sha256Digest,
}

fn inspect_ready_marker(
    payload: &ReadyGpuPayload,
    marker: &Path,
    cancel: &AtomicBool,
) -> Result<MarkerState, PayloadError> {
    check_cancelled(cancel)?;
    let metadata = match fs::symlink_metadata(marker) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(MarkerState::Missing),
        Err(error) => {
            return Err(PayloadError::io(
                "inspect ready marker",
                marker.into(),
                error,
            ));
        }
    };
    if !metadata.file_type().is_file()
        || is_reparse_point(&metadata)
        || metadata.len() > READY_MARKER_SIZE_LIMIT
    {
        return Ok(MarkerState::Corrupt);
    }
    let file = File::open(marker)
        .map_err(|error| PayloadError::io("open ready marker", marker.into(), error))?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(READY_MARKER_SIZE_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| PayloadError::io("read ready marker", marker.into(), error))?;
    if bytes.len() as u64 > READY_MARKER_SIZE_LIMIT {
        return Ok(MarkerState::Corrupt);
    }
    let parsed = match serde_json::from_slice::<ReadyMarkerDocument>(&bytes) {
        Ok(parsed) => parsed,
        Err(_) => return Ok(MarkerState::Corrupt),
    };
    if parsed.schema_version != 1 {
        return Ok(MarkerState::Corrupt);
    }
    if parsed.payload_id == payload.payload_id()
        && parsed.generation == *payload.generation()
        && parsed.payload_manifest_sha256 == *payload.payload_manifest_sha256()
    {
        Ok(MarkerState::Matching)
    } else {
        Err(PayloadError::ConflictingGeneration {
            path: marker.into(),
        })
    }
}

fn create_temporary_generation(
    generations_root: &Path,
    digest: &str,
) -> Result<OperationPath, PayloadError> {
    loop {
        let path = unique_operation_path(generations_root, ".tmp", digest);
        match fs::create_dir(&path) {
            Ok(()) => return Ok(OperationPath::new(path)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(PayloadError::io(
                    "create staging temporary directory",
                    path,
                    error,
                ));
            }
        }
    }
}

fn create_partial_marker(
    ready_root: &Path,
    digest: &str,
) -> Result<(OperationPath, File), PayloadError> {
    loop {
        let path = unique_operation_path(ready_root, "part", digest);
        match File::options().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((OperationPath::new(path), file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(PayloadError::io("create ready marker", path, error));
            }
        }
    }
}

fn quarantine(
    source: &Path,
    operation_root: &Path,
    digest: &str,
) -> Result<Option<OperationPath>, PayloadError> {
    loop {
        let destination = unique_operation_path(operation_root, ".corrupt", digest);
        match rename_noreplace(source, &destination) {
            Ok(()) => return Ok(Some(OperationPath::new(destination))),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(PayloadError::io(
                    "quarantine corrupt staged GPU payload path",
                    source.into(),
                    error,
                ));
            }
        }
    }
}

fn unique_operation_path(root: &Path, kind: &str, digest: &str) -> PathBuf {
    let sequence = NEXT_OPERATION.fetch_add(1, Ordering::Relaxed);
    if kind == "part" {
        root.join(format!(".{digest}.part-{}-{sequence}", std::process::id()))
    } else {
        root.join(format!("{kind}-{digest}-{}-{sequence}", std::process::id()))
    }
}

fn require_directory(path: &Path, operation: &'static str) -> Result<(), PayloadError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| PayloadError::io(operation, path.into(), error))?;
    if !metadata.file_type().is_dir() || is_reparse_point(&metadata) {
        return Err(PayloadError::InvalidManifest(format!(
            "staged GPU payload path is not a directory: {}",
            path.display()
        )));
    }
    Ok(())
}

fn require_regular_file(path: &Path, operation: &'static str) -> Result<u64, PayloadError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| PayloadError::io(operation, path.into(), error))?;
    if !metadata.file_type().is_file() || is_reparse_point(&metadata) {
        return Err(PayloadError::InvalidManifest(format!(
            "staged GPU payload path is not a regular file: {}",
            path.display()
        )));
    }
    Ok(metadata.len())
}

fn check_cancelled(cancel: &AtomicBool) -> Result<(), PayloadError> {
    if cancel.load(Ordering::Relaxed) {
        Err(PayloadError::Cancelled)
    } else {
        Ok(())
    }
}

fn path_exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

struct DigestLock {
    _file: File,
}

impl DigestLock {
    fn acquire(
        generations_root: &Path,
        digest: &str,
        cancel: &AtomicBool,
    ) -> Result<Self, PayloadError> {
        let path = generations_root.join(format!(".{digest}.lock"));
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| {
                PayloadError::io("open staging generation lock", path.clone(), error)
            })?;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(TryLockError::WouldBlock) => {
                    check_cancelled(cancel)?;
                    thread::sleep(Duration::from_millis(10));
                }
                Err(TryLockError::Error(error)) => {
                    return Err(PayloadError::io("lock staging generation", path, error));
                }
            }
        }
    }
}

fn rename_noreplace(source: &Path, destination: &Path) -> io::Result<()> {
    match platform::rename_noreplace(source, destination) {
        Err(error) if path_exists(destination) => {
            Err(io::Error::new(io::ErrorKind::AlreadyExists, error))
        }
        result => result,
    }
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
        if !self.armed {
            return;
        }
        if let Ok(metadata) = fs::symlink_metadata(&self.path) {
            if metadata.file_type().is_dir() && !is_reparse_point(&metadata) {
                let _ = fs::remove_dir_all(&self.path);
            } else if metadata.file_type().is_dir() {
                if fs::remove_dir(&self.path).is_err() {
                    let _ = fs::remove_file(&self.path);
                }
            } else {
                let _ = fs::remove_file(&self.path);
            }
        }
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

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::{self, Cursor, Write},
        path::{Path, PathBuf},
        sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            mpsc,
        },
        thread,
        time::Duration,
    };

    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    use crate::{PayloadCatalog, PayloadError, Sha256Digest};

    use super::{ensure_ready_marker_with, stage_payload, stage_with};

    const SOURCE_URL: &str = "https://github.com/example/source";
    const SOURCE_COMMIT: &str = "14794180686c2fb6307fbe359c359bec765249f3";
    static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

    struct TemporaryDirectory(PathBuf);

    impl TemporaryDirectory {
        fn new(label: &str) -> Self {
            loop {
                let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!(
                    "vmlord-gpu-payload-staging-{label}-{}-{sequence}",
                    std::process::id()
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("could not create test directory: {error}"),
                }
            }
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
        _temporary: TemporaryDirectory,
        ready: crate::ReadyGpuPayload,
        payload: Vec<u8>,
        content: Vec<u8>,
        license: Vec<u8>,
        sources: Vec<u8>,
        staging_root: PathBuf,
    }

    impl Fixture {
        fn new(label: &str) -> Self {
            let temporary = TemporaryDirectory::new(label);
            let content = b"original content".to_vec();
            let sources = serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "target": {
                    "distribution": "ubuntu",
                    "release": "26.04",
                    "architecture": "amd64",
                    "kernel_release": "test",
                    "payload_abi": 1
                },
                "mesa_policy": "bundled",
                "vmlord_revision": SOURCE_COMMIT,
                "builder_version": "vmlord-gpu-payload 1",
                "sources": [{
                    "url": SOURCE_URL,
                    "commit": SOURCE_COMMIT,
                    "version": "1",
                    "paths": ["content/file"],
                    "licenses": [{"path": "content/file", "spdx": "MIT"}],
                    "sha256": digest(&content)
                }],
                "overlays": []
            }))
            .unwrap();
            let license = b"MIT license text\n".to_vec();
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
                        "sha256": digest(&content)
                    },
                    {
                        "path": "licenses/MIT.txt",
                        "size": license.len(),
                        "sha256": digest(&license)
                    },
                    {
                        "path": "sources.json",
                        "size": sources.len(),
                        "sha256": digest(&sources)
                    }
                ]
            }))
            .unwrap();
            let archive = build_archive(&payload, &content, &license, &sources);
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
                    "expanded_size_limit": 1_000_000,
                    "file_count_limit": 3,
                    "archive_sha256": digest(&archive),
                    "payload_manifest_sha256": digest(&payload),
                    "required_renderers": ["d3d12-gallium"],
                    "mesa_policy": "bundled",
                    "vmlord_revision": SOURCE_COMMIT,
                    "builder_version": "vmlord-gpu-payload 1",
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
            fs::write(&archive_path, archive).unwrap();
            let cache_root = temporary.path().join("cache");
            let ready = crate::cache::prepare_verified_archive(
                &entry,
                &archive_path,
                &cache_root,
                &|_| {},
                &AtomicBool::new(false),
            )
            .unwrap();
            let staging_root = temporary.path().join("staging");
            Self {
                _temporary: temporary,
                ready,
                payload,
                content,
                license,
                sources,
                staging_root,
            }
        }

        fn stage_with_copy(&self) -> Result<super::StagedGpuPayload, PayloadError> {
            stage_with(
                &self.ready,
                &self.staging_root,
                &|_, _| Err(io::Error::from(io::ErrorKind::CrossesDevices)),
                &|_| {},
                &AtomicBool::new(false),
            )
        }

        fn generation_directory(&self) -> PathBuf {
            self.staging_root
                .join("generations")
                .join(self.ready.generation().as_hex())
        }

        fn ready_marker(&self) -> PathBuf {
            self.staging_root
                .join("ready")
                .join(format!("{}.json", self.ready.generation()))
        }

        fn assert_generation_matches(&self) {
            for (relative, expected) in [
                ("payload.json", self.payload.as_slice()),
                ("content/file", self.content.as_slice()),
                ("licenses/MIT.txt", self.license.as_slice()),
                ("sources.json", self.sources.as_slice()),
            ] {
                assert_eq!(
                    fs::read(self.generation_directory().join(relative)).unwrap(),
                    expected,
                    "staged {relative} did not match the verified payload"
                );
            }
        }
    }

    fn digest(bytes: &[u8]) -> String {
        Sha256Digest::hash_reader(bytes)
            .unwrap()
            .as_hex()
            .to_owned()
    }

    fn build_archive(payload: &[u8], content: &[u8], license: &[u8], sources: &[u8]) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .unix_permissions(0o644);
        for (name, bytes) in [
            ("payload.json", payload),
            ("content/file", content),
            ("licenses/MIT.txt", license),
            ("sources.json", sources),
        ] {
            writer.start_file(name, options).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn assert_matching_marker(fixture: &Fixture) {
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(fixture.ready_marker()).unwrap()).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["payload_id"], fixture.ready.payload_id());
        assert_eq!(value["generation"], fixture.ready.generation().as_hex());
        assert_eq!(value["payload_manifest_sha256"], digest(&fixture.payload));
    }

    #[test]
    fn a_generation_becomes_selectable_only_after_its_unique_ready_marker() {
        let fixture = Fixture::new("publish");

        let staged = stage_payload(
            &fixture.ready,
            &fixture.staging_root,
            &|_| {},
            &AtomicBool::new(false),
        )
        .unwrap();

        assert_eq!(staged.payload_id(), fixture.ready.payload_id());
        assert_eq!(staged.generation(), fixture.ready.generation());
        assert_eq!(
            staged.generation_directory(),
            fixture.generation_directory()
        );
        assert_eq!(staged.ready_marker_path(), fixture.ready_marker());
        fixture.assert_generation_matches();
        assert_matching_marker(&fixture);
        assert!(!fixture.staging_root.join("current.json").exists());

        stage_payload(
            &fixture.ready,
            &fixture.staging_root,
            &|_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
        fixture.assert_generation_matches();
    }

    #[test]
    fn a_failed_hard_link_falls_back_to_a_verified_copy() {
        let fixture = Fixture::new("copy-fallback");

        fixture.stage_with_copy().unwrap();

        fixture.assert_generation_matches();
        assert_matching_marker(&fixture);
    }

    #[test]
    fn corrupt_existing_generation_files_are_quarantined_and_repaired() {
        let fixture = Fixture::new("repair-generation");
        fixture.stage_with_copy().unwrap();

        for relative in ["payload.json", "content/file", "sources.json"] {
            fs::write(fixture.generation_directory().join(relative), b"tampered").unwrap();

            fixture.stage_with_copy().unwrap();

            fixture.assert_generation_matches();
        }
    }

    #[test]
    fn a_malformed_ready_marker_is_quarantined_and_repaired() {
        let fixture = Fixture::new("repair-marker");
        fixture.stage_with_copy().unwrap();
        fs::write(fixture.ready_marker(), b"not json").unwrap();

        fixture.stage_with_copy().unwrap();

        assert_matching_marker(&fixture);
    }

    #[test]
    fn an_unsupported_ready_marker_schema_is_quarantined_and_repaired() {
        let fixture = Fixture::new("repair-marker-schema");
        fixture.stage_with_copy().unwrap();
        let mut marker: serde_json::Value =
            serde_json::from_slice(&fs::read(fixture.ready_marker()).unwrap()).unwrap();
        marker["schema_version"] = serde_json::json!(2);
        fs::write(fixture.ready_marker(), serde_json::to_vec(&marker).unwrap()).unwrap();

        fixture.stage_with_copy().unwrap();

        assert_matching_marker(&fixture);
    }

    #[test]
    fn a_ready_marker_for_another_identity_is_a_conflicting_generation() {
        let fixture = Fixture::new("conflicting-marker");
        fixture.stage_with_copy().unwrap();
        let conflicting = serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "payload_id": "another-payload",
            "generation": fixture.ready.generation(),
            "payload_manifest_sha256": digest(&fixture.payload)
        }))
        .unwrap();
        fs::write(fixture.ready_marker(), conflicting).unwrap();

        let result = fixture.stage_with_copy();

        assert!(matches!(
            result,
            Err(PayloadError::ConflictingGeneration { path }) if path == fixture.ready_marker()
        ));
    }

    #[test]
    fn a_corrupt_temporary_tree_is_never_published() {
        let fixture = Fixture::new("verify-temporary");
        let result = stage_with(
            &fixture.ready,
            &fixture.staging_root,
            &|_, target| {
                fs::write(target, b"corrupt")?;
                Ok(())
            },
            &|_| {},
            &AtomicBool::new(false),
        );

        assert!(result.is_err());
        assert!(!fixture.generation_directory().exists());
        assert!(!fixture.ready_marker().exists());
    }

    #[test]
    fn a_concurrent_corrupt_generation_winner_is_reverified_and_repaired() {
        let fixture = Fixture::new("race-winner");
        let inserted_winner = AtomicBool::new(false);
        stage_with(
            &fixture.ready,
            &fixture.staging_root,
            &|source, target| {
                if !inserted_winner.swap(true, Ordering::Relaxed) {
                    fs::create_dir_all(fixture.generation_directory())?;
                    fs::write(
                        fixture.generation_directory().join("payload.json"),
                        b"corrupt race winner",
                    )?;
                }
                fs::hard_link(source, target)
            },
            &|_| {},
            &AtomicBool::new(false),
        )
        .unwrap();

        fixture.assert_generation_matches();
        assert_matching_marker(&fixture);
    }

    #[test]
    fn a_ready_marker_is_deactivated_before_its_corrupt_generation_is_repaired() {
        let fixture = Fixture::new("deactivate-marker");
        fixture.stage_with_copy().unwrap();
        fs::write(
            fixture.generation_directory().join("content/file"),
            b"tampered content",
        )
        .unwrap();
        let observed_repair = AtomicBool::new(false);

        stage_with(
            &fixture.ready,
            &fixture.staging_root,
            &|_, _| {
                assert!(!fixture.ready_marker().exists());
                observed_repair.store(true, Ordering::Relaxed);
                Err(io::Error::from(io::ErrorKind::CrossesDevices))
            },
            &|_| {},
            &AtomicBool::new(false),
        )
        .unwrap();

        assert!(observed_repair.load(Ordering::Relaxed));
        fixture.assert_generation_matches();
        assert_matching_marker(&fixture);
    }

    #[test]
    fn a_ready_marker_is_deactivated_before_its_missing_generation_is_rebuilt() {
        let fixture = Fixture::new("missing-generation");
        fixture.stage_with_copy().unwrap();
        fs::remove_dir_all(fixture.generation_directory()).unwrap();

        stage_with(
            &fixture.ready,
            &fixture.staging_root,
            &|_, _| {
                assert!(!fixture.ready_marker().exists());
                Err(io::Error::from(io::ErrorKind::CrossesDevices))
            },
            &|_| {},
            &AtomicBool::new(false),
        )
        .unwrap();

        fixture.assert_generation_matches();
        assert_matching_marker(&fixture);
    }

    #[test]
    fn same_digest_stage_calls_are_serialized_during_publication() {
        let fixture = Fixture::new("serialized-publication");
        let (first_entered_sender, first_entered_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let (second_entered_sender, second_entered_receiver) = mpsc::channel();
        let first_blocked = AtomicBool::new(false);

        thread::scope(|scope| {
            let first_ready = &fixture.ready;
            let first_root = &fixture.staging_root;
            let first_blocked = &first_blocked;
            let first = scope.spawn(move || {
                stage_with(
                    first_ready,
                    first_root,
                    &|_, _| {
                        if !first_blocked.swap(true, Ordering::Relaxed) {
                            first_entered_sender.send(()).unwrap();
                            release_receiver.recv().unwrap();
                        }
                        Err(io::Error::from(io::ErrorKind::CrossesDevices))
                    },
                    &|_| {},
                    &AtomicBool::new(false),
                )
            });
            first_entered_receiver.recv().unwrap();

            let second_ready = &fixture.ready;
            let second_root = &fixture.staging_root;
            let second = scope.spawn(move || {
                stage_with(
                    second_ready,
                    second_root,
                    &|_, _| {
                        second_entered_sender.send(()).unwrap();
                        Err(io::Error::from(io::ErrorKind::CrossesDevices))
                    },
                    &|_| {},
                    &AtomicBool::new(false),
                )
            });

            let serialized = second_entered_receiver
                .recv_timeout(Duration::from_millis(100))
                .is_err();
            release_sender.send(()).unwrap();
            first.join().unwrap().unwrap();
            second.join().unwrap().unwrap();
            assert!(
                serialized,
                "a second same-digest stage call entered materialization before the first published"
            );
        });

        fixture.assert_generation_matches();
        assert_matching_marker(&fixture);
    }

    #[test]
    fn a_changed_payload_manifest_source_cannot_remove_a_published_generation() {
        let fixture = Fixture::new("changed-manifest-source");
        fixture.stage_with_copy().unwrap();
        fs::write(
            fixture.ready.files_directory().join("payload.json"),
            b"tampered payload manifest source",
        )
        .unwrap();

        let result = fixture.stage_with_copy();

        assert!(matches!(result, Err(PayloadError::DigestMismatch { .. })));
        fixture.assert_generation_matches();
        assert_matching_marker(&fixture);
    }

    #[test]
    fn cancellation_at_publication_keeps_the_active_generation_without_a_marker() {
        let fixture = Fixture::new("cancel-publication");
        fixture.stage_with_copy().unwrap();
        fs::remove_file(fixture.ready_marker()).unwrap();
        let cancel = AtomicBool::new(false);
        let mut quarantines = Vec::new();
        let result = ensure_ready_marker_with(
            &fixture.ready,
            &fixture.ready_marker(),
            &fixture.staging_root.join("ready"),
            &cancel,
            &mut quarantines,
            &|| cancel.store(true, Ordering::Relaxed),
        );

        assert!(matches!(result, Err(PayloadError::Cancelled)));
        fixture.assert_generation_matches();
        assert!(!fixture.ready_marker().exists());
    }

    #[test]
    fn stale_pid_only_operation_names_do_not_collide_with_new_work() {
        let fixture = Fixture::new("unique-operations");
        let digest = fixture.ready.generation().as_hex();
        let old_temporary = fixture
            .staging_root
            .join("generations")
            .join(format!(".tmp-{digest}-{}", std::process::id()));
        let old_partial = fixture
            .staging_root
            .join("ready")
            .join(format!(".{digest}.part-{}", std::process::id()));
        fs::create_dir_all(&old_temporary).unwrap();
        fs::write(old_temporary.join("sentinel"), b"old temporary").unwrap();
        fs::create_dir_all(old_partial.parent().unwrap()).unwrap();
        fs::write(&old_partial, b"old partial").unwrap();

        fixture.stage_with_copy().unwrap();

        fixture.assert_generation_matches();
        assert_eq!(
            fs::read(old_temporary.join("sentinel")).unwrap(),
            b"old temporary"
        );
        assert_eq!(fs::read(old_partial).unwrap(), b"old partial");
    }
}
