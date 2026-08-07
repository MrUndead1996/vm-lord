//! Windows-only integration tests for the HCS platform layer.
//!
//! Run with a Windows host where Hyper-V and the Host Compute Service are
//! installed and running. Set `VMLORD_TEST_VM_ID` to an existing disposable HCS
//! compute-system identifier, then execute:
//!
//! `cargo test -p vmlord-platform --test hyperv -- --ignored`

#![cfg(windows)]

use std::{fs, path::PathBuf, time::Duration};

use vmlord_core::{GpuMode, NetworkMode, VmCreateRequest};
use vmlord_platform::{
    HcsClient, HcsOperation, HcsSystem, MetadataStore, VmCreationPipeline, VmShutdownPipeline,
    VmStartPipeline, list_known_vms, open_by_vm_id, open_by_vm_name,
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
        entry.present,
        "HCS must report the compute system just created"
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

/// Asserts that a graceful shutdown actually stops a VM whose guest OS is
/// running and able to service the request.
///
/// Set `VMLORD_TEST_VM_ID` to the compute-system id of a *running, disposable*
/// VM with an installed guest OS, and `VMLORD_TEST_VM_NAME` to the VM name it
/// is mapped to in `VMLORD_TEST_MAPPING_PATH`'s metadata store.
///
/// This is the test that distinguishes "this particular guest cannot service a
/// shutdown" from "HCS never delivers a shutdown to a plain Hyper-V VM": a
/// guest-less VM reports `ERROR_NOT_SUPPORTED`, and if a fully booted guest
/// reports it too, `HcsShutDownComputeSystem` is the wrong mechanism for
/// VMLord's VMs and graceful shutdown needs an in-guest agent instead (which
/// is how the legacy AppSandbox backend implemented it).
///
/// Run elevated with:
/// `cargo test -p vmlord-platform --test hyperv -- --ignored --exact shuts_down_a_running_guest --nocapture`
#[test]
#[ignore = "requires an elevated Windows host and a running disposable VM with a guest OS"]
fn shuts_down_a_running_guest() {
    let vm_name = std::env::var("VMLORD_TEST_VM_NAME")
        .expect("VMLORD_TEST_VM_NAME must name a running disposable VM");
    let mapping_path = std::env::var("VMLORD_TEST_MAPPING_PATH")
        .expect("VMLORD_TEST_MAPPING_PATH must point to the metadata store mapping that VM");
    let store = MetadataStore::new(PathBuf::from(mapping_path));

    VmShutdownPipeline::production()
        .shutdown(&store, &vm_name)
        .expect(
            "a running guest must accept a graceful shutdown; ERROR_NOT_SUPPORTED here means \
             HcsShutDownComputeSystem cannot shut down VMLord's VMs at all",
        );
}
