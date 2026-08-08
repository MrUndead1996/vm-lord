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
    time::{Duration, Instant},
};

use uuid::Uuid;
use vmlord_core::{
    GpuMode, NetworkMode, VmCreateRequest, VmDeleteRequest, VmRepository, VmState, VmSummary,
};
use vmlord_platform::{
    EndpointAddress, HcnEndpoint, HcnNetwork, HcsClient, HcsOperation, HcsSystem, HcsSystemState,
    HcsVmRepository, MetadataStore, ReconnectOutcome, VMLORD_NETWORK_ID, VmComputeSystemMapping,
    VmCreationPipeline, VmDeletionPipeline, VmEventSink, VmForceStopPipeline, VmShutdownPipeline,
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

/// Exercises TASK-33's watch against the real Host Compute Service: creates a
/// VM and starts it through the repository -- which holds its handle and
/// registers its watch, exactly as production does -- then terminates it
/// behind the repository's back and waits for the resulting exit diagnostic.
///
/// This is the only check that proves HCS calls back into VMLord at all --
/// every other watch test exercises code that runs after the callback. The
/// termination goes through `VmForceStopPipeline` directly rather than
/// `HcsVmRepository::force_stop_vm`, because that method releases the
/// repository's own handle -- and clears its watch's HCS callback
/// registration -- synchronously on its own operation outcome, before HCS's
/// asynchronous exit notification could ever arrive. That is correct
/// production behaviour, not something to route around: a stop VMLord itself
/// commanded needs no event to explain it. The watcher exists for exits
/// VMLord did not command -- a guest powering itself off, with nothing to
/// tell the repository to release its handle -- and terminating behind the
/// repository's back is exactly that scenario. This test asserts only that
/// the resulting diagnostic surfaces, not that the handle is released:
/// releasing it is asserted by `watch::drain_events`'s own unit tests.
///
/// Set `VMLORD_TEST_IMAGE_PATH` to a real bootable ISO.
///
/// Run elevated with:
/// `cargo test -p vmlord-platform --test hyperv -- --ignored --exact a_terminated_vm_reports_its_exit --nocapture`
#[test]
#[ignore = "requires an elevated Windows host with Hyper-V/HCS and VMLORD_TEST_IMAGE_PATH set"]
fn a_terminated_vm_reports_its_exit() {
    let image_path = std::env::var("VMLORD_TEST_IMAGE_PATH")
        .expect("VMLORD_TEST_IMAGE_PATH must point to a real ISO image");
    let root = std::env::temp_dir().join(format!("vmlord-hcs-watch-e2e-{}", std::process::id()));
    fs::create_dir_all(&root).expect("test root should be created");

    let request = VmCreateRequest {
        name: format!("vmlord-e2e-watch-test-{}", std::process::id()),
        image_path,
        ram_mb: 2048,
        disk_gb: 8,
        cpu_cores: 2,
        gpu_mode: GpuMode::None,
        network_mode: NetworkMode::None,
        username: "admin".into(),
        password: "not used by a watch".into(),
        ssh_enabled: false,
        ssh_deploy_key: false,
    };
    let vm_name = request.name.clone();
    // The same mapping file `HcsVmRepository::new` builds its own store from,
    // so a termination issued through it is invisible to the repository.
    let store = MetadataStore::new(root.join("vm-mapping.json"));

    let mut repository = HcsVmRepository::new(root.clone());
    repository
        .initialize()
        .expect("the HCS backend should initialize on a live host");
    repository
        .create_vm(request)
        .expect("VM creation should succeed on an elevated Hyper-V host");
    repository
        .start_vm(&vm_name)
        .expect("the created VM must start before its exit can be watched");

    let terminated = VmForceStopPipeline::production().force_stop(&store, &vm_name);

    // HCS delivers the exit asynchronously, on a thread of its own, after the
    // termination operation has already completed.
    let expected = format!("VM \"{vm_name}\" stopped");
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut diagnostics = Vec::new();
    while Instant::now() < deadline {
        diagnostics.extend(repository.take_diagnostics());
        if diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == expected)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    let _ = fs::remove_dir_all(&root);

    terminated.expect("a running VM must accept a forced stop");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == expected),
        "HCS must report the exit of a terminated compute system as a diagnostic; got {diagnostics:?}"
    );
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

    let first = reconnect_known_vms(&store, &VmEventSink::default());
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

    let second = reconnect_known_vms(&store, &VmEventSink::default());
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

    let reconnected = reconnect_known_vms(&store, &VmEventSink::default());
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
            endpoint_id: None,
            network_mode: NetworkMode::None,
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

    let deleted =
        VmDeletionPipeline::production().delete(&store, &request.name, &vm_directory, true);
    let directory_survived = vm_directory.exists();
    let remaining_mapping = store.find_by_vm_name(&request.name);

    // Best-effort cleanup regardless of the assertions below.
    let _ = fs::remove_dir_all(&root);

    deleted.expect("deletion should succeed on an elevated Hyper-V host");
    assert!(
        !directory_survived,
        "the VM directory must be gone once the VM is deleted"
    );
    assert!(
        remaining_mapping
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

/// Exercises TASK-43's shared NAT network against the real Host Network
/// Service: creates the network from its constant identifier, confirms that a
/// second `ensure` opens the same one instead of failing, and -- only when the
/// host did not already have the network -- deletes it twice to confirm the
/// deletion is idempotent.
///
/// A network that already existed is left alone: it is the installation's
/// network, and endpoints of real VMs may live in it.
///
/// Run elevated with:
/// `cargo test -p vmlord-platform --test hyperv -- --ignored --exact ensures_the_shared_vmlord_nat_network --nocapture`
#[test]
#[ignore = "requires an elevated Windows host with Hyper-V/HNS enabled"]
fn ensures_the_shared_vmlord_nat_network() {
    let preexisting = HcnNetwork::open_if_present(VMLORD_NETWORK_ID)
        .expect("HNS should answer whether the VMLord network exists")
        .is_some();

    let created = HcnNetwork::ensure().expect("HNS should create the VMLord NAT network");
    drop(created);

    let reopened =
        HcnNetwork::ensure().expect("a second ensure must open the network, not fail on it");
    drop(reopened);

    HcnNetwork::open(VMLORD_NETWORK_ID)
        .expect("the network must be openable by its constant identifier");

    if preexisting {
        return;
    }

    HcnNetwork::delete(VMLORD_NETWORK_ID).expect("HNS should delete the VMLord NAT network");
    HcnNetwork::delete(VMLORD_NETWORK_ID)
        .expect("deleting a network HNS no longer has must succeed");
    assert!(
        HcnNetwork::open_if_present(VMLORD_NETWORK_ID)
            .expect("HNS should answer whether it still has the network")
            .is_none(),
        "HNS must no longer know a deleted network"
    );
}

/// Exercises TASK-41's per-VM endpoint against the real Host Network Service:
/// creates one in the shared NAT network, confirms that it can be reopened by
/// its identifier alone -- which is what makes a recorded `endpoint_id` worth
/// recording -- and that deleting it is idempotent.
///
/// The endpoint outlives its handle here on purpose: production creates it
/// during a start and closes the handle at the end of that start, then finds
/// it again on the next one. A test that only used the handle it was given
/// would never show that.
///
/// The shared network is left in place either way: it is the installation's
/// network, and endpoints of real VMs may live in it.
///
/// Run elevated with:
/// `cargo test -p vmlord-platform --test hyperv -- --ignored --exact creates_opens_and_deletes_a_vm_endpoint --nocapture`
#[test]
#[ignore = "requires an elevated Windows host with Hyper-V/HNS enabled"]
fn creates_opens_and_deletes_a_vm_endpoint() {
    let network = HcnNetwork::ensure().expect("HNS should provide the VMLord NAT network");
    let endpoint_id = Uuid::new_v4();

    let created = HcnEndpoint::create(&network, endpoint_id, "vmlord-endpoint-probe")
        .expect("HNS should create an endpoint in the VMLord NAT network");
    drop(created);

    assert!(
        HcnEndpoint::open_if_present(endpoint_id)
            .expect("HNS should answer whether it has the endpoint")
            .is_some(),
        "an endpoint must outlive the handle its creation returned"
    );
    HcnEndpoint::open(endpoint_id).expect("the endpoint must be openable by its identifier");

    HcnEndpoint::delete(endpoint_id).expect("HNS should delete the endpoint");
    HcnEndpoint::delete(endpoint_id).expect("deleting an endpoint HNS no longer has must succeed");
    assert!(
        HcnEndpoint::open_if_present(endpoint_id)
            .expect("HNS should answer whether it still has the endpoint")
            .is_none(),
        "HNS must no longer know a deleted endpoint"
    );
}

/// Exercises TASK-38's start-time networking against the real Host Network
/// Service and Host Compute Service: creates a NAT VM, starts it, and confirms
/// that the start gave it an endpoint, recorded it, and wrote the adapter into
/// the stored configuration.
///
/// The second start is what makes this more than a smoke test: the endpoint
/// must be reused rather than re-created, because a new one would hand the
/// guest a different address every time the VM came up.
///
/// Set `VMLORD_TEST_IMAGE_PATH` to a real bootable ISO.
///
/// Run elevated with:
/// `cargo test -p vmlord-platform --test hyperv -- --ignored --exact starts_a_nat_vm_on_its_endpoint --nocapture`
#[test]
#[ignore = "requires an elevated Windows host with Hyper-V/HCS, HNS and VMLORD_TEST_IMAGE_PATH set"]
fn starts_a_nat_vm_on_its_endpoint() {
    let image_path = std::env::var("VMLORD_TEST_IMAGE_PATH")
        .expect("VMLORD_TEST_IMAGE_PATH must point to a real ISO image");
    let root = std::env::temp_dir().join(format!("vmlord-hns-start-e2e-{}", std::process::id()));
    fs::create_dir_all(&root).expect("test root should be created");

    let request = VmCreateRequest {
        name: format!("vmlord-e2e-nat-test-{}", std::process::id()),
        image_path,
        ram_mb: 2048,
        disk_gb: 8,
        cpu_cores: 2,
        gpu_mode: GpuMode::None,
        network_mode: NetworkMode::Nat,
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
    assert_eq!(
        store
            .find_by_vm_name(&mapping.vm_name)
            .expect("the mapping should be readable")
            .expect("creation should register the VM")
            .network_mode,
        NetworkMode::Nat,
        "creation must record the NAT mode the request asked for"
    );

    let outcome = (|| -> Result<(), String> {
        VmStartPipeline::production()
            .start(&store, &mapping.vm_name, &vm_directory)
            .map_err(|error| format!("the NAT VM must start: {error}"))?;

        let endpoint_id = store
            .find_by_vm_name(&mapping.vm_name)
            .map_err(|error| error.to_string())?
            .and_then(|mapping| mapping.endpoint_id)
            .ok_or("the start must record the endpoint it created")?;

        let configuration: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(vm_directory.join("config.json"))
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        let adapters = configuration
            .pointer("/VirtualMachine/Devices/NetworkAdapters")
            .ok_or("the start must write the adapter into the stored configuration")?;
        if adapters
            .as_object()
            .is_none_or(|adapters| adapters.len() != 1)
        {
            return Err(format!(
                "expected exactly one network adapter, got {adapters}"
            ));
        }

        // A stop destroys the compute system; the endpoint must survive it.
        if let Ok(system) = HcsSystem::open(&mapping.hcs_compute_system_id, HCS_ACCESS_ALL) {
            let _ = system
                .terminate()
                .and_then(|operation| operation.wait_for_completion(Duration::from_secs(30)));
        }
        VmStartPipeline::production()
            .start(&store, &mapping.vm_name, &vm_directory)
            .map_err(|error| format!("the NAT VM must start a second time: {error}"))?;

        let reused = store
            .find_by_vm_name(&mapping.vm_name)
            .map_err(|error| error.to_string())?
            .and_then(|mapping| mapping.endpoint_id);
        if reused != Some(endpoint_id) {
            return Err(format!(
                "the second start must reuse endpoint {endpoint_id}, not {reused:?}"
            ));
        }

        Ok(())
    })();

    // Best-effort cleanup regardless of the assertion below. The shared network
    // is left in place: it belongs to the installation, not to this VM.
    if let Ok(system) = HcsSystem::open(&mapping.hcs_compute_system_id, HCS_ACCESS_ALL) {
        let _ = system
            .terminate()
            .and_then(|operation| operation.wait_for_completion(Duration::from_secs(30)));
    }
    if let Ok(Some(mapping)) = store.find_by_vm_name(&mapping.vm_name)
        && let Some(endpoint_id) = mapping.endpoint_id
    {
        let _ = HcnEndpoint::delete(endpoint_id);
    }
    let _ = fs::remove_dir_all(&root);

    outcome.expect("the NAT start path must work end to end");
}

/// Exercises TASK-46 against a real Hyper-V host: the cycle that used to fail.
///
/// Before this task a forced stop destroyed the compute system with the adapter
/// still attached, HNS kept the endpoint bound to it, and the next start failed
/// with `HCN_E_ENDPOINT_ALREADY_ATTACHED` (0x803B0014). The detach before the
/// termination is what makes the second start work; the endpoint and its
/// address surviving it is what keeps the guest reachable where it was.
///
/// The stop goes through `VmForceStopPipeline` rather than
/// `HcsSystem::terminate` -- unlike `starts_a_nat_vm_on_its_endpoint`, which
/// terminates behind the pipeline's back -- because the detach is the
/// pipeline's, and terminating around it is precisely the case this test exists
/// to distinguish.
///
/// Set `VMLORD_TEST_IMAGE_PATH` to a real bootable ISO.
///
/// Run elevated with:
/// `cargo test -p vmlord-platform --test hyperv -- --ignored --exact a_forcibly_stopped_nat_vm_starts_again_on_the_same_endpoint --nocapture`
#[test]
#[ignore = "requires an elevated Windows host with Hyper-V/HCS, HNS and VMLORD_TEST_IMAGE_PATH set"]
fn a_forcibly_stopped_nat_vm_starts_again_on_the_same_endpoint() {
    let image_path = std::env::var("VMLORD_TEST_IMAGE_PATH")
        .expect("VMLORD_TEST_IMAGE_PATH must point to a real ISO image");
    let root = std::env::temp_dir().join(format!("vmlord-hns-restart-e2e-{}", std::process::id()));
    fs::create_dir_all(&root).expect("test root should be created");

    let request = VmCreateRequest {
        name: format!("vmlord-e2e-restart-test-{}", std::process::id()),
        image_path,
        ram_mb: 2048,
        disk_gb: 8,
        cpu_cores: 2,
        gpu_mode: GpuMode::None,
        network_mode: NetworkMode::Nat,
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

    let outcome = (|| -> Result<(), String> {
        VmStartPipeline::production()
            .start(&store, &mapping.vm_name, &vm_directory)
            .map_err(|error| format!("the NAT VM must start: {error}"))?;

        let first = recorded_endpoint(&store, &mapping.vm_name)?;
        let first_address = endpoint_address(first)?;

        VmForceStopPipeline::production()
            .force_stop(&store, &mapping.vm_name)
            .map_err(|error| format!("a running VM must accept a forced stop: {error}"))?;

        VmStartPipeline::production()
            .start(&store, &mapping.vm_name, &vm_directory)
            .map_err(|error| {
                format!(
                    "a forcibly stopped NAT VM must start again; \
                     HCN_E_ENDPOINT_ALREADY_ATTACHED (0x803B0014) here means the adapter was \
                     not detached before the compute system was destroyed: {error}"
                )
            })?;

        let second = recorded_endpoint(&store, &mapping.vm_name)?;
        if second != first {
            return Err(format!(
                "a forced stop must leave endpoint {first} in place for the next start, \
                 which instead ran on {second}"
            ));
        }

        let second_address = endpoint_address(second)?;
        if second_address != first_address {
            return Err(format!(
                "the guest must keep the address it had before the forced stop: \
                 {first_address:?} became {second_address:?}"
            ));
        }

        Ok(())
    })();

    // Best-effort cleanup regardless of the assertion below. The shared network
    // is left in place: it belongs to the installation, not to this VM.
    if let Ok(system) = HcsSystem::open(&mapping.hcs_compute_system_id, HCS_ACCESS_ALL) {
        let _ = system
            .terminate()
            .and_then(|operation| operation.wait_for_completion(Duration::from_secs(30)));
    }
    if let Ok(Some(mapping)) = store.find_by_vm_name(&mapping.vm_name)
        && let Some(endpoint_id) = mapping.endpoint_id
    {
        let _ = HcnEndpoint::delete(endpoint_id);
    }
    let _ = fs::remove_dir_all(&root);

    outcome.expect("a forced stop must release the endpoint for the next start");
}

/// The endpoint the store records for `vm_name`, which a started NAT VM has.
fn recorded_endpoint(store: &MetadataStore, vm_name: &str) -> Result<Uuid, String> {
    store
        .find_by_vm_name(vm_name)
        .map_err(|error| error.to_string())?
        .and_then(|mapping| mapping.endpoint_id)
        .ok_or_else(|| "a started NAT VM must have a recorded endpoint".to_owned())
}

/// The address HNS reports for `endpoint`.
fn endpoint_address(endpoint: Uuid) -> Result<Option<EndpointAddress>, String> {
    HcnEndpoint::open(endpoint)
        .map_err(|error| format!("HNS should still have endpoint {endpoint}: {error}"))?
        .address()
        .map_err(|error| format!("HNS should report the properties of {endpoint}: {error}"))
}

/// Exercises TASK-47 against a real host: starting a NAT VM must bring the
/// DHCP server up, and the address the server was given must be the one HNS
/// assigned to the VM's endpoint.
///
/// What this test cannot assert is the guest's own view -- whether its
/// interface actually came up with that address. That is the manual parity
/// check, run with a VHDX carrying an installed Linux; installer media is
/// enough for everything asserted here.
///
/// Set `VMLORD_TEST_IMAGE_PATH` to a real bootable ISO.
///
/// Run elevated with:
/// `cargo test -p vmlord-platform --test hyperv -- --ignored --exact a_started_nat_vm_is_served_the_address_hns_assigned --nocapture`
#[test]
#[ignore = "requires an elevated Windows host with Hyper-V/HCS, HNS, a free UDP 67 and VMLORD_TEST_IMAGE_PATH set"]
fn a_started_nat_vm_is_served_the_address_hns_assigned() {
    let image_path = std::env::var("VMLORD_TEST_IMAGE_PATH")
        .expect("VMLORD_TEST_IMAGE_PATH must point to a real ISO image");
    let root = std::env::temp_dir().join(format!("vmlord-dhcp-e2e-{}", std::process::id()));
    fs::create_dir_all(&root).expect("test root should be created");

    let request = VmCreateRequest {
        name: format!("vmlord-e2e-dhcp-test-{}", std::process::id()),
        image_path,
        ram_mb: 2048,
        disk_gb: 8,
        cpu_cores: 2,
        gpu_mode: GpuMode::None,
        network_mode: NetworkMode::Nat,
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

    let outcome = (|| -> Result<(), String> {
        // The start fails rather than coming up silently without a network if
        // the DHCP server cannot bind UDP 67, so this is also the check that
        // nothing else on the host is already serving it.
        VmStartPipeline::production()
            .start(&store, &mapping.vm_name, &vm_directory)
            .map_err(|error| {
                format!("a NAT VM must start with its DHCP server running: {error}")
            })?;

        let endpoint = recorded_endpoint(&store, &mapping.vm_name)?;
        let address = endpoint_address(endpoint)?
            .ok_or("HNS must assign the endpoint an address for the guest to be served")?;

        if address.prefix_length == 0 {
            return Err(format!(
                "HNS reported a prefix of 0 for {}, which describes no subnet",
                address.ip_address
            ));
        }
        address
            .ip_address
            .parse::<std::net::Ipv4Addr>()
            .map_err(|error| format!("HNS reported \"{}\": {error}", address.ip_address))?;

        // A second start must not fail on the reservation the first one made.
        if let Ok(system) = HcsSystem::open(&mapping.hcs_compute_system_id, HCS_ACCESS_ALL) {
            let _ = system
                .terminate()
                .and_then(|operation| operation.wait_for_completion(Duration::from_secs(30)));
        }
        VmStartPipeline::production()
            .start(&store, &mapping.vm_name, &vm_directory)
            .map_err(|error| {
                format!("a second start must not trip over the existing reservation: {error}")
            })?;

        Ok(())
    })();

    if let Ok(system) = HcsSystem::open(&mapping.hcs_compute_system_id, HCS_ACCESS_ALL) {
        let _ = system
            .terminate()
            .and_then(|operation| operation.wait_for_completion(Duration::from_secs(30)));
    }
    if let Ok(endpoint) = recorded_endpoint(&store, &mapping.vm_name) {
        let _ = HcnEndpoint::delete(endpoint);
    }
    let _ = fs::remove_dir_all(&root);

    outcome.expect("a NAT VM must start with the DHCP server serving the address HNS assigned");
}

/// The summary `repository` lists for `vm_name`.
fn listed_summary(repository: &HcsVmRepository, vm_name: &str) -> Result<VmSummary, String> {
    repository
        .list_vms()
        .map_err(|error| format!("the repository should list its VMs: {error}"))?
        .into_iter()
        .find(|summary| summary.name == vm_name)
        .ok_or_else(|| format!("VM \"{vm_name}\" should be listed"))
}

/// Exercises TASK-37 against a real host: the address a listing reports for a
/// running NAT VM is the one HNS assigned to that VM's endpoint, and a stopped
/// VM is listed without one even though its endpoint kept its address.
///
/// What this test cannot assert is the guest's own view -- that its interface
/// really came up on that address once it accepted the lease. That is the
/// manual parity check, run with a VHDX carrying an installed Linux; installer
/// media is enough for everything asserted here.
///
/// Set `VMLORD_TEST_IMAGE_PATH` to a real bootable ISO.
///
/// Run elevated with:
/// `cargo test -p vmlord-platform --test hyperv -- --ignored --exact a_running_nat_vm_is_listed_with_the_address_hns_assigned --nocapture`
#[test]
#[ignore = "requires an elevated Windows host with Hyper-V/HCS, HNS, a free UDP 67 and VMLORD_TEST_IMAGE_PATH set"]
fn a_running_nat_vm_is_listed_with_the_address_hns_assigned() {
    let image_path = std::env::var("VMLORD_TEST_IMAGE_PATH")
        .expect("VMLORD_TEST_IMAGE_PATH must point to a real ISO image");
    let root = std::env::temp_dir().join(format!("vmlord-ip-e2e-{}", std::process::id()));
    fs::create_dir_all(&root).expect("test root should be created");

    let vm_name = format!("vmlord-e2e-ip-test-{}", std::process::id());
    let request = VmCreateRequest {
        name: vm_name.clone(),
        image_path,
        ram_mb: 2048,
        disk_gb: 8,
        cpu_cores: 2,
        gpu_mode: GpuMode::None,
        network_mode: NetworkMode::Nat,
        username: "admin".into(),
        password: "not used by start".into(),
        ssh_enabled: false,
        ssh_deploy_key: false,
    };

    let mut repository = HcsVmRepository::new(&root);
    repository
        .initialize()
        .expect("the native backend should initialize on a Hyper-V host");
    repository
        .create_vm(request)
        .expect("VM creation should succeed on an elevated Hyper-V host");
    // The repository keeps its mapping under its own storage root, and the
    // endpoint identifier has to be read from there to compare the listed
    // address against HNS's own answer.
    let store = MetadataStore::new(root.join("vm-mapping.json"));

    let outcome = (|| -> Result<(), String> {
        // A VM that has never started has no endpoint, so there is nothing to
        // report and nothing to ask HNS about.
        if let Some(address) = listed_summary(&repository, &vm_name)?.ip_address {
            return Err(format!(
                "a created VM must be listed without an address, got {address}"
            ));
        }

        repository
            .start_vm(&vm_name)
            .map_err(|error| format!("the NAT VM must start: {error}"))?;

        let endpoint = recorded_endpoint(&store, &vm_name)?;
        let assigned = endpoint_address(endpoint)?
            .ok_or("HNS must assign the endpoint an address for the guest to be served")?;

        let running = listed_summary(&repository, &vm_name)?;
        if !matches!(running.state, VmState::Running { .. }) {
            return Err(format!(
                "the started VM must be listed as running, got {:?}",
                running.state
            ));
        }
        let reported = running
            .ip_address
            .ok_or("a running NAT VM must be listed with the address of its endpoint")?;
        if reported.to_string() != assigned.ip_address {
            return Err(format!(
                "the listing reports {reported}, but HNS assigned {} to endpoint {endpoint}",
                assigned.ip_address
            ));
        }

        repository
            .force_stop_vm(&vm_name)
            .map_err(|error| format!("the VM must stop: {error}"))?;

        // The endpoint outlives the stop with its address intact, but the guest
        // is no longer there to answer at it.
        if endpoint_address(endpoint)?.as_ref() != Some(&assigned) {
            return Err(format!(
                "endpoint {endpoint} must keep {} across a stop",
                assigned.ip_address
            ));
        }
        if let Some(address) = listed_summary(&repository, &vm_name)?.ip_address {
            return Err(format!(
                "a stopped VM must be listed without an address, got {address}"
            ));
        }

        Ok(())
    })();

    // Best-effort cleanup regardless of the assertion below. Deletion does not
    // remove the endpoint yet (TASK-42), so it goes first, while the mapping
    // still names it.
    let _ = repository.force_stop_vm(&vm_name);
    let endpoint = recorded_endpoint(&store, &vm_name);
    let _ = repository.delete_vm(VmDeleteRequest {
        name: vm_name,
        delete_disks: true,
    });
    if let Ok(endpoint) = endpoint {
        let _ = HcnEndpoint::delete(endpoint);
    }
    let _ = fs::remove_dir_all(&root);

    outcome.expect("a running NAT VM must be listed with the address HNS assigned to its endpoint");
}
