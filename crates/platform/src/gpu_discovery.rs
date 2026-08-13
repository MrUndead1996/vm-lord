//! Whether this host can give a VM a GPU, and whether a Linux guest could use
//! it.

use vmlord_core::{
    GpuAvailability, GpuFailure, GpuStatusCode, HostGpuAdapter, HostGpuCapabilities,
    RepositoryError,
};

/// Turns what was observed into the two verdicts.
///
/// Kept free of any Windows call so that every case below is a test rather
/// than a host someone has to find.
fn assemble(
    adapters: Vec<HostGpuAdapter>,
    service: Result<(), RepositoryError>,
    payload_present: bool,
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

    let linux_payload = if payload_present {
        GpuAvailability::Available
    } else {
        GpuAvailability::Unavailable(GpuFailure::new(
            GpuStatusCode::HostLinuxPayloadMissing,
            "the Linux GPU userspace is not staged on this host; install WSL",
        ))
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

    use super::assemble;

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
        let capabilities = assemble(vec![adapter(Some(r"C:\pkg"))], Ok(()), true);

        assert!(capabilities.assignment.is_available());
        assert!(capabilities.linux_payload.is_available());
        assert_eq!(capabilities.adapters.len(), 1);
    }

    #[test]
    fn no_adapters_makes_assignment_unavailable() {
        let capabilities = assemble(Vec::new(), Ok(()), true);

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
            true,
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
        let capabilities = assemble(vec![adapter(None), adapter(None)], Ok(()), true);

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
        let capabilities = assemble(vec![adapter(None), adapter(Some(r"C:\pkg"))], Ok(()), true);

        assert!(capabilities.assignment.is_available());
    }

    #[test]
    fn a_missing_payload_does_not_touch_assignment() {
        let capabilities = assemble(vec![adapter(Some(r"C:\pkg"))], Ok(()), false);

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
