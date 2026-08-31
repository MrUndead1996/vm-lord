//! VHDX disk creation for HCS-backed virtual machines.

use std::path::Path;

use vmlord_core::RepositoryError;
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE},
        Storage::Vhd::{
            CREATE_VIRTUAL_DISK_FLAG_NONE, CREATE_VIRTUAL_DISK_PARAMETERS,
            CREATE_VIRTUAL_DISK_PARAMETERS_0, CREATE_VIRTUAL_DISK_PARAMETERS_0_1,
            CREATE_VIRTUAL_DISK_VERSION_2, CreateVirtualDisk, GET_VIRTUAL_DISK_INFO,
            GET_VIRTUAL_DISK_INFO_SIZE, GetVirtualDiskInformation, OPEN_VIRTUAL_DISK_FLAG_NONE,
            OPEN_VIRTUAL_DISK_PARAMETERS, OPEN_VIRTUAL_DISK_PARAMETERS_0,
            OPEN_VIRTUAL_DISK_PARAMETERS_0_1, OPEN_VIRTUAL_DISK_VERSION_2, OpenVirtualDisk,
            RESIZE_VIRTUAL_DISK_FLAG_NONE, RESIZE_VIRTUAL_DISK_PARAMETERS,
            RESIZE_VIRTUAL_DISK_PARAMETERS_0, RESIZE_VIRTUAL_DISK_PARAMETERS_0_0,
            RESIZE_VIRTUAL_DISK_VERSION_1, ResizeVirtualDisk, VIRTUAL_DISK_ACCESS_GET_INFO,
            VIRTUAL_DISK_ACCESS_NONE, VIRTUAL_STORAGE_TYPE, VIRTUAL_STORAGE_TYPE_DEVICE_VHDX,
            VIRTUAL_STORAGE_TYPE_VENDOR_MICROSOFT,
        },
    },
    core::HSTRING,
};

use crate::error::windows_error;

/// Creates a new dynamically-expanding VHDX disk at `path`.
///
/// Rejects a zero size, an existing target, or a missing parent directory
/// before making any Windows API call.
pub(crate) fn create_dynamic_vhdx(path: &Path, size_bytes: u64) -> Result<(), RepositoryError> {
    if size_bytes == 0 {
        return Err(RepositoryError::new(
            "VHDX disk size must be greater than zero",
        ));
    }
    if path.exists() {
        return Err(RepositoryError::new(format!(
            "VHDX target already exists: {}",
            path.display()
        )));
    }
    match path.parent() {
        Some(parent) if parent.is_dir() => {}
        _ => {
            return Err(RepositoryError::new(format!(
                "VHDX parent directory does not exist: {}",
                path.display()
            )));
        }
    }

    tracing::debug!(
        "creating VHDX disk at {} ({size_bytes} bytes)",
        path.display()
    );

    let storage_type = VIRTUAL_STORAGE_TYPE {
        DeviceId: VIRTUAL_STORAGE_TYPE_DEVICE_VHDX,
        VendorId: VIRTUAL_STORAGE_TYPE_VENDOR_MICROSOFT,
    };
    // Version 1 parameters are documented for the legacy VHD format only;
    // creating a VHDX requires Version 2 (it adds `PhysicalSectorSizeInBytes`).
    // Using Version 1 here makes `CreateVirtualDisk` fail with a misleading
    // `ERROR_ACCESS_DENIED`, rather than a version/parameter-related error.
    let parameters = CREATE_VIRTUAL_DISK_PARAMETERS {
        Version: CREATE_VIRTUAL_DISK_VERSION_2,
        Anonymous: CREATE_VIRTUAL_DISK_PARAMETERS_0 {
            Version2: CREATE_VIRTUAL_DISK_PARAMETERS_0_1 {
                MaximumSize: size_bytes,
                ..Default::default()
            },
        },
    };
    // `HSTRING` has no `From<&OsStr>`; UTF-16 surrogates in paths are
    // extremely rare, so a lossy conversion is acceptable here.
    let wide_path = HSTRING::from(path.as_os_str().to_string_lossy().as_ref());
    let mut handle = HANDLE::default();
    // SAFETY: `storage_type`, `parameters` and `wide_path` outlive the call.
    // On success the returned handle is closed immediately below; the
    // created file persists on disk after the handle closes.
    let result = unsafe {
        CreateVirtualDisk(
            &storage_type,
            &wide_path,
            VIRTUAL_DISK_ACCESS_NONE,
            None,
            CREATE_VIRTUAL_DISK_FLAG_NONE,
            0,
            &parameters,
            None,
            &mut handle,
        )
    };
    result.ok().map_err(|error| {
        let error = windows_error("create virtual disk", None, error);
        tracing::error!("{error}");
        error
    })?;

    // SAFETY: `handle` was returned by the successful `CreateVirtualDisk`
    // call above and is closed exactly once here.
    unsafe { CloseHandle(handle) }.map_err(|error| {
        let error = windows_error("close virtual disk handle", None, error);
        tracing::error!("{error}");
        error
    })?;

    tracing::info!("created VHDX disk at {}", path.display());
    Ok(())
}

/// Grows the VHDX at `path` until the guest sees `size_bytes`.
///
/// Growth only. `ResizeVirtualDisk` shrinks a disk just as willingly as it
/// grows one, and cutting a disk off underneath a filesystem that still
/// believes in the old size destroys it, so anything that is not larger than
/// the disk already is refused here rather than passed on.
///
/// The VM has to be stopped: Hyper-V holds a running VM's VHDX open
/// exclusively, and the open below fails while it does.
pub(crate) fn expand_vhdx(path: &Path, size_bytes: u64) -> Result<(), RepositoryError> {
    if !path.is_file() {
        return Err(RepositoryError::new(format!(
            "the VHDX to expand does not exist: {}",
            path.display()
        )));
    }
    let current = virtual_size_bytes(path)?;
    if size_bytes <= current {
        return Err(RepositoryError::new(format!(
            "{} already presents {current} bytes, so it cannot be resized to \
             {size_bytes}: a disk may only grow",
            path.display()
        )));
    }

    tracing::debug!(
        "expanding VHDX disk at {} from {current} to {size_bytes} bytes",
        path.display()
    );

    let storage_type = VIRTUAL_STORAGE_TYPE {
        DeviceId: VIRTUAL_STORAGE_TYPE_DEVICE_VHDX,
        VendorId: VIRTUAL_STORAGE_TYPE_VENDOR_MICROSOFT,
    };
    let wide_path = HSTRING::from(path.as_os_str().to_string_lossy().as_ref());
    let mut handle = HANDLE::default();
    // A VHDX is opened for writing through Version 2 parameters and an empty
    // access mask; asking for `VIRTUAL_DISK_ACCESS_METAOPS` instead -- the mask
    // the resize is documented to need -- fails with `ERROR_ACCESS_DENIED`,
    // the same misleading error the version mismatch produces on creation.
    let open_parameters = OPEN_VIRTUAL_DISK_PARAMETERS {
        Version: OPEN_VIRTUAL_DISK_VERSION_2,
        Anonymous: OPEN_VIRTUAL_DISK_PARAMETERS_0 {
            Version2: OPEN_VIRTUAL_DISK_PARAMETERS_0_1 {
                GetInfoOnly: false.into(),
                ReadOnly: false.into(),
                ..Default::default()
            },
        },
    };
    // SAFETY: `storage_type`, `wide_path` and `open_parameters` outlive the
    // call, and `handle` is only read after the call reports success.
    let result = unsafe {
        OpenVirtualDisk(
            &storage_type,
            &wide_path,
            VIRTUAL_DISK_ACCESS_NONE,
            OPEN_VIRTUAL_DISK_FLAG_NONE,
            Some(&open_parameters),
            &mut handle,
        )
    };
    result.ok().map_err(|error| {
        let error = windows_error("open virtual disk", None, error);
        tracing::error!("{} for {}", error, path.display());
        error
    })?;

    let parameters = RESIZE_VIRTUAL_DISK_PARAMETERS {
        Version: RESIZE_VIRTUAL_DISK_VERSION_1,
        Anonymous: RESIZE_VIRTUAL_DISK_PARAMETERS_0 {
            Version1: RESIZE_VIRTUAL_DISK_PARAMETERS_0_0 {
                NewSize: size_bytes,
            },
        },
    };
    // SAFETY: `handle` is the disk opened above and `parameters` outlives the
    // call, which runs to completion because no `OVERLAPPED` is passed.
    let result =
        unsafe { ResizeVirtualDisk(handle, RESIZE_VIRTUAL_DISK_FLAG_NONE, &parameters, None) };
    // SAFETY: `handle` came from the successful `OpenVirtualDisk` above and is
    // closed exactly once here, after its last use.
    let closed = unsafe { CloseHandle(handle) };

    result.ok().map_err(|error| {
        let error = windows_error("resize virtual disk", None, error);
        tracing::error!("{} for {}", error, path.display());
        error
    })?;
    closed.map_err(|error| {
        let error = windows_error("close virtual disk handle", None, error);
        tracing::error!("{error}");
        error
    })?;

    tracing::info!(
        "expanded VHDX disk at {} to {size_bytes} bytes",
        path.display()
    );
    Ok(())
}

/// Reads the size the guest sees for the VHDX at `path`, in bytes.
///
/// A dynamically-expanding disk's file is far smaller than the disk it
/// presents, so the file length cannot answer this; only the disk's own
/// header can.
pub(crate) fn virtual_size_bytes(path: &Path) -> Result<u64, RepositoryError> {
    let storage_type = VIRTUAL_STORAGE_TYPE {
        DeviceId: VIRTUAL_STORAGE_TYPE_DEVICE_VHDX,
        VendorId: VIRTUAL_STORAGE_TYPE_VENDOR_MICROSOFT,
    };
    let wide_path = HSTRING::from(path.as_os_str().to_string_lossy().as_ref());
    let mut handle = HANDLE::default();
    // SAFETY: `storage_type` and `wide_path` outlive the call, and `handle` is
    // only read after the call reports success.
    let result = unsafe {
        OpenVirtualDisk(
            &storage_type,
            &wide_path,
            VIRTUAL_DISK_ACCESS_GET_INFO,
            OPEN_VIRTUAL_DISK_FLAG_NONE,
            None,
            &mut handle,
        )
    };
    result.ok().map_err(|error| {
        let error = windows_error("open virtual disk", None, error);
        tracing::error!("{} for {}", error, path.display());
        error
    })?;

    let mut information = GET_VIRTUAL_DISK_INFO {
        Version: GET_VIRTUAL_DISK_INFO_SIZE,
        ..Default::default()
    };
    let mut information_size = u32::try_from(size_of::<GET_VIRTUAL_DISK_INFO>())
        .expect("GET_VIRTUAL_DISK_INFO always fits in a u32");
    // SAFETY: `handle` is the disk opened above, and both out-parameters point
    // at live locals for the duration of the call.
    let result =
        unsafe { GetVirtualDiskInformation(handle, &mut information_size, &mut information, None) };
    // SAFETY: `handle` came from the successful `OpenVirtualDisk` above and is
    // closed exactly once here, after its last use.
    let closed = unsafe { CloseHandle(handle) };

    result.ok().map_err(|error| {
        let error = windows_error("get virtual disk information", None, error);
        tracing::error!("{} for {}", error, path.display());
        error
    })?;
    closed.map_err(|error| {
        let error = windows_error("close virtual disk handle", None, error);
        tracing::error!("{error}");
        error
    })?;

    // SAFETY: `GET_VIRTUAL_DISK_INFO_SIZE` is the version requested above, so
    // HCS filled in the `Size` arm of the union.
    let virtual_size = unsafe { information.Anonymous.Size.VirtualSize };
    tracing::debug!("{} presents {virtual_size} bytes", path.display());
    Ok(virtual_size)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::{create_dynamic_vhdx, expand_vhdx, virtual_size_bytes};

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn path(&self) -> &PathBuf {
            &self.0
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temp_root(label: &str) -> TempRoot {
        static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "vmlord-vhd-test-{label}-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("test root should be created");
        TempRoot(path)
    }

    #[test]
    fn rejects_a_zero_sized_disk_before_touching_the_filesystem() {
        let root = temp_root("zero");
        let target = root.path().join("disk.vhdx");

        let error = create_dynamic_vhdx(&target, 0).unwrap_err();

        assert!(error.to_string().contains("disk size"));
        assert!(!target.exists());
    }

    #[test]
    fn rejects_an_existing_target_without_overwriting_it() {
        let root = temp_root("existing");
        let target = root.path().join("disk.vhdx");
        fs::write(&target, b"original").unwrap();

        let error = create_dynamic_vhdx(&target, 1 << 30).unwrap_err();

        assert!(error.to_string().contains("already exists"));
        assert_eq!(fs::read(&target).unwrap(), b"original");
    }

    #[test]
    fn rejects_a_target_whose_parent_directory_is_missing() {
        let root = temp_root("orphan");
        let target = root.path().join("missing/disk.vhdx");

        let error = create_dynamic_vhdx(&target, 1 << 30).unwrap_err();

        assert!(error.to_string().contains("parent directory"));
        assert!(!target.exists());
    }

    #[test]
    fn refuses_to_expand_a_disk_that_is_not_there() {
        let root = temp_root("absent");
        let target = root.path().join("disk.vhdx");

        let error = expand_vhdx(&target, 1 << 30).unwrap_err();

        assert!(error.to_string().contains("does not exist"));
    }

    /// The guard clauses above need no disk; growth and the refusal to shrink
    /// cannot be checked without one, so these two create a real VHDX. Run
    /// them with:
    /// `cargo test-windows -p vmlord-platform --lib vhd::tests::expands -- --ignored --nocapture`
    #[test]
    #[ignore = "touches the real filesystem"]
    fn expands_a_real_disk_to_the_size_it_was_given() {
        let root = temp_root("expand");
        let target = root.path().join("disk.vhdx");
        create_dynamic_vhdx(&target, 1 << 30).unwrap();

        expand_vhdx(&target, 2 << 30).expect("a disk should grow");

        assert_eq!(virtual_size_bytes(&target).unwrap(), 2 << 30);
    }

    #[test]
    #[ignore = "touches the real filesystem"]
    fn expands_nothing_when_asked_for_a_smaller_disk() {
        let root = temp_root("shrink");
        let target = root.path().join("disk.vhdx");
        create_dynamic_vhdx(&target, 2 << 30).unwrap();

        let error = expand_vhdx(&target, 1 << 30).unwrap_err();

        assert!(error.to_string().contains("may only grow"));
        assert_eq!(virtual_size_bytes(&target).unwrap(), 2 << 30);
    }

    /// Every other test here only exercises the guard clauses; none actually
    /// calls `CreateVirtualDisk`. This is the regression test for the
    /// `CREATE_VIRTUAL_DISK_VERSION_1`-with-VHDX bug, which surfaced as
    /// `ERROR_ACCESS_DENIED` (HRESULT 0x80070005) instead of a version- or
    /// parameter-related error. Run elevated with:
    /// `cargo test -p vmlord-platform --lib vhd::tests::creates_a_real_disk_when_run_elevated -- --ignored --exact --nocapture`
    #[test]
    #[ignore = "requires an elevated process and touches the real filesystem"]
    fn creates_a_real_disk_when_run_elevated() {
        let root = temp_root("real");
        let target = root.path().join("disk.vhdx");

        create_dynamic_vhdx(&target, 1 << 30).expect("disk creation should succeed when elevated");

        assert!(target.is_file());
        println!("created {}", target.display());
    }
}
