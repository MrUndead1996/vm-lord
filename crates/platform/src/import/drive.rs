//! The disk Windows presents a VHDX as, and the handle that writes to it.
//!
//! Attaching a virtual disk hands it to the same storage stack as any other
//! disk. That is the point -- it is how bytes get inside a VHDX without a VHDX
//! writer -- and it is also the hazard: the volume manager is watching, and a
//! disk it recognises is a disk it may take. Nothing here calls
//! `IOCTL_DISK_UPDATE_PROPERTIES`, which is what asks it to look.

use std::{
    alloc::{self, Layout},
    path::Path,
    ptr::NonNull,
};

use vmlord_core::RepositoryError;
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE},
        Storage::{
            FileSystem::{
                CreateFileW, FILE_BEGIN, FILE_FLAG_NO_BUFFERING, FILE_FLAG_WRITE_THROUGH,
                FILE_SHARE_READ, FILE_SHARE_WRITE, FlushFileBuffers, OPEN_EXISTING, ReadFile,
                SetFilePointerEx, WriteFile,
            },
            Vhd::{
                ATTACH_VIRTUAL_DISK_FLAG_BYPASS_DEFAULT_ENCRYPTION_POLICY,
                ATTACH_VIRTUAL_DISK_FLAG_NO_DRIVE_LETTER, ATTACH_VIRTUAL_DISK_PARAMETERS,
                ATTACH_VIRTUAL_DISK_VERSION_1, AttachVirtualDisk, DETACH_VIRTUAL_DISK_FLAG_NONE,
                DetachVirtualDisk, GetVirtualDiskPhysicalPath, OPEN_VIRTUAL_DISK_FLAG_NONE,
                OpenVirtualDisk, VIRTUAL_DISK_ACCESS_ATTACH_RW, VIRTUAL_DISK_ACCESS_GET_INFO,
                VIRTUAL_STORAGE_TYPE, VIRTUAL_STORAGE_TYPE_DEVICE_VHDX,
                VIRTUAL_STORAGE_TYPE_VENDOR_MICROSOFT,
            },
        },
    },
    core::{HSTRING, PWSTR},
};

use crate::{
    error::windows_error,
    import::{
        copy::DiskBlocks,
        plan::{SECTOR_ALIGNMENT, padded_length},
    },
};

/// Generic read and write access, spelled out because `windows-rs` types
/// `dwDesiredAccess` as a bare `u32`.
const GENERIC_READ_WRITE: u32 = 0x8000_0000 | 0x4000_0000;

/// A VHDX attached to the host as a disk, detached again when this is dropped.
pub(crate) struct AttachedDisk {
    handle: HANDLE,
    physical_path: String,
    detached: bool,
}

impl AttachedDisk {
    /// Attaches the VHDX at `path` and works out which `\\.\PhysicalDriveN` it
    /// became.
    ///
    /// No drive letter is asked for: a letter is an invitation to everything on
    /// the host that reacts to one. The encryption policy is bypassed for the
    /// reason AppSandbox bypasses it (`tools/iso-patch/ubuntu_vhdx.c:1918`) --
    /// on a host with default-encrypt enabled the new disk would inherit
    /// BitLocker, and UEFI cannot read the boot sectors of a disk it has to
    /// decrypt first.
    pub(crate) fn attach(path: &Path) -> Result<Self, RepositoryError> {
        let storage_type = VIRTUAL_STORAGE_TYPE {
            DeviceId: VIRTUAL_STORAGE_TYPE_DEVICE_VHDX,
            VendorId: VIRTUAL_STORAGE_TYPE_VENDOR_MICROSOFT,
        };
        let wide_path = HSTRING::from(path.as_os_str().to_string_lossy().as_ref());
        let mut handle = HANDLE::default();
        // SAFETY: `storage_type` and `wide_path` outlive the call, and `handle`
        // is only used after the call reports success.
        let result = unsafe {
            OpenVirtualDisk(
                &storage_type,
                &wide_path,
                VIRTUAL_DISK_ACCESS_ATTACH_RW | VIRTUAL_DISK_ACCESS_GET_INFO,
                OPEN_VIRTUAL_DISK_FLAG_NONE,
                None,
                &mut handle,
            )
        };
        result.ok().map_err(|error| {
            let error = windows_error("open virtual disk for attach", None, error);
            tracing::error!("{} for {}", error, path.display());
            error
        })?;

        let parameters = ATTACH_VIRTUAL_DISK_PARAMETERS {
            Version: ATTACH_VIRTUAL_DISK_VERSION_1,
            ..Default::default()
        };
        // SAFETY: `handle` is the disk opened above and `parameters` outlives
        // the call.
        let result = unsafe {
            AttachVirtualDisk(
                handle,
                None,
                ATTACH_VIRTUAL_DISK_FLAG_NO_DRIVE_LETTER
                    | ATTACH_VIRTUAL_DISK_FLAG_BYPASS_DEFAULT_ENCRYPTION_POLICY,
                0,
                Some(&parameters),
                None,
            )
        };
        if let Err(error) = result.ok() {
            let error = windows_error("attach virtual disk", None, error);
            tracing::error!("{} for {}", error, path.display());
            // SAFETY: the disk was opened above and never attached, so closing
            // the handle is all the undoing there is.
            let _ = unsafe { CloseHandle(handle) };
            return Err(error);
        }

        let mut attached = Self {
            handle,
            physical_path: String::new(),
            detached: false,
        };
        attached.physical_path = attached.read_physical_path(path)?;
        tracing::info!("attached {} as {}", path.display(), attached.physical_path);
        Ok(attached)
    }

    /// `\\.\PhysicalDriveN` for the attached disk.
    pub(crate) fn physical_path(&self) -> &str {
        &self.physical_path
    }

    /// Detaches the disk, reporting a failure rather than swallowing it.
    ///
    /// The drop glue detaches too, but it has nowhere to put an error, and a
    /// VHDX still attached at the end of an import is a file the caller cannot
    /// move, copy or hand to a VM.
    pub(crate) fn detach(mut self) -> Result<(), RepositoryError> {
        self.detached = true;
        // SAFETY: `handle` came from the successful `OpenVirtualDisk` above,
        // has not been closed, and is closed exactly once below.
        let detached = unsafe { DetachVirtualDisk(self.handle, DETACH_VIRTUAL_DISK_FLAG_NONE, 0) };
        // SAFETY: as above; the handle is not used again after this point.
        let closed = unsafe { CloseHandle(self.handle) };

        detached.ok().map_err(|error| {
            let error = windows_error("detach virtual disk", None, error);
            tracing::error!("{error}");
            error
        })?;
        closed.map_err(|error| {
            let error = windows_error("close virtual disk handle", None, error);
            tracing::error!("{error}");
            error
        })?;
        tracing::debug!("detached {}", self.physical_path);
        Ok(())
    }

    fn read_physical_path(&self, path: &Path) -> Result<String, RepositoryError> {
        // `\\.\PhysicalDrive` plus a number; MAX_PATH is what the API is
        // documented against and orders of magnitude more than it needs.
        let mut buffer = [0u16; 260];
        let mut size = u32::try_from(size_of_val(&buffer)).expect("260 u16s fit in a u32");
        // SAFETY: `handle` is the attached disk, and both out-parameters point
        // at live locals for the duration of the call.
        let result = unsafe {
            GetVirtualDiskPhysicalPath(self.handle, &mut size, PWSTR(buffer.as_mut_ptr()))
        };
        result.ok().map_err(|error| {
            let error = windows_error("get virtual disk physical path", None, error);
            tracing::error!("{} for {}", error, path.display());
            error
        })?;

        let length = buffer.iter().position(|unit| *unit == 0).unwrap_or(0);
        Ok(String::from_utf16_lossy(&buffer[..length]))
    }
}

impl Drop for AttachedDisk {
    fn drop(&mut self) {
        if self.detached {
            return;
        }
        tracing::warn!(
            "detaching {} during unwind; the import did not finish",
            self.physical_path
        );
        // SAFETY: the handle is live here precisely because `detach` was not
        // called, and this runs once.
        unsafe {
            let _ = DetachVirtualDisk(self.handle, DETACH_VIRTUAL_DISK_FLAG_NONE, 0);
            let _ = CloseHandle(self.handle);
        }
    }
}

/// A handle on `\\.\PhysicalDriveN`, opened so that writes go to the disk
/// instead of into the system cache.
///
/// `FILE_FLAG_NO_BUFFERING` is what makes the read-back after an import mean
/// anything: a cached read would answer out of the same memory the write went
/// into, and would agree with it whether or not the disk ever saw it. It is
/// also what AppSandbox found made the difference between 10-30 MB/s and a
/// sensible rate (`tools/iso-patch/ubuntu_vhdx.c:1939-1948`). The price is that
/// every offset, every length and the address of every buffer must be sector
/// aligned, which is what `scratch` is for.
pub(crate) struct PhysicalDrive {
    handle: HANDLE,
    path: String,
    scratch: AlignedBuffer,
}

impl PhysicalDrive {
    /// Opens the drive at `physical_path` for unbuffered read and write,
    /// with room to move `chunk_bytes` at a time.
    pub(crate) fn open(physical_path: &str, chunk_bytes: usize) -> Result<Self, RepositoryError> {
        let wide_path = HSTRING::from(physical_path);
        // SAFETY: `wide_path` outlives the call and the handle is owned by the
        // value returned here, which closes it on drop.
        let handle = unsafe {
            CreateFileW(
                &wide_path,
                GENERIC_READ_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH,
                None,
            )
        }
        .map_err(|error| {
            let error = windows_error("open physical drive", None, error);
            tracing::error!("{error} for {physical_path}");
            error
        })?;

        tracing::debug!("opened {physical_path} unbuffered, {chunk_bytes} bytes at a time");
        Ok(Self {
            handle,
            path: physical_path.to_owned(),
            scratch: AlignedBuffer::new(
                padded_length(chunk_bytes, SECTOR_ALIGNMENT),
                SECTOR_ALIGNMENT,
            ),
        })
    }

    fn seek(&self, offset: u64) -> Result<(), RepositoryError> {
        let distance = i64::try_from(offset).map_err(|_| {
            RepositoryError::new(format!("the offset {offset} is past the end of any disk"))
        })?;
        // SAFETY: `handle` is the drive opened above and no out-parameter is
        // asked for.
        unsafe { SetFilePointerEx(self.handle, distance, None, FILE_BEGIN) }.map_err(|error| {
            let error = windows_error("seek on physical drive", None, error);
            tracing::error!("{error} to {offset} on {}", self.path);
            error
        })
    }
}

impl DiskBlocks for PhysicalDrive {
    /// Writes `bytes` at `offset`, padding with zeros up to the sector the
    /// unbuffered handle insists on. Only the last chunk of an image is ever
    /// short, and the bytes past its end already read as zeros.
    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<(), RepositoryError> {
        let length = padded_length(bytes.len(), SECTOR_ALIGNMENT);
        let scratch = self.scratch.as_mut_slice(length)?;
        scratch[..bytes.len()].copy_from_slice(bytes);
        scratch[bytes.len()..].fill(0);

        self.seek(offset)?;
        let mut written = 0u32;
        // SAFETY: `handle` is the drive opened above, the slice is the aligned
        // buffer this value owns, and `written` is a live local.
        unsafe {
            WriteFile(
                self.handle,
                Some(self.scratch.slice(length)),
                Some(&mut written),
                None,
            )
        }
        .map_err(|error| {
            let error = windows_error("write to physical drive", None, error);
            tracing::error!("{error} at {offset} on {}", self.path);
            error
        })?;

        if written as usize != length {
            let error = RepositoryError::new(format!(
                "a {length}-byte write at {offset} on {} moved {written} bytes",
                self.path
            ));
            tracing::error!("{error}");
            return Err(error);
        }
        tracing::debug!("wrote {length} bytes at {offset} on {}", self.path);
        Ok(())
    }

    /// Reads `bytes.len()` bytes from `offset`, reading the whole sector they
    /// sit in and handing back the part that was asked for.
    fn read_at(&mut self, offset: u64, bytes: &mut [u8]) -> Result<(), RepositoryError> {
        let length = padded_length(bytes.len(), SECTOR_ALIGNMENT);
        self.seek(offset)?;
        let scratch = self.scratch.as_mut_slice(length)?;
        let mut read = 0u32;
        // SAFETY: as in `write_at`; the buffer is owned here and outlives the
        // call.
        unsafe { ReadFile(self.handle, Some(scratch), Some(&mut read), None) }.map_err(
            |error| {
                let error = windows_error("read from physical drive", None, error);
                tracing::error!("{error} at {offset} on {}", self.path);
                error
            },
        )?;

        if (read as usize) < bytes.len() {
            let error = RepositoryError::new(format!(
                "a read of {} bytes at {offset} on {} came back with {read}",
                bytes.len(),
                self.path
            ));
            tracing::error!("{error}");
            return Err(error);
        }
        let wanted = bytes.len();
        bytes.copy_from_slice(self.scratch.slice(wanted));
        Ok(())
    }

    fn flush(&mut self) -> Result<(), RepositoryError> {
        // SAFETY: `handle` is the drive opened above and still open.
        unsafe { FlushFileBuffers(self.handle) }.map_err(|error| {
            let error = windows_error("flush physical drive", None, error);
            tracing::error!("{error} on {}", self.path);
            error
        })?;
        tracing::debug!("flushed {}", self.path);
        Ok(())
    }
}

impl Drop for PhysicalDrive {
    fn drop(&mut self) {
        // SAFETY: the handle came from the successful `CreateFileW` in `open`
        // and is closed exactly once, here.
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

/// A buffer whose address is sector aligned, which `FILE_FLAG_NO_BUFFERING`
/// requires of anything it reads into or writes out of.
///
/// `Vec<u8>` cannot promise this: its allocation is aligned for a `u8`, which
/// is to say not at all.
struct AlignedBuffer {
    pointer: NonNull<u8>,
    layout: Layout,
}

impl AlignedBuffer {
    fn new(length: usize, alignment: usize) -> Self {
        let layout = Layout::from_size_align(length.max(alignment), alignment)
            .expect("a sector-aligned buffer layout is always valid");
        // SAFETY: the layout has a non-zero size, so this is the documented use
        // of `alloc_zeroed`.
        let pointer = unsafe { alloc::alloc_zeroed(layout) };
        match NonNull::new(pointer) {
            Some(pointer) => Self { pointer, layout },
            None => alloc::handle_alloc_error(layout),
        }
    }

    fn as_mut_slice(&mut self, length: usize) -> Result<&mut [u8], RepositoryError> {
        self.check(length)?;
        // SAFETY: the allocation is `self.layout.size()` bytes long and
        // `length` is no larger, the memory was zeroed on allocation, and the
        // borrow is exclusive for the lifetime of the returned slice.
        Ok(unsafe { std::slice::from_raw_parts_mut(self.pointer.as_ptr(), length) })
    }

    fn slice(&self, length: usize) -> &[u8] {
        debug_assert!(length <= self.layout.size());
        // SAFETY: as above, and shared rather than exclusive.
        unsafe { std::slice::from_raw_parts(self.pointer.as_ptr(), length) }
    }

    fn check(&self, length: usize) -> Result<(), RepositoryError> {
        if length > self.layout.size() {
            return Err(RepositoryError::new(format!(
                "a {length}-byte transfer does not fit the {}-byte buffer",
                self.layout.size()
            )));
        }
        Ok(())
    }
}

impl Drop for AlignedBuffer {
    fn drop(&mut self) {
        // SAFETY: the pointer came from `alloc_zeroed` with this exact layout
        // and is freed once.
        unsafe { alloc::dealloc(self.pointer.as_ptr(), self.layout) };
    }
}

#[cfg(test)]
mod tests {
    use super::AlignedBuffer;
    use crate::import::plan::SECTOR_ALIGNMENT;

    #[test]
    fn a_buffer_starts_on_a_sector_boundary_and_starts_out_zeroed() {
        let buffer = AlignedBuffer::new(1024 * 1024, SECTOR_ALIGNMENT);

        assert_eq!(buffer.pointer.as_ptr() as usize % SECTOR_ALIGNMENT, 0);
        assert!(buffer.slice(1024 * 1024).iter().all(|byte| *byte == 0));
    }

    #[test]
    fn a_transfer_larger_than_the_buffer_is_refused_rather_than_read_past_it() {
        let mut buffer = AlignedBuffer::new(SECTOR_ALIGNMENT, SECTOR_ALIGNMENT);

        let error = buffer
            .as_mut_slice(SECTOR_ALIGNMENT + 1)
            .expect_err("a transfer past the end of the buffer must be refused");

        assert!(error.to_string().contains("does not fit"));
    }
}
