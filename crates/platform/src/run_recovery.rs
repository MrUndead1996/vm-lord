//! Rebuilding the per-process payload offers of a VM this process did not start.

use std::{
    fs,
    path::{Path, PathBuf},
};

use vmlord_core::{DisplayShare, GpuMode, GpuShareManifest};

use crate::{
    display_exports,
    display_runs::DisplayRuns,
    gpu_enumerate::partition_adapters,
    gpu_exports::GpuExports,
    gpu_runs::GpuRuns,
    layout::{display_payload_active_directory, gpu_payload_staging_directory},
    metadata::VmComputeSystemMapping,
    start::canonicalize_for_export,
};

/// Rebuilds offers from payload directories that survived the VMLord process.
/// It never stages, publishes, grants or changes the running compute system.
pub(crate) fn restore(
    mapping: &VmComputeSystemMapping,
    vm_directory: &Path,
    gpu_runs: &GpuRuns,
    display_runs: &DisplayRuns,
) {
    restore_with(
        mapping,
        vm_directory,
        gpu_runs,
        display_runs,
        |payload| {
            let adapters = partition_adapters().unwrap_or_else(|error| {
                tracing::warn!(
                    "cannot recover the GPU shares of VM \"{}\": {error}",
                    mapping.vm_name
                );
                Vec::new()
            });
            GpuExports::build(&adapters, vm_directory, payload).map(|exports| exports.manifest())
        },
        |active| {
            display_exports::build(
                vm_directory,
                active.join("payload.json").is_file().then_some(active),
                &canonicalize_for_export,
            )
            .map(|export| export.share().clone())
        },
    );
}

/// Restores the shares an already-running VM was built with into this process's
/// run registries before its first agent session is accepted.
pub(crate) fn restore_with(
    mapping: &VmComputeSystemMapping,
    vm_directory: &Path,
    gpu_runs: &GpuRuns,
    display_runs: &DisplayRuns,
    gpu_manifest: impl FnOnce(Option<&Path>) -> Option<GpuShareManifest>,
    display_share: impl FnOnce(&Path) -> Option<DisplayShare>,
) {
    if mapping.gpu_mode != GpuMode::None {
        let generation = staged_gpu_generation(vm_directory);
        if let Some(manifest) = gpu_manifest(generation.as_deref()) {
            gpu_runs.record_shares(mapping.vm_id, manifest);
        }
    }

    if mapping.desktop_profile.wants_desktop() {
        let active = display_payload_active_directory(vm_directory);
        if let Some(share) = display_share(&active) {
            display_runs.record_share(mapping.vm_id, share);
        }
    }
}

/// Finds a complete staged GPU generation. Its share name and role are stable
/// across generations; only the host path, already fixed in HCS, differs.
fn staged_gpu_generation(vm_directory: &Path) -> Option<PathBuf> {
    let generations = gpu_payload_staging_directory(vm_directory).join("generations");
    fs::read_dir(generations)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.join("payload.json").is_file())
        .max()
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use uuid::Uuid;
    use vmlord_core::{
        DesktopProfile, DisplayShare, GpuMode, GpuShare, GpuShareManifest, NetworkMode,
    };

    use super::{restore, restore_with};
    use crate::{display_runs::DisplayRuns, gpu_runs::GpuRuns, metadata::VmComputeSystemMapping};

    struct TempRoot(PathBuf);

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn mapping(vm_id: Uuid) -> VmComputeSystemMapping {
        VmComputeSystemMapping {
            vm_id,
            vm_name: "desktop".into(),
            hcs_compute_system_id: "vmlord-desktop".into(),
            disk_gb: 20,
            endpoint_id: None,
            network_mode: NetworkMode::None,
            ssh: None,
            ssh_daemon: None,
            gpu_mode: GpuMode::Default,
            desktop_profile: DesktopProfile::Gnome,
            display_provisioning: vmlord_core::DisplayProvisioning::Ready,
            display_mode: None,
            guest_target: None,
        }
    }

    #[test]
    fn production_recovery_accepts_an_already_published_display_payload() {
        let vm_id = Uuid::from_u128(2);
        let root = std::env::temp_dir().join(format!(
            "vmlord-display-run-recovery-{}-{vm_id}",
            std::process::id()
        ));
        let root = TempRoot(root);
        let active = root.0.join("display-payload/active");
        fs::create_dir_all(&active).unwrap();
        fs::write(active.join("payload.json"), b"published").unwrap();
        let mut mapping = mapping(vm_id);
        mapping.gpu_mode = GpuMode::None;
        let display_runs = DisplayRuns::default();

        restore(&mapping, &root.0, &GpuRuns::default(), &display_runs);

        assert_eq!(
            display_runs.share(vm_id).unwrap().name,
            vmlord_core::DISPLAY_PAYLOAD_SHARE
        );
    }

    #[test]
    fn a_reconnected_run_offers_its_staged_payloads_to_the_first_agent_session() {
        let vm_id = Uuid::from_u128(1);
        let root = std::env::temp_dir().join(format!(
            "vmlord-run-recovery-{}-{vm_id}",
            std::process::id()
        ));
        let root = TempRoot(root);
        let generation = root.0.join("gpu-payload/generations/abc");
        fs::create_dir_all(&generation).unwrap();
        fs::write(generation.join("payload.json"), b"complete").unwrap();
        let active = root.0.join("display-payload/active");
        fs::create_dir_all(&active).unwrap();

        let gpu_runs = GpuRuns::default();
        let display_runs = DisplayRuns::default();
        restore_with(
            &mapping(vm_id),
            &root.0,
            &gpu_runs,
            &display_runs,
            |payload| {
                assert_eq!(payload, Some(generation.as_path()));
                Some(GpuShareManifest {
                    shares: vec![GpuShare::payload()],
                })
            },
            |payload| {
                assert_eq!(payload, active);
                Some(DisplayShare {
                    name: vmlord_core::DISPLAY_PAYLOAD_SHARE.into(),
                })
            },
        );

        assert_eq!(
            gpu_runs.shares(vm_id).unwrap().shares,
            vec![GpuShare::payload()]
        );
        assert_eq!(
            display_runs.share(vm_id).unwrap().name,
            vmlord_core::DISPLAY_PAYLOAD_SHARE
        );
    }
}
