use std::path::{Path, PathBuf};

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(windows)]
use vmlord_core::RepositoryError;
#[cfg(windows)]
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE},
        Globalization::{CSTR_EQUAL, CompareStringOrdinal},
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_READ_ATTRIBUTES,
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle,
            OPEN_EXISTING,
        },
    },
    core::PCWSTR,
};

#[cfg(windows)]
use crate::error::windows_error;

/// Native identity of the exact source file observed during discovery.
///
/// Deliberately neither `Debug` nor `Display`: it is platform-private evidence,
/// not a stable ID or user-facing value.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct SourceFileIdentity {
    volume_serial_number: u32,
    file_index: u64,
}

pub(super) fn paths_equal(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    {
        let left = left.as_os_str().encode_wide().collect::<Vec<_>>();
        let right = right.as_os_str().encode_wide().collect::<Vec<_>>();
        // SAFETY: both exact UTF-16 slices live for the call and carry their
        // own lengths. Ordinal comparison matches Windows path case rules
        // without converting through UTF-8.
        (unsafe { CompareStringOrdinal(&left, &right, true) }) == CSTR_EQUAL
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

impl SourceFileIdentity {
    pub(crate) const fn new(volume_serial_number: u32, file_index: u64) -> Self {
        Self {
            volume_serial_number,
            file_index,
        }
    }
}

/// Paths and source identity resolved by discovery and owned by the platform.
///
/// This type deliberately has no `Debug` implementation: it contains the
/// AppSandbox private-key path and must never cross into the application or UI
/// layers. A later import resolves an opaque source ID only through the latest
/// discovery snapshot and revalidates these observations before copying.
pub(crate) struct ValidatedSource {
    pub(crate) config_path: PathBuf,
    pub(crate) vm_ordinal: usize,
    pub(crate) source_disk: PathBuf,
    pub(crate) source_identity: SourceFileIdentity,
    pub(crate) private_key: PathBuf,
}

#[cfg(windows)]
pub(super) fn source_file_identity(path: &Path) -> Result<SourceFileIdentity, RepositoryError> {
    let wide = wide_path(path);
    // SAFETY: the exact UTF-16 path buffer is NUL-terminated and lives through
    // the call. The returned handle is immediately owned below.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .map_err(|error| windows_error("identify AppSandbox source disk", None, error))?;
    let handle = OwnedHandle(handle);
    identity_from_handle(handle.0)
}

#[cfg(not(windows))]
pub(super) fn source_file_identity(
    _path: &Path,
) -> Result<SourceFileIdentity, vmlord_core::RepositoryError> {
    Err(vmlord_core::RepositoryError::new(
        "native AppSandbox source identity requires Windows",
    ))
}

#[cfg(windows)]
pub(super) fn identity_from_handle(handle: HANDLE) -> Result<SourceFileIdentity, RepositoryError> {
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle is live and `information` is a correctly sized out
    // structure owned by this call.
    unsafe { GetFileInformationByHandle(handle, &raw mut information) }
        .map_err(|error| windows_error("read AppSandbox source disk identity", None, error))?;
    let file_index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
    Ok(SourceFileIdentity::new(
        information.dwVolumeSerialNumber,
        file_index,
    ))
}

#[cfg(windows)]
fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
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
