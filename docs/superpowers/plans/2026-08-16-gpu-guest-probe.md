# Guest GPU probe Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ask a guest whether anything actually renders on the GPU it was given, and answer with one verdict and a list of checks the host logs.

**Architecture:** One new request and one new response on the wire (revision 1.5). What decides is pure and lives in `crates/agent/src/gpu_probe.rs`; what needs a guest -- opening `/dev/dxg`, installing the probe programs, running them under the environment the recipe wrote -- lives in `crates/agent/src/gpu_render.rs`, the way `gpu_recipe.rs` and `gpu_kernel.rs` are split. The host asks once per session, after the recipe report, and logs check by check; deriving `VmGpuFacts` is task #98.

**Tech Stack:** Rust 2024, `prost` for Protobuf, the agent's own `command::run` for bounded external programs, no new dependencies.

**Spec:** `docs/superpowers/specs/2026-08-16-gpu-guest-probe-design.md`

## Global Constraints

* The agent is built for `x86_64-unknown-linux-musl` with `cargo agent`; **never** add a dependency that makes it link against system C libraries. The agent therefore cannot link or `dlopen` `libEGL`/`libvulkan`: every hardware operation is an external program run through `crate::command::run`.
* Nothing in the probe may fail as a whole. Every failure is a check in the report and a VM that keeps running.
* Only a failed `DEVICE` ends the probe; every other failed check leaves the rest running.
* Paths, verbatim: device `/dev/dxg`, module `dxgkrnl`, WSL libraries `/usr/lib/wsl/lib` (`gpu_targets::WSL_LIB`), payload `/opt/vmlord/gpu-payload` (`gpu_targets::PAYLOAD`), bundled Mesa prefix `/opt/vmlord/wsl-mesa`, profile script `/etc/profile.d/vmlord-gpu.sh`, DRM nodes `/dev/dri`.
* Budgets: `APT_BUDGET` 300 s (matching `gpu_kernel.rs`), `PROBE_BUDGET` 60 s for a renderer program, `VENDOR_BUDGET` 15 s.
* Commands are run without a `timeout` prefix: `cargo test -p vmlord-agent`, `cargo test -p vmlord-agent-protocol`, `cargo agent`, `cargo test-windows`, `cargo check-windows`.
* Commit subjects are `TASK-97: <comment>` and end with the `Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>` trailer.
* Branch: `task-97-gpu-guest-probe`. Do not open a merge request without explicit user approval.

---

### Task 1: A probe on the wire

**Files:**
- Modify: `crates/agent-protocol/proto/vmlord/agent/v1/agent.proto` (the `Request` and `Response` `oneof`s, and new messages and enums after `GpuRecipeStageState`)
- Modify: `crates/agent-protocol/src/handshake.rs:19` (`CURRENT_VERSION`)
- Create: `crates/agent-protocol/tests/probe.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `vmlord_agent_protocol::v1::{ProbeGpuRequest, ProbeGpuResponse, GpuProbeCheck, GpuProbeStep, GpuProbeCheckState, GpuProbeVerdict}`; `request::Kind::ProbeGpu`, `response::Kind::ProbeGpu`; `CURRENT_VERSION` = `ProtocolVersion { major: 1, minor: 5 }`.

- [ ] **Step 1: Write the failing test**

Create `crates/agent-protocol/tests/probe.rs`:

```rust
//! What a probe report has to survive on the wire.
//!
//! The verdict is what a later task derives a GPU status from and the checks
//! are what the host logs, so the shape they arrive in is worth a test of its
//! own: a report is written by a guest that may be older or newer than the
//! host reading it.

use prost::Message;
use vmlord_agent_protocol::{
    handshake::CURRENT_VERSION,
    v1::{
        Envelope, GpuProbeCheck, GpuProbeCheckState, GpuProbeStep, GpuProbeVerdict,
        ProbeGpuResponse, envelope, response,
    },
};

#[test]
fn a_probe_report_belongs_to_revision_one_five() {
    // Messages and enum values only, so an agent from 1.4 is simply never
    // asked and a host from 1.4 never has to read one.
    assert_eq!((CURRENT_VERSION.major, CURRENT_VERSION.minor), (1, 5));
}

#[test]
fn a_probe_report_survives_the_round_trip() {
    let report = Envelope::response(
        11,
        response::Kind::ProbeGpu(ProbeGpuResponse {
            verdict: i32::from(GpuProbeVerdict::Renders),
            checks: vec![
                GpuProbeCheck {
                    step: i32::from(GpuProbeStep::Device),
                    state: i32::from(GpuProbeCheckState::Ok),
                    message: "/dev/dxg is a usable device".to_owned(),
                },
                GpuProbeCheck {
                    step: i32::from(GpuProbeStep::Vulkan),
                    state: i32::from(GpuProbeCheckState::Failed),
                    message: "vulkaninfo named no device".to_owned(),
                },
            ],
            renderer: "D3D12 (NVIDIA GeForce RTX 4070)".to_owned(),
            driver: "dxgkrnl".to_owned(),
            render_node: String::new(),
        }),
    );

    let decoded = Envelope::decode(report.encode_to_vec().as_slice()).expect("a decodable report");
    assert_eq!(decoded.request_id, 11);
    let Some(envelope::Body::Response(response)) = decoded.body else {
        panic!("a report is a response");
    };
    let Some(response::Kind::ProbeGpu(report)) = response.kind else {
        panic!("a report is a probe report");
    };
    assert_eq!(report.verdict(), GpuProbeVerdict::Renders);
    assert_eq!(report.checks[0].step(), GpuProbeStep::Device);
    assert_eq!(report.checks[0].state(), GpuProbeCheckState::Ok);
    assert_eq!(report.checks[1].state(), GpuProbeCheckState::Failed);
    assert_eq!(report.renderer, "D3D12 (NVIDIA GeForce RTX 4070)");
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p vmlord-agent-protocol --test probe`
Expected: FAIL to compile -- `ProbeGpuResponse` and the enums do not exist.

- [ ] **Step 3: Add the messages to the schema**

In `crates/agent-protocol/proto/vmlord/agent/v1/agent.proto`, add the arm to `Request`'s `oneof kind` after `ApplyGpuRecipeRequest apply_gpu_recipe = 5;`:

```proto
    ProbeGpuRequest probe_gpu = 6;
```

and delete the comment block below it that reserved field 6 for this message. Add to `Response`'s `oneof kind` after `ApplyGpuRecipeResponse apply_gpu_recipe = 6;`:

```proto
    ProbeGpuResponse probe_gpu = 7;
```

Then append, after `enum GpuRecipeStageState`:

```proto
// Asks the guest whether anything renders on the GPU it was given.
//
// Sent once per session, after the recipe report of the same session, on a
// session that agreed `CAPABILITY_GPU`. Empty for the reason
// `ApplyGpuRecipeRequest` is: what there is to look at is in the guest, and a
// field here would be the host dictating something it cannot know better.
message ProbeGpuRequest {}

// What the guest found when it looked.
message ProbeGpuResponse {
  // What the guest makes of what it saw. The guest decides it, because the
  // guest is the only side that saw the output of the programs it ran; a host
  // that re-derived a verdict from the checks could disagree with the peer
  // that produced them.
  GpuProbeVerdict verdict = 1;

  // Every check, in the order it is attempted, including the ones that never
  // ran. A report that stopped at the failure would leave the host guessing
  // whether the rest was skipped or the agent hung up.
  repeated GpuProbeCheck checks = 2;

  // What the hardware renderer calls itself, when one answered, and empty
  // when none did.
  string renderer = 3;

  // What the guest kernel driver calls itself, when it says.
  string driver = 4;

  // The DRM render node the guest has, such as `/dev/dri/renderD128`, and
  // empty when it has none. The d3d12 path needs no render node, so this is a
  // fact for diagnostics and never a requirement.
  string render_node = 5;
}

message GpuProbeCheck {
  GpuProbeStep step = 1;

  GpuProbeCheckState state = 2;

  // Free text for the host's log: what was found, or what the program that
  // failed had to say. `state` is what a peer branches on.
  string message = 3;
}

// One thing the probe looks at.
enum GpuProbeStep {
  GPU_PROBE_STEP_UNSPECIFIED = 0;

  // The device node, opened rather than merely found.
  GPU_PROBE_STEP_DEVICE = 1;

  // The kernel module behind that device.
  GPU_PROBE_STEP_KERNEL_MODULE = 2;

  // The libraries a renderer has to be able to open.
  GPU_PROBE_STEP_LIBRARIES = 3;

  // The programs the two renderer checks run.
  GPU_PROBE_STEP_TOOLS = 4;

  // A bounded OpenGL operation on the device.
  GPU_PROBE_STEP_OPENGL = 5;

  // A bounded Vulkan operation on the device.
  GPU_PROBE_STEP_VULKAN = 6;

  // Whatever the vendor's own tool has to say. Diagnostics only: it never
  // decides the verdict.
  GPU_PROBE_STEP_VENDOR = 7;
}

// The three values of a check.
//
// The same three a recipe stage has and deliberately not the same enum: a
// recipe stage is work that was done and a check is a fact that was looked
// at, and a value added to one must not appear in the other.
enum GpuProbeCheckState {
  GPU_PROBE_CHECK_STATE_UNSPECIFIED = 0;

  // What the check looked for is there.
  GPU_PROBE_CHECK_STATE_OK = 1;

  // The check did not run, and the message says why.
  GPU_PROBE_CHECK_STATE_SKIPPED = 2;

  // The check ran and what it looked for is not there.
  GPU_PROBE_CHECK_STATE_FAILED = 3;
}

// What the guest makes of its GPU.
enum GpuProbeVerdict {
  GPU_PROBE_VERDICT_UNSPECIFIED = 0;

  // The device node is not there, or will not open.
  GPU_PROBE_VERDICT_NO_DEVICE = 1;

  // The device opened and no hardware renderer answered.
  GPU_PROBE_VERDICT_DEVICE_ONLY = 2;

  // At least one hardware renderer answered, which is what makes a GPU a GPU.
  GPU_PROBE_VERDICT_RENDERS = 3;
}
```

- [ ] **Step 4: Move the revision to 1.5**

In `crates/agent-protocol/src/handshake.rs`, change `CURRENT_VERSION` to `ProtocolVersion { major: 1, minor: 5 }`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p vmlord-agent-protocol`
Expected: PASS, including the existing `recipe.rs` tests. If a test there asserts the revision is 1.4, update it to read `(1, 5)`; the probe test above is where the revision is now asserted, so a duplicate assertion in `recipe.rs` should be deleted rather than doubled.

- [ ] **Step 6: Commit**

```bash
git add crates/agent-protocol
git commit -m "$(cat <<'EOF'
TASK-97: Carry a GPU probe report on the wire

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: What a probe's output means

**Files:**
- Create: `crates/agent/src/gpu_probe.rs`
- Modify: `crates/agent/src/main.rs:39-46` (the module list)

**Interfaces:**
- Consumes: `vmlord_agent_protocol::v1::{GpuProbeCheck, GpuProbeCheckState, GpuProbeStep, GpuProbeVerdict}` from Task 1.
- Produces, all `pub` in `crate::gpu_probe`:
  - `const CHECKS: [GpuProbeStep; 7]`
  - `struct Checks` with `new()`, `ok(GpuProbeStep, impl Into<String>)`, `skipped(..)`, `failed(..)`, `finish(&str) -> Vec<GpuProbeCheck>`
  - `enum Renderer { Hardware(String), Software(String) }` with `pub fn name(&self) -> &str`
  - `fn classify(name: &str) -> Option<Renderer>`
  - `fn eglinfo_renderers(output: &str) -> Vec<String>`
  - `struct VulkanDevice { pub name: String, pub driver: String, pub is_cpu: bool }`
  - `fn vulkaninfo_devices(output: &str) -> Vec<VulkanDevice>`
  - `fn hardware_renderer(found: &[Renderer]) -> Option<&str>`
  - `fn verdict(device: bool, hardware: Option<&str>) -> GpuProbeVerdict`
  - `fn required_libraries(triplet: &str, mesa_prefix: Option<&str>) -> Vec<String>`
  - `fn shell_command(program: &str) -> String`

- [ ] **Step 1: Write the failing tests**

Create `crates/agent/src/gpu_probe.rs` with the module documentation and the test module only, so the tests name the functions before they exist:

```rust
//! What a probe's output means, decided without a GPU in the room.
//!
//! Everything here is a function of text: the name a renderer gives itself,
//! what `eglinfo` and `vulkaninfo` printed, which files a renderer needs to be
//! able to open. That is what makes the judgement of a probe testable on a
//! machine that is neither Ubuntu nor a Hyper-V guest, while `gpu_render`
//! keeps the parts that need one.

use vmlord_agent_protocol::v1::{GpuProbeCheck, GpuProbeCheckState, GpuProbeStep, GpuProbeVerdict};

#[cfg(test)]
mod tests {
    use vmlord_agent_protocol::v1::{GpuProbeCheckState, GpuProbeStep, GpuProbeVerdict};

    use super::{
        CHECKS, Checks, Renderer, classify, eglinfo_renderers, hardware_renderer,
        required_libraries, shell_command, verdict, vulkaninfo_devices,
    };

    #[test]
    fn a_software_rasteriser_is_never_hardware_whatever_it_is_called() {
        for software in [
            "llvmpipe (LLVM 17.0.6, 256 bits)",
            "softpipe",
            "Mesa Intel(R) swrast",
            "lavapipe (LLVM 17.0.6, 256 bits)",
            "SwiftShader Device (LLVM 10.0.0)",
        ] {
            assert!(
                matches!(classify(software), Some(Renderer::Software(_))),
                "{software} is a CPU rasteriser"
            );
        }
    }

    #[test]
    fn a_renderer_this_build_has_never_heard_of_is_hardware() {
        // A deny list rather than an allow list: an allow list would report
        // "no hardware renderer" on the first stack this build was not
        // written against, and the milder failure is the right one here.
        for hardware in [
            "D3D12 (NVIDIA GeForce RTX 4070)",
            "D3D12 (AMD Radeon RX 7900 XT)",
            "Something Nobody Has Shipped Yet",
        ] {
            assert!(
                matches!(classify(hardware), Some(Renderer::Hardware(_))),
                "{hardware} has to count as hardware"
            );
        }
    }

    #[test]
    fn a_renderer_with_no_name_is_no_renderer() {
        assert!(classify("").is_none());
        assert!(classify("   ").is_none());
    }

    #[test]
    fn every_renderer_eglinfo_printed_is_read_once_and_in_order() {
        let output = "\
EGL API version: 1.5
Device platform:
OpenGL renderer string: D3D12 (NVIDIA GeForce RTX 4070)
Surfaceless platform:
OpenGL renderer string: D3D12 (NVIDIA GeForce RTX 4070)
OpenGL ES profile renderer string: llvmpipe (LLVM 17.0.6, 256 bits)
";

        assert_eq!(
            eglinfo_renderers(output),
            vec![
                "D3D12 (NVIDIA GeForce RTX 4070)".to_owned(),
                "llvmpipe (LLVM 17.0.6, 256 bits)".to_owned(),
            ]
        );
        assert!(eglinfo_renderers("eglinfo: command not found").is_empty());
    }

    #[test]
    fn vulkaninfo_names_every_device_it_summarised() {
        let output = "\
Devices:
========
GPU0:
\tapiVersion         = 1.3.255
\tdriverName         = llvmpipe
\tdeviceName         = llvmpipe (LLVM 17.0.6, 256 bits)
\tdeviceType         = PHYSICAL_DEVICE_TYPE_CPU
GPU1:
\tdriverName         = Microsoft Direct3D12
\tdeviceName         = Microsoft Direct3D12 (NVIDIA GeForce RTX 4070)
\tdeviceType         = PHYSICAL_DEVICE_TYPE_DISCRETE_GPU
";

        let devices = vulkaninfo_devices(output);

        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].driver, "llvmpipe");
        assert!(devices[0].is_cpu);
        assert_eq!(
            devices[1].name,
            "Microsoft Direct3D12 (NVIDIA GeForce RTX 4070)"
        );
        assert!(!devices[1].is_cpu);
        assert!(vulkaninfo_devices("ERROR: no Vulkan loader").is_empty());
    }

    #[test]
    fn a_device_that_says_it_is_a_cpu_is_software_whatever_it_is_called() {
        // A driver nobody recognises that reports CPU is still not a GPU.
        let output = "\
GPU0:
\tdriverName         = something-new
\tdeviceName         = Something New
\tdeviceType         = PHYSICAL_DEVICE_TYPE_CPU
";

        let devices = vulkaninfo_devices(output);

        assert!(devices[0].is_cpu);
    }

    #[test]
    fn the_first_hardware_renderer_is_the_one_that_is_reported() {
        let found = vec![
            Renderer::Software("llvmpipe".to_owned()),
            Renderer::Hardware("D3D12 (NVIDIA GeForce RTX 4070)".to_owned()),
            Renderer::Hardware("Microsoft Direct3D12".to_owned()),
        ];

        assert_eq!(
            hardware_renderer(&found),
            Some("D3D12 (NVIDIA GeForce RTX 4070)")
        );
        assert_eq!(
            hardware_renderer(&[Renderer::Software("lavapipe".to_owned())]),
            None
        );
    }

    #[test]
    fn one_hardware_renderer_is_what_makes_a_guest_render() {
        // One is enough and has to be: under the distro Mesa policy Vulkan is
        // lavapipe, and GL is the only hardware path such a guest has.
        assert_eq!(verdict(true, Some("D3D12")), GpuProbeVerdict::Renders);
        assert_eq!(verdict(true, None), GpuProbeVerdict::DeviceOnly);
        assert_eq!(verdict(false, None), GpuProbeVerdict::NoDevice);
        assert_eq!(
            verdict(false, Some("D3D12")),
            GpuProbeVerdict::NoDevice,
            "a renderer without a device is a renderer this guest did not have"
        );
    }

    #[test]
    fn a_bundled_userspace_is_looked_for_where_it_was_staged() {
        let required = required_libraries("x86_64-linux-gnu", Some("/opt/vmlord/wsl-mesa"));

        assert!(
            required.contains(
                &"/opt/vmlord/wsl-mesa/lib/x86_64-linux-gnu/dri/d3d12_dri.so".to_owned()
            ),
            "{required:?}"
        );
        // The WSL userspace d3d12_dri.so itself opens, whatever the policy.
        assert!(
            required.contains(&"/usr/lib/wsl/lib/libd3d12.so".to_owned()),
            "{required:?}"
        );
        assert!(
            required.contains(&"/usr/lib/wsl/lib/libdxcore.so".to_owned()),
            "{required:?}"
        );
    }

    #[test]
    fn a_distribution_userspace_is_looked_for_in_the_distributions_own_path() {
        let required = required_libraries("x86_64-linux-gnu", None);

        assert!(
            required.contains(&"/usr/lib/x86_64-linux-gnu/dri/d3d12_dri.so".to_owned()),
            "{required:?}"
        );
        assert!(
            required.contains(&"/usr/lib/x86_64-linux-gnu/libvulkan.so.1".to_owned()),
            "{required:?}"
        );
        assert!(
            !required.iter().any(|path| path.contains("wsl-mesa")),
            "nothing is staged under this policy: {required:?}"
        );
    }

    #[test]
    fn a_probe_program_runs_under_the_environment_the_recipe_wrote() {
        // Through the file rather than through variables set here: the file is
        // what a person gets over SSH, and a copy of that decision in the
        // agent could disagree with it.
        let command = shell_command("eglinfo -B");

        assert!(
            command.contains("/etc/profile.d/vmlord-gpu.sh"),
            "{command}"
        );
        // A guest whose recipe never wrote the file still runs the program:
        // sourcing a missing file must not be what fails the check.
        assert!(command.contains("[ -r "), "{command}");
        assert!(command.contains("exec eglinfo -B"), "{command}");
    }

    #[test]
    fn a_finished_report_has_every_check_exactly_once_and_in_order() {
        let mut checks = Checks::new();
        checks.ok(GpuProbeStep::Device, "/dev/dxg is a usable device");

        let reported = checks.finish("the probe stopped before this check");

        assert_eq!(reported.len(), CHECKS.len());
        for (check, step) in reported.iter().zip(CHECKS) {
            assert_eq!(check.step(), step);
        }
        assert_eq!(reported[0].state(), GpuProbeCheckState::Ok);
        assert_eq!(reported[1].state(), GpuProbeCheckState::Skipped);
        assert_eq!(reported[1].message, "the probe stopped before this check");
    }

    #[test]
    fn the_checks_a_missing_device_never_reached_carry_its_reason() {
        let mut checks = Checks::new();
        checks.failed(GpuProbeStep::Device, "/dev/dxg is missing");

        let reported = checks.finish("/dev/dxg never opened");

        assert_eq!(reported[0].state(), GpuProbeCheckState::Failed);
        for check in &reported[1..] {
            assert_eq!(check.state(), GpuProbeCheckState::Skipped);
            assert_eq!(check.message, "/dev/dxg never opened");
        }
    }

    #[test]
    fn a_check_recorded_twice_keeps_the_first_answer() {
        let mut checks = Checks::new();
        checks.ok(GpuProbeStep::Opengl, "D3D12 (NVIDIA GeForce RTX 4070)");
        checks.failed(GpuProbeStep::Opengl, "gone");

        let reported = checks.finish("unreached");

        let opengl: Vec<_> = reported
            .iter()
            .filter(|check| check.step() == GpuProbeStep::Opengl)
            .collect();
        assert_eq!(opengl.len(), 1);
        assert_eq!(opengl[0].state(), GpuProbeCheckState::Ok);
    }
}
```

Add `mod gpu_probe;` to the module list in `crates/agent/src/main.rs`, keeping it alphabetical: after `mod gpu_mounts;`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-agent gpu_probe`
Expected: FAIL to compile -- none of the functions exist.

- [ ] **Step 3: Write the implementation**

Above the test module in `crates/agent/src/gpu_probe.rs`:

```rust
/// Every check, in the order it is attempted.
///
/// The order is the report's order, and the report is what the host logs, so
/// it is written once here rather than implied by the sequence of calls in
/// `gpu_render`.
pub const CHECKS: [GpuProbeStep; 7] = [
    GpuProbeStep::Device,
    GpuProbeStep::KernelModule,
    GpuProbeStep::Libraries,
    GpuProbeStep::Tools,
    GpuProbeStep::Opengl,
    GpuProbeStep::Vulkan,
    GpuProbeStep::Vendor,
];

/// The names of the renderers that are a CPU pretending to be a GPU.
///
/// A deny list and not an allow list of the drivers this build knows: an allow
/// list would report "no hardware renderer" on the first guest whose stack
/// renders through something nobody here wrote code against, and the failure
/// mode of a deny list is the milder one -- a new software rasteriser counts
/// as hardware once, until its name is added here.
const SOFTWARE: [&str; 5] = ["llvmpipe", "softpipe", "swrast", "lavapipe", "swiftshader"];

/// Where the environment the recipe wrote lives.
const PROFILE: &str = "/etc/profile.d/vmlord-gpu.sh";

/// The host's WSL userspace, which `d3d12_dri.so` opens by itself.
const WSL_LIB: &str = crate::gpu_targets::WSL_LIB;

/// What a probe found out so far.
///
/// Collected rather than sent as it goes, for the reason a recipe's report is:
/// a check list is one answer to one request.
#[derive(Default)]
pub struct Checks {
    recorded: Vec<GpuProbeCheck>,
}

impl Checks {
    pub fn new() -> Self {
        Self {
            recorded: Vec::with_capacity(CHECKS.len()),
        }
    }

    pub fn ok(&mut self, step: GpuProbeStep, message: impl Into<String>) {
        self.record(step, GpuProbeCheckState::Ok, message.into());
    }

    pub fn skipped(&mut self, step: GpuProbeStep, message: impl Into<String>) {
        self.record(step, GpuProbeCheckState::Skipped, message.into());
    }

    pub fn failed(&mut self, step: GpuProbeStep, message: impl Into<String>) {
        self.record(step, GpuProbeCheckState::Failed, message.into());
    }

    /// Keeps the first answer a check was given.
    fn record(&mut self, step: GpuProbeStep, state: GpuProbeCheckState, message: String) {
        if self.recorded.iter().any(|check| check.step() == step) {
            return;
        }
        self.recorded.push(GpuProbeCheck {
            step: i32::from(step),
            state: i32::from(state),
            message,
        });
    }

    /// The whole report: what was looked at, and `reason` for what was not.
    pub fn finish(mut self, reason: &str) -> Vec<GpuProbeCheck> {
        for step in CHECKS {
            self.skipped(step, reason);
        }
        self.recorded
            .sort_by_key(|check| CHECKS.iter().position(|step| *step == check.step()));
        self.recorded
    }
}

/// A renderer that answered, and what it turned out to be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Renderer {
    Hardware(String),
    Software(String),
}

impl Renderer {
    pub fn name(&self) -> &str {
        match self {
            Self::Hardware(name) | Self::Software(name) => name,
        }
    }
}

/// What a renderer's own name says it is, or nothing for no name at all.
pub fn classify(name: &str) -> Option<Renderer> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }

    let lowercase = name.to_lowercase();
    if SOFTWARE
        .iter()
        .any(|software| lowercase.contains(software))
    {
        return Some(Renderer::Software(name.to_owned()));
    }
    Some(Renderer::Hardware(name.to_owned()))
}

/// Every renderer `eglinfo` named, in order and without repeats.
///
/// It walks several platforms and prints the same renderer for most of them,
/// and a check's message is read by a person.
pub fn eglinfo_renderers(output: &str) -> Vec<String> {
    let mut renderers: Vec<String> = Vec::new();
    for line in output.lines() {
        let Some((_, name)) = line.split_once("renderer string:") else {
            continue;
        };
        let name = name.trim().to_owned();
        if !name.is_empty() && !renderers.contains(&name) {
            renderers.push(name);
        }
    }
    renderers
}

/// One physical device out of a `vulkaninfo --summary`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VulkanDevice {
    pub name: String,
    pub driver: String,
    /// Whether the driver says the device is a CPU, which is software however
    /// it is named.
    pub is_cpu: bool,
}

/// Every device `vulkaninfo --summary` listed.
///
/// Read as text and never trusted to have a fixed shape: output the parser
/// does not recognise is an empty list, which the caller reports as a check
/// that failed with the program's own output attached.
pub fn vulkaninfo_devices(output: &str) -> Vec<VulkanDevice> {
    let mut devices: Vec<VulkanDevice> = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.starts_with("GPU") && line.ends_with(':') {
            devices.push(VulkanDevice {
                name: String::new(),
                driver: String::new(),
                is_cpu: false,
            });
            continue;
        }
        let Some(device) = devices.last_mut() else {
            continue;
        };
        let Some((field, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match field.trim() {
            "deviceName" => device.name = value.to_owned(),
            "driverName" => device.driver = value.to_owned(),
            "deviceType" => device.is_cpu = value.ends_with("_CPU"),
            _ => {}
        }
    }
    devices
}

/// The first hardware renderer that answered, when one did.
pub fn hardware_renderer(found: &[Renderer]) -> Option<&str> {
    found.iter().find_map(|renderer| match renderer {
        Renderer::Hardware(name) => Some(name.as_str()),
        Renderer::Software(_) => None,
    })
}

/// What the guest makes of what it saw.
pub fn verdict(device: bool, hardware: Option<&str>) -> GpuProbeVerdict {
    if !device {
        return GpuProbeVerdict::NoDevice;
    }
    if hardware.is_some() {
        return GpuProbeVerdict::Renders;
    }
    GpuProbeVerdict::DeviceOnly
}

/// The files a renderer has to be able to open, in the order they are named.
///
/// `mesa_prefix` is where a bundled Mesa was staged, and nothing under the
/// `distro` policy -- the two policies put the same driver in different
/// places, and a probe that looked in one of them would report a missing
/// library on a guest that renders.
pub fn required_libraries(triplet: &str, mesa_prefix: Option<&str>) -> Vec<String> {
    let mesa = match mesa_prefix {
        Some(prefix) => format!("{prefix}/lib/{triplet}"),
        None => format!("/usr/lib/{triplet}"),
    };

    vec![
        format!("{mesa}/dri/d3d12_dri.so"),
        format!("/usr/lib/{triplet}/libvulkan.so.1"),
        // `d3d12_dri.so` opens these itself, out of the host's mounted WSL
        // userspace: without them the GL path loads and then falls back.
        format!("{WSL_LIB}/libd3d12.so"),
        format!("{WSL_LIB}/libdxcore.so"),
    ]
}

/// The shell that runs one probe program under the recipe's environment.
///
/// Sourced rather than set here: the file is what a person gets over SSH, and
/// a second copy of those variables in the agent could disagree with it. The
/// guard is what keeps a guest whose recipe never wrote the file running the
/// program anyway -- sourcing a missing file must not be the thing that fails.
pub fn shell_command(program: &str) -> String {
    format!("[ -r {PROFILE} ] && . {PROFILE}; exec {program}")
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-agent gpu_probe`
Expected: PASS, 13 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/agent/src/gpu_probe.rs crates/agent/src/main.rs
git commit -m "$(cat <<'EOF'
TASK-97: Decide what a probe's output means

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: Running the probe in this guest

**Files:**
- Create: `crates/agent/src/gpu_render.rs`
- Modify: `crates/agent/src/gpu_kernel.rs` (make `guest_facts`, `device_is_usable` and `read` visible to the new module)
- Modify: `crates/agent/src/main.rs` (module list)

**Interfaces:**
- Consumes: everything Task 2 produced; `crate::command::{self, Ending, Outcome}`; `crate::gpu_recipe::{GuestFacts, MesaPolicy, library_triplet, module_is_loaded, parse_mesa_policy}`; `crate::gpu_targets::{PAYLOAD, WSL_LIB}`.
- Produces: `pub fn crate::gpu_render::probe(stopping: &AtomicBool) -> ProbeGpuResponse`.

- [ ] **Step 1: Widen what `gpu_kernel` shares**

In `crates/agent/src/gpu_kernel.rs`, change three private items to `pub`, each keeping its documentation: `fn guest_facts()`, `fn device_is_usable()` and `fn read(path: &Path)`. Add to the doc comment of `device_is_usable` one line: `Read by the probe as well as the recipe: the recipe ran minutes ago, and a device that has since gone is exactly what a probe exists to notice.`

- [ ] **Step 2: Write the failing test**

Add `mod gpu_render;` to `crates/agent/src/main.rs` after `mod gpu_recipe;`, and create `crates/agent/src/gpu_render.rs` holding the module documentation and this test module:

```rust
#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use vmlord_agent_protocol::v1::{GpuProbeCheckState, GpuProbeStep, GpuProbeVerdict};

    use super::probe;
    use crate::gpu_probe::CHECKS;

    #[test]
    fn a_machine_with_no_device_reports_no_device_and_skips_the_rest() {
        // The build machine is not a Hyper-V guest, so this is the one path of
        // the probe that can run under `cargo test` -- and it is the path that
        // has to be right, because it is what a VM with a failed recipe takes.
        let report = probe(&AtomicBool::new(false));

        assert_eq!(report.verdict(), GpuProbeVerdict::NoDevice);
        assert_eq!(report.checks.len(), CHECKS.len());
        assert_eq!(report.checks[0].step(), GpuProbeStep::Device);
        assert_eq!(report.checks[0].state(), GpuProbeCheckState::Failed);
        for check in &report.checks[1..] {
            assert_eq!(check.state(), GpuProbeCheckState::Skipped);
        }
        assert!(report.renderer.is_empty());
    }

    #[test]
    fn a_guest_that_is_shutting_down_is_not_made_to_render() {
        // systemd is holding the guest open for this process to exit.
        let report = probe(&AtomicBool::new(true));

        assert_eq!(report.checks.len(), CHECKS.len());
        assert!(
            report
                .checks
                .iter()
                .all(|check| check.state() != GpuProbeCheckState::Ok)
        );
    }
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p vmlord-agent gpu_render`
Expected: FAIL to compile -- `probe` does not exist.

- [ ] **Step 4: Write the implementation**

Above the tests in `crates/agent/src/gpu_render.rs`:

```rust
//! Asking this guest whether anything renders on the GPU it was given.
//!
//! What decides is in `gpu_probe`; what is here is the part that needs a guest
//! with a device in it: opening `/dev/dxg`, looking for the libraries a
//! renderer opens, installing the two programs that can hold a GL context and
//! a Vulkan instance, and running them under the environment the recipe wrote.
//!
//! The agent cannot do the rendering itself. It is a statically linked musl
//! binary with no C toolchain behind it, so it can neither link nor `dlopen`
//! `libEGL` or `libvulkan`, and every real operation on this GPU is therefore
//! another program run with a budget.
//!
//! Nothing here fails as a whole. Every check that does not succeed is a check
//! in the report and a VM that keeps running with less GPU than it asked for.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use vmlord_agent_protocol::v1::{GpuProbeStep, GpuProbeVerdict, ProbeGpuResponse};

use crate::{
    command::{self, Outcome},
    gpu_kernel::{device_is_usable, guest_facts, read},
    gpu_probe::{
        Checks, Renderer, classify, eglinfo_renderers, hardware_renderer, required_libraries,
        shell_command, verdict, vulkaninfo_devices,
    },
    gpu_recipe::{MesaPolicy, library_triplet, module_is_loaded, parse_mesa_policy},
    gpu_targets::{PAYLOAD, WSL_LIB},
};

/// The kernel module behind the device.
const MODULE: &str = "dxgkrnl";

/// Where a bundled Mesa was staged by the recipe.
const MESA_PREFIX: &str = "/opt/vmlord/wsl-mesa";

/// Where the kernel puts the DRM nodes a guest may or may not have.
const DRM: &str = "/dev/dri";

/// The two programs the renderer checks run, and the packages they come in.
///
/// Mesa's and Khronos's own, never a vendor's: that is what makes the probe
/// read the same on a host with an NVIDIA, an AMD or an Intel adapter behind
/// `/dev/dxg`.
const OPENGL_TOOL: &str = "/usr/bin/eglinfo";
const VULKAN_TOOL: &str = "/usr/bin/vulkaninfo";
const TOOL_PACKAGES: [&str; 2] = ["mesa-utils", "vulkan-tools"];

/// The vendor tools that are worth quoting when they happen to be there.
///
/// Diagnostics only. A guest whose vendor tool prints an adapter and whose
/// Mesa renders on llvmpipe is not ready, and a guest with no vendor tool at
/// all that draws through d3d12 is.
const VENDOR_TOOLS: [&str; 2] = ["nvidia-smi", "rocm-smi"];

const APT_BUDGET: Duration = Duration::from_secs(300);
const PROBE_BUDGET: Duration = Duration::from_secs(60);
const VENDOR_BUDGET: Duration = Duration::from_secs(15);

/// Looks at this guest's GPU and says what it found.
///
/// Called once per session, after the recipe of the same session: the probe
/// asks about a userspace the recipe has just installed.
pub fn probe(stopping: &AtomicBool) -> ProbeGpuResponse {
    let mut checks = Checks::new();
    let mut found: Vec<Renderer> = Vec::new();

    let device = device_is_usable();
    if !device {
        checks.failed(
            GpuProbeStep::Device,
            "/dev/dxg is missing, is not a character device, or will not open",
        );
        return report(checks, "/dev/dxg never opened", false, &found);
    }
    checks.ok(GpuProbeStep::Device, "/dev/dxg is a usable device");

    if stopping.load(Ordering::Relaxed) {
        return report(checks, "the guest is shutting down", device, &found);
    }

    let driver = module_check(&mut checks);
    libraries_check(&mut checks);

    if stopping.load(Ordering::Relaxed) {
        return report(checks, "the guest is shutting down", device, &found);
    }

    if tools_check(&mut checks) {
        found.extend(opengl_check(&mut checks));
        found.extend(vulkan_check(&mut checks));
    } else {
        for step in [GpuProbeStep::Opengl, GpuProbeStep::Vulkan] {
            checks.skipped(step, "the probe programs are not installed");
        }
    }

    vendor_check(&mut checks);

    let mut answer = report(checks, "the probe did not need this check", device, &found);
    answer.driver = driver;
    answer
}

/// The finished report, with the verdict the checks add up to.
fn report(
    checks: Checks,
    reason: &str,
    device: bool,
    found: &[Renderer],
) -> ProbeGpuResponse {
    let hardware = hardware_renderer(found);

    ProbeGpuResponse {
        verdict: i32::from(verdict(device, hardware)),
        checks: checks.finish(reason),
        renderer: hardware.unwrap_or_default().to_owned(),
        driver: String::new(),
        render_node: render_node().unwrap_or_default(),
    }
}

/// Whether the module behind the device is loaded, and what it is called.
fn module_check(checks: &mut Checks) -> String {
    if module_is_loaded(&read(Path::new("/proc/modules")), MODULE) {
        checks.ok(
            GpuProbeStep::KernelModule,
            format!("{MODULE} is loaded"),
        );
        return MODULE.to_owned();
    }

    // A device that opens without the module that creates it is a device node
    // left behind, and worth saying so rather than assuming a driver name.
    checks.failed(
        GpuProbeStep::KernelModule,
        format!("{MODULE} is not in /proc/modules"),
    );
    String::new()
}

/// Whether the files a renderer opens are there.
///
/// Never ends the probe: the renderers are what decide, and a library check
/// that was wrong about a path must not veto a guest that draws.
fn libraries_check(checks: &mut Checks) {
    let Ok(guest) = guest_facts() else {
        checks.skipped(
            GpuProbeStep::Libraries,
            "this guest does not say what it is, so there is no library path to look in",
        );
        return;
    };
    let Some(triplet) = library_triplet(&guest.architecture) else {
        checks.skipped(
            GpuProbeStep::Libraries,
            format!(
                "vmlord-agent has no library path for architecture {}",
                guest.architecture
            ),
        );
        return;
    };

    let prefix = match parse_mesa_policy(&read(&Path::new(PAYLOAD).join("sources.json"))) {
        Ok(MesaPolicy::Bundled) => Some(MESA_PREFIX),
        Ok(MesaPolicy::Distro) => None,
        // A payload that is not mounted is not a reason to look nowhere: the
        // distribution's own path is where a guest without one would have it.
        Err(_) => None,
    };

    let required = required_libraries(triplet, prefix);
    let missing: Vec<&String> = required
        .iter()
        .filter(|path| !Path::new(path).exists())
        .collect();

    if missing.is_empty() {
        checks.ok(
            GpuProbeStep::Libraries,
            format!("every library a renderer opens is there, including {WSL_LIB}/libd3d12.so"),
        );
    } else {
        checks.failed(
            GpuProbeStep::Libraries,
            format!(
                "a renderer would not find {}",
                missing
                    .iter()
                    .map(|path| path.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
    }
}

/// Makes sure the two programs the renderer checks run are installed.
///
/// Present-first, as every apt stage in this recipe is: the second start of a
/// VM installs nothing and needs no network.
fn tools_check(checks: &mut Checks) -> bool {
    if tools_are_present() {
        checks.skipped(
            GpuProbeStep::Tools,
            format!("{OPENGL_TOOL} and {VULKAN_TOOL} are already installed"),
        );
        return true;
    }

    let mut outcome = apt_tools();
    if !outcome.succeeded() {
        // A cloud image's package lists are as old as the image.
        let _ = command::run(
            "apt-get",
            &["update"],
            &[("DEBIAN_FRONTEND", "noninteractive")],
            APT_BUDGET,
        );
        outcome = apt_tools();
    }

    if tools_are_present() {
        checks.ok(
            GpuProbeStep::Tools,
            format!("installed {}", TOOL_PACKAGES.join(" and ")),
        );
        true
    } else {
        checks.failed(
            GpuProbeStep::Tools,
            failure("apt-get install", &outcome),
        );
        false
    }
}

fn tools_are_present() -> bool {
    Path::new(OPENGL_TOOL).exists() && Path::new(VULKAN_TOOL).exists()
}

fn apt_tools() -> Outcome {
    let mut arguments = vec!["install", "-y"];
    arguments.extend(TOOL_PACKAGES);
    command::run(
        "apt-get",
        &arguments,
        &[("DEBIAN_FRONTEND", "noninteractive")],
        APT_BUDGET,
    )
}

/// A bounded OpenGL operation, and what rendered it.
fn opengl_check(checks: &mut Checks) -> Vec<Renderer> {
    let outcome = run_probe("eglinfo -B");
    let found: Vec<Renderer> = eglinfo_renderers(&outcome.output)
        .iter()
        .filter_map(|name| classify(name))
        .collect();

    if found.is_empty() {
        checks.failed(
            GpuProbeStep::Opengl,
            format!("eglinfo named no renderer: {}", outcome.output),
        );
        return found;
    }

    match hardware_renderer(&found) {
        Some(name) => checks.ok(GpuProbeStep::Opengl, format!("GL renders on {name}")),
        None => checks.failed(
            GpuProbeStep::Opengl,
            format!(
                "GL renders on the CPU: {}",
                found
                    .iter()
                    .map(Renderer::name)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ),
    }
    found
}

/// A bounded Vulkan operation, and what answered it.
fn vulkan_check(checks: &mut Checks) -> Vec<Renderer> {
    let outcome = run_probe("vulkaninfo --summary");
    let devices = vulkaninfo_devices(&outcome.output);
    if devices.is_empty() {
        checks.failed(
            GpuProbeStep::Vulkan,
            format!("vulkaninfo named no device: {}", outcome.output),
        );
        return Vec::new();
    }

    // A device that says it is a CPU is software however it is named, so the
    // type decides and the name only describes.
    let found: Vec<Renderer> = devices
        .iter()
        .map(|device| {
            let name = if device.name.is_empty() {
                device.driver.clone()
            } else {
                device.name.clone()
            };
            if device.is_cpu {
                Renderer::Software(name)
            } else {
                classify(&name).unwrap_or(Renderer::Software(name))
            }
        })
        .collect();

    match hardware_renderer(&found) {
        Some(name) => checks.ok(GpuProbeStep::Vulkan, format!("Vulkan renders on {name}")),
        None => checks.failed(
            GpuProbeStep::Vulkan,
            format!(
                "Vulkan renders on the CPU: {}",
                found
                    .iter()
                    .map(Renderer::name)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ),
    }
    found
}

/// Whatever a vendor tool has to say, when the mounted userspace carries one.
fn vendor_check(checks: &mut Checks) {
    for tool in VENDOR_TOOLS {
        let path = PathBuf::from(WSL_LIB).join(tool);
        if !path.exists() {
            continue;
        }
        let outcome = command::run(
            &path.to_string_lossy(),
            &["-L"],
            &[],
            VENDOR_BUDGET,
        );
        let first = outcome.output.lines().next().unwrap_or_default().trim();
        checks.ok(
            GpuProbeStep::Vendor,
            format!("{tool}: {first}"),
        );
        return;
    }

    checks.skipped(
        GpuProbeStep::Vendor,
        format!("the mounted userspace in {WSL_LIB} carries no vendor tool"),
    );
}

/// Runs one probe program under the environment the recipe wrote.
fn run_probe(program: &str) -> Outcome {
    command::run(
        "/bin/sh",
        &["-c", &shell_command(program)],
        &[],
        PROBE_BUDGET,
    )
}

/// The DRM render node this guest has, when it has one.
///
/// The d3d12 path needs none, so this is reported and never required.
fn render_node() -> Option<String> {
    let mut nodes: Vec<String> = fs::read_dir(DRM)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("renderD"))
        .collect();
    nodes.sort();
    nodes
        .first()
        .map(|name| format!("{DRM}/{name}"))
}

/// One line about a program that did not succeed.
fn failure(what: &str, outcome: &Outcome) -> String {
    let ending = match outcome.ending {
        command::Ending::Exited(code) => format!("exited with {code}"),
        command::Ending::TimedOut => "outran its time budget".to_owned(),
        command::Ending::NotStarted => "could not be started".to_owned(),
    };
    format!("{what} {ending}: {}", outcome.output)
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p vmlord-agent`
Expected: PASS. If `failure` is now duplicated between `gpu_kernel.rs` and `gpu_render.rs`, move it: make `gpu_kernel::failure` `pub(crate)` and import it in `gpu_render.rs` instead of copying it, then re-run.

- [ ] **Step 6: Commit**

```bash
git add crates/agent/src
git commit -m "$(cat <<'EOF'
TASK-97: Ask this guest whether anything renders

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: Answering the host's probe

**Files:**
- Modify: `crates/agent/src/session.rs` (the `run`/`serve` signatures, the request arm, `kind_name`, and the test helpers)
- Modify: `crates/agent/src/main.rs:167-177` (the call to `session::run`)

**Interfaces:**
- Consumes: `crate::gpu_render::probe`; `ProbeGpuResponse` from Task 1.
- Produces: `session::run(..., attach: A, apply: R, probe: P)` where `P: FnMut() -> ProbeGpuResponse`.

- [ ] **Step 1: Write the failing test**

In the test module of `crates/agent/src/session.rs`, add a helper beside `apply_nothing`:

```rust
    /// A probe that looks at nothing, for the tests about message order.
    fn probe_nothing() -> ProbeGpuResponse {
        ProbeGpuResponse::default()
    }
```

and two tests:

```rust
    #[test]
    fn a_probe_on_a_gpu_session_is_carried_out_and_reported_back() {
        // The host reads this answer to find out whether the guest renders, so
        // it has to arrive as the response to the request that asked.
        let secret = Secret::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
            .expect("a valid test secret");
        let nonce = Nonce::from_wire(&[7; auth::LEN]).expect("a valid nonce");
        let mut stream = ScriptedStream::new([
            ScriptedStream::frame(vmlord_agent_protocol::v1::Envelope::response(
                1,
                response::Kind::Hello(HelloResponse {
                    version: Some(ProtocolVersion::current()),
                    capabilities: vec![i32::from(Capability::Gpu)],
                }),
            )),
            ScriptedStream::frame(vmlord_agent_protocol::v1::Envelope::request(
                1,
                request::Kind::Authenticate(AuthenticateRequest {
                    nonce: nonce.as_bytes().to_vec(),
                }),
            )),
            ScriptedStream::frame(vmlord_agent_protocol::v1::Envelope::request(
                6,
                request::Kind::ProbeGpu(ProbeGpuRequest {}),
            )),
        ]);

        let mut probed = 0;
        run(
            &mut stream,
            &secret,
            "test-agent",
            &mut None,
            refuse_to_mount,
            apply_nothing,
            || {
                probed += 1;
                ProbeGpuResponse {
                    verdict: i32::from(GpuProbeVerdict::Renders),
                    checks: vec![GpuProbeCheck {
                        step: i32::from(GpuProbeStep::Opengl),
                        state: i32::from(GpuProbeCheckState::Ok),
                        message: "GL renders on D3D12 (NVIDIA GeForce RTX 4070)".to_owned(),
                    }],
                    renderer: "D3D12 (NVIDIA GeForce RTX 4070)".to_owned(),
                    driver: "dxgkrnl".to_owned(),
                    render_node: String::new(),
                }
            },
        )
        .expect("the host closes after its probe was answered");

        assert_eq!(probed, 1);
        let frames = stream.written_frames();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[2].request_id, 6);
        let Some(envelope::Body::Response(response)) = &frames[2].body else {
            panic!("a probe needs a response");
        };
        let Some(response::Kind::ProbeGpu(report)) = &response.kind else {
            panic!("a probe needs a probe report");
        };
        assert_eq!(report.verdict(), GpuProbeVerdict::Renders);
        assert_eq!(report.checks[0].step(), GpuProbeStep::Opengl);
    }

    #[test]
    fn a_probe_on_a_session_without_the_gpu_capability_is_refused() {
        // The capability is what says the two builds agreed this session may
        // carry a probe at all.
        let secret = Secret::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
            .expect("a valid test secret");
        let nonce = Nonce::from_wire(&[7; auth::LEN]).expect("a valid nonce");
        let mut stream = ScriptedStream::new([
            ScriptedStream::frame(vmlord_agent_protocol::v1::Envelope::response(
                1,
                response::Kind::Hello(HelloResponse {
                    version: Some(ProtocolVersion::current()),
                    capabilities: vec![],
                }),
            )),
            ScriptedStream::frame(vmlord_agent_protocol::v1::Envelope::request(
                1,
                request::Kind::Authenticate(AuthenticateRequest {
                    nonce: nonce.as_bytes().to_vec(),
                }),
            )),
            ScriptedStream::frame(vmlord_agent_protocol::v1::Envelope::request(
                6,
                request::Kind::ProbeGpu(ProbeGpuRequest {}),
            )),
        ]);

        run(
            &mut stream,
            &secret,
            "test-agent",
            &mut None,
            refuse_to_mount,
            apply_nothing,
            || panic!("a probe that was never agreed on must not be run"),
        )
        .expect("the host closes after the refusal");

        let frames = stream.written_frames();
        assert_eq!(frames.len(), 3);
        let Some(envelope::Body::Response(response)) = &frames[2].body else {
            panic!("the probe needs a response");
        };
        let Some(response::Kind::Error(error)) = &response.kind else {
            panic!("the probe needs an error response");
        };
        assert_eq!(error.code(), ErrorCode::UnsupportedRequest);
    }
```

Extend the test module's `use` of `vmlord_agent_protocol::v1::{...}` with `GpuProbeCheck, GpuProbeCheckState, GpuProbeStep, GpuProbeVerdict, ProbeGpuRequest, ProbeGpuResponse`, and add `probe_nothing` as the seventh argument to every existing `run(...)` call in the module.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-agent session`
Expected: FAIL to compile -- `run` takes six arguments.

- [ ] **Step 3: Write the implementation**

In `crates/agent/src/session.rs`:

Add `ProbeGpuResponse` to the `vmlord_agent_protocol::v1` import list. Give `run` and `serve` a seventh parameter, documented where `attach` and `apply` are:

```rust
pub fn run<S, A, R, P>(
    stream: &mut S,
    secret: &Secret,
    version: &str,
    opened: &mut Option<Session>,
    attach: A,
    apply: R,
    probe: P,
) -> Result<(), SessionError>
where
    S: Read + Write,
    A: FnMut(&[GpuShare]) -> (Vec<GpuMount>, bool),
    R: FnMut() -> Vec<GpuRecipeStage>,
    P: FnMut() -> ProbeGpuResponse,
```

with the same change to `serve`, and `serve(stream, session, attach, apply, probe, &mut buffer)` at the end of `run`. Add the arm after the `ApplyGpuRecipe` one:

```rust
            // The probe follows the recipe and answers from the same place:
            // it runs two short programs rather than a build, and a thread of
            // its own would be two conversations on one socket.
            Body::Request(request::Kind::ProbeGpu(_))
                if session.capabilities.contains(&Capability::Gpu) =>
            {
                let report = Envelope::response(request_id, response::Kind::ProbeGpu(probe()));
                frame::write(stream, &report, buffer).map_err(SessionError::Frame)?;
            }
```

and the arm to `kind_name`:

```rust
        request::Kind::ProbeGpu(_) => "a GPU probe request out of order",
```

In `crates/agent/src/main.rs`, pass the probe to the session:

```rust
        || gpu_kernel::apply(&STOPPING),
        || gpu_render::probe(&STOPPING),
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-agent`
Expected: PASS.

- [ ] **Step 5: Build the agent the way it ships**

Run: `cargo agent`
Expected: a clean build for `x86_64-unknown-linux-musl`.

- [ ] **Step 6: Commit**

```bash
git add crates/agent/src
git commit -m "$(cat <<'EOF'
TASK-97: Probe the GPU when the host asks

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: Asking a guest to probe, once per session

**Files:**
- Modify: `crates/platform/src/agent_session.rs` (`PROBE_REQUEST_ID`, `serve`, `probe_gpu`, `report_probe`, `answer`, and the tests)

**Interfaces:**
- Consumes: `ProbeGpuRequest`, `ProbeGpuResponse`, `GpuProbeCheckState`, `GpuProbeVerdict` from Task 1.
- Produces: nothing for later tasks in this plan; #98 reads the response where `report_probe` logs it.

- [ ] **Step 1: Write the failing test**

In the test module of `crates/platform/src/agent_session.rs`, add:

```rust
    #[test]
    fn a_session_probes_once_the_recipe_has_answered() {
        // The probe follows the recipe and never precedes it: it asks about a
        // userspace the recipe has just installed.
        let secret = Secret::generate();
        let mut guest = Guest::opening_with(
            Secret::from_base64(&secret.to_base64()).expect("the secret"),
            hello(ProtocolVersion::current(), &[Capability::Gpu]),
        );
        let session = open(&mut guest, &secret, VM).expect("a session that authenticated");
        let manifest = GpuShareManifest {
            shares: vec![vmlord_core::GpuShare::payload()],
        };
        guest.say(&Envelope::response(
            super::ATTACH_REQUEST_ID,
            response::Kind::AttachGpuShares(AttachGpuSharesResponse {
                mounts: vec![GpuMount {
                    share: "vmlord.gpu.payload".to_owned(),
                    state: i32::from(GpuMountState::Mounted),
                    path: "/opt/vmlord/gpu-payload".to_owned(),
                    message: "mounted".to_owned(),
                }],
                libraries_refreshed: true,
            }),
        ));
        guest.say(&Envelope::response(
            super::APPLY_REQUEST_ID,
            response::Kind::ApplyGpuRecipe(ApplyGpuRecipeResponse {
                stages: vec![GpuRecipeStage {
                    step: i32::from(GpuRecipeStep::Device),
                    state: i32::from(GpuRecipeStageState::Ok),
                    message: "/dev/dxg is a usable device".to_owned(),
                }],
            }),
        ));
        guest.say(&Envelope::response(
            super::PROBE_REQUEST_ID,
            response::Kind::ProbeGpu(ProbeGpuResponse {
                verdict: i32::from(GpuProbeVerdict::Renders),
                checks: vec![GpuProbeCheck {
                    step: i32::from(GpuProbeStep::Opengl),
                    state: i32::from(GpuProbeCheckState::Ok),
                    message: "GL renders on D3D12 (NVIDIA GeForce RTX 4070)".to_owned(),
                }],
                renderer: "D3D12 (NVIDIA GeForce RTX 4070)".to_owned(),
                driver: "dxgkrnl".to_owned(),
                render_node: String::new(),
            }),
        ));

        serve(&mut guest, &session, Some(&manifest), VM).expect("a session the agent closed");

        let asked = guest.answer_to(super::PROBE_REQUEST_ID);
        let Some(envelope::Body::Request(ref request)) = asked.body else {
            panic!("the probe should have been asked for as a request");
        };
        assert!(matches!(request.kind, Some(request::Kind::ProbeGpu(_))));
        assert_eq!(
            guest
                .received
                .iter()
                .filter(|envelope| matches!(
                    envelope.body,
                    Some(envelope::Body::Request(ref request))
                        if matches!(request.kind, Some(request::Kind::ProbeGpu(_)))
                ))
                .count(),
            1,
            "one probe per session"
        );
    }

    #[test]
    fn a_session_that_never_applied_a_recipe_never_probes() {
        // A guest with no shares has no payload, no recipe and nothing to
        // render with; asking it to probe would install two packages on a VM
        // that was never given a GPU.
        let secret = Secret::generate();
        let mut guest = Guest::opening_with(
            Secret::from_base64(&secret.to_base64()).expect("the secret"),
            hello(ProtocolVersion::current(), &[Capability::Gpu]),
        );
        let session = open(&mut guest, &secret, VM).expect("a session that authenticated");

        serve(&mut guest, &session, None, VM).expect("a session the agent closed");

        assert!(
            !guest.received.iter().any(|envelope| matches!(
                envelope.body,
                Some(envelope::Body::Request(ref request))
                    if matches!(request.kind, Some(request::Kind::ProbeGpu(_)))
            )),
            "a VM with no manifest is asked for no probe"
        );
    }
```

Extend the test module's `vmlord_agent_protocol::v1` import list with `GpuProbeCheck, GpuProbeCheckState, GpuProbeStep, GpuProbeVerdict, ProbeGpuResponse`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-platform agent_session`
Expected: FAIL to compile -- `PROBE_REQUEST_ID` does not exist.

- [ ] **Step 3: Write the implementation**

In `crates/platform/src/agent_session.rs`:

Add to the `vmlord_agent_protocol::v1` import list: `GpuProbeCheckState, GpuProbeVerdict, ProbeGpuRequest, ProbeGpuResponse`.

After `APPLY_REQUEST_ID`:

```rust
/// The id the host asks a guest to probe its GPU with.
///
/// One probe per session, after the recipe of the same session: the probe asks
/// about a userspace the recipe has just installed.
const PROBE_REQUEST_ID: u32 = APPLY_REQUEST_ID + 1;
```

In `serve`, add `let mut pending_probe = None;` beside `pending_recipe`, and extend the recipe arm and add the probe arm:

```rust
            Body::Response(response::Kind::ApplyGpuRecipe(report))
                if pending_recipe == Some(request_id) =>
            {
                pending_recipe = None;
                report_recipe(&report, vm_name);
                pending_probe = probe_gpu(stream, vm_name, &mut buffer)?;
            }
            Body::Response(response::Kind::ProbeGpu(report))
                if pending_probe == Some(request_id) =>
            {
                pending_probe = None;
                report_probe(&report, vm_name);
            }
```

Add the two functions beside `apply_recipe` and `report_recipe`:

```rust
/// Asks the guest whether anything renders, and says which id asked.
///
/// After the recipe of the same session, because what it looks at is what the
/// recipe has just installed. Once per session, for the same reason the recipe
/// is asked for once: the answer describes a moment, and the next session asks
/// again.
fn probe_gpu<S: Read + Write>(
    stream: &mut S,
    vm_name: &str,
    buffer: &mut Vec<u8>,
) -> Result<Option<u32>, SessionError> {
    let request = Envelope::request(
        PROBE_REQUEST_ID,
        request::Kind::ProbeGpu(ProbeGpuRequest {}),
    );
    frame::write(stream, &request, buffer).map_err(SessionError::Frame)?;
    log::debug!("VMLord asked the agent of VM \"{vm_name}\" to probe its GPU");

    Ok(Some(PROBE_REQUEST_ID))
}

/// Says what the guest found, at the volume each check earns.
///
/// Nothing is kept: the next session probes again, and turning a verdict into
/// a `VmGpuFacts` is the application layer's work.
fn report_probe(report: &ProbeGpuResponse, vm_name: &str) {
    match report.verdict() {
        GpuProbeVerdict::Renders => log::info!(
            "the agent of VM \"{vm_name}\" renders on {}",
            report.renderer
        ),
        verdict => log::warn!(
            "the agent of VM \"{vm_name}\" does not render on its GPU ({verdict:?})"
        ),
    }

    for check in &report.checks {
        match check.state() {
            GpuProbeCheckState::Ok | GpuProbeCheckState::Skipped => log::debug!(
                "the agent of VM \"{vm_name}\" GPU check {:?} ({:?}): {}",
                check.step(),
                check.state(),
                check.message
            ),
            state => log::warn!(
                "the agent of VM \"{vm_name}\" failed GPU check {:?} ({state:?}): {}",
                check.step(),
                check.message
            ),
        }
    }
}
```

Add the arm to `answer`, beside the recipe's:

```rust
        // Likewise: the probe is the guest's to run and the host's to ask for,
        // and there is no GPU to probe from a Windows host's side of this
        // socket.
        request::Kind::ProbeGpu(_) => Envelope::error(
            request_id,
            ErrorCode::UnsupportedRequest,
            "a GPU probe is the host's to ask for",
        ),
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-platform`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/platform/src/agent_session.rs
git commit -m "$(cat <<'EOF'
TASK-97: Ask a guest to probe its GPU once per session

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 6: Documentation and the whole-workspace checks

**Files:**
- Modify: `ARCHITECTURE.md` (the `### GPU: the guest's Ubuntu recipe` section, and a new `### GPU: the guest probe` section after it)

**Interfaces:**
- Consumes: everything above.
- Produces: nothing.

- [ ] **Step 1: Correct what the recipe section promises**

In `ARCHITECTURE.md`, in `### GPU: the guest's Ubuntu recipe`, replace the sentence "The userspace of the next task and the probe after it add values to `GpuRecipeStep` rather than messages of their own." with:

```markdown
The userspace stages add values to `GpuRecipeStep` rather than messages of
their own; the probe that follows them is a message of its own, because it
answers a different question -- what was done, against what works.
```

- [ ] **Step 2: Write the new section**

Insert after the end of `### GPU: the guest's Ubuntu recipe`:

```markdown
### GPU: the guest probe

A recipe says what was done and every one of its stages can report `OK` on a
guest where nothing draws. The probe is the other question: the host asks for
it once per session, right after the recipe report, and the guest answers with
one verdict and a list of checks. The schema gains `ProbeGpuRequest` and
`ProbeGpuResponse`, so the revision moved to **1.5**.

The verdict is the guest's, and it is the one thing on this message the host
does not re-derive: the guest is the only side that saw the output of the
programs it ran. `RENDERS` needs one hardware renderer from either API and not
both -- Ubuntu does not build Mesa with `microsoft-experimental`, so under the
`distro` policy Vulkan is lavapipe and GL is the only hardware path such a
guest has. `DEVICE_ONLY` is a `/dev/dxg` that opens with nothing above it, and
`NO_DEVICE` is the one check that ends the probe early.

Hardware is decided by a deny list -- `llvmpipe`, `softpipe`, `swrast`,
`lavapipe`, `SwiftShader` -- and never an allow list of the drivers this build
knows: an allow list reports "no hardware renderer" on the first stack nobody
wrote code against, and a new software rasteriser counting as hardware once is
the milder failure. Vulkan adds one fact of its own: a `deviceType` of
`PHYSICAL_DEVICE_TYPE_CPU` is software whatever the device calls itself.

The operation on the hardware is an external program, because the agent is a
static musl binary that can neither link nor `dlopen` `libEGL`. The programs
are Mesa's and Khronos's own -- `eglinfo` from `mesa-utils` and
`vulkaninfo --summary` from `vulkan-tools` -- installed present-first by the
`TOOLS` check and run through `/etc/profile.d/vmlord-gpu.sh`, the same file a
person gets over SSH: setting those variables again inside the agent would be
a second copy of the recipe's decision, and running through the file is what
proves the file. Vendor tools are quoted when the mounted WSL userspace
carries one and never decide anything, which is the difference between a probe
that is vendor-neutral and one that only knows one vendor.

The checks are `DEVICE`, `KERNEL_MODULE`, `LIBRARIES` (including the
`libd3d12.so` and `libdxcore.so` that `d3d12_dri.so` opens out of the host's
mounted userspace), `TOOLS`, `OPENGL`, `VULKAN` and `VENDOR`. Only a failed
`DEVICE` ends the run: a missing library is a fact worth reporting and never a
veto over a guest that turns out to draw anyway.
```

- [ ] **Step 3: Run every check the workspace has**

Run, in order:

```bash
cargo test -p vmlord-agent
cargo test -p vmlord-agent-protocol
cargo agent
cargo test-windows
cargo check-windows
```

Expected: all pass. Fix anything that does not before committing.

- [ ] **Step 4: Commit**

```bash
git add ARCHITECTURE.md
git commit -m "$(cat <<'EOF'
TASK-97: Document the guest GPU probe

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 5: Report what is left to prove by hand**

The probe's apt install and both renderer programs cannot run under `cargo test`. Say so in the hand-off, with the commands to run on a real GPU-PV VM over SSH:

```bash
ls -l /dev/dxg && lsmod | grep dxgkrnl
eglinfo -B | grep -i "renderer string"
vulkaninfo --summary | grep -E "driverName|deviceName|deviceType"
journalctl -u vmlord-agent | grep -i probe
```

Expected on a working guest: a renderer string naming `D3D12 (<adapter>)`, and the host log line `the agent of VM "<name>" renders on D3D12 (<adapter>)`.
