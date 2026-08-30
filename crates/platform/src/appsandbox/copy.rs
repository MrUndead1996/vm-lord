//! Revalidating and copying an AppSandbox disk into VMLord-owned staging.

use std::{
    any::Any,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

use vmlord_core::RepositoryError;

use super::{
    ValidatedSource,
    config::parse_vms_cfg,
    source::{SourceFileIdentity, paths_equal},
};

#[cfg(windows)]
use super::source::identity_from_handle;

#[cfg(windows)]
use std::{fs, os::windows::ffi::OsStrExt, panic::AssertUnwindSafe};
#[cfg(windows)]
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE},
        Storage::FileSystem::{
            COPY_FILE_FAIL_IF_EXISTS, COPYPROGRESSROUTINE_PROGRESS, CopyFileExW, CreateFileW,
            FILE_FLAG_SEQUENTIAL_SCAN, FILE_GENERIC_READ, FILE_SHARE_READ, GetDiskFreeSpaceExW,
            GetFileSizeEx, MOVEFILE_WRITE_THROUGH, MoveFileExW, OPEN_EXISTING, PROGRESS_CANCEL,
            PROGRESS_CONTINUE,
        },
    },
    core::PCWSTR,
};

#[cfg(windows)]
use crate::error::windows_error;

const ERROR_REQUEST_ABORTED_HRESULT: u32 = 0x8007_04D3;

/// The private paths resolved by discovery and the VMLord-owned staging path.
pub(super) struct CopyRequest<'a> {
    pub source: &'a ValidatedSource,
    pub target: &'a Path,
    pub cancel: &'a AtomicBool,
    pub publish: &'a dyn Fn(u64, u64),
}

/// Byte counts from a completed copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CopySummary {
    pub copied_bytes: u64,
    pub total_bytes: u64,
}

/// An opened source whose handle remains alive until copying finishes.
struct LockedSource {
    path: PathBuf,
    identity: SourceFileIdentity,
    bytes: u64,
    _lease: Box<dyn Any>,
}

impl LockedSource {
    fn new(
        path: PathBuf,
        identity: SourceFileIdentity,
        bytes: u64,
        lease: impl Any + 'static,
    ) -> Self {
        Self {
            path,
            identity,
            bytes,
            _lease: Box::new(lease),
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn bytes(&self) -> u64 {
        self.bytes
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProgressDecision {
    Continue,
    Cancel,
}

enum CopyFailure {
    Cancelled,
    Other(RepositoryError),
}

/// The filesystem boundary kept replaceable so the policy runs portably.
trait CopyFileSystem {
    fn read_config(&self, path: &Path) -> Result<String, RepositoryError>;
    fn canonicalize(&self, path: &Path) -> Result<PathBuf, RepositoryError>;
    fn lock_source(&self, path: &Path) -> Result<LockedSource, RepositoryError>;
    fn available_bytes(&self, directory: &Path) -> Result<u64, RepositoryError>;
    fn target_exists(&self, path: &Path) -> bool;
    fn copy_file(
        &self,
        source: &LockedSource,
        target: &Path,
        progress: &mut dyn FnMut(u64, u64) -> ProgressDecision,
    ) -> Result<(), CopyFailure>;
    fn promote_file(&self, staged: &Path, target: &Path) -> Result<(), RepositoryError>;
    fn remove_file(&self, path: &Path) -> Result<(), RepositoryError>;
}

/// Revalidates and copies one source disk without ever mutating its files.
#[cfg(windows)]
pub(super) fn copy_vhdx(request: CopyRequest<'_>) -> Result<CopySummary, RepositoryError> {
    copy_vhdx_with(request, &WindowsFileSystem)
}

fn copy_vhdx_with(
    request: CopyRequest<'_>,
    files: &dyn CopyFileSystem,
) -> Result<CopySummary, RepositoryError> {
    check_cancelled(request.cancel)?;
    let source = revalidate_and_lock(request.source, files)?;
    check_cancelled(request.cancel)?;

    let target = canonical_staging_target(request.target, files)?;
    if files.target_exists(&target) {
        return Err(RepositoryError::new(
            "the AppSandbox import staging disk already exists",
        ));
    }
    let target_directory = target.parent().ok_or_else(|| {
        RepositoryError::new("the AppSandbox import staging disk has no parent directory")
    })?;
    let available = files.available_bytes(target_directory)?;
    if available < source.bytes {
        return Err(RepositoryError::new(format!(
            "the destination has {available} bytes of free space but the AppSandbox disk needs {} bytes",
            source.bytes
        )));
    }
    check_cancelled(request.cancel)?;
    let staged = unique_staging_artifact(&target)?;

    let mut copied_bytes = 0;
    let mut total_bytes = source.bytes;
    let mut progress = |copied, total| {
        if request.cancel.load(Ordering::Relaxed) {
            return ProgressDecision::Cancel;
        }
        copied_bytes = copied;
        total_bytes = total;
        (request.publish)(copied, total);
        if request.cancel.load(Ordering::Relaxed) {
            ProgressDecision::Cancel
        } else {
            ProgressDecision::Continue
        }
    };

    match files.copy_file(&source, &staged, &mut progress) {
        Ok(()) => match files.promote_file(&staged, &target) {
            Ok(()) => Ok(CopySummary {
                copied_bytes,
                total_bytes,
            }),
            Err(error) => Err(remove_partial_target(files, &staged, error)),
        },
        Err(failure) => {
            let error = match failure {
                CopyFailure::Cancelled => cancelled_error(),
                CopyFailure::Other(error) => error,
            };
            Err(remove_partial_target(files, &staged, error))
        }
    }
}

fn unique_staging_artifact(target: &Path) -> Result<PathBuf, RepositoryError> {
    let name = target.file_name().ok_or_else(|| {
        RepositoryError::new("the AppSandbox import staging disk has no file name")
    })?;
    let parent = target.parent().ok_or_else(|| {
        RepositoryError::new("the AppSandbox import staging disk has no parent directory")
    })?;
    let mut artifact = OsString::from(".");
    artifact.push(name);
    artifact.push(".");
    artifact.push(uuid::Uuid::new_v4().as_hyphenated().to_string());
    artifact.push(".copying");
    Ok(parent.join(artifact))
}

fn revalidate_and_lock(
    source: &ValidatedSource,
    files: &dyn CopyFileSystem,
) -> Result<LockedSource, RepositoryError> {
    let config = files.read_config(&source.config_path)?;
    let parsed = parse_vms_cfg(&config)?;
    let configured = parsed
        .into_iter()
        .find(|vm| vm.ordinal() == source.vm_ordinal)
        .ok_or_else(|| {
            RepositoryError::new("the selected AppSandbox VM is no longer present in vms.cfg")
        })?;
    let resolved = files.canonicalize(configured.vhdx_path())?;
    if !paths_equal(&resolved, &source.source_disk) {
        return Err(RepositoryError::new(
            "the selected AppSandbox source identity changed after discovery",
        ));
    }

    let locked = files.lock_source(&resolved)?;
    if locked.identity != source.source_identity {
        return Err(RepositoryError::new(
            "the selected AppSandbox source identity changed while it was opened",
        ));
    }
    Ok(locked)
}

fn canonical_staging_target(
    target: &Path,
    files: &dyn CopyFileSystem,
) -> Result<PathBuf, RepositoryError> {
    let name = target.file_name().ok_or_else(|| {
        RepositoryError::new("the AppSandbox import staging disk has no file name")
    })?;
    let parent = target.parent().ok_or_else(|| {
        RepositoryError::new("the AppSandbox import staging disk has no parent directory")
    })?;
    Ok(files.canonicalize(parent)?.join(name))
}

fn remove_partial_target(
    files: &dyn CopyFileSystem,
    target: &Path,
    error: RepositoryError,
) -> RepositoryError {
    if !files.target_exists(target) {
        return error;
    }
    match files.remove_file(target) {
        Ok(()) => error,
        Err(cleanup) => RepositoryError::new(format!(
            "{error}; the incomplete AppSandbox staging disk could not be removed: {cleanup}"
        )),
    }
}

fn check_cancelled(cancel: &AtomicBool) -> Result<(), RepositoryError> {
    if cancel.load(Ordering::Relaxed) {
        Err(cancelled_error())
    } else {
        Ok(())
    }
}

fn cancelled_error() -> RepositoryError {
    RepositoryError::windows(
        "copy AppSandbox source disk",
        None,
        ERROR_REQUEST_ABORTED_HRESULT,
        "copying the AppSandbox disk was cancelled",
    )
}

#[cfg(windows)]
struct WindowsFileSystem;

#[cfg(windows)]
impl CopyFileSystem for WindowsFileSystem {
    fn read_config(&self, path: &Path) -> Result<String, RepositoryError> {
        fs::read_to_string(path).map_err(|error| {
            RepositoryError::new(format!(
                "failed to revalidate the AppSandbox VM configuration: {error}"
            ))
        })
    }

    fn canonicalize(&self, path: &Path) -> Result<PathBuf, RepositoryError> {
        fs::canonicalize(path).map_err(|error| {
            RepositoryError::new(format!(
                "failed to resolve an AppSandbox import path: {error}"
            ))
        })
    }

    fn lock_source(&self, path: &Path) -> Result<LockedSource, RepositoryError> {
        let wide = wide_path(path);
        // SAFETY: `wide` outlives the call. The returned handle is immediately
        // owned and stays alive through CopyFileExW. Sharing read but neither
        // write nor delete rejects a running VM and prevents a new writer or
        // path replacement from appearing during the copy.
        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                FILE_GENERIC_READ.0,
                FILE_SHARE_READ,
                None,
                OPEN_EXISTING,
                FILE_FLAG_SEQUENTIAL_SCAN,
                None,
            )
        }
        .map_err(|error| windows_error("lock AppSandbox source disk", None, error))?;
        let handle = OwnedHandle(handle);

        let mut size = 0_i64;
        // SAFETY: `handle` is live and `size` is a correctly sized out value.
        unsafe { GetFileSizeEx(handle.0, &raw mut size) }
            .map_err(|error| windows_error("read AppSandbox source disk size", None, error))?;
        let bytes = u64::try_from(size)
            .map_err(|_| RepositoryError::new("the AppSandbox source disk has a negative size"))?;
        let identity = identity_from_handle(handle.0)?;

        Ok(LockedSource::new(
            path.to_path_buf(),
            identity,
            bytes,
            handle,
        ))
    }

    fn available_bytes(&self, directory: &Path) -> Result<u64, RepositoryError> {
        let wide = wide_path(directory);
        let mut available = 0_u64;
        // SAFETY: `wide` and the out value live for the call; the unused totals
        // are explicitly omitted.
        unsafe { GetDiskFreeSpaceExW(PCWSTR(wide.as_ptr()), Some(&raw mut available), None, None) }
            .map_err(|error| windows_error("read AppSandbox import free space", None, error))?;
        Ok(available)
    }

    fn target_exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn copy_file(
        &self,
        source: &LockedSource,
        target: &Path,
        progress: &mut dyn FnMut(u64, u64) -> ProgressDecision,
    ) -> Result<(), CopyFailure> {
        let source_wide = wide_path(&source.path);
        let target_wide = wide_path(target);
        let mut context = Box::new(ProgressContext {
            progress,
            cancelled: false,
            panicked: false,
        });
        let context_pointer = (&raw mut *context).cast::<std::ffi::c_void>().cast_const();

        // SAFETY: both strings and the boxed callback context outlive this
        // synchronous call. CopyFileExW does not retain `lpdata`; the callback
        // casts it back to the exact `ProgressContext` allocated above.
        let result = unsafe {
            CopyFileExW(
                PCWSTR(source_wide.as_ptr()),
                PCWSTR(target_wide.as_ptr()),
                Some(copy_progress),
                Some(context_pointer),
                None,
                COPY_FILE_FAIL_IF_EXISTS,
            )
        };

        if context.panicked {
            return Err(CopyFailure::Other(RepositoryError::new(
                "the AppSandbox copy progress publisher panicked",
            )));
        }
        if context.cancelled
            || result
                .as_ref()
                .is_err_and(|error| error.code().0 as u32 == ERROR_REQUEST_ABORTED_HRESULT)
        {
            return Err(CopyFailure::Cancelled);
        }
        result.map_err(|error| {
            CopyFailure::Other(windows_error("copy AppSandbox source disk", None, error))
        })
    }

    fn promote_file(&self, staged: &Path, target: &Path) -> Result<(), RepositoryError> {
        let staged = wide_path(staged);
        let target = wide_path(target);
        // SAFETY: both exact UTF-16 buffers are NUL-terminated and live for
        // the call. Omitting `MOVEFILE_REPLACE_EXISTING` makes promotion fail
        // if another operation created the final target during the copy.
        unsafe {
            MoveFileExW(
                PCWSTR(staged.as_ptr()),
                PCWSTR(target.as_ptr()),
                MOVEFILE_WRITE_THROUGH,
            )
        }
        .map_err(|error| windows_error("promote AppSandbox staging disk", None, error))
    }

    fn remove_file(&self, path: &Path) -> Result<(), RepositoryError> {
        fs::remove_file(path).map_err(|error| {
            RepositoryError::new(format!(
                "failed to remove the incomplete AppSandbox staging disk: {error}"
            ))
        })
    }
}

#[cfg(windows)]
fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(windows)]
struct ProgressContext<'a> {
    progress: &'a mut dyn FnMut(u64, u64) -> ProgressDecision,
    cancelled: bool,
    panicked: bool,
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn copy_progress(
    total_file_size: i64,
    total_bytes_transferred: i64,
    _stream_size: i64,
    _stream_bytes_transferred: i64,
    _stream_number: u32,
    _callback_reason: windows::Win32::Storage::FileSystem::LPPROGRESS_ROUTINE_CALLBACK_REASON,
    _source: HANDLE,
    _destination: HANDLE,
    data: *const std::ffi::c_void,
) -> COPYPROGRESSROUTINE_PROGRESS {
    // SAFETY: `data` points to the boxed context owned by `copy_file`; the API
    // invokes this callback only during the synchronous CopyFileExW call.
    let context = unsafe { &mut *data.cast_mut().cast::<ProgressContext<'_>>() };
    let copied = u64::try_from(total_bytes_transferred).unwrap_or_default();
    let total = u64::try_from(total_file_size).unwrap_or_default();
    let decision = std::panic::catch_unwind(AssertUnwindSafe(|| (context.progress)(copied, total)));
    match decision {
        Ok(ProgressDecision::Continue) => PROGRESS_CONTINUE,
        Ok(ProgressDecision::Cancel) => {
            context.cancelled = true;
            PROGRESS_CANCEL
        }
        Err(_) => {
            context.panicked = true;
            PROGRESS_CANCEL
        }
    }
}

#[cfg(windows)]
struct OwnedHandle(HANDLE);

#[cfg(windows)]
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: this value uniquely owns the handle returned by CreateFileW.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs::{self, File},
        io::{Read, Seek, SeekFrom, Write},
        path::{Path, PathBuf},
        sync::{
            Mutex,
            atomic::{AtomicBool, Ordering},
        },
    };

    use uuid::Uuid;
    use vmlord_core::{AppSandboxImportProgress, AppSandboxImportStage, RepositoryError};

    use super::{
        CopyFailure, CopyFileSystem, CopyRequest, LockedSource, ProgressDecision, copy_vhdx_with,
    };
    use crate::appsandbox::{ValidatedSource, source::SourceFileIdentity};

    const SOURCE_BYTES: usize = 256 * 1024;
    const CHUNK_BYTES: usize = 32 * 1024;

    struct Fixture {
        root: PathBuf,
        source: ValidatedSource,
        target: PathBuf,
        neighbour: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("vmlord-appsandbox-copy-{}", Uuid::new_v4()));
            fs::create_dir_all(root.join("source")).unwrap();
            fs::create_dir_all(root.join("staging")).unwrap();
            fs::create_dir_all(root.join("ssh")).unwrap();

            let source_disk = root.join("source").join("disk.vhdx");
            let mut disk = File::create(&source_disk).unwrap();
            disk.set_len(SOURCE_BYTES as u64).unwrap();
            disk.write_all(b"VHDX-start").unwrap();
            disk.seek(SeekFrom::End(-8)).unwrap();
            disk.write_all(b"VHDX-end").unwrap();
            drop(disk);

            let config_path = root.join("vms.cfg");
            fs::write(&config_path, config(&source_disk)).unwrap();
            let private_key = root.join("ssh").join("id_appsandbox");
            fs::write(&private_key, b"private fixture").unwrap();
            let target = root.join("staging").join("disk.vhdx");
            let neighbour = root.join("staging").join("journal.json");
            fs::write(&neighbour, b"keep me").unwrap();

            Self {
                root,
                source: ValidatedSource {
                    config_path,
                    vm_ordinal: 1,
                    source_disk: fs::canonicalize(source_disk).unwrap(),
                    source_identity: SourceFileIdentity::new(7, 11),
                    private_key,
                },
                target,
                neighbour,
            }
        }

        fn request<'a>(
            &'a self,
            cancel: &'a AtomicBool,
            publish: &'a dyn Fn(u64, u64),
        ) -> CopyRequest<'a> {
            CopyRequest {
                source: &self.source,
                target: &self.target,
                cancel,
                publish,
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn config(source_disk: &Path) -> String {
        format!(
            "[VM]\nName=ubuntu\nOsType=Linux\nRamMB=4096\nCpuCores=4\nHddGB=64\n\
             NetworkMode=1\nGpuMode=1\nAdminUser=ubuntu\nSshEnabled=1\nSshPort=22\n\
             SshDeployKey=1\nInstallComplete=1\nVhdxPath={}\n",
            source_disk.display()
        )
    }

    struct TestFileSystem {
        free_bytes: u64,
        opened_identity: Option<SourceFileIdentity>,
        lock_error: Option<&'static str>,
        promotion_race: bool,
        copy_attempted: AtomicBool,
        removed: Mutex<Vec<PathBuf>>,
    }

    impl TestFileSystem {
        fn with_free_bytes(free_bytes: u64) -> Self {
            Self {
                free_bytes,
                opened_identity: None,
                lock_error: None,
                promotion_race: false,
                copy_attempted: AtomicBool::new(false),
                removed: Mutex::new(Vec::new()),
            }
        }
    }

    impl CopyFileSystem for TestFileSystem {
        fn read_config(&self, path: &Path) -> Result<String, RepositoryError> {
            fs::read_to_string(path).map_err(|error| RepositoryError::new(error.to_string()))
        }

        fn canonicalize(&self, path: &Path) -> Result<PathBuf, RepositoryError> {
            fs::canonicalize(path).map_err(|error| RepositoryError::new(error.to_string()))
        }

        fn lock_source(&self, path: &Path) -> Result<LockedSource, RepositoryError> {
            if let Some(error) = self.lock_error {
                return Err(RepositoryError::new(error));
            }
            let file = File::open(path).map_err(|error| RepositoryError::new(error.to_string()))?;
            let bytes = file
                .metadata()
                .map_err(|error| RepositoryError::new(error.to_string()))?
                .len();
            Ok(LockedSource::new(
                path.to_path_buf(),
                self.opened_identity
                    .unwrap_or_else(|| SourceFileIdentity::new(7, 11)),
                bytes,
                file,
            ))
        }

        fn available_bytes(&self, _directory: &Path) -> Result<u64, RepositoryError> {
            Ok(self.free_bytes)
        }

        fn target_exists(&self, path: &Path) -> bool {
            path.exists()
        }

        fn copy_file(
            &self,
            source: &LockedSource,
            target: &Path,
            progress: &mut dyn FnMut(u64, u64) -> ProgressDecision,
        ) -> Result<(), CopyFailure> {
            self.copy_attempted.store(true, Ordering::Relaxed);
            let mut input = File::open(source.path())
                .map_err(|error| CopyFailure::Other(RepositoryError::new(error.to_string())))?;
            let mut output = File::create_new(target)
                .map_err(|error| CopyFailure::Other(RepositoryError::new(error.to_string())))?;
            let total = source.bytes();
            if progress(0, total) == ProgressDecision::Cancel {
                return Err(CopyFailure::Cancelled);
            }

            let mut copied = 0_u64;
            let mut buffer = [0_u8; CHUNK_BYTES];
            loop {
                let used = input
                    .read(&mut buffer)
                    .map_err(|error| CopyFailure::Other(RepositoryError::new(error.to_string())))?;
                if used == 0 {
                    return Ok(());
                }
                output
                    .write_all(&buffer[..used])
                    .map_err(|error| CopyFailure::Other(RepositoryError::new(error.to_string())))?;
                copied += used as u64;
                if progress(copied, total) == ProgressDecision::Cancel {
                    return Err(CopyFailure::Cancelled);
                }
            }
        }

        fn remove_file(&self, path: &Path) -> Result<(), RepositoryError> {
            self.removed.lock().unwrap().push(path.to_path_buf());
            fs::remove_file(path).map_err(|error| RepositoryError::new(error.to_string()))
        }

        fn promote_file(&self, staged: &Path, target: &Path) -> Result<(), RepositoryError> {
            if self.promotion_race {
                fs::write(target, b"another operation's disk").unwrap();
                return Err(RepositoryError::new("the staging destination now exists"));
            }
            fs::rename(staged, target).map_err(|error| RepositoryError::new(error.to_string()))
        }
    }

    #[test]
    fn insufficient_free_bytes_refuses_before_copying() {
        let fixture = Fixture::new();
        let files = TestFileSystem::with_free_bytes((SOURCE_BYTES - 1) as u64);

        let error = copy_vhdx_with(fixture.request(&AtomicBool::new(false), &|_, _| {}), &files)
            .expect_err("a disk larger than the available space must be refused");

        assert!(error.to_string().contains("free space"), "got {error}");
        assert!(!files.copy_attempted.load(Ordering::Relaxed));
        assert!(!fixture.target.exists());
    }

    #[test]
    fn cancellation_before_copy_does_not_create_a_target() {
        let fixture = Fixture::new();
        let files = TestFileSystem::with_free_bytes(u64::MAX);
        let cancel = AtomicBool::new(true);

        let error = copy_vhdx_with(fixture.request(&cancel, &|_, _| {}), &files)
            .expect_err("a cancelled copy must stop");

        assert!(error.to_string().contains("cancelled"), "got {error}");
        assert!(!files.copy_attempted.load(Ordering::Relaxed));
        assert!(!fixture.target.exists());
    }

    #[test]
    fn cancellation_from_the_progress_publisher_removes_the_partial_target() {
        let fixture = Fixture::new();
        let files = TestFileSystem::with_free_bytes(u64::MAX);
        let cancel = AtomicBool::new(false);
        let publish = |copied, _| {
            if copied > 0 {
                cancel.store(true, Ordering::Relaxed);
            }
        };

        let error = copy_vhdx_with(fixture.request(&cancel, &publish), &files)
            .expect_err("publisher cancellation must abort the native copy");

        assert!(error.to_string().contains("cancelled"), "got {error}");
        assert!(fixture.source.source_disk.exists());
        assert!(!fixture.target.exists());
        assert_eq!(fs::read(&fixture.neighbour).unwrap(), b"keep me");
    }

    #[test]
    fn a_target_created_during_copy_is_preserved_when_promotion_loses_the_race() {
        let fixture = Fixture::new();
        let mut files = TestFileSystem::with_free_bytes(u64::MAX);
        files.promotion_race = true;

        let error = copy_vhdx_with(fixture.request(&AtomicBool::new(false), &|_, _| {}), &files)
            .expect_err("promotion must not replace a destination that appeared during copy");

        assert!(error.to_string().contains("now exists"), "got {error}");
        assert_eq!(
            fs::read(&fixture.target).unwrap(),
            b"another operation's disk"
        );
        assert!(fixture.source.source_disk.exists());
        let staging_files = fs::read_dir(fixture.target.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(staging_files.len(), 2, "owned partial artifact leaked");
    }

    #[test]
    fn a_preexisting_target_is_never_removed_or_replaced() {
        let fixture = Fixture::new();
        fs::write(&fixture.target, b"preexisting disk").unwrap();
        let files = TestFileSystem::with_free_bytes(u64::MAX);

        copy_vhdx_with(fixture.request(&AtomicBool::new(false), &|_, _| {}), &files)
            .expect_err("an existing destination must be refused");

        assert_eq!(fs::read(&fixture.target).unwrap(), b"preexisting disk");
        assert!(!files.copy_attempted.load(Ordering::Relaxed));
    }

    #[test]
    fn source_identity_changing_between_validation_and_open_is_rejected() {
        let fixture = Fixture::new();
        let mut files = TestFileSystem::with_free_bytes(u64::MAX);
        files.opened_identity = Some(SourceFileIdentity::new(7, 12));

        let error = copy_vhdx_with(fixture.request(&AtomicBool::new(false), &|_, _| {}), &files)
            .expect_err("an opened file with a different identity must be rejected");

        assert!(
            error.to_string().contains("identity changed"),
            "got {error}"
        );
        assert!(!files.copy_attempted.load(Ordering::Relaxed));
    }

    #[test]
    fn a_locked_source_is_rejected_without_touching_it() {
        let fixture = Fixture::new();
        let mut files = TestFileSystem::with_free_bytes(u64::MAX);
        files.lock_error = Some("the AppSandbox source disk is still in use");

        let error = copy_vhdx_with(fixture.request(&AtomicBool::new(false), &|_, _| {}), &files)
            .expect_err("a source held by a running VM must be rejected");

        assert!(error.to_string().contains("still in use"), "got {error}");
        assert!(fixture.source.source_disk.exists());
        assert!(!fixture.target.exists());
    }

    #[test]
    fn a_sparse_source_is_copied_byte_for_byte() {
        let fixture = Fixture::new();
        let files = TestFileSystem::with_free_bytes(u64::MAX);
        let progress = Mutex::new(Vec::new());

        let summary = copy_vhdx_with(
            fixture.request(&AtomicBool::new(false), &|copied, total| {
                progress.lock().unwrap().push(AppSandboxImportProgress {
                    stage: AppSandboxImportStage::Copying,
                    copied_bytes: copied,
                    total_bytes: Some(total),
                });
            }),
            &files,
        )
        .expect("a valid source should copy");

        assert_eq!(
            fs::read(&fixture.target).unwrap(),
            fs::read(&fixture.source.source_disk).unwrap()
        );
        assert_eq!(summary.copied_bytes, SOURCE_BYTES as u64);
        assert_eq!(summary.total_bytes, SOURCE_BYTES as u64);
        let progress = progress.lock().unwrap();
        assert!(!progress.is_empty());
        assert_eq!(progress[0].copied_bytes, 0);
        assert_eq!(progress.last().unwrap().copied_bytes, SOURCE_BYTES as u64);
        for report in progress.iter() {
            assert_eq!(report.stage, AppSandboxImportStage::Copying);
            assert_eq!(report.total_bytes, Some(SOURCE_BYTES as u64));
            assert!(report.copied_bytes <= SOURCE_BYTES as u64);
        }
        for pair in progress.windows(2) {
            assert!(pair[0].copied_bytes <= pair[1].copied_bytes);
        }
    }
}
