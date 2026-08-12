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
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use uuid::Uuid;
use vmlord_core::{
    GpuMode, NetworkMode, VmCreateRequest, VmDeleteRequest, VmRepository, VmSource, VmState,
    VmSummary,
};
use vmlord_platform::{
    Com1Launcher, EndpointAddress, HcnEndpoint, HcnNetwork, HcsClient, HcsOperation,
    HcsStartFailure, HcsSystem, HcsSystemState, HcsVmRepository, MetadataStore, ReconnectOutcome,
    VMLORD_NETWORK_ID, VmComputeSystemMapping, VmCreationPipeline, VmDeletionPipeline, VmEventSink,
    VmForceStopPipeline, VmShutdownPipeline, VmStartPipeline, list_known_vms, open_by_vm_id,
    open_by_vm_name, reconnect_known_vms,
};

// `GENERIC_ALL`; matches the legacy AppSandbox backend's `hcs_vm.c` usage and
// `vmlord_platform`'s own internal `HCS_ACCESS_ALL`. An earlier, unverified
// value here (`0x000F_FFFF`) was invalid and made `HcsOpenComputeSystem` fail
// with `E_INVALIDARG` rather than a meaningful not-found error.
const HCS_ACCESS_ALL: u32 = 0x1000_0000;

/// `HCS_E_INVALID_JSON`, as it appears in a `RepositoryError`'s message. HCS
/// reports it when `HcsShutDownComputeSystem` receives null options.
const HCS_E_INVALID_JSON: &str = "0x8037010D";

/// A repository whose importer is the one the composition root builds.
fn cloud_repository(root: &std::path::Path) -> HcsVmRepository {
    let cache = root.join("cache");
    HcsVmRepository::new(
        root,
        Box::new(
            move |image: &vmlord_core::CloudImage,
                  size,
                  target: &std::path::Path,
                  monitor: &vmlord_core::BuildMonitor| {
                monitor.report(vmlord_core::BuildStep::Downloading);
                let mut source = vmlord_image::open_cloud_image(
                    &image.profile,
                    &image.release,
                    &cache,
                    size,
                    monitor.downloads(),
                    monitor.cancel_flag(),
                )?;
                monitor.report(vmlord_core::BuildStep::WritingDisk);
                vmlord_platform::import_image(&mut source, target, size, monitor.cancel_flag())
                    .map(|_| ())
            },
        ),
    )
}

fn background_cloud_request(name: &str) -> VmCreateRequest {
    VmCreateRequest {
        name: name.to_owned(),
        source: VmSource::CloudImage {
            image: vmlord_core::CloudImage {
                profile: vmlord_core::ubuntu(),
                release: "24.04".into(),
            },
            provisioning: vmlord_core::Provisioning {
                username: "dev".into(),
                password: None,
                ssh: vmlord_core::SshAccess::Enabled { deploy_key: true },
                locale: "en_US.UTF-8".into(),
                keyboard: "us".into(),
                timezone: "Europe/Moscow".into(),
            },
        },
        ram_mb: 2048,
        disk_gb: 16,
        cpu_cores: 2,
        gpu_mode: GpuMode::None,
        network_mode: NetworkMode::None,
    }
}

/// Creating a VM from a real cloud image must return at once and finish on its
/// own thread: this is the whole point of the task, and it cannot be observed
/// anywhere but on a host with HCS.
#[test]
#[ignore = "requires an elevated Windows host with Hyper-V/HCS and downloads a cloud image"]
fn a_cloud_vm_is_built_in_the_background() {
    let root = std::env::temp_dir().join(format!("vmlord-bg-build-{}", std::process::id()));
    fs::create_dir_all(&root).expect("test root should be created");
    let mut repository = cloud_repository(&root);
    repository
        .initialize()
        .expect("the native backend should initialize on a Hyper-V host");

    let accepted_at = std::time::Instant::now();
    repository
        .create_vm(background_cloud_request("bg-build"))
        .expect("the creation should be accepted");
    let accepted_in = accepted_at.elapsed();

    let outcome =
        wait_until_build_finishes(&mut repository, "bg-build", Duration::from_secs(20 * 60));
    let seen_building = outcome.as_ref().copied().unwrap_or(false);

    // Best-effort cleanup regardless of the assertions below.
    let _ = repository.delete_vm(VmDeleteRequest {
        name: "bg-build".into(),
        delete_disks: true,
    });
    drop(repository);
    let _ = fs::remove_dir_all(&root);

    outcome.expect("the VM should finish building");
    assert!(
        accepted_in < Duration::from_secs(2),
        "create_vm must not wait for the build, took {accepted_in:?}"
    );
    assert!(
        seen_building,
        "the VM must be listed as Building while it builds"
    );
}

/// A build now ends where the guest says it is ready, not where HCS accepted a
/// compute system: create a VM from a real cloud image and watch it become a
/// running VM with an address and a cloud-init that reported `done`.
///
/// Ignored by default -- it needs Hyper-V, an elevated process, a network and
/// the better part of ten minutes.
#[test]
#[ignore = "requires an elevated Windows host with Hyper-V/HCS and downloads a cloud image"]
fn a_created_vm_becomes_ready_before_its_build_finishes() {
    let root = std::env::temp_dir().join(format!("vmlord-readiness-{}", std::process::id()));
    fs::create_dir_all(&root).expect("test root should be created");
    let mut repository = cloud_repository(&root);
    repository
        .initialize()
        .expect("the native backend should initialize on a Hyper-V host");

    repository
        .create_vm(background_cloud_request("ready-vm"))
        .expect("the creation should be accepted");

    // Longer than the default cloud-init timeout: the point is to observe the
    // wait finishing, not to race it.
    let outcome =
        wait_until_build_finishes(&mut repository, "ready-vm", Duration::from_secs(30 * 60));
    let summary = repository
        .list_vms()
        .expect("listing should work")
        .into_iter()
        .find(|vm| vm.name == "ready-vm");
    let diagnostics = repository.take_diagnostics();
    let transcript = fs::read_to_string(root.join("ready-vm").join("cloud-init-status.log"));

    // Best-effort cleanup regardless of the assertions below.
    let _ = repository.force_stop_vm("ready-vm");
    let _ = repository.delete_vm(VmDeleteRequest {
        name: "ready-vm".into(),
        delete_disks: true,
    });
    drop(repository);
    let _ = fs::remove_dir_all(&root);

    outcome.expect("the VM should finish building");
    let summary = summary.expect("a built VM is in the list");
    assert!(
        matches!(summary.state, VmState::Running { .. }),
        "a build that waited for its guest leaves the VM running: {:?}",
        summary.state
    );
    assert!(
        summary.ip_address.is_some(),
        "a guest that answered SSH has an address"
    );
    let errors: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.level == vmlord_core::DiagnosticLevel::Error)
        .collect();
    assert!(errors.is_empty(), "the build reported errors: {errors:?}");
    let transcript = transcript.expect("the readiness wait writes a transcript");
    assert!(
        transcript.contains("status: done"),
        "cloud-init should have reported that it is done: {transcript}"
    );
}

/// Waits until VM `name` stops being listed as building, reporting whether it
/// was ever seen building on the way.
///
/// A build that disappears from the list failed and rolled itself back, so the
/// diagnostics it left are what the failure says.
fn wait_until_build_finishes(
    repository: &mut HcsVmRepository,
    name: &str,
    timeout: Duration,
) -> Result<bool, String> {
    let mut seen_building = false;
    let deadline = Instant::now() + timeout;
    loop {
        let summaries = repository.list_vms().expect("listing should work");
        match summaries.iter().find(|vm| vm.name == name) {
            Some(vm) if matches!(vm.state, VmState::Building { .. }) => seen_building = true,
            Some(_) => return Ok(seen_building),
            None => {
                return Err(format!(
                    "the build failed: {:?}",
                    repository.take_diagnostics()
                ));
            }
        }
        if Instant::now() >= deadline {
            return Err("the build did not finish".to_owned());
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// Waits for cloud-init to appear in a VM's captured serial output.
fn wait_for_cloud_init_log(path: &Path, timeout: Duration) -> Result<String, String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(bytes) = fs::read(path) {
            // Lossy on purpose: a serial console carries bytes, and this only
            // has to find a word in them.
            let text = String::from_utf8_lossy(&bytes);
            if text.to_ascii_lowercase().contains("cloud-init") {
                return Ok(text.into_owned());
            }
        }
        if Instant::now() >= deadline {
            return Err(format!("cloud-init did not appear in {}", path.display()));
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// What COM1 is for: an Ubuntu cloud image says what it is doing on its serial
/// port long before SSH exists, and that output has to reach `com1.log`.
#[test]
#[ignore = "requires an elevated Windows host with Hyper-V/HCS and downloads a cloud image"]
fn ubuntu_cloud_init_is_visible_on_com1() {
    let root = std::env::temp_dir().join(format!("vmlord-com1-cloud-init-{}", std::process::id()));
    fs::create_dir_all(&root).expect("test root should be created");
    let mut repository = cloud_repository(&root);
    repository
        .initialize()
        .expect("the native backend should initialize on a Hyper-V host");
    let name = "com1-cloud-init";

    repository
        .create_vm(background_cloud_request(name))
        .expect("the creation should be accepted");
    wait_until_build_finishes(&mut repository, name, Duration::from_secs(20 * 60))
        .expect("the VM should finish building");
    repository
        .start_vm(name)
        .expect("the built VM should start");

    let result = wait_for_cloud_init_log(
        &root.join(name).join("com1.log"),
        Duration::from_secs(10 * 60),
    );

    // Cleanup after the capture and before the assertion: a VM left running is
    // worse than a failed test.
    let _ = repository.force_stop_vm(name);
    let _ = repository.delete_vm(VmDeleteRequest {
        name: name.into(),
        delete_disks: true,
    });
    drop(repository);
    let _ = fs::remove_dir_all(&root);

    result.expect("Ubuntu cloud-init output should reach COM1");
}

/// A cancelled build leaves nothing: not the directory, not a metadata entry,
/// not a row in the list.
#[test]
#[ignore = "requires an elevated Windows host with Hyper-V/HCS and downloads a cloud image"]
fn a_cancelled_build_leaves_nothing_behind() {
    let root = std::env::temp_dir().join(format!("vmlord-bg-cancel-{}", std::process::id()));
    fs::create_dir_all(&root).expect("test root should be created");
    let mut repository = cloud_repository(&root);
    repository
        .initialize()
        .expect("the native backend should initialize on a Hyper-V host");

    repository
        .create_vm(background_cloud_request("bg-cancel"))
        .expect("the creation should be accepted");
    std::thread::sleep(Duration::from_secs(3));
    repository
        .cancel_create("bg-cancel")
        .expect("a build in flight is cancellable");

    let deadline = std::time::Instant::now() + Duration::from_secs(180);
    let outcome = loop {
        let listed = repository
            .list_vms()
            .expect("listing should work")
            .iter()
            .any(|vm| vm.name == "bg-cancel");
        if !listed {
            break Ok(repository.take_diagnostics());
        }
        if std::time::Instant::now() >= deadline {
            break Err("the cancelled build did not go away".to_owned());
        }
        std::thread::sleep(Duration::from_secs(1));
    };
    let left_behind = root.join("bg-cancel").exists();
    drop(repository);
    let _ = fs::remove_dir_all(&root);

    let diagnostics = outcome.expect("the cancelled build should disappear from the list");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("bg-cancel")),
        "the user has to be told why the VM went away: {diagnostics:?}"
    );
    assert!(
        !left_behind,
        "the cancelled build must remove its directory"
    );
}

/// A monitor for the tests that drive the creation pipeline directly.
///
/// They call it on the calling thread and read nothing back from it: what they
/// check is the VM the pipeline built, not the steps it reported on the way.
fn build_monitor() -> vmlord_core::BuildMonitor {
    vmlord_core::BuildMonitor::new(vmlord_core::BuildStep::WritingDisk)
}

/// The importer for tests that create VMs from local media only.
///
/// A cloud image would download hundreds of megabytes; the tests that need one
/// build their own importer.
fn no_cloud_images() -> vmlord_platform::CloudDiskImporter {
    Box::new(|_, _, _, _| {
        Err(vmlord_core::RepositoryError::new(
            "this test creates no VM from a cloud image",
        ))
    })
}

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
        source: VmSource::LocalMedia { path: image_path },
        ram_mb: 2048,
        disk_gb: 8,
        cpu_cores: 2,
        gpu_mode: GpuMode::None,
        network_mode: NetworkMode::None,
    };
    let vm_name = request.name.clone();
    // The same mapping file `HcsVmRepository::new` builds its own store from,
    // so a termination issued through it is invisible to the repository.
    let store = MetadataStore::new(root.join("vm-mapping.json"));

    let mut repository = HcsVmRepository::new(root.clone(), no_cloud_images());
    repository
        .initialize()
        .expect("the HCS backend should initialize on a live host");
    repository
        .create_vm(request)
        .expect("VM creation should succeed on an elevated Hyper-V host");
    wait_for_build(&repository, &vm_name).expect("the build should finish");
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
        source: VmSource::LocalMedia {
            path: image_path.to_string_lossy().into_owned(),
        },
        ram_mb: 512,
        disk_gb: 1,
        cpu_cores: 1,
        gpu_mode: GpuMode::None,
        network_mode: NetworkMode::None,
    };
    let store = MetadataStore::new(root.join("vm-mapping.json"));
    let vm_directory = root.join("vm");

    let mapping = VmCreationPipeline::production(no_cloud_images())
        .create(&store, &request, &vm_directory, &build_monitor())
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
        source: VmSource::LocalMedia {
            path: image_path.to_string_lossy().into_owned(),
        },
        ram_mb: 512,
        disk_gb: 1,
        cpu_cores: 1,
        gpu_mode: GpuMode::None,
        network_mode: NetworkMode::None,
    };
    let store = MetadataStore::new(root.join("vm-mapping.json"));
    let vm_directory = root.join("vm");

    let mapping = VmCreationPipeline::production(no_cloud_images())
        .create(&store, &request, &vm_directory, &build_monitor())
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
        source: VmSource::LocalMedia { path: image_path },
        ram_mb: 2048,
        disk_gb: 8,
        cpu_cores: 2,
        gpu_mode: GpuMode::None,
        network_mode: NetworkMode::None,
    };
    let store = MetadataStore::new(root.join("vm-mapping.json"));
    let vm_directory = root.join("vm");

    let mapping = VmCreationPipeline::production(no_cloud_images())
        .create(&store, &request, &vm_directory, &build_monitor())
        .expect("VM creation should succeed on an elevated Hyper-V host");

    let started = VmStartPipeline::production(Com1Launcher::production()).start(
        &store,
        &mapping.vm_name,
        &vm_directory,
    );

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
        source: VmSource::LocalMedia { path: image_path },
        ram_mb: 2048,
        disk_gb: 8,
        cpu_cores: 2,
        gpu_mode: GpuMode::None,
        network_mode: NetworkMode::None,
    };
    let store = MetadataStore::new(root.join("vm-mapping.json"));
    let vm_directory = root.join("vm");

    let mapping = VmCreationPipeline::production(no_cloud_images())
        .create(&store, &request, &vm_directory, &build_monitor())
        .expect("VM creation should succeed on an elevated Hyper-V host");
    VmStartPipeline::production(Com1Launcher::production())
        .start(&store, &mapping.vm_name, &vm_directory)
        .expect("the created VM must start before it can be forcibly stopped");

    let force_stopped = VmForceStopPipeline::production().force_stop(&store, &mapping.vm_name);
    let restarted = force_stopped.as_ref().ok().map(|()| {
        VmStartPipeline::production(Com1Launcher::production()).start(
            &store,
            &mapping.vm_name,
            &vm_directory,
        )
    });

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
/// `HCS_E_INVALID_JSON`, so the pipeline must pass a parsable one, and since
/// #70 that document also names the mechanism the request travels by. It
/// deliberately does not assert that the shutdown succeeds: this VM boots
/// installer media, so whether anything in it answers the shutdown service is
/// the installer's business. `shuts_down_a_running_guest` is the test that
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
        source: VmSource::LocalMedia { path: image_path },
        ram_mb: 2048,
        disk_gb: 8,
        cpu_cores: 2,
        gpu_mode: GpuMode::None,
        network_mode: NetworkMode::None,
    };
    let store = MetadataStore::new(root.join("vm-mapping.json"));
    let vm_directory = root.join("vm");

    let mapping = VmCreationPipeline::production(no_cloud_images())
        .create(&store, &request, &vm_directory, &build_monitor())
        .expect("VM creation should succeed on an elevated Hyper-V host");
    VmStartPipeline::production(Com1Launcher::production())
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
        source: VmSource::LocalMedia {
            path: image_path.to_string_lossy().into_owned(),
        },
        ram_mb: 512,
        disk_gb: 1,
        cpu_cores: 1,
        gpu_mode: GpuMode::None,
        network_mode: NetworkMode::None,
    };
    let store = MetadataStore::new(root.join("vm-mapping.json"));
    let vm_directory = root.join("vm");

    let mapping = VmCreationPipeline::production(no_cloud_images())
        .create(&store, &request, &vm_directory, &build_monitor())
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
        source: VmSource::LocalMedia { path: image_path },
        ram_mb: 2048,
        disk_gb: 8,
        cpu_cores: 2,
        gpu_mode: GpuMode::None,
        network_mode: NetworkMode::None,
    };
    let store = MetadataStore::new(root.join("vm-mapping.json"));
    let vm_directory = root.join("vm");

    let mapping = VmCreationPipeline::production(no_cloud_images())
        .create(&store, &request, &vm_directory, &build_monitor())
        .expect("VM creation should succeed on an elevated Hyper-V host");
    VmStartPipeline::production(Com1Launcher::production())
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
/// running and able to service the request -- and that the guest, not the test,
/// is what stops it.
///
/// This is #70's regression test, and it covers both halves of that bug at
/// once, because either one alone brings `ERROR_NOT_SUPPORTED` back: the
/// configuration has to offer the guest a shutdown integration service, and the
/// shutdown call has to name that service as the mechanism to carry the
/// request. Before both, a graceful stop failed for every guest VMLord could
/// build, and the only way to stop a VM was to pull its power.
///
/// A `shutdown` that returns `Ok` proves only that HCS took the request, so the
/// assertion is on the compute system's disappearance: HCS destroys it as the
/// VM stops, which nothing but the guest actually halting produces here.
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
            ssh: None,
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
        "a running guest must accept a graceful shutdown and act on it; ERROR_NOT_SUPPORTED \
         here means the VM was not offered a shutdown service or the request did not name it \
         as its mechanism (#70)",
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
        // 2.5 and the `Services` section together are what #70 fixed: an older
        // schema has no integration components in its model, so the guest is
        // offered no shutdown channel to answer through and the shutdown
        // operation fails however healthy the guest is.
        "SchemaVersion": { "Major": 2, "Minor": 5 },
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
            },
            "Services": { "Shutdown": {}, "Timesync": {} }
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

    VmShutdownPipeline::production().shutdown(store, "guest-shutdown-probe")?;
    println!("the shutdown request was delivered; waiting for the guest to power itself off");
    wait_until_gone(hcs_id, POWEROFF_WAIT)
}

/// How long a guest is given to finish powering off once it has been asked to.
///
/// The request returns as soon as HCS has delivered it, so this is the guest's
/// own unmount-and-halt, not an API call: generous, because a slow disk is not
/// a failed shutdown.
const POWEROFF_WAIT: Duration = Duration::from_secs(180);

/// Waits until HCS no longer knows `hcs_id`, which is what a guest that really
/// powered itself off leaves behind.
///
/// HCS destroys a compute system as it stops, exactly as it does for a
/// termination, so the system's absence -- and not the shutdown call returning
/// `Ok`, which only means the request was delivered -- is the proof the guest
/// acted on the request.
fn wait_until_gone(hcs_id: &str, timeout: Duration) -> Result<(), vmlord_core::RepositoryError> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if HcsSystem::open_if_present(hcs_id, HCS_ACCESS_ALL)?.is_none() {
            println!("HCS no longer knows \"{hcs_id}\": the guest powered itself off");
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(vmlord_core::RepositoryError::new(format!(
                "compute system \"{hcs_id}\" was still there {}s after its guest accepted the \
                 shutdown request",
                timeout.as_secs()
            )));
        }
        std::thread::sleep(Duration::from_secs(2));
    }
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
        source: VmSource::LocalMedia {
            path: image_path.to_string_lossy().into_owned(),
        },
        ram_mb: 512,
        disk_gb: 1,
        cpu_cores: 1,
        gpu_mode: GpuMode::None,
        network_mode: NetworkMode::None,
    };
    let store = MetadataStore::new(root.join("vm-mapping.json"));
    let vm_directory = root.join("vm");

    let mapping = VmCreationPipeline::production(no_cloud_images())
        .create(&store, &request, &vm_directory, &build_monitor())
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
        source: VmSource::LocalMedia { path: image_path },
        ram_mb: 2048,
        disk_gb: 8,
        cpu_cores: 2,
        gpu_mode: GpuMode::None,
        network_mode: NetworkMode::Nat,
    };
    let store = MetadataStore::new(root.join("vm-mapping.json"));
    let vm_directory = root.join("vm");

    let mapping = VmCreationPipeline::production(no_cloud_images())
        .create(&store, &request, &vm_directory, &build_monitor())
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
        VmStartPipeline::production(Com1Launcher::production())
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
        VmStartPipeline::production(Com1Launcher::production())
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
        source: VmSource::LocalMedia { path: image_path },
        ram_mb: 2048,
        disk_gb: 8,
        cpu_cores: 2,
        gpu_mode: GpuMode::None,
        network_mode: NetworkMode::Nat,
    };
    let store = MetadataStore::new(root.join("vm-mapping.json"));
    let vm_directory = root.join("vm");

    let mapping = VmCreationPipeline::production(no_cloud_images())
        .create(&store, &request, &vm_directory, &build_monitor())
        .expect("VM creation should succeed on an elevated Hyper-V host");

    let outcome = (|| -> Result<(), String> {
        VmStartPipeline::production(Com1Launcher::production())
            .start(&store, &mapping.vm_name, &vm_directory)
            .map_err(|error| format!("the NAT VM must start: {error}"))?;

        let first = recorded_endpoint(&store, &mapping.vm_name)?;
        let first_address = endpoint_address(first)?;

        VmForceStopPipeline::production()
            .force_stop(&store, &mapping.vm_name)
            .map_err(|error| format!("a running VM must accept a forced stop: {error}"))?;

        VmStartPipeline::production(Com1Launcher::production())
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
        source: VmSource::LocalMedia { path: image_path },
        ram_mb: 2048,
        disk_gb: 8,
        cpu_cores: 2,
        gpu_mode: GpuMode::None,
        network_mode: NetworkMode::Nat,
    };
    let store = MetadataStore::new(root.join("vm-mapping.json"));
    let vm_directory = root.join("vm");

    let mapping = VmCreationPipeline::production(no_cloud_images())
        .create(&store, &request, &vm_directory, &build_monitor())
        .expect("VM creation should succeed on an elevated Hyper-V host");

    let outcome = (|| -> Result<(), String> {
        // The start fails rather than coming up silently without a network if
        // the DHCP server cannot bind UDP 67, so this is also the check that
        // nothing else on the host is already serving it.
        VmStartPipeline::production(Com1Launcher::production())
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
        VmStartPipeline::production(Com1Launcher::production())
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
/// Waits for a VM's background creation to finish.
///
/// `create_vm` returns as soon as the build is accepted, so a test that acts
/// on the VM has to wait for it the way the UI does: by looking at the list.
fn wait_for_build(repository: &HcsVmRepository, vm_name: &str) -> Result<(), String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(600);
    loop {
        let summaries = repository
            .list_vms()
            .map_err(|error| format!("listing should work: {error}"))?;
        match summaries.iter().find(|vm| vm.name == vm_name) {
            None => return Err(format!("the build of VM \"{vm_name}\" failed")),
            Some(vm) if !matches!(vm.state, VmState::Building { .. }) => return Ok(()),
            Some(_) => {}
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("the build of VM \"{vm_name}\" did not finish"));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

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
        source: VmSource::LocalMedia { path: image_path },
        ram_mb: 2048,
        disk_gb: 8,
        cpu_cores: 2,
        gpu_mode: GpuMode::None,
        network_mode: NetworkMode::Nat,
    };

    let mut repository = HcsVmRepository::new(&root, no_cloud_images());
    repository
        .initialize()
        .expect("the native backend should initialize on a Hyper-V host");
    repository
        .create_vm(request)
        .expect("VM creation should succeed on an elevated Hyper-V host");
    wait_for_build(&repository, &vm_name).expect("the build should finish");
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

    // Best-effort cleanup regardless of the assertion below. Deletion removes
    // the endpoint with the VM; the explicit delete afterwards only covers a
    // deletion that did not get that far.
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

/// Exercises TASK-42's deletion of a VM's endpoint against the real Host
/// Network Service: gives a created VM the endpoint its first start would have
/// recorded, deletes the VM, and confirms HNS no longer has the endpoint while
/// the shared network is still there.
///
/// The VM is never started, so no ISO is needed: what is under test is that
/// deletion removes the endpoint the mapping names, not what the guest does
/// with it. The network surviving is the other half -- it belongs to the
/// installation, and re-creating it would re-pick the subnet and move every
/// remaining guest's address.
///
/// Run elevated with:
/// `cargo test -p vmlord-platform --test hyperv -- --ignored --exact deletes_the_endpoint_of_a_deleted_vm --nocapture`
#[test]
#[ignore = "requires an elevated Windows host with Hyper-V/HCS and HNS enabled"]
fn deletes_the_endpoint_of_a_deleted_vm() {
    let root =
        std::env::temp_dir().join(format!("vmlord-endpoint-delete-e2e-{}", std::process::id()));
    fs::create_dir_all(&root).expect("test root should be created");
    let image_path = root.join("installer.iso");
    fs::write(&image_path, b"placeholder installer media").expect("test image should be written");

    let vm_name = format!("vmlord-e2e-endpoint-delete-{}", std::process::id());
    let request = VmCreateRequest {
        name: vm_name.clone(),
        source: VmSource::LocalMedia {
            path: image_path.to_string_lossy().into_owned(),
        },
        ram_mb: 512,
        disk_gb: 1,
        cpu_cores: 1,
        gpu_mode: GpuMode::None,
        network_mode: NetworkMode::Nat,
    };
    let store = MetadataStore::new(root.join("vm-mapping.json"));
    let vm_directory = root.join("vm");

    let mapping = VmCreationPipeline::production(no_cloud_images())
        .create(&store, &request, &vm_directory, &build_monitor())
        .expect("VM creation should succeed on an elevated Hyper-V host");

    let network = HcnNetwork::ensure().expect("HNS should provide the VMLord NAT network");
    let endpoint_id = Uuid::new_v4();
    HcnEndpoint::create(&network, endpoint_id, &vm_name)
        .expect("HNS should create the VM's endpoint");
    drop(network);
    store
        .insert(VmComputeSystemMapping {
            endpoint_id: Some(endpoint_id),
            ..mapping
        })
        .expect("the endpoint should be recorded the way a start records it");

    let deleted = VmDeletionPipeline::production().delete(&store, &vm_name, &vm_directory, true);
    let endpoint_survived = HcnEndpoint::open_if_present(endpoint_id)
        .expect("HNS should answer whether it still has the endpoint")
        .is_some();
    let network_survived = HcnNetwork::open_if_present(VMLORD_NETWORK_ID)
        .expect("HNS should answer whether it still has the network")
        .is_some();

    // Best-effort cleanup regardless of the assertions below.
    let _ = HcnEndpoint::delete(endpoint_id);
    let _ = fs::remove_dir_all(&root);

    deleted.expect("deletion should succeed on an elevated Hyper-V host");
    assert!(
        !endpoint_survived,
        "an endpoint left in HNS holds an address of the network's subnet forever"
    );
    assert!(
        network_survived,
        "the shared network must outlive the VMs in it, including the last one"
    );
}

/// Exercises TASK-42's cleanup on `initialize` against the real Host Network
/// Service: puts an endpoint no mapping names into the shared network, brings a
/// repository up, and confirms the orphan is gone while the endpoint a mapping
/// does name is untouched.
///
/// The endpoints the host already had are recorded in the test's own store
/// first, so this collects nothing but the orphan it created: run on a
/// developer's machine, the VMs in a real VMLord's storage directory are not
/// known to a store under a temporary root, and their endpoints would otherwise
/// be collected as orphans -- which is the behaviour under test, not something
/// to inflict on the host.
///
/// Run elevated with:
/// `cargo test -p vmlord-platform --test hyperv -- --ignored --exact initialize_collects_the_endpoints_no_vm_owns --nocapture`
#[test]
#[ignore = "requires an elevated Windows host with Hyper-V/HCS and HNS enabled"]
fn initialize_collects_the_endpoints_no_vm_owns() {
    let root = std::env::temp_dir().join(format!("vmlord-orphan-e2e-{}", std::process::id()));
    fs::create_dir_all(&root).expect("test root should be created");
    let store = MetadataStore::new(root.join("vm-mapping.json"));

    let network = HcnNetwork::ensure().expect("HNS should provide the VMLord NAT network");
    for (index, id) in HcnEndpoint::list_in_vmlord_network()
        .expect("HNS should list the endpoints in the VMLord network")
        .into_iter()
        .enumerate()
    {
        store
            .insert(mapping_owning(&format!("preexisting-{index}"), id))
            .expect("the host's own endpoints should be claimed before the cleanup runs");
    }

    let owned_id = Uuid::new_v4();
    let orphan_id = Uuid::new_v4();
    HcnEndpoint::create(&network, owned_id, "vmlord-orphan-probe-owned")
        .expect("HNS should create the owned endpoint");
    HcnEndpoint::create(&network, orphan_id, "vmlord-orphan-probe-orphan")
        .expect("HNS should create the orphaned endpoint");
    drop(network);
    store
        .insert(mapping_owning("vmlord-orphan-probe-owned", owned_id))
        .expect("the owned endpoint should be recorded the way a start records it");

    let initialized = HcsVmRepository::new(&root, no_cloud_images()).initialize();
    let owned_survived = HcnEndpoint::open_if_present(owned_id)
        .expect("HNS should answer whether it still has the owned endpoint")
        .is_some();
    let orphan_survived = HcnEndpoint::open_if_present(orphan_id)
        .expect("HNS should answer whether it still has the orphaned endpoint")
        .is_some();
    let network_survived = HcnNetwork::open_if_present(VMLORD_NETWORK_ID)
        .expect("HNS should answer whether it still has the network")
        .is_some();

    // Best-effort cleanup regardless of the assertions below.
    let _ = HcnEndpoint::delete(owned_id);
    let _ = HcnEndpoint::delete(orphan_id);
    let _ = fs::remove_dir_all(&root);

    initialized.expect("the native backend should initialize on a Hyper-V host");
    assert!(
        !orphan_survived,
        "an endpoint no VM owns is one nothing can ever find again"
    );
    assert!(
        owned_survived,
        "the endpoint of a VM VMLord knows must survive the cleanup"
    );
    assert!(
        network_survived,
        "the cleanup must collect endpoints, never the network they are in"
    );
}

/// A mapping for a VM that owns `endpoint_id`, as a first start would leave it.
///
/// The compute system is named rather than created: the cleanup reads mappings
/// and never asks HCS about them.
fn mapping_owning(vm_name: &str, endpoint_id: Uuid) -> VmComputeSystemMapping {
    VmComputeSystemMapping {
        vm_id: Uuid::new_v4(),
        vm_name: vm_name.to_owned(),
        hcs_compute_system_id: format!("vmlord-{vm_name}-{endpoint_id}"),
        disk_gb: 1,
        endpoint_id: Some(endpoint_id),
        network_mode: NetworkMode::Nat,
        ssh: None,
    }
}

/// The whole path on a real host: a real Ubuntu cloud image becomes a VM's
/// disk, and the seed the guest reads sits beside it.
///
/// Ignored by default -- it needs Hyper-V, an elevated process and a few
/// hundred megabytes off the network. The importer is assembled from the same
/// two calls the composition root makes, so what is exercised is what ships.
#[test]
#[ignore = "requires an elevated Windows host with Hyper-V/HCS enabled"]
fn a_vm_is_created_from_a_real_cloud_image() {
    let root = std::env::temp_dir().join(format!("vmlord-cloud-image-{}", std::process::id()));
    fs::create_dir_all(&root).expect("test root should be created");
    let cache = root.join("cache");
    let vm_directory = root.join("cloud-vm");
    let store = MetadataStore::new(root.join("vm-mapping.json"));

    let pipeline = VmCreationPipeline::production(Box::new(
        move |image, size, target, monitor: &vmlord_core::BuildMonitor| {
            monitor.report(vmlord_core::BuildStep::Downloading);
            let mut source = vmlord_image::open_cloud_image(
                &image.profile,
                &image.release,
                &cache,
                size,
                monitor.downloads(),
                monitor.cancel_flag(),
            )?;
            monitor.report(vmlord_core::BuildStep::WritingDisk);
            vmlord_platform::import_image(&mut source, target, size, monitor.cancel_flag())
                .map(|_| ())
        },
    ));

    let request = VmCreateRequest {
        name: "vmlord-cloud-test".into(),
        source: vmlord_core::VmSource::CloudImage {
            image: vmlord_core::CloudImage {
                profile: vmlord_core::ubuntu(),
                release: "24.04".into(),
            },
            provisioning: vmlord_core::Provisioning {
                username: "dev".into(),
                password: None,
                ssh: vmlord_core::SshAccess::Enabled { deploy_key: true },
                locale: "en_US.UTF-8".into(),
                keyboard: "us".into(),
                timezone: "Europe/Moscow".into(),
            },
        },
        ram_mb: 2048,
        disk_gb: 16,
        cpu_cores: 2,
        gpu_mode: GpuMode::None,
        network_mode: NetworkMode::Nat,
    };

    let created = pipeline.create(
        &store,
        &request,
        &vm_directory,
        &vmlord_core::BuildMonitor::new(vmlord_core::BuildStep::Downloading),
    );
    // Captured while the files still exist -- the cleanup below removes them
    // regardless of whether the assertions that need them ever run.
    let seed = created
        .as_ref()
        .ok()
        .and_then(|_| fs::read(vm_directory.join("seed.iso")).ok());
    let document: Option<serde_json::Value> = created.as_ref().ok().and_then(|_| {
        fs::read_to_string(vm_directory.join("config.json"))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
    });

    // Best-effort cleanup regardless of the assertions below.
    if let Ok(mapping) = &created
        && let Ok(system) = HcsSystem::open(&mapping.hcs_compute_system_id, HCS_ACCESS_ALL)
    {
        let _ = system
            .terminate()
            .and_then(|operation| operation.wait_for_completion(Duration::from_secs(30)));
    }
    let _ = fs::remove_dir_all(&root);

    created.expect("a cloud image should become a VM");

    let seed = seed.expect("the seed should be written");
    assert_eq!(&seed[16 * 2048 + 40..16 * 2048 + 46], b"CIDATA");
    let text = String::from_utf8_lossy(&seed);
    assert!(text.contains("user-data") && text.contains("meta-data"));

    let document = document.expect("the configuration should be written and valid JSON");
    assert_eq!(
        document.pointer("/VirtualMachine/Devices/Scsi/Primary/Attachments/1/Path"),
        Some(&serde_json::json!(vm_directory.join("seed.iso")))
    );
}

/// The COM1 pipe the helper opens, derived the way `hcs_config` derives it.
fn com1_pipe_path(vm_id: Uuid) -> String {
    format!(r"\\.\pipe\vmlord-{}.com1", vm_id.as_simple())
}

/// Everything the guest has said so far, collected by a thread so that the test
/// can wait for a prompt while the guest is still talking.
fn read_com1_in_background(pipe: fs::File) -> Arc<Mutex<String>> {
    let transcript = Arc::new(Mutex::new(String::new()));
    let collected = Arc::clone(&transcript);
    std::thread::spawn(move || {
        let mut pipe = pipe;
        let mut buffer = [0u8; 4096];
        while let Ok(read) = pipe.read(&mut buffer) {
            if read == 0 {
                break;
            }
            collected
                .lock()
                .unwrap()
                .push_str(&String::from_utf8_lossy(&buffer[..read]));
        }
    });
    transcript
}

/// Waits until the guest has said `expected`, or gives up with what it did say.
fn wait_for_console(
    transcript: &Arc<Mutex<String>>,
    expected: &str,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        if transcript.lock().unwrap().contains(expected) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let seen = transcript.lock().unwrap().clone();
            let tail: String = seen
                .chars()
                .rev()
                .take(400)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            return Err(format!(
                "\"{expected}\" never appeared on COM1; last output was: {tail}"
            ));
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// The reason this task exists: a guest with no network is reachable only
/// through its serial port, and only if the console can be typed into.
///
/// The VM is started through HCS directly rather than through the repository,
/// because a repository start opens its own console helper and HCS serves one
/// pipe instance -- the test has to be the only client.
#[test]
#[ignore = "requires an elevated Windows host with Hyper-V/HCS and downloads a cloud image"]
fn a_guest_can_be_logged_into_over_com1() {
    let root = std::env::temp_dir().join(format!("vmlord-com1-login-{}", std::process::id()));
    fs::create_dir_all(&root).expect("test root should be created");
    let mut repository = cloud_repository(&root);
    repository
        .initialize()
        .expect("the native backend should initialize on a Hyper-V host");
    let name = "com1-login";
    let mut request = background_cloud_request(name);
    if let VmSource::CloudImage { provisioning, .. } = &mut request.source {
        // Without a password there is no console login: cloud-init turns
        // password authentication off and the user has no password at all.
        provisioning.password = Some(vmlord_core::Password::new("vmlord-console"));
    }

    repository
        .create_vm(request)
        .expect("the creation should be accepted");
    wait_until_build_finishes(&mut repository, name, Duration::from_secs(20 * 60))
        .expect("the VM should finish building");
    let store = MetadataStore::new(root.join("vm-mapping.json"));
    let mapping = store
        .find_by_vm_name(name)
        .expect("the mapping file should be readable")
        .expect("a built VM has a mapping");
    let system = open_by_vm_name(&store, name, HCS_ACCESS_ALL).expect("the VM should open");
    system
        .start_and_wait(Duration::from_secs(120))
        .map_err(HcsStartFailure::into_error)
        .expect("the built VM should start");

    let result = (|| -> Result<(), String> {
        let pipe = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(com1_pipe_path(mapping.vm_id))
            .map_err(|error| format!("the COM1 pipe should open duplex: {error}"))?;
        let mut input = pipe
            .try_clone()
            .map_err(|error| format!("the pipe handle should clone: {error}"))?;
        let transcript = read_com1_in_background(pipe);

        wait_for_console(&transcript, "login:", Duration::from_secs(10 * 60))?;
        // Carriage return, not newline: this is a tty, and Enter is 0x0d.
        input
            .write_all(b"dev\r")
            .map_err(|error| error.to_string())?;
        wait_for_console(&transcript, "Password:", Duration::from_secs(60))?;
        input
            .write_all(b"vmlord-console\r")
            .map_err(|error| error.to_string())?;
        wait_for_console(&transcript, "dev@", Duration::from_secs(120))?;
        input
            .write_all(b"echo vmlord-console-works\r")
            .map_err(|error| error.to_string())?;
        wait_for_console(&transcript, "vmlord-console-works", Duration::from_secs(60))
    })();

    // Cleanup before the assertion: a VM left running is worse than a failed
    // test.
    let _ = system.terminate_and_wait(Duration::from_secs(120));
    drop(system);
    let _ = repository.delete_vm(VmDeleteRequest {
        name: name.into(),
        delete_disks: true,
    });
    drop(repository);
    let _ = fs::remove_dir_all(&root);

    result.expect("a guest should accept a login typed into COM1");
}

/// Exercises TASK-69's reopening of the COM port against a real compute
/// system: a running VM refuses a second console, and a stopped one refuses
/// any.
///
/// The case the action exists for -- a window that a person closed, reopened
/// from the list -- cannot be asserted from here: closing the terminal is what
/// ends the session, and no test can close a window a person opened. That half
/// stays a manual check. What is checked here is everything the button must
/// not do: open a second reader on the one pipe HCS serves, or a window on a
/// VM whose compute system no longer exists.
///
/// A boot is enough, so installer media does: set `VMLORD_TEST_IMAGE_PATH` to
/// a real bootable ISO.
///
/// Run elevated with:
/// `cargo test -p vmlord-platform --test hyperv -- --ignored --exact the_com_port_of_a_running_vm_is_opened_once --nocapture`
#[test]
#[ignore = "requires an elevated Windows host with Hyper-V/HCS and VMLORD_TEST_IMAGE_PATH set"]
fn the_com_port_of_a_running_vm_is_opened_once() {
    let image_path = std::env::var("VMLORD_TEST_IMAGE_PATH")
        .expect("VMLORD_TEST_IMAGE_PATH must point to a real ISO image");
    let root = std::env::temp_dir().join(format!("vmlord-com1-reopen-{}", std::process::id()));
    fs::create_dir_all(&root).expect("test root should be created");
    let vm_name = format!("vmlord-e2e-com1-open-{}", std::process::id());

    let mut repository = HcsVmRepository::new(&root, no_cloud_images());
    repository
        .initialize()
        .expect("the native backend should initialize on a Hyper-V host");
    repository
        .create_vm(VmCreateRequest {
            name: vm_name.clone(),
            source: VmSource::LocalMedia { path: image_path },
            ram_mb: 2048,
            disk_gb: 8,
            cpu_cores: 2,
            gpu_mode: GpuMode::None,
            network_mode: NetworkMode::None,
        })
        .expect("VM creation should succeed on an elevated Hyper-V host");
    wait_for_build(&repository, &vm_name).expect("the build should finish");

    let outcome = (|| -> Result<(), String> {
        // Before the VM runs there is no pipe: HCS creates it with the compute
        // system.
        let stopped = repository
            .open_console(&vm_name)
            .expect_err("a VM that has never run has no COM port to open");
        if !stopped.to_string().contains("start it first") {
            return Err(format!("a stopped VM should say so, got: {stopped}"));
        }

        repository
            .start_vm(&vm_name)
            .map_err(|error| format!("the VM must start: {error}"))?;

        // The start opened one, and HCS serves this pipe to one client.
        let running = repository
            .open_console(&vm_name)
            .expect_err("a VM whose console is open must not get a second one");
        if !running.to_string().contains("already has a COM1 console") {
            return Err(format!(
                "an open console should be reported as open, got: {running}"
            ));
        }

        repository
            .force_stop_vm(&vm_name)
            .map_err(|error| format!("the VM must stop: {error}"))?;

        // The forced stop cancelled the session with the compute system it read
        // from, so what is refused now is the VM's state and not its console.
        let gone = repository
            .open_console(&vm_name)
            .expect_err("a stopped VM has no COM port to open");
        if !gone.to_string().contains("start it first") {
            return Err(format!("a stopped VM should say so, got: {gone}"));
        }
        Ok(())
    })();

    let _ = repository.force_stop_vm(&vm_name);
    let _ = repository.delete_vm(VmDeleteRequest {
        name: vm_name,
        delete_disks: true,
    });
    drop(repository);
    let _ = fs::remove_dir_all(&root);

    outcome.expect("the COM port of a running VM is opened once and only while it runs");
}
