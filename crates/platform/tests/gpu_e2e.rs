//! Windows-only end-to-end tests for the GPU-PV path.
//!
//! Separate from `hyperv.rs` because these tests ask a different question.
//! `hyperv.rs` asks whether HCS and HNS do what the platform layer expects;
//! this file asks whether a guest ends up rendering, which no fake can answer.
//! Everything the start pipeline decides on its own -- that a failed
//! attachment never fails a start, that a GPU is attached exactly once, that a
//! failed start still records what it learned -- already has unit tests beside
//! the code in `start.rs`, and is deliberately not repeated here.
//!
//! Every test is `#[ignore]`d and every one of them costs a cloud image
//! download and a boot.
//!
//! # Preconditions
//!
//! * An elevated Windows host with Hyper-V and the Host Compute Service.
//! * A GPU partition adapter, which is what `Default` and `Mirror` attach.
//!   `a_host_without_a_partition_adapter_still_starts_the_vm` is the one test
//!   that wants the opposite and says so.
//! * **The guest agent and the GPU payload beside the test binary.** Both are
//!   found from `current_exe`, which under `cargo test` is the test binary in
//!   `target\debug\deps\` and not `target\debug\`. So that directory needs
//!   `vmlord-agent`, the musl binary `cargo agent` builds, and a
//!   `gpu-payload\` child holding the archive and catalog entry
//!   `cargo gpu-payload pack` produces. Without the agent nothing reports at
//!   all; without the payload every report is a guest with no userspace to
//!   render with.
//! * Network access for the cloud image, and roughly twenty minutes per test.
//!
//! Run them one at a time:
//!
//! `cargo test -p vmlord-platform --test gpu_e2e -- --ignored --test-threads=1`

#![cfg(windows)]

use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use vmlord_core::{
    AgentStatus, Diagnostic, GpuAssignment, GpuMode, GpuStatusCode, GuestGpuReport, NetworkMode,
    VmCreateRequest, VmDeleteRequest, VmGpuFacts, VmRepository, VmSource, VmState, VmSummary,
    VmUpdateRequest,
};
use vmlord_platform::{HcsVmRepository, discover_host_gpu};

/// How long a cloud image takes to download and import on a cold cache.
const BUILD_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// How long the guest gets to boot, bring its GPU up and report on it.
///
/// Generous on purpose: the first boot of a GPU VM runs the recipe, which
/// builds a kernel module, and a test that fails because it was impatient
/// tells nobody anything.
const GUEST_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// A repository over `root`, with the importer the composition root builds.
fn repository(root: &Path) -> HcsVmRepository {
    let cache = root.join("cache");
    HcsVmRepository::new(
        root,
        Box::new(
            move |image: &vmlord_core::CloudImage,
                  size,
                  target: &Path,
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

/// A cloud VM asking for `gpu_mode`.
///
/// No networking: the agent reaches VMLord over HvSocket, so everything these
/// tests observe about a guest arrives without an adapter. One less moving
/// part between the host and the answer.
fn gpu_request(name: &str, gpu_mode: GpuMode) -> VmCreateRequest {
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
                ssh: vmlord_core::SshAccess::Enabled {
                    deploy_key: true,
                    port: vmlord_core::SshPort::DEFAULT,
                },
                locale: "en_US.UTF-8".into(),
                keyboard: "us".into(),
                timezone: "Europe/Moscow".into(),
                desktop: vmlord_core::DesktopProfile::Headless,
            },
        },
        ram_mb: 4096,
        disk_gb: 16,
        cpu_cores: 2,
        gpu_mode,
        network_mode: NetworkMode::None,
    }
}

/// A test's own storage root, removed whatever the test does.
///
/// The GPU path writes more than the VM's disks -- a staged payload under the
/// VM and an unpacked generation in the shared cache -- and a leaked one of
/// those is gigabytes on the tester's disk.
struct TestRoot {
    path: PathBuf,
}

impl TestRoot {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!("vmlord-gpu-{label}-{}", std::process::id()));
        fs::create_dir_all(&path).expect("the test root should be created");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Builds VM `name` and leaves it running with its guest reporting.
///
/// The three steps every test below starts with, because a GPU says nothing
/// about itself until a guest has booted onto it.
fn build_and_run(repository: &mut HcsVmRepository, name: &str, mode: GpuMode) {
    repository
        .create_vm(gpu_request(name, mode))
        .expect("the creation should be accepted");
    wait_until_built(repository, name).expect("the VM should finish building");
    // A build leaves the VM running, but a build that stopped short of it does
    // not; starting an already-running VM is refused, so ask what it is first.
    if !matches!(state(repository, name), VmState::Running { .. }) {
        repository.start_vm(name).expect("the VM should start");
    }
}

/// Tears `name` down as far as it can, and says nothing about how it went.
///
/// Called before a test's assertions, never after: a VM left running on the
/// tester's host outlives the failure that skipped its cleanup.
fn tear_down(repository: &mut HcsVmRepository, name: &str) {
    let _ = repository.force_stop_vm(name);
    let _ = repository.delete_vm(VmDeleteRequest {
        name: name.into(),
        delete_disks: true,
    });
}

/// One application refresh: list the VMs, then reap and read.
///
/// Both halves, because `refresh` is the `&mut self` call where the repository
/// joins what has finished -- an answered shutdown, a compute system HCS has
/// released and, with it, the GPU facts of the run that ended. The application
/// makes both calls on every refresh; a test that only listed would wait
/// forever for a change that is applied in the call it never makes.
///
/// What was recorded is appended to `seen`, so a wait that times out can report
/// everything it was told rather than whatever the last call happened to hold.
fn refresh(repository: &mut HcsVmRepository, seen: &mut Vec<Diagnostic>) -> Vec<VmSummary> {
    let listed = repository.list_vms().expect("listing should work");
    seen.extend(drain(repository));
    listed
}

fn summary(repository: &mut HcsVmRepository, name: &str) -> Option<VmSummary> {
    refresh(repository, &mut Vec::new())
        .into_iter()
        .find(|vm| vm.name == name)
}

fn facts(repository: &mut HcsVmRepository, name: &str) -> VmGpuFacts {
    summary(repository, name)
        .expect("the VM should be listed")
        .gpu
}

fn state(repository: &mut HcsVmRepository, name: &str) -> VmState {
    summary(repository, name)
        .expect("the VM should be listed")
        .state
}

/// Waits until `name` stops being listed as building.
fn wait_until_built(repository: &mut HcsVmRepository, name: &str) -> Result<(), String> {
    let deadline = Instant::now() + BUILD_TIMEOUT;
    let mut seen = Vec::new();
    loop {
        match refresh(repository, &mut seen)
            .into_iter()
            .find(|vm| vm.name == name)
        {
            Some(vm) if matches!(vm.state, VmState::Building { .. }) => {}
            Some(_) => return Ok(()),
            // A build that failed rolls itself back, so the VM is gone and the
            // diagnostics it left are the only account of why.
            None => return Err(format!("the build failed: {seen:?}")),
        }
        if Instant::now() >= deadline {
            return Err(format!("the build did not finish: {seen:?}"));
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// Waits until the guest of `name` has reported on its GPU.
///
/// Any report ends the wait, a failure included: what the guest says is the
/// test's subject, and swallowing a `Failed` here would turn a real answer
/// into a timeout.
fn wait_for_guest_report(
    repository: &mut HcsVmRepository,
    name: &str,
) -> Result<GuestGpuReport, String> {
    let deadline = Instant::now() + GUEST_TIMEOUT;
    let mut seen = Vec::new();
    loop {
        let observed = refresh(repository, &mut seen)
            .into_iter()
            .find(|vm| vm.name == name)
            .map(|vm| vm.gpu);
        if let Some(report) = observed.and_then(|gpu| gpu.guest) {
            return Ok(report);
        }
        if Instant::now() >= deadline {
            return Err(format!("the guest never reported on its GPU: {seen:?}"));
        }
        std::thread::sleep(Duration::from_secs(5));
    }
}

/// Waits until nothing is recorded about the GPU of `name`.
///
/// A run's facts are forgotten when HCS reports the compute system released,
/// which the refresh after a stop drains. Waiting for it rather than reading
/// once is what keeps a test from racing the event that ends the run.
fn wait_until_gpu_forgotten(repository: &mut HcsVmRepository, name: &str) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(5 * 60);
    loop {
        if facts(repository, name) == VmGpuFacts::default() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "a stopped VM kept the facts of the run that ended: {:?}",
                facts(repository, name)
            ));
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// Waits until the agent of `name` is connected.
fn wait_for_agent(repository: &mut HcsVmRepository, name: &str) -> Result<(), String> {
    let deadline = Instant::now() + GUEST_TIMEOUT;
    let mut seen = Vec::new();
    loop {
        let observed = refresh(repository, &mut seen)
            .into_iter()
            .find(|vm| vm.name == name)
            .map(|vm| vm.state);
        if let Some(VmState::Running {
            agent_status: AgentStatus::Online,
        }) = observed
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("the agent never came online: {seen:?}"));
        }
        std::thread::sleep(Duration::from_secs(5));
    }
}

/// Waits until `name` is stopped.
fn wait_until_stopped(repository: &mut HcsVmRepository, name: &str) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(5 * 60);
    loop {
        if matches!(state(repository, name), VmState::Stopped) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("the VM did not stop".to_owned());
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// How many GPU partition adapters this host presents.
fn host_adapters() -> usize {
    discover_host_gpu().adapters.len()
}

/// A VM that asks for no GPU must have nothing said about one.
///
/// The baseline the other tests are read against: without it, "the guest
/// reported nothing" could mean the mode was honoured or that the whole GPU
/// path is silent on this host.
#[test]
#[ignore = "requires an elevated Windows host with Hyper-V/HCS and downloads a cloud image"]
fn a_vm_without_a_gpu_has_nothing_observed_about_one() {
    let root = TestRoot::new("mode-none");
    let mut repository = repository(root.path());
    repository
        .initialize()
        .expect("the native backend should initialize on a Hyper-V host");
    let name = "gpu-none";

    build_and_run(&mut repository, name, GpuMode::None);
    let agent = wait_for_agent(&mut repository, name);
    let observed = facts(&mut repository, name);

    tear_down(&mut repository, name);

    agent.expect("the agent of a running VM should connect");
    assert_eq!(
        observed,
        VmGpuFacts::default(),
        "a VM that asked for no GPU had one observed: {observed:?}"
    );
}

/// `Default` attaches the host's preferred adapter, and the guest renders on
/// it.
///
/// The whole vertical slice in one test: HCS accepted the assignment, the
/// driver package and the payload reached the guest, and the probe found
/// something to render with.
#[test]
#[ignore = "requires an elevated Windows host with a GPU partition adapter and the payload beside the test binary"]
fn a_default_vm_renders_on_the_host_adapter() {
    let root = TestRoot::new("mode-default");
    let mut repository = repository(root.path());
    repository
        .initialize()
        .expect("the native backend should initialize on a Hyper-V host");
    let name = "gpu-default";

    build_and_run(&mut repository, name, GpuMode::Default);
    let report = wait_for_guest_report(&mut repository, name);
    let observed = facts(&mut repository, name);

    tear_down(&mut repository, name);

    let report = report.expect("the guest should report on its GPU");
    assert!(
        matches!(
            observed.assignment,
            Some(GpuAssignment::Complete(_) | GpuAssignment::Partial { .. })
        ),
        "the host attached nothing under Default: {:?}",
        observed.assignment
    );
    let GuestGpuReport::Ready(detail) = report else {
        panic!("the guest does not render on the adapter it was given: {report:?}");
    };
    assert!(
        detail.render_node.is_some(),
        "a rendering guest names the render node it found: {detail:?}"
    );
}

/// `Mirror` attaches every adapter the host has, not just the preferred one.
///
/// The only place the difference between the two modes is visible: both are a
/// working GPU, and only the adapter count says which one was applied.
#[test]
#[ignore = "requires an elevated Windows host with a GPU partition adapter and the payload beside the test binary"]
fn a_mirror_vm_is_given_every_adapter_the_host_has() {
    let expected = host_adapters();
    assert!(
        expected > 0,
        "this host presents no GPU partition adapter, so Mirror has nothing to mirror"
    );

    let root = TestRoot::new("mode-mirror");
    let mut repository = repository(root.path());
    repository
        .initialize()
        .expect("the native backend should initialize on a Hyper-V host");
    let name = "gpu-mirror";

    build_and_run(&mut repository, name, GpuMode::Mirror);
    let report = wait_for_guest_report(&mut repository, name);
    let observed = facts(&mut repository, name);

    tear_down(&mut repository, name);

    report.expect("the guest should report on its GPU");
    let detail = match observed.assignment {
        Some(GpuAssignment::Complete(detail) | GpuAssignment::Partial { detail, .. }) => detail,
        other => panic!("the host attached nothing under Mirror: {other:?}"),
    };
    assert_eq!(
        usize::try_from(detail.adapters).unwrap_or(usize::MAX),
        expected,
        "Mirror must cover every adapter the host presents"
    );
    assert!(
        detail.adapter.is_none(),
        "Mirror names no single adapter, because it did not pick one: {detail:?}"
    );
}

/// A restarted VM is described by the run it is in, not the one before it.
///
/// The facts are per-run and held in memory; a stop must clear them and the
/// next start must earn them again. Only a real guest can show the second
/// half, because only a real guest reports.
#[test]
#[ignore = "requires an elevated Windows host with a GPU partition adapter and the payload beside the test binary"]
fn a_restarted_vm_reports_on_its_new_run_and_not_its_old_one() {
    let root = TestRoot::new("vm-restart");
    let mut repository = repository(root.path());
    repository
        .initialize()
        .expect("the native backend should initialize on a Hyper-V host");
    let name = "gpu-restart";

    build_and_run(&mut repository, name, GpuMode::Default);
    let first = wait_for_guest_report(&mut repository, name);
    let first_observed_at = facts(&mut repository, name).observed_at;

    let stopped = repository
        .stop_vm(name)
        .map_err(|error| error.to_string())
        .and_then(|()| wait_until_stopped(&mut repository, name));
    let forgotten = wait_until_gpu_forgotten(&mut repository, name);

    let restarted = repository.start_vm(name).map_err(|error| error.to_string());
    let second = wait_for_guest_report(&mut repository, name);
    let second_observed_at = facts(&mut repository, name).observed_at;

    tear_down(&mut repository, name);

    first.expect("the first run should report");
    stopped.expect("the VM should stop");
    forgotten.expect("a stopped VM should keep nothing of the run that ended");
    restarted.expect("the VM should start again");
    second.expect("the second run should report");
    assert!(
        second_observed_at > first_observed_at,
        "the second run was described by the first run's observations"
    );
}

/// A VMLord that did not start a VM says so, rather than inventing an answer.
///
/// Restarting VMLord is modelled the way it happens: the process that started
/// the VM goes away and a new repository over the same storage root reclaims
/// it. What is attached to that VM was never observed here, and `Unknown` is
/// the only honest word for it.
#[test]
#[ignore = "requires an elevated Windows host with a GPU partition adapter and the payload beside the test binary"]
fn a_reclaimed_vm_is_not_described_as_if_this_process_had_started_it() {
    let root = TestRoot::new("vmlord-restart");
    let name = "gpu-reclaimed";

    let mut first = repository(root.path());
    first
        .initialize()
        .expect("the native backend should initialize on a Hyper-V host");
    build_and_run(&mut first, name, GpuMode::Default);
    let started = wait_for_guest_report(&mut first, name);
    // The process that started the VM goes away; the VM does not.
    drop(first);

    let mut second = repository(root.path());
    let reclaimed = second.initialize().map_err(|error| error.to_string());
    let agent = wait_for_agent(&mut second, name);
    let observed = facts(&mut second, name);

    tear_down(&mut second, name);

    started.expect("the first process should see the guest report");
    reclaimed.expect("a second VMLord should reclaim the running VM");
    agent.expect("the agent should reconnect to the new process");
    assert_eq!(
        observed.assignment,
        Some(GpuAssignment::Unknown),
        "a VM this process never started was described as if it had: {:?}",
        observed.assignment
    );
}

/// The mode is desired state: it may only change under a stopped VM, and it
/// takes effect at the next start.
///
/// The refusal has a unit test; what needs a host is the other half -- that
/// the mode really is applied on the following start, rather than only stored.
#[test]
#[ignore = "requires an elevated Windows host with a GPU partition adapter and the payload beside the test binary"]
fn a_mode_changes_only_under_a_stopped_vm_and_applies_at_the_next_start() {
    let root = TestRoot::new("mode-change");
    let mut repository = repository(root.path());
    repository
        .initialize()
        .expect("the native backend should initialize on a Hyper-V host");
    let name = "gpu-mode-change";

    build_and_run(&mut repository, name, GpuMode::None);
    let agent = wait_for_agent(&mut repository, name);
    let while_running = repository.update_vm(VmUpdateRequest {
        name: name.into(),
        ram_mb: 4096,
        cpu_cores: 2,
        gpu_mode: GpuMode::Default,
        network_mode: NetworkMode::None,
        ssh_port: None,
    });

    let stopped = repository
        .stop_vm(name)
        .map_err(|error| error.to_string())
        .and_then(|()| wait_until_stopped(&mut repository, name));
    let while_stopped = repository.update_vm(VmUpdateRequest {
        name: name.into(),
        ram_mb: 4096,
        cpu_cores: 2,
        gpu_mode: GpuMode::Default,
        network_mode: NetworkMode::None,
        ssh_port: None,
    });
    let stored_mode = summary(&mut repository, name).map(|vm| vm.gpu_mode);

    let restarted = repository.start_vm(name).map_err(|error| error.to_string());
    let report = wait_for_guest_report(&mut repository, name);
    let observed = facts(&mut repository, name);

    tear_down(&mut repository, name);

    agent.expect("the agent of a running VM should connect");
    let refusal = while_running.expect_err("a running VM must refuse a GPU mode change");
    assert!(
        refusal.to_string().contains("stop it before changing"),
        "the refusal should say what to do about it: {refusal}"
    );
    stopped.expect("the VM should stop");
    while_stopped.expect("a stopped VM should accept a GPU mode change");
    assert_eq!(
        stored_mode,
        Some(GpuMode::Default),
        "the accepted mode should be what the VM now asks for"
    );
    restarted.expect("the VM should start again");
    report.expect("the guest should report on the GPU the new mode gave it");
    assert!(
        matches!(
            observed.assignment,
            Some(GpuAssignment::Complete(_) | GpuAssignment::Partial { .. })
        ),
        "the mode the VM was restarted with was never applied: {:?}",
        observed.assignment
    );
}

/// A host driver that changed under a VM is repaired by restarting it.
///
/// Drift is the one failure nothing on this host can stage: the exports name
/// the DriverStore folder of the driver installed at the time of the start, and
/// only an actual driver update replaces it. So the test is two-phase and the
/// tester supplies the middle:
///
/// 1. `VMLORD_TEST_GPU_DRIFT=before` builds the VM under `VMLORD_TEST_GPU_ROOT`
///    and leaves it running and rendering.
/// 2. Update or roll back the host's GPU driver, leaving the VM alone.
/// 3. `VMLORD_TEST_GPU_DRIFT=after` restarts that VM and requires the guest to
///    render again -- on the new driver, because a start exports what the host
///    has now.
///
/// Without the variables the test does nothing and says so: an empty run is
/// better than a green one that checked nothing.
#[test]
#[ignore = "two-phase and manual: requires a host GPU driver change between the phases"]
fn a_driver_that_changed_under_a_vm_is_picked_up_by_the_next_start() {
    let Some((phase, root)) = drift_phase() else {
        eprintln!(
            "skipped: set VMLORD_TEST_GPU_DRIFT to \"before\" or \"after\" and \
             VMLORD_TEST_GPU_ROOT to a storage root that outlives both phases"
        );
        return;
    };
    let mut repository = repository(&root);
    repository
        .initialize()
        .expect("the native backend should initialize on a Hyper-V host");
    let name = "gpu-drift";

    match phase.as_str() {
        "before" => {
            build_and_run(&mut repository, name, GpuMode::Default);
            let report = wait_for_guest_report(&mut repository, name);
            let report = report.expect("the guest should report before the driver changes");
            assert!(
                matches!(report, GuestGpuReport::Ready(_)),
                "the VM must render before the driver changes, or the second phase \
                 proves nothing: {report:?}"
            );
            eprintln!(
                "phase \"before\" is done: change the host GPU driver, then run the \
                 phase \"after\""
            );
        }
        "after" => {
            let stopped = repository
                .stop_vm(name)
                .map_err(|error| error.to_string())
                .and_then(|()| wait_until_stopped(&mut repository, name));
            let restarted = repository.start_vm(name).map_err(|error| error.to_string());
            let report = wait_for_guest_report(&mut repository, name);

            tear_down(&mut repository, name);

            stopped.expect("the VM should stop");
            restarted.expect("the VM should start on the changed driver");
            let report = report.expect("the guest should report after the driver changed");
            assert!(
                matches!(report, GuestGpuReport::Ready(_)),
                "a restart did not repair the drift: {report:?}"
            );
        }
        other => panic!("VMLORD_TEST_GPU_DRIFT must be \"before\" or \"after\", not {other:?}"),
    }
}

/// The phase and storage root of the drift test, when it was asked for.
fn drift_phase() -> Option<(String, PathBuf)> {
    let phase = std::env::var("VMLORD_TEST_GPU_DRIFT").ok()?;
    let root = std::env::var("VMLORD_TEST_GPU_ROOT").ok()?;
    let root = PathBuf::from(root);
    fs::create_dir_all(&root).expect("the drift storage root should be created");
    Some((phase, root))
}

/// A host with nothing to hand over still runs the VM.
///
/// GPU-PV is best effort by design, and the shape of that promise is only
/// visible where it is tested: a VM that asked for a GPU on a host that has
/// none must be running, with the reason recorded rather than raised.
#[test]
#[ignore = "requires an elevated Windows host with Hyper-V/HCS and *no* GPU partition adapter"]
fn a_host_without_a_partition_adapter_still_starts_the_vm() {
    assert_eq!(
        host_adapters(),
        0,
        "this host has a GPU partition adapter, so it cannot show what happens without one"
    );

    let root = TestRoot::new("no-adapter");
    let mut repository = repository(root.path());
    repository
        .initialize()
        .expect("the native backend should initialize on a Hyper-V host");
    let name = "gpu-no-adapter";

    build_and_run(&mut repository, name, GpuMode::Default);
    let agent = wait_for_agent(&mut repository, name);
    let running = state(&mut repository, name);
    let observed = facts(&mut repository, name);

    tear_down(&mut repository, name);

    agent.expect("a VM with no GPU to attach still boots and connects its agent");
    assert!(
        matches!(running, VmState::Running { .. }),
        "a host with no adapter must not cost the VM its start: {running:?}"
    );
    let Some(GpuAssignment::Failed(failure)) = observed.assignment else {
        panic!(
            "a host with nothing to attach should say so: {:?}",
            observed.assignment
        );
    };
    assert_eq!(failure.code, GpuStatusCode::HostNoAdapter);
}

/// A host with adapters but no payload gives the guest a device and no
/// userspace, and says which half is missing.
///
/// Run it by *not* copying the payload beside the test binary -- the state
/// every unprepared host is in. `Partial` rather than `Failed` is the point:
/// the adapters were attached and only the Linux userspace is absent.
#[test]
#[ignore = "requires a GPU partition adapter and *no* GPU payload beside the test binary"]
fn a_host_without_a_staged_payload_attaches_the_adapter_anyway() {
    assert!(
        host_adapters() > 0,
        "this test is about a host that has adapters and no payload"
    );

    let root = TestRoot::new("no-payload");
    let mut repository = repository(root.path());
    repository
        .initialize()
        .expect("the native backend should initialize on a Hyper-V host");
    let name = "gpu-no-payload";

    build_and_run(&mut repository, name, GpuMode::Default);
    let agent = wait_for_agent(&mut repository, name);
    let observed = facts(&mut repository, name);

    tear_down(&mut repository, name);

    agent.expect("a VM with no GPU userspace still boots and connects its agent");
    let Some(GpuAssignment::Partial { reason, .. }) = observed.assignment else {
        panic!(
            "a missing payload is less GPU than was asked for, not none and not all: {:?}",
            observed.assignment
        );
    };
    assert_eq!(reason.code, GpuStatusCode::AssignmentPartial);
    assert!(
        reason.message.contains("payload"),
        "the reason should name the half that is missing: {}",
        reason.message
    );
}

/// A deleted GPU VM leaves nothing of its GPU behind.
///
/// The staged payload is the largest thing a GPU VM writes and the easiest to
/// leak: it is unpacked per VM, under the VM's own directory, by a start
/// rather than by the build that deletion was written against.
#[test]
#[ignore = "requires an elevated Windows host with a GPU partition adapter and the payload beside the test binary"]
fn a_deleted_gpu_vm_leaves_no_payload_behind() {
    let root = TestRoot::new("deletion");
    let mut repository = repository(root.path());
    repository
        .initialize()
        .expect("the native backend should initialize on a Hyper-V host");
    let name = "gpu-deleted";

    build_and_run(&mut repository, name, GpuMode::Default);
    let report = wait_for_guest_report(&mut repository, name);
    // The literal names of `layout::gpu_payload_staging_directory` and
    // `layout::vm_directory`, which are crate-private: a test outside the
    // crate can only look where they write.
    let vm_directory = root.path().join(name);
    let staged = vm_directory.join("gpu-payload");
    let staged_before = staged.is_dir();

    let stopped = repository
        .stop_vm(name)
        .map_err(|error| error.to_string())
        .and_then(|()| wait_until_stopped(&mut repository, name));
    let deleted = repository
        .delete_vm(VmDeleteRequest {
            name: name.into(),
            delete_disks: true,
        })
        .map_err(|error| error.to_string());
    let listed_after = summary(&mut repository, name).is_some();

    tear_down(&mut repository, name);

    report.expect("the guest should report, or there was no payload to leak");
    assert!(
        staged_before,
        "the payload was never staged at {}, so this test proves nothing about leaks",
        staged.display()
    );
    stopped.expect("the VM should stop");
    deleted.expect("a stopped VM should delete");
    assert!(!listed_after, "the deleted VM is still listed");
    assert!(
        !vm_directory.exists(),
        "deletion left the VM's directory behind: {}",
        vm_directory.display()
    );
}

/// The diagnostics this test binary records.
///
/// One subscriber for the process rather than one per test: these tests are
/// `#[ignore]`d and run one at a time against a real host, so a shared sink is
/// enough, and a scoped one would have to be threaded through every helper.
fn records() -> &'static vmlord_core::DiagnosticsSink {
    static SINK: std::sync::OnceLock<vmlord_core::DiagnosticsSink> = std::sync::OnceLock::new();
    SINK.get_or_init(|| {
        use tracing_subscriber::layer::SubscriberExt as _;

        let sink = vmlord_core::DiagnosticsSink::new();
        let subscriber =
            tracing_subscriber::registry().with(vmlord_core::DiagnosticsLayer::new(sink.clone()));
        tracing::subscriber::set_global_default(subscriber)
            .expect("nothing else installs a subscriber in this test binary");
        sink
    })
}

/// One application refresh: reap what has finished, then read what it recorded.
///
/// `refresh` is the `&mut self` call where the repository joins what is over --
/// an answered shutdown, a compute system HCS has released and, with it, the
/// facts of the run that ended. A test that only listed would wait forever for
/// a change that is applied in the call it never makes.
fn drain(repository: &mut vmlord_platform::HcsVmRepository) -> Vec<vmlord_core::Diagnostic> {
    repository.refresh();
    records().take()
}
