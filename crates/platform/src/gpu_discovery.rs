//! Whether this host can give a VM a GPU, and whether a Linux guest could use
//! it.

use std::path::PathBuf;

use vmlord_core::{
    GpuAvailability, GpuFailure, GpuStatusCode, HostGpuAdapter, HostGpuCapabilities,
    RepositoryError,
};
use windows::Win32::System::SystemInformation::GetSystemDirectoryW;

use crate::{gpu_enumerate::partition_adapters, gpu_exports::program_files_directory, hcs};

/// What this host can do for GPU-PV, right now.
///
/// Not a `Result`: "GPU-PV is unavailable here" is an answer rather than a
/// failure, and a Windows error on the way to it is one of the reasons an axis
/// can be unavailable. Nothing is cached -- the enumeration is cheap, and a
/// driver update or a WSL install changes the answer with nothing to
/// invalidate a cache on.
#[must_use]
pub fn discover_host_gpu() -> HostGpuCapabilities {
    let service = hcs::service_available();
    let adapters = match partition_adapters() {
        Ok(adapters) => adapters,
        Err(error) => {
            log::warn!("enumerating GPU partition adapters failed: {error}");
            Vec::new()
        }
    };

    assemble(adapters, service, linux_payload_present())
}

/// Which halves of the Linux GPU userspace this host has.
///
/// Two fields because they are two directories on every host that installs WSL
/// from the Store: the vendor's libraries under `System32\lxss\lib`, and the
/// Microsoft ones beside the WSL package. A guest needs both, and a host with
/// one of them is a host that cannot render -- which is exactly what the first
/// real host looked like.
pub(crate) struct LinuxPayload {
    pub(crate) wsl_lib: bool,
    pub(crate) wsl_d3d12: bool,
}

/// Whether the Linux GPU userspace WSL stages is on this host, half by half.
///
/// Only the verdict is reported. The paths are what an export is built from,
/// and that is decided where the export is built.
fn linux_payload_present() -> LinuxPayload {
    let mut buffer = [0_u16; 260];
    // SAFETY: `buffer` is passed as a sized slice; a zero return means the
    // call did not fill it.
    let length = unsafe { GetSystemDirectoryW(Some(&mut buffer)) } as usize;
    let wsl_lib = if length == 0 || length > buffer.len() {
        false
    } else {
        PathBuf::from(String::from_utf16_lossy(&buffer[..length]))
            .join("lxss")
            .join("lib")
            .is_dir()
    };

    LinuxPayload {
        wsl_lib,
        wsl_d3d12: program_files_directory()
            .is_some_and(|program_files| program_files.join("WSL").join("lib").is_dir()),
    }
}

/// Turns what was observed into the two verdicts.
///
/// Kept free of any Windows call so that every case below is a test rather
/// than a host someone has to find.
fn assemble(
    adapters: Vec<HostGpuAdapter>,
    service: Result<(), RepositoryError>,
    payload: LinuxPayload,
) -> HostGpuCapabilities {
    let assignment = if let Err(error) = service {
        // A service that is not answering makes the adapter question moot:
        // reporting "no adapters" here would blame the wrong thing.
        GpuAvailability::Unavailable(GpuFailure::new(
            GpuStatusCode::HostServiceUnavailable,
            format!("the Host Compute Service is not available: {error}"),
        ))
    } else if adapters.is_empty() {
        GpuAvailability::Unavailable(GpuFailure::new(
            GpuStatusCode::HostNoAdapter,
            "this host presents no GPU partition adapter",
        ))
    } else if adapters
        .iter()
        .all(|adapter| adapter.driver_store.is_none())
    {
        GpuAvailability::Unavailable(GpuFailure::new(
            GpuStatusCode::HostDriverStoreMissing,
            "no driver package could be located for any GPU partition adapter",
        ))
    } else {
        GpuAvailability::Available
    };

    // Named half by half rather than as one verdict: a host with the vendor's
    // libraries and none of Microsoft's looks installed to anyone reading
    // "install WSL", and it is the commonest way to arrive here.
    let linux_payload = match (payload.wsl_lib, payload.wsl_d3d12) {
        (true, true) => GpuAvailability::Available,
        (true, false) => GpuAvailability::Unavailable(GpuFailure::new(
            GpuStatusCode::HostLinuxPayloadMissing,
            "the Microsoft Direct3D 12 libraries are missing from \"Program Files\\WSL\\lib\"; \
             install or update WSL",
        )),
        (false, true) => GpuAvailability::Unavailable(GpuFailure::new(
            GpuStatusCode::HostLinuxPayloadMissing,
            "the WSL Linux userspace is missing from \"System32\\lxss\\lib\"; install a GPU \
             driver with WSL support",
        )),
        (false, false) => GpuAvailability::Unavailable(GpuFailure::new(
            GpuStatusCode::HostLinuxPayloadMissing,
            "the Linux GPU userspace is not staged on this host; install WSL",
        )),
    };

    HostGpuCapabilities {
        assignment,
        linux_payload,
        adapters,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use vmlord_core::{GpuStatusCode, HostGpuAdapter, RepositoryError};

    use super::{LinuxPayload, assemble};

    /// A host with both halves of the userspace, which is what the cases
    /// about assignment want to hold still.
    const BOTH: LinuxPayload = LinuxPayload {
        wsl_lib: true,
        wsl_d3d12: true,
    };

    fn adapter(driver_store: Option<&str>) -> HostGpuAdapter {
        HostGpuAdapter {
            name: "Microsoft Virtual Render Driver".to_owned(),
            instance_id: r"PCI\VEN_10DE&DEV_1234\3&11583659&0&08".to_owned(),
            interface_path: r"\\?\pci#ven_10de".to_owned(),
            driver_store: driver_store.map(PathBuf::from),
            service: Some("nvlddmkm".to_owned()),
        }
    }

    #[test]
    fn an_adapter_with_a_package_and_a_payload_is_fully_available() {
        let capabilities = assemble(vec![adapter(Some(r"C:\pkg"))], Ok(()), BOTH);

        assert!(capabilities.assignment.is_available());
        assert!(capabilities.linux_payload.is_available());
        assert_eq!(capabilities.adapters.len(), 1);
    }

    #[test]
    fn no_adapters_makes_assignment_unavailable() {
        let capabilities = assemble(Vec::new(), Ok(()), BOTH);

        assert_eq!(
            capabilities.assignment.failure().map(|failure| failure.code),
            Some(GpuStatusCode::HostNoAdapter)
        );
        assert!(capabilities.linux_payload.is_available());
    }

    #[test]
    fn a_dead_service_outranks_the_adapter_question() {
        let capabilities = assemble(
            Vec::new(),
            Err(RepositoryError::new("HCS is not answering")),
            BOTH,
        );

        let failure = capabilities.assignment.failure().expect("unavailable");
        assert_eq!(failure.code, GpuStatusCode::HostServiceUnavailable);
        assert!(
            failure.message.contains("HCS is not answering"),
            "the service's own words have to survive: {}",
            failure.message
        );
    }

    #[test]
    fn adapters_without_any_package_make_assignment_unavailable() {
        let capabilities = assemble(vec![adapter(None), adapter(None)], Ok(()), BOTH);

        assert_eq!(
            capabilities.assignment.failure().map(|failure| failure.code),
            Some(GpuStatusCode::HostDriverStoreMissing)
        );
        assert_eq!(
            capabilities.adapters.len(),
            2,
            "an unresolved adapter is still reported"
        );
    }

    #[test]
    fn one_resolved_package_is_enough_for_assignment() {
        let capabilities = assemble(vec![adapter(None), adapter(Some(r"C:\pkg"))], Ok(()), BOTH);

        assert!(capabilities.assignment.is_available());
    }

    #[test]
    fn a_host_with_only_the_vendor_libraries_has_no_usable_linux_payload() {
        // What the first real host had: System32\lxss\lib full of NVIDIA
        // libraries and no libd3d12.so anywhere a guest could reach.
        let capabilities = assemble(
            vec![adapter(Some(r"C:\pkg"))],
            Ok(()),
            LinuxPayload {
                wsl_lib: true,
                wsl_d3d12: false,
            },
        );

        assert!(!capabilities.linux_payload.is_available());
        let failure = capabilities.linux_payload.failure().expect("a reason");
        assert_eq!(failure.code, GpuStatusCode::HostLinuxPayloadMissing);
        assert!(
            failure.message.contains("Program Files"),
            "the half that is missing is the half worth naming: {}",
            failure.message
        );
    }

    #[test]
    fn a_host_with_both_halves_has_a_usable_linux_payload() {
        let capabilities = assemble(vec![adapter(Some(r"C:\pkg"))], Ok(()), BOTH);

        assert!(capabilities.linux_payload.is_available());
    }

    #[test]
    fn a_missing_payload_does_not_touch_assignment() {
        let capabilities = assemble(
            vec![adapter(Some(r"C:\pkg"))],
            Ok(()),
            LinuxPayload {
                wsl_lib: false,
                wsl_d3d12: false,
            },
        );

        assert!(capabilities.assignment.is_available());
        assert_eq!(
            capabilities
                .linux_payload
                .failure()
                .map(|failure| failure.code),
            Some(GpuStatusCode::HostLinuxPayloadMissing)
        );
    }
}
