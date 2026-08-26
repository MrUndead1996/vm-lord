//! The Win32 edge of a file transfer: the calls that open, create and
//! enumerate, and nothing else.
//!
//! Every handle is turned into an owning [`std::fs::File`] before it is used,
//! so the rest of the viewer reads and writes trees in safe Rust; what stays
//! here is the flag that opens a reparse point as itself, the disposition that
//! refuses an existing destination, and the query that says what a handle is.

use std::{
    ffi::OsString,
    fs::File,
    io,
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        io::FromRawHandle,
    },
    path::{Path, PathBuf},
};

use vmlord_display_protocol::clipboard::files::{EntryKind, MAX_ENTRIES};
use windows::{
    Win32::{
        Foundation::{
            ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS, GENERIC_READ, GENERIC_WRITE, HANDLE,
        },
        Storage::FileSystem::{
            CREATE_NEW, CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_DIRECTORY,
            FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO,
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
            FileAttributeTagInfo, GetFileInformationByHandleEx, OPEN_EXISTING,
        },
        UI::Shell::{DragQueryFileW, HDROP},
    },
    core::PCWSTR,
};

use crate::clipboard::files::FileError;

/// The paths a `CF_HDROP` names.
///
/// # Errors
///
/// [`FileError::NoName`] if it names nothing and [`FileError::TooMany`] if it
/// names more than one tree may hold.
pub fn hdrop_paths(drop: HDROP) -> Result<Vec<PathBuf>, FileError> {
    // SAFETY: `drop` is a clipboard handle the caller holds for the call, and
    // `0xFFFF_FFFF` is how many names it has rather than one of them.
    let count = unsafe { DragQueryFileW(drop, 0xFFFF_FFFF, None) };
    if count == 0 {
        return Err(FileError::NoName);
    }
    if count as usize > MAX_ENTRIES {
        return Err(FileError::TooMany);
    }

    let mut paths = Vec::with_capacity(count as usize);
    for index in 0..count {
        // SAFETY: as above; with no buffer this answers with the length.
        let units = unsafe { DragQueryFileW(drop, index, None) };
        let mut name = vec![0u16; units as usize + 1];
        // SAFETY: the buffer is one unit longer than the name, which is what
        // this writes into it.
        let written = unsafe { DragQueryFileW(drop, index, Some(name.as_mut_slice())) };
        if written == 0 {
            return Err(FileError::NoName);
        }

        paths.push(PathBuf::from(OsString::from_wide(
            &name[..written as usize],
        )));
    }

    Ok(paths)
}

/// One opened filesystem object, and what the handle says it is.
pub struct Opened {
    /// The object itself, owned.
    pub file: File,
    /// What the handle says it is.
    pub kind: EntryKind,
    /// How long a regular file is.
    pub size: u64,
}

/// Opens a path as itself, and refuses anything that stands for something else.
pub fn open_no_reparse(path: &Path) -> Result<Opened, FileError> {
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY: `wide` is a NUL-terminated wide path living across the call, and
    // the handle it answers with is owned below.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            GENERIC_READ.0,
            FILE_SHARE_READ,
            None,
            OPEN_EXISTING,
            // The first opens a directory; the second opens a reparse point as
            // itself rather than following it.
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .map_err(windows_error)?;

    let attributes = attributes_of(handle)?;
    // SAFETY: the handle came from the `CreateFileW` above and is owned from
    // here on by the file.
    let file = unsafe { File::from_raw_handle(handle.0.cast()) };

    if attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return Err(FileError::Unsupported);
    }

    if attributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
        return Ok(Opened {
            file,
            kind: EntryKind::Directory,
            size: 0,
        });
    }

    let size = file.metadata()?.len();

    Ok(Opened {
        file,
        kind: EntryKind::File,
        size,
    })
}

/// What an opened handle says about itself.
fn attributes_of(handle: HANDLE) -> Result<u32, FileError> {
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();

    // SAFETY: the handle is open, and the buffer is the structure this class
    // of information is defined to write.
    unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileAttributeTagInfo,
            (&raw mut info).cast(),
            u32::try_from(size_of::<FILE_ATTRIBUTE_TAG_INFO>()).unwrap_or(0),
        )
    }
    .map_err(windows_error)?;

    Ok(info.FileAttributes)
}

/// Creates a directory, and never over something that is already there.
pub fn make_directory(path: &Path) -> Result<(), FileError> {
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY: `wide` is a NUL-terminated wide path living across the call.
    unsafe { CreateDirectoryW(PCWSTR(wide.as_ptr()), None) }.map_err(windows_error)
}

/// Creates a file, and never over something that is already there.
pub fn create_new(path: &Path) -> Result<File, FileError> {
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY: as above; `CREATE_NEW` is what refuses an existing destination,
    // including a link someone put there.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            GENERIC_WRITE.0,
            FILE_SHARE_READ,
            None,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .map_err(windows_error)?;

    // SAFETY: the handle came from the `CreateFileW` above and is owned from
    // here on by the file.
    Ok(unsafe { File::from_raw_handle(handle.0.cast()) })
}

/// What a Win32 failure is here.
fn windows_error(error: windows::core::Error) -> FileError {
    let code = error.code();
    if code == ERROR_FILE_EXISTS.to_hresult() || code == ERROR_ALREADY_EXISTS.to_hresult() {
        return FileError::Exists;
    }

    FileError::Io(io::Error::from_raw_os_error(code.0 & 0xFFFF))
}
