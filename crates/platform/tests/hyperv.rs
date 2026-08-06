//! Windows-only integration tests for the HCS platform layer.
//!
//! Run with a Windows host where Hyper-V and the Host Compute Service are
//! installed and running. Set `VMLORD_TEST_VM_ID` to an existing disposable HCS
//! compute-system identifier, then execute:
//!
//! `cargo test -p vmlord-platform --test hyperv -- --ignored`

#![cfg(windows)]

use std::{fs, time::Duration};

use vmlord_core::{GpuMode, NetworkMode, VmCreateRequest};
use vmlord_platform::{HcsClient, HcsOperation, HcsSystem, MetadataStore, VmCreationPipeline};

const HCS_ACCESS_ALL: u32 = 0x000F_FFFF;

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
    let root =
        std::env::temp_dir().join(format!("vmlord-hcs-create-e2e-{}", std::process::id()));
    fs::create_dir_all(&root).expect("test root should be created");
    let image_path = root.join("installer.iso");
    fs::write(&image_path, b"placeholder installer media")
        .expect("test image should be written");

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
