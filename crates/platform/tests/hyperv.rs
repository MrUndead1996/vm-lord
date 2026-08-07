//! Windows-only integration tests for the HCS platform layer.
//!
//! Run with a Windows host where Hyper-V and the Host Compute Service are
//! installed and running. Set `VMLORD_TEST_VM_ID` to an existing disposable HCS
//! compute-system identifier, then execute:
//!
//! `cargo test -p vmlord-platform --test hyperv -- --ignored`

#![cfg(windows)]

use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use uuid::Uuid;
use vmlord_core::{GpuMode, NetworkMode, VmCreateRequest};
use vmlord_platform::{
    HcsClient, HcsOperation, HcsSystem, HcsSystemState, MetadataStore, ReconnectOutcome,
    VmComputeSystemMapping,
    VmCreationPipeline, VmDeletionPipeline, VmForceStopPipeline, VmShutdownPipeline,
    VmStartPipeline, list_known_vms, open_by_vm_id, open_by_vm_name, reconnect_known_vms,
};

// `GENERIC_ALL`; matches the legacy AppSandbox backend's `hcs_vm.c` usage and
// `vmlord_platform`'s own internal `HCS_ACCESS_ALL`. An earlier, unverified
// value here (`0x000F_FFFF`) was invalid and made `HcsOpenComputeSystem` fail
// with `E_INVALIDARG` rather than a meaningful not-found error.
const HCS_ACCESS_ALL: u32 = 0x1000_0000;

/// `HCS_E_INVALID_JSON`, as it appears in a `RepositoryError`'s message. HCS
/// reports it when `HcsShutDownComputeSystem` receives null options.
const HCS_E_INVALID_JSON: &str = "0x8037010D";

#[test]
#[ignore = "requires Windows with Hyper-V/HCS"]
fn initializes_when_host_compute_service_is_available() {
    let mut client = HcsClient::new();

    client
        .initialize()
        .expect("Host Compute Service should accept a service-properties query");

    assert!(client.is_initialized());
}

#[test]
#[ignore = "requires Windows with Hyper-V/HCS and VMLORD_TEST_VM_ID set to a disposable VM"]
fn opens_the_configured_hcs_compute_system() {
    let vm_id = std::env::var("VMLORD_TEST_VM_ID")
        .expect("VMLORD_TEST_VM_ID must identify a disposable HCS compute system");

    let _operation = HcsOperation::new();
    let _system = HcsSystem::open(&vm_id, HCS_ACCESS_ALL)
        .expect("configured HCS compute system should be openable");
}

/// Exercises TASK-28's create pipeline end to end against the real Host
/// Compute Service: creates a VHDX, writes the HCS configuration, grants
/// access, creates the compute system, then reopens it after every handle
/// held during `create` has closed.
///
/// The reopen is the regression check for `ShouldTerminateOnLastHandleClosed:
/// false` -- if HCS tore the system down when the creating process's handles
/// closed, reopening fails with `HCS_E_SYSTEM_NOT_FOUND`. The initial disk
/// creation is the regression check for `CREATE_VIRTUAL_DISK_VERSION_2`.
///
/// Run elevated with:
/// `cargo test -p vmlord-platform --test hyperv -- --ignored --exact creates_and_persists_a_compute_system_end_to_end --nocapture`
#[test]
#[ignore = "requires an elevated Windows host with Hyper-V/HCS enabled"]
fn creates_and_persists_a_compute_system_end_to_end() {
    let root = std::env::temp_dir().join(format!("vmlord-hcs-create-e2e-{}", std::process::id()));
    fs::create_dir_all(&root).expect("test root should be created");
    let image_path = root.join("installer.iso");
    fs::write(&image_path, b"placeholder installer media").expect("test image should be written");

    let request = VmCreateRequest {
        name: format!("vmlord-e2e-test-{}", std::process::id()),
        image_path: image_path.to_string_lossy().into_owned(),
        ram_mb: 512,
        disk_gb: 1,
        cpu_cores: 1,
        gpu_mode: GpuMode::None,
        network_mode: NetworkMode::None,
        username: "admin".into(),
        password: "not used by create".into(),
        ssh_enabled: false,
        ssh_deploy_key: false,
    };
    let store = MetadataStore::new(root.join("vm-mapping.json"));
    let vm_directory = root.join("vm");

    let mapping = VmCreationPipeline::production()
        .create(&store, &request, &vm_directory)
        .expect("VM creation should succeed on an elevated Hyper-V host");
    println!(
        "created HCS compute system \"{}\" for VM {}",
        mapping.hcs_compute_system_id, mapping.vm_id
    );

    let reopened = HcsSystem::open(&mapping.hcs_compute_system_id, HCS_ACCESS_ALL);

    // Best-effort cleanup regardless of the assertion below.
    if let Ok(system) = &reopened {
        let _ = system
            .terminate()
            .and_then(|operation| operation.wait_for_completion(Duration::from_secs(30)));
    }
    let _ = fs::remove_dir_all(&root);

    reopened.expect(
        "the compute system must still be open-able after create()'s handles closed \
         (ShouldTerminateOnLastHandleClosed must be false)",
    );
}

/// Exercises TASK-29's enumerate/open against the real Host Compute Service:
/// creates a VM, confirms it is reported by `list_known_vms`, then reopens it
/// by both VM id and VM name through the metadata mapping.
///
/// Run elevated with:
/// `cargo test -p vmlord-platform --test hyperv -- --ignored --exact enumerates_and_reopens_a_created_vm --nocapture`
#[test]
#[ignore = "requires an elevated Windows host with Hyper-V/HCS enabled"]
fn enumerates_and_reopens_a_created_vm() {
    let root =
        std::env::temp_dir().join(format!("vmlord-hcs-enumerate-e2e-{}", std::process::id()));
    fs::create_dir_all(&root).expect("test root should be created");
    let image_path = root.join("installer.iso");
    fs::write(&image_path, b"placeholder installer media").expect("test image should be written");

    let request = VmCreateRequest {
        name: format!("vmlord-e2e-enum-test-{}", std::process::id()),
        image_path: image_path.to_string_lossy().into_owned(),
        ram_mb: 512,
        disk_gb: 1,
        cpu_cores: 1,
        gpu_mode: GpuMode::None,
        network_mode: NetworkMode::None,
        username: "admin".into(),
        password: "not used by create".into(),
        ssh_enabled: false,
        ssh_deploy_key: false,
    };
    let store = MetadataStore::new(root.join("vm-mapping.json"));
    let vm_directory = root.join("vm");

    let mapping = VmCreationPipeline::production()
        .create(&store, &request, &vm_directory)
        .expect("VM creation should succeed on an elevated Hyper-V host");

    let mut client = HcsClient::new();
    client
        .initialize()
        .expect("Host Compute Service should be available");

    let known_vms =
        list_known_vms(&client, &store).expect("enumeration should succeed against a live host");
    let entry = known_vms
        .iter()
        .find(|vm| vm.mapping.vm_id == mapping.vm_id)
        .expect("the just-created VM must appear in the enumeration");
    assert!(
        entry.is_present(),
        "HCS must report the compute system just created"
    );
    assert_eq!(
        entry.state,
        Some(HcsSystemState::Created),
        "a VM that was created but never started must not look like a running one"
    );

    let by_id = open_by_vm_id(&store, mapping.vm_id, HCS_ACCESS_ALL);
    let by_name = open_by_vm_name(&store, &mapping.vm_name, HCS_ACCESS_ALL);

    // Best-effort cleanup regardless of the assertions below.
    if let Ok(system) = &by_id {
        let _ = system
            .terminate()
            .and_then(|operation| operation.wait_for_completion(Duration::from_secs(30)));
    }
    let _ = fs::remove_dir_all(&root);

    by_id.expect("the compute system must be open-able by VM id through the metadata mapping");
    by_name.expect("the compute system must be open-able by VM name through the metadata mapping");
}

/// Exercises TASK-30's start against the real Host Compute Service: creates a
/// VM, starts it, then terminates it again.
///
/// The start is the regression check for `HcsGrantVmAccess` -- without the
/// grants the pipeline issues before starting, Hyper-V opens the VHDX as the
/// VM's own security principal and the start fails with
/// `ERROR_ACCESS_DENIED`.
///
/// Set `VMLORD_TEST_IMAGE_PATH` to a real bootable ISO: HCS refuses to attach
/// a placeholder file as installer media.
///
/// Run elevated with:
/// `cargo test -p vmlord-platform --test hyperv -- --ignored --exact starts_a_created_vm --nocapture`
#[test]
#[ignore = "requires an elevated Windows host with Hyper-V/HCS and VMLORD_TEST_IMAGE_PATH set"]
fn starts_a_created_vm() {
    let image_path = std::env::var("VMLORD_TEST_IMAGE_PATH")
        .expect("VMLORD_TEST_IMAGE_PATH must point to a real ISO image");
    let root = std::env::temp_dir().join(format!("vmlord-hcs-start-e2e-{}", std::process::id()));
    fs::create_dir_all(&root).expect("test root should be created");

    let request = VmCreateRequest {
        name: format!("vmlord-e2e-start-test-{}", std::process::id()),
        image_path,
        ram_mb: 2048,
        disk_gb: 8,
        cpu_cores: 2,
        gpu_mode: GpuMode::None,
        network_mode: NetworkMode::None,
        username: "admin".into(),
        password: "not used by start".into(),
        ssh_enabled: false,
        ssh_deploy_key: false,
    };
    let store = MetadataStore::new(root.join("vm-mapping.json"));
    let vm_directory = root.join("vm");

    let mapping = VmCreationPipeline::production()
        .create(&store, &request, &vm_directory)
        .expect("VM creation should succeed on an elevated Hyper-V host");

    let started = VmStartPipeline::production().start(&store, &mapping.vm_name, &vm_directory);

    // Best-effort cleanup regardless of the assertion below.
    if let Ok(system) = HcsSystem::open(&mapping.hcs_compute_system_id, HCS_ACCESS_ALL) {
        let _ = system
            .terminate()
            .and_then(|operation| operation.wait_for_completion(Duration::from_secs(30)));
    }
    let _ = fs::remove_dir_all(&root);

    started
        .expect("the created VM must start (HcsGrantVmAccess must precede HcsStartComputeSystem)");
}

/// Exercises TASK-25's forced stop against the real Host Compute Service:
/// creates a VM, starts it, terminates it, and starts it a second time.
///
/// The second start is the actual assertion, and it is the regression check
/// for what a first run of this test established on a live host: HCS destroys
/// a compute system when it exits, so after a forced stop reopening it fails
/// with `HCS_E_SYSTEM_NOT_FOUND` (0x8037010E). A forced stop must nevertheless
/// leave the VM itself intact, which is why `VmStartPipeline` re-creates the
/// compute system from the stored `config.json` rather than only opening it.
/// Were it not to, a forced stop would silently be a delete.
///
/// A forced stop needs nothing from the guest, so installer media is enough
/// here: set `VMLORD_TEST_IMAGE_PATH` to a real bootable ISO.
///
/// Run elevated with:
/// `cargo test -p vmlord-platform --test hyperv -- --ignored --exact force_stopped_vm_can_be_started_again --nocapture`
#[test]
#[ignore = "requires an elevated Windows host with Hyper-V/HCS and VMLORD_TEST_IMAGE_PATH set"]
fn force_stopped_vm_can_be_started_again() {
    let image_path = std::env::var("VMLORD_TEST_IMAGE_PATH")
        .expect("VMLORD_TEST_IMAGE_PATH must point to a real ISO image");
    let root =
        std::env::temp_dir().join(format!("vmlord-hcs-force-stop-e2e-{}", std::process::id()));
    fs::create_dir_all(&root).expect("test root should be created");

    let request = VmCreateRequest {
        name: format!("vmlord-e2e-force-stop-test-{}", std::process::id()),
        image_path,
        ram_mb: 2048,
        disk_gb: 8,
        cpu_cores: 2,
        gpu_mode: GpuMode::None,
        network_mode: NetworkMode::None,
        username: "admin".into(),
        password: "not used by a forced stop".into(),
        ssh_enabled: false,
        ssh_deploy_key: false,
    };
    let store = MetadataStore::new(root.join("vm-mapping.json"));
    let vm_directory = root.join("vm");

    let mapping = VmCreationPipeline::production()
        .create(&store, &request, &vm_directory)
        .expect("VM creation should succeed on an elevated Hyper-V host");
    VmStartPipeline::production()
        .start(&store, &mapping.vm_name, &vm_directory)
        .expect("the created VM must start before it can be forcibly stopped");

    let force_stopped = VmForceStopPipeline::production().force_stop(&store, &mapping.vm_name);
    let restarted = force_stopped
        .as_ref()
        .ok()
        .map(|()| VmStartPipeline::production().start(&store, &mapping.vm_name, &vm_directory));

    // Best-effort cleanup regardless of the assertions below: a successful
    // restart leaves the VM running again.
    let _ = VmForceStopPipeline::production().force_stop(&store, &mapping.vm_name);
    let _ = fs::remove_dir_all(&root);

    force_stopped.expect("a running VM must accept a forced stop");
    restarted
        .expect("the restart must have been attempted")
        .expect(
            "a forcibly stopped VM must stay start-able; HCS_E_SYSTEM_NOT_FOUND \
             (0x8037010E) here means the start did not re-create the compute \
             system HCS destroyed when it terminated",
        );
}

/// Exercises TASK-31's options document against the real Host Compute
/// Service: creates a VM, starts it, then asks its guest to shut down.
///
/// This is the regression check for the options document alone --
/// `HcsShutDownComputeSystem` rejects a null options pointer with
/// `HCS_E_INVALID_JSON`, so the pipeline must pass `"{}"`. It deliberately
/// does not assert that the shutdown succeeds: this VM boots installer media,
/// so it runs no guest OS that could service the request, and HCS reports
/// `ERROR_NOT_SUPPORTED`. `shuts_down_a_running_guest` is the test that
/// asserts a shutdown actually works.
///
/// Set `VMLORD_TEST_IMAGE_PATH` to a real bootable ISO.
///
/// Run elevated with:
/// `cargo test -p vmlord-platform --test hyperv -- --ignored --exact accepts_the_shutdown_options_document --nocapture`
#[test]
#[ignore = "requires an elevated Windows host with Hyper-V/HCS and VMLORD_TEST_IMAGE_PATH set"]
fn accepts_the_shutdown_options_document() {
    let image_path = std::env::var("VMLORD_TEST_IMAGE_PATH")
        .expect("VMLORD_TEST_IMAGE_PATH must point to a real ISO image");
    let root = std::env::temp_dir().join(format!("vmlord-hcs-shutdown-e2e-{}", std::process::id()));
    fs::create_dir_all(&root).expect("test root should be created");

    let request = VmCreateRequest {
        name: format!("vmlord-e2e-shutdown-test-{}", std::process::id()),
        image_path,
        ram_mb: 2048,
        disk_gb: 8,
        cpu_cores: 2,
        gpu_mode: GpuMode::None,
        network_mode: NetworkMode::None,
        username: "admin".into(),
        password: "not used by shutdown".into(),
        ssh_enabled: false,
        ssh_deploy_key: false,
    };
    let store = MetadataStore::new(root.join("vm-mapping.json"));
    let vm_directory = root.join("vm");

    let mapping = VmCreationPipeline::production()
        .create(&store, &request, &vm_directory)
        .expect("VM creation should succeed on an elevated Hyper-V host");
    VmStartPipeline::production()
        .start(&store, &mapping.vm_name, &vm_directory)
        .expect("the created VM must start before it can be shut down");

    let shut_down = VmShutdownPipeline::production().shutdown(&store, &mapping.vm_name);

    // Best-effort cleanup regardless of the assertion below: a guest that
    // ignores the request is still running here.
    if let Ok(system) = HcsSystem::open(&mapping.hcs_compute_system_id, HCS_ACCESS_ALL) {
        let _ = system
            .terminate()
            .and_then(|operation| operation.wait_for_completion(Duration::from_secs(30)));
    }
    let _ = fs::remove_dir_all(&root);

    if let Err(error) = shut_down {
        println!("shutdown of a guest-less VM reported: {error}");
        assert!(
            !error.to_string().contains(HCS_E_INVALID_JSON),
            "HCS rejected the shutdown options document as invalid JSON; \
             HcsShutDownComputeSystem must receive non-null JSON options: {error}"
        );
    }
}

/// Exercises TASK-34's reconnect against the real Host Compute Service:
/// creates a VM, reconnects to it from the metadata mapping alone -- every
/// handle `create` held is closed by then, exactly as after a VMLord restart
/// -- then terminates it and reconnects a second time.
///
/// The second reconnect is what pins the contract for a VM HCS no longer
/// reports: it must be reported `Absent` and keep its mapping, because a
/// stopped VM looks the same and dropping the mapping would delete it.
///
/// Run elevated with:
/// `cargo test -p vmlord-platform --test hyperv -- --ignored --exact reconnects_to_a_created_vm --nocapture`
#[test]
#[ignore = "requires an elevated Windows host with Hyper-V/HCS enabled"]
fn reconnects_to_a_created_vm() {
    let root =
        std::env::temp_dir().join(format!("vmlord-hcs-reconnect-e2e-{}", std::process::id()));
    fs::create_dir_all(&root).expect("test root should be created");
    let image_path = root.join("installer.iso");
    fs::write(&image_path, b"placeholder installer media").expect("test image should be written");

    let request = VmCreateRequest {
        name: format!("vmlord-e2e-reconnect-test-{}", std::process::id()),
        image_path: image_path.to_string_lossy().into_owned(),
        ram_mb: 512,
        disk_gb: 1,
        cpu_cores: 1,
        gpu_mode: GpuMode::None,
        network_mode: NetworkMode::None,
        username: "admin".into(),
        password: "not used by reconnect".into(),
        ssh_enabled: false,
        ssh_deploy_key: false,
    };
    let store = MetadataStore::new(root.join("vm-mapping.json"));
    let vm_directory = root.join("vm");

    let mapping = VmCreationPipeline::production()
        .create(&store, &request, &vm_directory)
        .expect("VM creation should succeed on an elevated Hyper-V host");

    let first = reconnect_known_vms(&store);
    let first_report = first.as_ref().ok().map(|report| {
        (
            report.connections.is_connected(mapping.vm_id),
            report.outcomes.len(),
            report.outcomes.first().map(|vm| vm.outcome.clone()),
        )
    });
    // Every handle the reconnect took must be closed before the compute system
    // is terminated, so the second reconnect observes HCS's view of the VM
    // rather than this test's.
    drop(first);

    if let Ok(system) = HcsSystem::open(&mapping.hcs_compute_system_id, HCS_ACCESS_ALL) {
        let _ = system
            .terminate()
            .and_then(|operation| operation.wait_for_completion(Duration::from_secs(30)));
    }

    let second = reconnect_known_vms(&store);
    let stored = store.find_by_vm_id(mapping.vm_id);
    let _ = fs::remove_dir_all(&root);

    assert_eq!(
        first_report,
        Some((true, 1, Some(ReconnectOutcome::Reconnected))),
        "a reconnect must hold an open handle to every VM HCS still reports"
    );
    let report = second.expect("a reconnect must succeed even when HCS knows none of the VMs");
    assert_eq!(report.outcomes[0].outcome, ReconnectOutcome::Absent);
    assert!(
        report.connections.is_empty(),
        "no handle may be held for a compute system HCS no longer reports"
    );
    assert_eq!(
        stored.unwrap().as_ref(),
        Some(&report.outcomes[0].mapping),
        "an absent VM must keep its mapping; a stopped VM is absent too"
    );
}

/// Asserts that a running VM survives a VMLord restart and stays manageable:
/// creates and starts a VM, reconnects to it from the metadata mapping, then
/// forcibly stops it through the reconnected VMLord's own pipeline.
///
/// This is the acceptance check TASK-34 calls for. The reconnect is what makes
/// a restarted VMLord the VM's owner again; the forced stop afterwards is what
/// proves the VM is still controllable rather than merely visible.
///
/// Set `VMLORD_TEST_IMAGE_PATH` to a real bootable ISO: HCS refuses to attach
/// a placeholder file as installer media.
///
/// Run elevated with:
/// `cargo test -p vmlord-platform --test hyperv -- --ignored --exact reconnects_to_a_running_vm --nocapture`
#[test]
#[ignore = "requires an elevated Windows host with Hyper-V/HCS and VMLORD_TEST_IMAGE_PATH set"]
fn reconnects_to_a_running_vm() {
    let image_path = std::env::var("VMLORD_TEST_IMAGE_PATH")
        .expect("VMLORD_TEST_IMAGE_PATH must point to a real ISO image");
    let root = std::env::temp_dir().join(format!(
        "vmlord-hcs-reconnect-running-e2e-{}",
        std::process::id()
    ));
    fs::create_dir_all(&root).expect("test root should be created");

    let request = VmCreateRequest {
        name: format!("vmlord-e2e-reconnect-running-test-{}", std::process::id()),
        image_path,
        ram_mb: 2048,
        disk_gb: 8,
        cpu_cores: 2,
        gpu_mode: GpuMode::None,
        network_mode: NetworkMode::None,
        username: "admin".into(),
        password: "not used by reconnect".into(),
        ssh_enabled: false,
        ssh_deploy_key: false,
    };
    let store = MetadataStore::new(root.join("vm-mapping.json"));
    let vm_directory = root.join("vm");

    let mapping = VmCreationPipeline::production()
        .create(&store, &request, &vm_directory)
        .expect("VM creation should succeed on an elevated Hyper-V host");
    VmStartPipeline::production()
        .start(&store, &mapping.vm_name, &vm_directory)
        .expect("the created VM must start before a reconnect can be observed");

    let reconnected = reconnect_known_vms(&store);
    // The handle the reconnect holds must not stand in the way of the actions
    // a reconnected VMLord performs on the VM, so the forced stop is issued
    // while that handle is still open.
    let stopped = reconnected
        .as_ref()
        .ok()
        .map(|_report| VmForceStopPipeline::production().force_stop(&store, &mapping.vm_name));

    // Best-effort cleanup regardless of the assertions below.
    let _ = VmForceStopPipeline::production().force_stop(&store, &mapping.vm_name);
    let _ = fs::remove_dir_all(&root);

    let report = reconnected.expect("the reconnect must succeed against a live host");
    assert_eq!(
        report.outcomes[0].outcome,
        ReconnectOutcome::Reconnected,
        "a running VM must be reconnected after every creating handle has closed"
    );
    assert!(report.connections.is_connected(mapping.vm_id));
    stopped
        .expect("the forced stop must have been attempted")
        .expect("a reconnected VM must still be controllable");
}

/// Asserts that a graceful shutdown actually stops a VM whose guest OS is
/// running and able to service the request.
///
/// This is the test that distinguishes "this particular guest cannot service a
/// shutdown" from "HCS never delivers a shutdown to a plain Hyper-V VM": a
/// guest-less VM reports `ERROR_NOT_SUPPORTED`, and if a fully booted guest
/// reports it too, `HcsShutDownComputeSystem` is the wrong mechanism for
/// VMLord's VMs and graceful shutdown needs an in-guest agent instead (which
/// is how the legacy AppSandbox backend implemented it).
///
/// `VmCreationPipeline` always formats a fresh empty disk and attaches an
/// installer ISO, so it cannot produce a VM with a booted guest. This test
/// therefore assembles the compute system directly from an existing VHDX:
///
/// * `VMLORD_TEST_VHDX_PATH` -- a VHDX with an installed, UEFI-bootable guest.
///   It is copied first, so the original is never written to or shut down.
/// * `VMLORD_TEST_BOOT_SECONDS` -- how long to wait for the guest to boot far
///   enough to run its shutdown integration service (default 90). On Linux
///   that service is the kernel's `hv_utils` module.
///
/// Run elevated with:
/// `cargo test -p vmlord-platform --test hyperv -- --ignored --exact shuts_down_a_running_guest --nocapture`
#[test]
#[ignore = "requires an elevated Windows host and a VHDX with an installed guest OS"]
fn shuts_down_a_running_guest() {
    let source_vhdx = PathBuf::from(
        std::env::var("VMLORD_TEST_VHDX_PATH")
            .expect("VMLORD_TEST_VHDX_PATH must point to a VHDX with an installed guest OS"),
    );
    let boot_wait = Duration::from_secs(
        std::env::var("VMLORD_TEST_BOOT_SECONDS")
            .ok()
            .and_then(|seconds| seconds.parse().ok())
            .unwrap_or(90),
    );

    let root = std::env::temp_dir().join(format!("vmlord-hcs-guest-e2e-{}", std::process::id()));
    fs::create_dir_all(&root).expect("test root should be created");
    // A booted guest writes to its disk and this test powers it off, so the
    // VHDX under test is always a throwaway copy.
    let disk_path = root.join("system.vhdx");
    println!(
        "copying {} to {}",
        source_vhdx.display(),
        disk_path.display()
    );
    fs::copy(&source_vhdx, &disk_path).expect("the source VHDX should be copyable");

    let vm_id = Uuid::new_v4();
    let hcs_id = format!("vmlord-{}", vm_id.as_simple());
    let store = MetadataStore::new(root.join("vm-mapping.json"));
    store
        .insert(VmComputeSystemMapping {
            vm_id,
            vm_name: "guest-shutdown-probe".into(),
            hcs_compute_system_id: hcs_id.clone(),
            disk_gb: 20,
        })
        .expect("mapping should be persisted");

    let result = boot_and_shut_down(&hcs_id, &disk_path, &store, boot_wait);

    // Best-effort cleanup regardless of the assertion below: a guest that
    // ignored the request is still running here.
    if let Ok(system) = HcsSystem::open(&hcs_id, HCS_ACCESS_ALL) {
        let _ = system
            .terminate()
            .and_then(|operation| operation.wait_for_completion(Duration::from_secs(30)));
    }
    let _ = fs::remove_dir_all(&root);

    result.expect(
        "a running guest must accept a graceful shutdown; ERROR_NOT_SUPPORTED here means \
         HcsShutDownComputeSystem cannot shut down VMLord's VMs at all",
    );
}

/// Creates, starts and gracefully shuts down a compute system booting `disk_path`.
///
/// The configuration mirrors `HcsVmConfigBuilder`'s, minus the installer ISO
/// that builder always attaches; that builder is crate-private, so this test
/// spells the document out.
fn boot_and_shut_down(
    hcs_id: &str,
    disk_path: &Path,
    store: &MetadataStore,
    boot_wait: Duration,
) -> Result<(), vmlord_core::RepositoryError> {
    let configuration = serde_json::json!({
        "SchemaVersion": { "Major": 2, "Minor": 1 },
        "Owner": "VMLord",
        "ShouldTerminateOnLastHandleClosed": false,
        "VirtualMachine": {
            "Chipset": { "Uefi": { "Console": "Default" } },
            "ComputeTopology": {
                "Memory": {
                    "SizeInMB": 2048,
                    "AllowOvercommit": true,
                    "EnableDeferredCommit": true,
                    "EnableColdDiscardHint": true
                },
                "Processor": { "Count": 2 }
            },
            "Devices": {
                "Scsi": { "Primary": { "Attachments": {
                    "0": { "Type": "VirtualDisk", "Path": disk_path }
                }}},
                "HvSocket": { "HvSocketConfig": { "ServiceTable": {} } },
                "Keyboard": {},
                "Mouse": {}
            }
        }
    })
    .to_string();

    let client = HcsClient::new();
    // Hyper-V opens the disk as the VM's own security principal, so the grant
    // must precede both create and start.
    client.grant_vm_access(hcs_id, disk_path)?;
    let (system, creation) = client.create_system(hcs_id, &configuration)?;
    creation.wait_for_completion(Duration::from_secs(30))?;

    system
        .start()?
        .wait_for_completion(Duration::from_secs(60))?;
    println!(
        "started \"{hcs_id}\"; waiting {}s for the guest to boot",
        boot_wait.as_secs()
    );
    std::thread::sleep(boot_wait);

    VmShutdownPipeline::production().shutdown(store, "guest-shutdown-probe")
}

/// Exercises TASK-32's deletion against the real Host Compute Service: creates
/// a VM, deletes it, and confirms nothing it was made of is left behind --
/// neither the compute system, nor its directory, nor its metadata mapping.
///
/// Run elevated with:
/// `cargo test -p vmlord-platform --test hyperv -- --ignored --exact deletes_a_created_vm_completely --nocapture`
#[test]
#[ignore = "requires an elevated Windows host with Hyper-V/HCS enabled"]
fn deletes_a_created_vm_completely() {
    let root = std::env::temp_dir().join(format!("vmlord-hcs-delete-e2e-{}", std::process::id()));
    fs::create_dir_all(&root).expect("test root should be created");
    let image_path = root.join("installer.iso");
    fs::write(&image_path, b"placeholder installer media").expect("test image should be written");

    let request = VmCreateRequest {
        name: format!("vmlord-e2e-delete-test-{}", std::process::id()),
        image_path: image_path.to_string_lossy().into_owned(),
        ram_mb: 512,
        disk_gb: 1,
        cpu_cores: 1,
        gpu_mode: GpuMode::None,
        network_mode: NetworkMode::None,
        username: "admin".into(),
        password: "not used by create".into(),
        ssh_enabled: false,
        ssh_deploy_key: false,
    };
    let store = MetadataStore::new(root.join("vm-mapping.json"));
    let vm_directory = root.join("vm");

    let mapping = VmCreationPipeline::production()
        .create(&store, &request, &vm_directory)
        .expect("VM creation should succeed on an elevated Hyper-V host");
    println!(
        "created HCS compute system \"{}\" for VM {}",
        mapping.hcs_compute_system_id, mapping.vm_id
    );

    let deleted = VmDeletionPipeline::production().delete(&store, &request.name, &vm_directory, true);

    // Best-effort cleanup regardless of the assertions below.
    let _ = fs::remove_dir_all(&root);

    deleted.expect("deletion should succeed on an elevated Hyper-V host");
    assert!(
        !vm_directory.exists(),
        "the VM directory must be gone once the VM is deleted"
    );
    assert!(
        store
            .find_by_vm_name(&request.name)
            .expect("the store should be readable")
            .is_none(),
        "a deleted VM must no longer be known to VMLord"
    );
    assert!(
        HcsSystem::open_if_present(&mapping.hcs_compute_system_id, HCS_ACCESS_ALL)
            .expect("HCS should answer whether it still knows the compute system")
            .is_none(),
        "HCS must no longer know the compute system of a deleted VM"
    );
}
