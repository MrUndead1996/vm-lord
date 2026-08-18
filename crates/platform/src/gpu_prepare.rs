//! Everything a VM's GPU needs before its compute system is started.
//!
//! Staging, exports and the access grants belong together because they are
//! one decision seen three times: how much of what this host has can actually
//! be handed to this guest. What comes out is the shares to write into the
//! configuration the system is built from, the manifest the agent will offer,
//! and the assignment fact the status is read from.
//!
//! All of it happens before the start, and none of it may be repeated during
//! one: a compute system's Plan9 section is written when the system is built
//! and is immutable for the lifetime of a boot. The shares are therefore never
//! written back to the stored `config.json` -- they name this host's paths and
//! this run's staging directory, which is exactly as long as they are true.
//!
//! Nothing here fails a start. Every way this can go wrong leaves a VM running
//! with less GPU than it asked for, which is an ordinary outcome that
//! [`vmlord_core::VmGpuStatus`] has words for.

use std::{path::Path, sync::atomic::AtomicBool};

use vmlord_core::{
    GpuAssignment, GpuFailure, GpuMode, GpuShareManifest, GpuShareRole, GpuStatusCode,
    HostGpuAdapter, NativeGpuDetail,
};

use crate::{
    HcsClient,
    gpu_enumerate::partition_adapters,
    gpu_exports::GpuExports,
    gpu_staging::{StageGpuPayloadRequest, stage_for_vm},
    metadata::VmComputeSystemMapping,
};

/// What a VM's GPU needs, ready for the system to be started with.
pub(crate) struct PreparedGpu {
    /// The shares to write into the configuration this system is built from.
    ///
    /// `None` is a host that justified none, which is still a VM that starts.
    pub(crate) exports: Option<GpuExports>,
    /// What the guest will be told to mount, and what the agent listener
    /// offers on every session of this run.
    pub(crate) manifest: GpuShareManifest,
    /// What the host managed to hand over, for the status to be read from.
    pub(crate) assignment: GpuAssignment,
}

/// Stages the payload, then builds and grants the exports.
///
/// `None` is a VM that asks for no GPU: there is nothing to prepare and
/// nothing to say about it. Everything else answers with something, a host
/// that could hand over nothing at all included -- that is a `Failed`
/// assignment and a start that carries on regardless.
pub(crate) fn prepare(
    mapping: &VmComputeSystemMapping,
    vm_directory: &Path,
    executable_directory: &Path,
    cache_root: &Path,
    cancel: &AtomicBool,
) -> Option<PreparedGpu> {
    if mapping.gpu_mode == GpuMode::None {
        return None;
    }

    let payload_staged = stage(mapping, vm_directory, executable_directory, cache_root, cancel);
    let adapters = partition_adapters().unwrap_or_else(|error| {
        log::warn!(
            "the GPU adapters of this host could not be enumerated for VM \"{}\": {error}",
            mapping.vm_name
        );
        Vec::new()
    });
    // The grants are asked for before anything is offered, and every one of
    // them is expected to be refused: these paths live under `System32`. See
    // `GpuExports::granted_to` for why that is not a problem.
    let exports = GpuExports::build(&adapters, vm_directory).map(|exports| {
        exports.granted_to(&mapping.hcs_compute_system_id, &|id, path| {
            HcsClient::new().grant_vm_access(id, path)
        })
    });

    let assignment = coverage(&adapters, exports.as_ref(), payload_staged, mapping.gpu_mode);
    let manifest = exports
        .as_ref()
        .map(GpuExports::manifest)
        .unwrap_or_default();

    Some(PreparedGpu {
        exports,
        manifest,
        assignment,
    })
}

/// How much of what the host has was actually handed over.
///
/// HCS reports nothing about partiality -- it either accepted the update or it
/// did not -- so coverage is the only honest source of it. An adapter whose
/// driver package could not be exported is attached to a guest that cannot
/// mount its driver, and a missing payload is a guest with no userspace to
/// render with. Both are a VM that runs with less GPU than it asked for.
pub(crate) fn coverage(
    adapters: &[HostGpuAdapter],
    exports: Option<&GpuExports>,
    payload_staged: bool,
    mode: GpuMode,
) -> GpuAssignment {
    if adapters.is_empty() {
        return GpuAssignment::Failed(GpuFailure::new(
            GpuStatusCode::HostNoAdapter,
            "this host presents no GPU partition adapter",
        ));
    }

    let packages = exports.map_or(0, |exports| {
        exports
            .iter()
            .filter(|export| matches!(export.share().role, GpuShareRole::DriverPackage { .. }))
            .count()
    });
    let detail = NativeGpuDetail {
        // Under `Default` HCS attaches the host's preferred adapter, and
        // naming the only adapter there is is the one case where a name is not
        // a guess about which one that was.
        adapter: (matches!(mode, GpuMode::Default) && adapters.len() == 1)
            .then(|| adapters[0].name.clone()),
        adapters: u32::try_from(adapters.len()).unwrap_or(u32::MAX),
    };

    let mut missing = Vec::new();
    if packages < adapters.len() {
        missing.push(format!(
            "a driver package was exported for {packages} of {} adapter(s)",
            adapters.len()
        ));
    }
    if !payload_staged {
        missing.push("the Linux GPU payload is not staged for this VM".to_owned());
    }

    if missing.is_empty() {
        return GpuAssignment::Complete(detail);
    }
    GpuAssignment::Partial {
        detail,
        reason: GpuFailure::new(GpuStatusCode::AssignmentPartial, missing.join("; ")),
    }
}

/// Stages the payload, and answers whether the VM has one.
///
/// A failure is logged and nothing more. The catalog compiled into this build
/// may have no entry for this guest at all -- today it has none for anyone --
/// and a VM whose guest cannot render is still a VM that runs.
fn stage(
    mapping: &VmComputeSystemMapping,
    vm_directory: &Path,
    executable_directory: &Path,
    cache_root: &Path,
    cancel: &AtomicBool,
) -> bool {
    let Some(target) = &mapping.guest_target else {
        log::info!(
            "VM \"{}\" was not built from a cloud image, so VMLord has no GPU payload to \
             stage for it",
            mapping.vm_name
        );
        return false;
    };

    match stage_for_vm(StageGpuPayloadRequest {
        executable_directory,
        cache_root,
        vm_directory,
        guest: target.selector(),
        // The staging of a start nobody is watching: the stages a person sees
        // are the GPU status, and a byte count inside one of them would be a
        // progress bar with nowhere to go.
        progress: &|_progress| {},
        cancel,
    }) {
        Ok(_staged) => true,
        Err(error) => {
            log::warn!(
                "no GPU payload was staged for VM \"{}\": {error}",
                mapping.vm_name
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use vmlord_core::{GpuAssignment, GpuMode, GpuShare, GpuStatusCode, HostGpuAdapter};

    use super::{coverage, prepare};
    use crate::{gpu_exports::GpuExports, metadata::VmComputeSystemMapping};

    fn adapter(name: &str, has_package: bool) -> HostGpuAdapter {
        HostGpuAdapter {
            name: name.into(),
            instance_id: format!("PCI\\{name}"),
            interface_path: format!("\\\\?\\{name}"),
            driver_store: has_package.then(|| PathBuf::from(format!("C:\\DriverStore\\{name}"))),
            service: None,
        }
    }

    fn exports_for(packages: &[&str], payload: bool) -> GpuExports {
        let mut shares = vec![(GpuShare::wsl_lib(), PathBuf::from("C:\\lxss\\lib"))];
        if payload {
            shares.push((GpuShare::payload(), PathBuf::from("C:\\vm\\gpu-payload")));
        }
        for package in packages {
            shares.push((
                GpuShare::driver_package(package).expect("a package name must become a share"),
                PathBuf::from(format!("C:\\DriverStore\\{package}")),
            ));
        }
        GpuExports::for_test(shares)
    }

    fn mapping_with(gpu_mode: GpuMode) -> VmComputeSystemMapping {
        VmComputeSystemMapping {
            vm_id: uuid::Uuid::from_u128(1),
            vm_name: "dev".into(),
            hcs_compute_system_id: "vmlord-dev".into(),
            disk_gb: 20,
            endpoint_id: None,
            network_mode: vmlord_core::NetworkMode::None,
            ssh: None,
            gpu_mode,
            // No guest target: a VM VMLord cannot pick a payload for is the
            // case every host runs today, because the shipped catalog is empty.
            guest_target: None,
        }
    }

    struct TemporaryDirectory(PathBuf);

    impl TemporaryDirectory {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "vmlord-gpu-prepare-{label}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("a temporary directory");
            Self(path)
        }
    }

    impl Drop for TemporaryDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_vm_that_asks_for_no_gpu_has_nothing_prepared_for_it() {
        let directory = TemporaryDirectory::new("none");

        let prepared = prepare(
            &mapping_with(GpuMode::None),
            &directory.0,
            &directory.0,
            &directory.0,
            &std::sync::atomic::AtomicBool::new(false),
        );

        assert!(
            prepared.is_none(),
            "a VM with no GPU is not a VM whose GPU has a state"
        );
    }

    #[test]
    fn a_vm_with_no_payload_to_stage_is_prepared_with_less_than_it_asked_for() {
        // The path every host takes today: the compiled catalog is empty, so
        // nothing is staged, and the VM still starts.
        let directory = TemporaryDirectory::new("no-payload");

        let prepared = prepare(
            &mapping_with(GpuMode::Default),
            &directory.0,
            &directory.0,
            &directory.0,
            &std::sync::atomic::AtomicBool::new(false),
        )
        .expect("a VM that asks for a GPU always has something prepared");

        assert!(
            !matches!(prepared.assignment, GpuAssignment::Complete(_)),
            "a guest with no payload has less GPU than it asked for: {:?}",
            prepared.assignment
        );
    }

    #[test]
    fn every_adapter_handed_over_with_its_payload_is_complete() {
        let exports = exports_for(&["nvidia"], true);

        let assignment = coverage(
            &[adapter("nvidia", true)],
            Some(&exports),
            true,
            GpuMode::Default,
        );

        let GpuAssignment::Complete(detail) = assignment else {
            panic!("nothing was missing: {assignment:?}");
        };
        assert_eq!(detail.adapters, 1);
        assert_eq!(detail.adapter.as_deref(), Some("nvidia"));
    }

    #[test]
    fn an_adapter_whose_driver_could_not_be_exported_is_partial() {
        let exports = exports_for(&["nvidia"], true);

        let assignment = coverage(
            &[adapter("nvidia", true), adapter("intel", false)],
            Some(&exports),
            true,
            GpuMode::Mirror,
        );

        let GpuAssignment::Partial { detail, reason } = assignment else {
            panic!("one of two adapters has no package: {assignment:?}");
        };
        assert_eq!(detail.adapters, 2);
        assert_eq!(reason.code, GpuStatusCode::AssignmentPartial);
        assert!(
            reason.message.contains("1 of 2"),
            "the reason has to say how much is missing: {}",
            reason.message
        );
    }

    #[test]
    fn a_missing_payload_is_partial_and_says_so_in_its_own_words() {
        let exports = exports_for(&["nvidia"], false);

        let assignment = coverage(
            &[adapter("nvidia", true)],
            Some(&exports),
            false,
            GpuMode::Default,
        );

        let GpuAssignment::Partial { reason, .. } = assignment else {
            panic!("the payload is what a guest renders with: {assignment:?}");
        };
        assert!(
            reason.message.contains("payload"),
            "the reason has to name what is missing: {}",
            reason.message
        );
    }

    #[test]
    fn a_host_with_no_adapter_has_failed_rather_than_partly_succeeded() {
        let assignment = coverage(&[], None, false, GpuMode::Default);

        let GpuAssignment::Failed(reason) = assignment else {
            panic!("there is no GPU here to be partly attached: {assignment:?}");
        };
        assert_eq!(reason.code, GpuStatusCode::HostNoAdapter);
    }

    #[test]
    fn an_adapter_with_nothing_exported_for_it_at_all_is_partial() {
        // The host has a GPU and could hand over none of it. Not `Failed`: the
        // adapters below are still attached, and the guest will see a device.
        let assignment = coverage(&[adapter("nvidia", true)], None, false, GpuMode::Default);

        assert!(
            matches!(assignment, GpuAssignment::Partial { .. }),
            "an attached adapter with no driver share is a degraded GPU: {assignment:?}"
        );
    }

    #[test]
    fn a_single_adapter_is_named_and_several_are_only_counted() {
        let exports = exports_for(&["nvidia", "intel"], true);

        let assignment = coverage(
            &[adapter("nvidia", true), adapter("intel", true)],
            Some(&exports),
            true,
            GpuMode::Mirror,
        );

        let GpuAssignment::Complete(detail) = assignment else {
            panic!("both adapters were handed over: {assignment:?}");
        };
        assert_eq!(detail.adapters, 2);
        assert_eq!(
            detail.adapter, None,
            "there is no single adapter to name under Mirror"
        );
    }
}
