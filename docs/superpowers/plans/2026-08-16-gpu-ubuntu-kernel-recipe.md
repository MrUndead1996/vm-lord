# Ubuntu GPU kernel recipe implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A guest that has the GPU payload mounted ends up with a `dxgkrnl`
module that is built, installed, loaded on every boot, and reported stage by
stage to the host.

**Architecture:** The host sends one new request per session,
`ApplyGpuRecipe`, right after the attach report. The agent answers it with a
list of stages. Everything that decides is a pure function in
`crates/agent/src/gpu_recipe.rs`; everything that touches the system is in
`crates/agent/src/gpu_kernel.rs`, and every external program it runs goes
through one bounded runner in `crates/agent/src/command.rs`.

**Tech Stack:** Rust 2024, `prost`/`protox` for the wire (no `protoc`),
`libc` for `uname`, `serde_json` for the payload's `sources.json`, DKMS and
apt in the guest.

**Spec:** `docs/superpowers/specs/2026-08-16-gpu-ubuntu-kernel-recipe-design.md`

## Global Constraints

* Protocol revision moves from **1.2** to **1.3**; messages, fields and enum
  values may be added, never renumbered or repurposed.
* Nothing in this task may stop a VM or end a session: every failure is a
  stage in the report and a warning in the host log.
* The report carries facts only -- a stage and what became of it. No
  `VmGpuStatus`, no verdict, no UI (that is task #98).
* The guest never receives a host path, and the host never receives one from
  the guest beyond the free-text stage messages meant for its log.
* The agent must keep cross-compiling with `cargo agent` to
  `x86_64-unknown-linux-musl` with no C toolchain: no dependency may link
  against a system C library.
* Every external program the agent runs has a wall-clock budget: 300 s for
  apt, 900 s for `dkms build`, 30 s for everything else.
* Commit subjects are `TASK-95: <comment>`; the branch is
  `task-95-gpu-ubuntu-recipe`.
* Final checks for the whole plan: `cargo test -p vmlord-agent`,
  `cargo agent`, `cargo test-windows`, `cargo check-windows`.

---

### Task 1: The `ApplyGpuRecipe` exchange on the wire

**Files:**
- Modify: `crates/agent-protocol/proto/vmlord/agent/v1/agent.proto`
- Modify: `crates/agent-protocol/src/handshake.rs:19`
- Modify (regenerated): `crates/agent-protocol/proto/agent.descriptor.bin`
- Test: `crates/agent-protocol/tests/recipe.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `vmlord_agent_protocol::v1::{ApplyGpuRecipeRequest,
  ApplyGpuRecipeResponse, GpuRecipeStage, GpuRecipeStep, GpuRecipeStageState}`;
  `request::Kind::ApplyGpuRecipe(ApplyGpuRecipeRequest)`;
  `response::Kind::ApplyGpuRecipe(ApplyGpuRecipeResponse)`;
  `handshake::CURRENT_VERSION` is `1.3`.

- [ ] **Step 1: Write the failing test**

Create `crates/agent-protocol/tests/recipe.rs`:

```rust
//! What a recipe report has to survive on the wire.
//!
//! The stages are what the host logs and what task #98 will derive a status
//! from, so the shape they arrive in is worth a test of its own: a report is
//! written by a guest that may be older or newer than the host reading it.

use prost::Message;
use vmlord_agent_protocol::{
    handshake::CURRENT_VERSION,
    v1::{
        ApplyGpuRecipeRequest, ApplyGpuRecipeResponse, Envelope, GpuRecipeStage,
        GpuRecipeStageState, GpuRecipeStep, envelope, request, response,
    },
};

#[test]
fn a_recipe_report_survives_the_round_trip() {
    let report = Envelope::response(
        7,
        response::Kind::ApplyGpuRecipe(ApplyGpuRecipeResponse {
            stages: vec![
                GpuRecipeStage {
                    step: i32::from(GpuRecipeStep::Distribution),
                    state: i32::from(GpuRecipeStageState::Ok),
                    message: "ubuntu 26.04 amd64".to_owned(),
                },
                GpuRecipeStage {
                    step: i32::from(GpuRecipeStep::ModuleBuild),
                    state: i32::from(GpuRecipeStageState::Failed),
                    message: "dkms build failed".to_owned(),
                },
            ],
        }),
    );

    let decoded = Envelope::decode(report.encode_to_vec().as_slice()).expect("a decodable report");
    assert_eq!(decoded.request_id, 7);
    let Some(envelope::Body::Response(response)) = decoded.body else {
        panic!("a report is a response");
    };
    let Some(response::Kind::ApplyGpuRecipe(report)) = response.kind else {
        panic!("a report is a recipe report");
    };
    assert_eq!(report.stages.len(), 2);
    assert_eq!(report.stages[0].step(), GpuRecipeStep::Distribution);
    assert_eq!(report.stages[1].state(), GpuRecipeStageState::Failed);
}

#[test]
fn an_apply_request_carries_nothing_and_still_arrives() {
    // Empty on purpose: everything the guest needs is in the guest or in the
    // payload it was told to mount. The request must therefore survive as an
    // arm rather than as bytes -- an empty message encodes to nothing.
    let request = Envelope::request(3, request::Kind::ApplyGpuRecipe(ApplyGpuRecipeRequest {}));

    let decoded = Envelope::decode(request.encode_to_vec().as_slice()).expect("a decodable request");
    let Some(envelope::Body::Request(request)) = decoded.body else {
        panic!("an apply is a request");
    };
    assert!(matches!(
        request.kind,
        Some(request::Kind::ApplyGpuRecipe(_))
    ));
}

#[test]
fn the_recipe_report_belongs_to_revision_one_three() {
    assert_eq!((CURRENT_VERSION.major, CURRENT_VERSION.minor), (1, 3));
}
```

- [ ] **Step 2: Run it to make sure it fails**

Run: `cargo test -p vmlord-agent-protocol --test recipe`
Expected: FAIL — `ApplyGpuRecipeRequest` and friends do not exist.

- [ ] **Step 3: Add the messages to the schema**

In `crates/agent-protocol/proto/vmlord/agent/v1/agent.proto`, add the arm to
`Request` (replacing the "Field 5 onwards" comment):

```proto
    ApplyGpuRecipeRequest apply_gpu_recipe = 5;
  }

  // Field 6 onwards is where the rest of the protocol lands: the guest's GPU
  // report. It arrives with the task that implements it, so that the shape is
  // designed against working code rather than guessed at here.
}
```

the arm to `Response`:

```proto
    ApplyGpuRecipeResponse apply_gpu_recipe = 6;
```

and, after `GpuMountState`, the new messages:

```proto
// Asks the guest to apply its distribution's GPU recipe.
//
// Sent once per session, after the manifest the guest mounted, on a session
// that agreed `CAPABILITY_GPU`. Empty on purpose: everything the guest needs
// to decide is either in the guest -- `/etc/os-release`, `uname` -- or in the
// payload it was told to mount one message earlier, and a field here would be
// the host dictating something it cannot know better than the guest does.
message ApplyGpuRecipeRequest {}

// What the guest's recipe did, stage by stage.
//
// A list of stages and never a verdict: "the module built and /dev/dxg never
// appeared" and "the headers would not install" are one word apart in a
// summary and are different problems. Deriving a state from these facts
// belongs to the host's application layer.
message ApplyGpuRecipeResponse {
  // Every step of the recipe, in the order it was attempted, including the
  // ones that never ran. A report that stops at the failure would leave the
  // host guessing whether the rest was skipped or the agent hung up.
  repeated GpuRecipeStage stages = 1;
}

message GpuRecipeStage {
  GpuRecipeStep step = 1;

  GpuRecipeStageState state = 2;

  // Free text for the host's log: the reason a stage was skipped, or the tail
  // of what the program that failed had to say. `state` is what a peer
  // branches on.
  string message = 3;
}

// One step of a guest's GPU recipe.
//
// The kernel steps arrive with the recipe that has them; a distribution that
// needs a different sequence gets its own values rather than a reinterpretation
// of these.
enum GpuRecipeStep {
  GPU_RECIPE_STEP_UNSPECIFIED = 0;

  // Whether this guest is one the agent has a recipe for at all.
  GPU_RECIPE_STEP_DISTRIBUTION = 1;

  // The mounted payload: its provenance, its target and its module sources.
  GPU_RECIPE_STEP_PAYLOAD = 2;

  // The compiler, DKMS and the running kernel's headers.
  GPU_RECIPE_STEP_BUILD_DEPENDENCIES = 3;

  // The module sources, staged where the build framework expects them.
  GPU_RECIPE_STEP_MODULE_SOURCE = 4;

  // Building and installing the module for the running kernel.
  GPU_RECIPE_STEP_MODULE_BUILD = 5;

  // Loading the module now and on every boot after this one.
  GPU_RECIPE_STEP_MODULE_LOAD = 6;

  // The device node the module is there to create.
  GPU_RECIPE_STEP_DEVICE = 7;
}

enum GpuRecipeStageState {
  GPU_RECIPE_STAGE_STATE_UNSPECIFIED = 0;

  // The stage did what it is for.
  GPU_RECIPE_STAGE_STATE_OK = 1;

  // The stage did not have to run: the guest already satisfies it, or the
  // recipe stopped before reaching it.
  GPU_RECIPE_STAGE_STATE_SKIPPED = 2;

  // The stage ran and did not succeed.
  GPU_RECIPE_STAGE_STATE_FAILED = 3;
}
```

- [ ] **Step 4: Move the revision to 1.3**

In `crates/agent-protocol/src/handshake.rs`:

```rust
pub const CURRENT_VERSION: ProtocolVersion = ProtocolVersion { major: 1, minor: 3 };
```

- [ ] **Step 5: Refresh the checked-in descriptor**

Run: `VMLORD_REFRESH_DESCRIPTOR=1 cargo test -p vmlord-agent-protocol`

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p vmlord-agent-protocol`
Expected: PASS, including `descriptor.rs` against the refreshed binary.

- [ ] **Step 7: Commit**

```bash
git add crates/agent-protocol
git commit -m "TASK-95: Carry a GPU recipe report on the wire"
```

---

### Task 2: A bounded runner for external programs

**Files:**
- Create: `crates/agent/src/command.rs`
- Modify: `crates/agent/src/main.rs` (add `mod command;`)

**Interfaces:**
- Consumes: nothing.
- Produces: `command::run(program: &str, arguments: &[&str], environment:
  &[(&str, &str)], budget: Duration) -> Outcome`;
  `command::Outcome { ending: Ending, output: String }` with
  `Outcome::succeeded(&self) -> bool`; `command::Ending::{Exited(i32),
  TimedOut, NotStarted}`.

- [ ] **Step 1: Write the failing tests**

Create `crates/agent/src/command.rs` with the module documentation and the
tests only:

```rust
//! Running one external program, with a bound on how long it may take.
//!
//! The recipe runs `apt-get`, `dkms` and `modprobe`, and all three are
//! distribution-owned operations with no library form. What they have in
//! common is that none of them may run forever: a hung `apt-get` behind a
//! broken NAT would be an agent that never answers its host again.
//!
//! Output is captured on threads rather than read after the wait, because a
//! program that fills its pipe while nobody reads it blocks, and `dkms build`
//! produces far more than a pipe holds.

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{Ending, run, tail};

    #[test]
    fn a_program_that_succeeds_reports_its_output() {
        let outcome = run(
            "/bin/sh",
            &["-c", "printf 'first\\nsecond\\n'"],
            &[],
            Duration::from_secs(10),
        );

        assert_eq!(outcome.ending, Ending::Exited(0));
        assert!(outcome.succeeded());
        assert!(outcome.output.contains("second"), "{}", outcome.output);
    }

    #[test]
    fn a_program_that_fails_keeps_its_code_and_its_standard_error() {
        let outcome = run(
            "/bin/sh",
            &["-c", "echo bad 1>&2; exit 3"],
            &[],
            Duration::from_secs(10),
        );

        assert_eq!(outcome.ending, Ending::Exited(3));
        assert!(!outcome.succeeded());
        assert!(outcome.output.contains("bad"), "{}", outcome.output);
    }

    #[test]
    fn the_environment_reaches_the_program() {
        let outcome = run(
            "/bin/sh",
            &["-c", "printf '%s' \"$DEBIAN_FRONTEND\""],
            &[("DEBIAN_FRONTEND", "noninteractive")],
            Duration::from_secs(10),
        );

        assert_eq!(outcome.output.trim(), "noninteractive");
    }

    #[test]
    fn a_program_that_outruns_its_budget_is_killed() {
        let started = Instant::now();
        let outcome = run("/bin/sh", &["-c", "sleep 30"], &[], Duration::from_millis(200));

        assert_eq!(outcome.ending, Ending::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the budget was not enforced"
        );
    }

    #[test]
    fn a_program_that_cannot_be_started_is_not_a_panic() {
        let outcome = run(
            "/vmlord/no/such/program",
            &[],
            &[],
            Duration::from_secs(10),
        );

        assert_eq!(outcome.ending, Ending::NotStarted);
        assert!(!outcome.output.is_empty());
    }

    #[test]
    fn only_the_last_lines_of_output_are_kept() {
        let long: String = (0..100).map(|line| format!("line {line}\n")).collect();

        let kept = tail(&long, 3);

        assert_eq!(kept, "line 97\nline 98\nline 99");
    }
}
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p vmlord-agent command::`
Expected: FAIL — `run`, `tail` and `Ending` do not exist. (`mod command;` must
already be in `main.rs` for the test to compile; add it in this step.)

- [ ] **Step 3: Write the implementation**

Above the test module in `crates/agent/src/command.rs`:

```rust
use std::{
    io::Read,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

/// How many lines of a program's output are kept for a stage's message.
const KEPT_LINES: usize = 20;

/// How often a running program is asked whether it has finished.
const POLL: Duration = Duration::from_millis(50);

/// How a program ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ending {
    /// It exited on its own, with this code. A signal counts as a non-zero
    /// code that this build does not need to tell apart.
    Exited(i32),
    /// It outran its budget and was killed.
    TimedOut,
    /// It could not be started: not installed, or not executable.
    NotStarted,
}

/// What running one program produced.
pub struct Outcome {
    pub ending: Ending,
    /// The last [`KEPT_LINES`] lines of its standard output and error.
    pub output: String,
}

impl Outcome {
    /// Whether the program ran and said it succeeded.
    pub fn succeeded(&self) -> bool {
        self.ending == Ending::Exited(0)
    }
}

/// Runs `program` with a wall-clock budget, and keeps the tail of its output.
///
/// Never fails: a program that cannot be started, one that fails and one that
/// hangs are three endings of the same call, because every caller here turns
/// all three into the same thing -- a stage that did not succeed.
pub fn run(
    program: &str,
    arguments: &[&str],
    environment: &[(&str, &str)],
    budget: Duration,
) -> Outcome {
    let mut command = Command::new(program);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, value) in environment {
        command.env(name, value);
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return Outcome {
                ending: Ending::NotStarted,
                output: format!("{program} could not be started: {error}"),
            };
        }
    };

    // Both pipes are drained on their own threads. A program that fills a
    // pipe nobody reads blocks in `write`, which would turn every long build
    // into a timeout.
    let standard_output = child.stdout.take().map(drain);
    let standard_error = child.stderr.take().map(drain);

    let started = Instant::now();
    let ending = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ending::Exited(status.code().unwrap_or(-1)),
            Ok(None) => {}
            // Asking a child whether it has finished does not fail short of a
            // kernel-level surprise, and there is nothing to report about one
            // but a non-zero ending.
            Err(_) => {
                let _ = child.kill();
                break Ending::Exited(-1);
            }
        }
        if started.elapsed() >= budget {
            let _ = child.kill();
            let _ = child.wait();
            break Ending::TimedOut;
        }
        thread::sleep(POLL);
    };

    let mut output = String::new();
    for reader in [standard_output, standard_error].into_iter().flatten() {
        output.push_str(&reader.join().unwrap_or_default());
    }

    Outcome {
        ending,
        output: tail(&output, KEPT_LINES),
    }
}

/// Reads one pipe to its end on a thread of its own.
fn drain<R: Read + Send + 'static>(mut reader: R) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = reader.read_to_end(&mut bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

/// The last `lines` lines of `output`, without a trailing newline.
///
/// A stage's message ends up in the host's log, and the useful part of a
/// failing build is at the end of it.
fn tail(output: &str, lines: usize) -> String {
    let kept: Vec<&str> = output
        .lines()
        .rev()
        .take(lines)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    kept.join("\n")
}
```

Note on the `Err(error)` arm: `try_wait` failing is a kernel-level surprise
with nothing to report but a non-zero ending; bind it as `Err(_)` if the
compiler warns about the unused name.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-agent command::`
Expected: PASS (6 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/agent/src/command.rs crates/agent/src/main.rs
git commit -m "TASK-95: Run a guest program under a time budget"
```

---

### Task 3: What the guest is, and whether the recipe applies to it

**Files:**
- Create: `crates/agent/src/gpu_recipe.rs`
- Modify: `crates/agent/src/main.rs` (add `mod gpu_recipe;`)
- Modify: `crates/agent/Cargo.toml` (add `serde_json`)

**Interfaces:**
- Consumes: nothing.
- Produces, all in `gpu_recipe`:
  `GuestFacts { distribution: String, release: String, architecture: String,
  kernel_release: String }`;
  `parse_os_release(text: &str) -> Option<(String, String)>`;
  `GpuRecipe::Ubuntu` with `recipe_for(distribution: &str) -> Option<GpuRecipe>`;
  `PayloadTarget { distribution, release, architecture, kernel_release }` with
  `parse_payload_target(json: &str) -> Result<PayloadTarget, String>`;
  `Applicability::{Applies { kernel: Option<String> }, NotApplicable(String)}`
  with `applicability(&PayloadTarget, &GuestFacts) -> Applicability`;
  `DkmsPackage { name: String, version: String }` with
  `parse_dkms_conf(text: &str) -> Result<DkmsPackage, String>`;
  `module_is_loaded(proc_modules: &str, module: &str) -> bool`;
  `dkms_reports_installed(status: &str, package: &DkmsPackage, kernel: &str) -> bool`.

- [ ] **Step 1: Add the one new dependency**

In `crates/agent/Cargo.toml`, under `[dependencies]`:

```toml
# The payload states what it was built for in its own `sources.json`, and the
# guest is what compares that with itself. Pure Rust with no C library behind
# it, so `cargo agent` still cross-compiles to musl with no toolchain.
serde_json = { version = "1", default-features = false, features = ["std"] }
```

- [ ] **Step 2: Write the failing tests**

Create `crates/agent/src/gpu_recipe.rs` with its module documentation and the
tests only:

```rust
//! Which guests the GPU recipe applies to, and what the payload says it is.
//!
//! Everything here is a function of text: `/etc/os-release`, the payload's
//! `sources.json`, a `dkms.conf`, `/proc/modules`, the output of
//! `dkms status`. That is deliberate -- it is what makes the decisions of a
//! recipe testable on a machine that is neither Ubuntu nor a Hyper-V guest,
//! while `gpu_kernel` keeps the parts that need one.

#[cfg(test)]
mod tests {
    use super::{
        Applicability, DkmsPackage, GpuRecipe, GuestFacts, PayloadTarget, applicability,
        dkms_reports_installed, module_is_loaded, parse_dkms_conf, parse_os_release, recipe_for,
    };

    fn ubuntu_guest() -> GuestFacts {
        GuestFacts {
            distribution: "ubuntu".to_owned(),
            release: "26.04".to_owned(),
            architecture: "amd64".to_owned(),
            kernel_release: "7.0.0-14-generic".to_owned(),
        }
    }

    fn payload_for(release: &str, architecture: &str, kernel: &str) -> PayloadTarget {
        PayloadTarget {
            distribution: "ubuntu".to_owned(),
            release: release.to_owned(),
            architecture: architecture.to_owned(),
            kernel_release: kernel.to_owned(),
        }
    }

    #[test]
    fn os_release_values_are_read_with_or_without_quotes() {
        let text = "PRETTY_NAME=\"Ubuntu 26.04 LTS\"\nID=ubuntu\nVERSION_ID=\"26.04\"\n";

        assert_eq!(
            parse_os_release(text),
            Some(("ubuntu".to_owned(), "26.04".to_owned()))
        );
    }

    #[test]
    fn an_os_release_without_an_id_names_nothing() {
        assert_eq!(parse_os_release("VERSION_ID=\"26.04\"\n"), None);
        assert_eq!(parse_os_release(""), None);
    }

    #[test]
    fn only_ubuntu_has_a_recipe_in_this_build() {
        assert!(matches!(recipe_for("ubuntu"), Some(GpuRecipe::Ubuntu)));
        assert!(recipe_for("debian").is_none());
        assert!(recipe_for("").is_none());
    }

    #[test]
    fn a_payload_built_for_this_guest_applies() {
        let applies = applicability(
            &payload_for("26.04", "amd64", "7.0.0-14-generic"),
            &ubuntu_guest(),
        );

        assert!(matches!(applies, Applicability::Applies { kernel: None }));
    }

    #[test]
    fn another_kernel_is_recorded_and_never_refuses() {
        // DKMS builds against the headers of the running kernel, so an exact
        // match is not needed to compile -- and requiring one would mean an
        // unattended kernel upgrade kills GPU-PV until a payload is repacked.
        let applies = applicability(
            &payload_for("26.04", "amd64", "7.0.0-11-generic"),
            &ubuntu_guest(),
        );

        let Applicability::Applies { kernel: Some(note) } = applies else {
            panic!("a different kernel must still apply");
        };
        assert!(note.contains("7.0.0-11-generic"), "{note}");
        assert!(note.contains("7.0.0-14-generic"), "{note}");
    }

    #[test]
    fn another_release_or_architecture_does_not_apply() {
        for payload in [
            payload_for("24.04", "amd64", "7.0.0-14-generic"),
            payload_for("26.04", "arm64", "7.0.0-14-generic"),
        ] {
            assert!(matches!(
                applicability(&payload, &ubuntu_guest()),
                Applicability::NotApplicable(_)
            ));
        }
    }

    #[test]
    fn a_payload_target_is_read_out_of_its_sources_document() {
        let document = r#"{
          "schema_version": 1,
          "target": {
            "distribution": "ubuntu",
            "release": "26.04",
            "architecture": "amd64",
            "kernel_release": "7.0.0-14-generic",
            "payload_abi": 1
          },
          "mesa_policy": "bundled"
        }"#;

        let target = super::parse_payload_target(document).expect("a readable target");

        assert_eq!(target.release, "26.04");
        assert_eq!(target.kernel_release, "7.0.0-14-generic");
    }

    #[test]
    fn a_sources_document_without_a_target_is_an_error() {
        for document in ["{}", "{\"target\": {}}", "not json"] {
            assert!(super::parse_payload_target(document).is_err(), "{document}");
        }
    }

    #[test]
    fn a_dkms_conf_names_its_package_and_version() {
        let text = "PACKAGE_NAME=\"dxgkrnl\"\nPACKAGE_VERSION=2.0.3\nAUTOINSTALL=\"yes\"\n";

        let package = parse_dkms_conf(text).expect("a readable dkms.conf");

        assert_eq!(package.name, "dxgkrnl");
        assert_eq!(package.version, "2.0.3");
    }

    #[test]
    fn a_dkms_conf_missing_a_field_is_an_error() {
        assert!(parse_dkms_conf("PACKAGE_NAME=dxgkrnl\n").is_err());
        assert!(parse_dkms_conf("").is_err());
    }

    #[test]
    fn a_loaded_module_is_recognised_by_name_alone() {
        let modules = "dxgkrnl 315392 0 - Live 0x0000000000000000\nvsock 45056 2 - Live 0x0\n";

        assert!(module_is_loaded(modules, "dxgkrnl"));
        assert!(!module_is_loaded(modules, "dxg"));
        assert!(!module_is_loaded("", "dxgkrnl"));
    }

    #[test]
    fn dkms_status_says_whether_this_kernel_already_has_the_module() {
        let package = DkmsPackage {
            name: "dxgkrnl".to_owned(),
            version: "2.0.3".to_owned(),
        };

        assert!(dkms_reports_installed(
            "dxgkrnl/2.0.3, 7.0.0-14-generic, x86_64: installed",
            &package,
            "7.0.0-14-generic"
        ));
        // Built for another kernel, or built and not installed: both are work
        // still to do for the kernel this guest is running.
        assert!(!dkms_reports_installed(
            "dxgkrnl/2.0.3, 7.0.0-11-generic, x86_64: installed",
            &package,
            "7.0.0-14-generic"
        ));
        assert!(!dkms_reports_installed(
            "dxgkrnl/2.0.3, 7.0.0-14-generic, x86_64: built",
            &package,
            "7.0.0-14-generic"
        ));
        assert!(!dkms_reports_installed("", &package, "7.0.0-14-generic"));
    }
}
```

- [ ] **Step 3: Run them to make sure they fail**

Run: `cargo test -p vmlord-agent gpu_recipe::`
Expected: FAIL — nothing in the module exists yet.

- [ ] **Step 4: Write the implementation**

Above the tests in `crates/agent/src/gpu_recipe.rs`:

```rust
/// What the guest says it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuestFacts {
    /// `ID` from `/etc/os-release`, lowercased by the file's own convention.
    pub distribution: String,
    /// `VERSION_ID` from `/etc/os-release`.
    pub release: String,
    /// The Debian architecture name, not the machine name `uname` gives.
    pub architecture: String,
    /// `uname -r`: the kernel that is running now, which is the one DKMS
    /// builds against.
    pub kernel_release: String,
}

/// A distribution this build knows how to bring a GPU up on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuRecipe {
    Ubuntu,
}

/// The recipe for a distribution, or nothing for one with none.
///
/// The whole "unsupported release gives Degraded and does not stop the VM"
/// rule starts here: a guest with no recipe is a skipped first stage, not an
/// error.
pub fn recipe_for(distribution: &str) -> Option<GpuRecipe> {
    match distribution {
        "ubuntu" => Some(GpuRecipe::Ubuntu),
        _ => None,
    }
}

/// Reads `ID` and `VERSION_ID` out of an `/etc/os-release`.
pub fn parse_os_release(text: &str) -> Option<(String, String)> {
    let mut id = None;
    let mut version = None;
    for line in text.lines() {
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"').to_owned();
        match name.trim() {
            "ID" => id = Some(value),
            "VERSION_ID" => version = Some(value),
            _ => {}
        }
    }

    Some((id?, version?))
}

/// What a payload says it was built for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PayloadTarget {
    pub distribution: String,
    pub release: String,
    pub architecture: String,
    pub kernel_release: String,
}

/// Reads the target out of a payload's `sources.json`.
///
/// Only the target: the rest of that document is provenance the host has
/// already verified against the catalog, and re-deciding it here would be a
/// second validation boundary that could disagree with the first.
pub fn parse_payload_target(json: &str) -> Result<PayloadTarget, String> {
    let document: serde_json::Value =
        serde_json::from_str(json).map_err(|error| format!("sources.json is unreadable: {error}"))?;
    let target = document
        .get("target")
        .ok_or_else(|| "sources.json names no target".to_owned())?;

    let field = |name: &str| -> Result<String, String> {
        target
            .get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("the payload target has no {name}"))
    };

    Ok(PayloadTarget {
        distribution: field("distribution")?,
        release: field("release")?,
        architecture: field("architecture")?,
        kernel_release: field("kernel_release")?,
    })
}

/// Whether a payload's recipe applies to this guest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Applicability {
    /// It applies. `kernel` carries a note when the payload was proven on a
    /// different kernel than the one running.
    Applies { kernel: Option<String> },
    /// It does not, and this is why.
    NotApplicable(String),
}

/// Compares what the payload was built for with what the guest is.
///
/// Distribution, release and architecture are the hard gate. The kernel is
/// not: DKMS builds against the running kernel's headers, so the payload's
/// `kernel_release` records what the recipe was proven on rather than what it
/// requires.
pub fn applicability(payload: &PayloadTarget, guest: &GuestFacts) -> Applicability {
    for (what, expected, actual) in [
        ("distribution", &payload.distribution, &guest.distribution),
        ("release", &payload.release, &guest.release),
        ("architecture", &payload.architecture, &guest.architecture),
    ] {
        if expected != actual {
            return Applicability::NotApplicable(format!(
                "the payload was built for {what} {expected} and this guest is {actual}"
            ));
        }
    }

    let kernel = (payload.kernel_release != guest.kernel_release).then(|| {
        format!(
            "the payload was proven on kernel {} and this guest runs {}",
            payload.kernel_release, guest.kernel_release
        )
    });
    Applicability::Applies { kernel }
}

/// The module package a `dkms.conf` describes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DkmsPackage {
    pub name: String,
    pub version: String,
}

/// Reads `PACKAGE_NAME` and `PACKAGE_VERSION` out of a `dkms.conf`.
///
/// The payload names its own package and version rather than the agent
/// hard-coding them: a repacked payload with a newer module must not need a
/// new agent.
pub fn parse_dkms_conf(text: &str) -> Result<DkmsPackage, String> {
    let mut name = None;
    let mut version = None;
    for line in text.lines() {
        let Some((field, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"').to_owned();
        match field.trim() {
            "PACKAGE_NAME" => name = Some(value),
            "PACKAGE_VERSION" => version = Some(value),
            _ => {}
        }
    }

    match (name, version) {
        (Some(name), Some(version)) if !name.is_empty() && !version.is_empty() => {
            Ok(DkmsPackage { name, version })
        }
        _ => Err("dkms.conf names no package and version".to_owned()),
    }
}

/// Whether `/proc/modules` says the module is loaded.
pub fn module_is_loaded(proc_modules: &str, module: &str) -> bool {
    proc_modules
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .any(|loaded| loaded == module)
}

/// Whether `dkms status` says this kernel already has the module installed.
///
/// Installed and not merely built: a built module that was never installed is
/// not in `/lib/modules`, and `modprobe` would not find it.
pub fn dkms_reports_installed(status: &str, package: &DkmsPackage, kernel: &str) -> bool {
    status.lines().any(|line| {
        line.contains(&format!("{}/{}", package.name, package.version))
            && line.contains(kernel)
            && line.contains("installed")
    })
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p vmlord-agent gpu_recipe::`
Expected: PASS (12 tests).

- [ ] **Step 6: Verify the cross-build still needs no toolchain**

Run: `cargo agent`
Expected: builds `x86_64-unknown-linux-musl` cleanly with the new dependency.

- [ ] **Step 7: Commit**

```bash
git add crates/agent/Cargo.toml crates/agent/src/gpu_recipe.rs crates/agent/src/main.rs Cargo.lock
git commit -m "TASK-95: Decide which guests the GPU recipe applies to"
```

---

### Task 4: The report a recipe fills in

**Files:**
- Modify: `crates/agent/src/gpu_recipe.rs`

**Interfaces:**
- Consumes: task 1's `GpuRecipeStage`, `GpuRecipeStep`, `GpuRecipeStageState`.
- Produces: `gpu_recipe::Report` with `Report::new()`,
  `ok(&mut self, GpuRecipeStep, impl Into<String>)`,
  `skipped(&mut self, GpuRecipeStep, impl Into<String>)`,
  `failed(&mut self, GpuRecipeStep, impl Into<String>)` and
  `finish(self, reason: &str) -> Vec<GpuRecipeStage>`; `gpu_recipe::STEPS`.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module of `crates/agent/src/gpu_recipe.rs`:

```rust
    use vmlord_agent_protocol::v1::{GpuRecipeStageState, GpuRecipeStep};

    use super::{Report, STEPS};

    #[test]
    fn a_finished_report_has_every_step_exactly_once_and_in_order() {
        let mut report = Report::new();
        report.ok(GpuRecipeStep::Distribution, "ubuntu 26.04 amd64");

        let stages = report.finish("the recipe stopped before this stage");

        assert_eq!(stages.len(), STEPS.len());
        for (stage, step) in stages.iter().zip(STEPS) {
            assert_eq!(stage.step(), step);
        }
        assert_eq!(stages[0].state(), GpuRecipeStageState::Ok);
        assert_eq!(stages[1].state(), GpuRecipeStageState::Skipped);
        assert_eq!(stages[1].message, "the recipe stopped before this stage");
    }

    #[test]
    fn the_steps_a_recipe_never_reached_carry_the_reason_it_stopped() {
        // A report that stops at the failure would leave the host guessing
        // whether the rest was skipped or the agent hung up.
        let mut report = Report::new();
        report.ok(GpuRecipeStep::Distribution, "ubuntu");
        report.ok(GpuRecipeStep::Payload, "dxgkrnl 2.0.3");
        report.failed(GpuRecipeStep::BuildDependencies, "apt-get exited with 100");

        let stages = report.finish("the build dependencies were not installed");

        assert_eq!(stages[2].state(), GpuRecipeStageState::Failed);
        assert!(stages[2].message.contains("100"));
        for stage in &stages[3..] {
            assert_eq!(stage.state(), GpuRecipeStageState::Skipped);
            assert_eq!(stage.message, "the build dependencies were not installed");
        }
    }

    #[test]
    fn a_stage_recorded_twice_keeps_the_first_answer() {
        // Nothing should record a step twice; if something does, the report
        // must not grow a second copy of a step the host reads by position.
        let mut report = Report::new();
        report.ok(GpuRecipeStep::Device, "/dev/dxg opens");
        report.failed(GpuRecipeStep::Device, "gone");

        let stages = report.finish("unreached");

        let device: Vec<_> = stages
            .iter()
            .filter(|stage| stage.step() == GpuRecipeStep::Device)
            .collect();
        assert_eq!(device.len(), 1);
        assert_eq!(device[0].state(), GpuRecipeStageState::Ok);
    }
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p vmlord-agent gpu_recipe::`
Expected: FAIL — `Report` and `STEPS` do not exist.

- [ ] **Step 3: Write the implementation**

In `crates/agent/src/gpu_recipe.rs`, above the tests:

```rust
use vmlord_agent_protocol::v1::{GpuRecipeStage, GpuRecipeStageState, GpuRecipeStep};

/// Every step of the recipe, in the order it is attempted.
///
/// The order is the report's order, and the report is what the host logs, so
/// it is written once here rather than implied by the sequence of calls in
/// `gpu_kernel`.
pub const STEPS: [GpuRecipeStep; 7] = [
    GpuRecipeStep::Distribution,
    GpuRecipeStep::Payload,
    GpuRecipeStep::BuildDependencies,
    GpuRecipeStep::ModuleSource,
    GpuRecipeStep::ModuleBuild,
    GpuRecipeStep::ModuleLoad,
    GpuRecipeStep::Device,
];

/// What a recipe run has found out so far.
///
/// Collected rather than sent as it goes, because a stage list is one answer
/// to one request: the host asked what the recipe did, not to be narrated at.
#[derive(Default)]
pub struct Report {
    recorded: Vec<GpuRecipeStage>,
}

impl Report {
    pub fn new() -> Self {
        Self {
            recorded: Vec::with_capacity(STEPS.len()),
        }
    }

    pub fn ok(&mut self, step: GpuRecipeStep, message: impl Into<String>) {
        self.record(step, GpuRecipeStageState::Ok, message.into());
    }

    pub fn skipped(&mut self, step: GpuRecipeStep, message: impl Into<String>) {
        self.record(step, GpuRecipeStageState::Skipped, message.into());
    }

    pub fn failed(&mut self, step: GpuRecipeStep, message: impl Into<String>) {
        self.record(step, GpuRecipeStageState::Failed, message.into());
    }

    fn record(&mut self, step: GpuRecipeStep, state: GpuRecipeStageState, message: String) {
        if self.recorded.iter().any(|stage| stage.step() == step) {
            return;
        }
        self.recorded.push(GpuRecipeStage {
            step: i32::from(step),
            state: i32::from(state),
            message,
        });
    }

    /// The whole report: what happened, and `reason` for what never ran.
    pub fn finish(mut self, reason: &str) -> Vec<GpuRecipeStage> {
        for step in STEPS {
            self.skipped(step, reason);
        }
        self.recorded
            .sort_by_key(|stage| STEPS.iter().position(|step| *step == stage.step()));
        self.recorded
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-agent gpu_recipe::`
Expected: PASS (15 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/agent/src/gpu_recipe.rs
git commit -m "TASK-95: Report a GPU recipe stage by stage"
```

---

### Task 5: Applying the recipe in the guest

**Files:**
- Create: `crates/agent/src/gpu_kernel.rs`
- Modify: `crates/agent/src/main.rs` (add `mod gpu_kernel;`)

**Interfaces:**
- Consumes: `command::{run, Ending, Outcome}`, everything from `gpu_recipe`,
  `gpu_targets::PAYLOAD`.
- Produces: `gpu_kernel::apply(stopping: &AtomicBool) -> Vec<GpuRecipeStage>`;
  `gpu_kernel::copy_tree(source: &Path, destination: &Path) -> io::Result<bool>`.

- [ ] **Step 1: Write the failing tests**

Create `crates/agent/src/gpu_kernel.rs` with its documentation and the tests
only:

```rust
//! Applying the Ubuntu GPU recipe to the guest this agent runs in.
//!
//! What decides is in `gpu_recipe`; what is here is the part that needs an
//! Ubuntu guest with a payload mounted: reading the guest's own files,
//! staging the module sources somewhere DKMS can write beside them, running
//! apt, DKMS and `modprobe`, and looking at `/dev/dxg` afterwards.
//!
//! Nothing here fails as a whole. Every stage that does not succeed is a
//! stage in the report and a VM that keeps running with less GPU than it
//! asked for.

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf, sync::atomic::AtomicU64};

    use super::copy_tree;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temporary(label: &str) -> PathBuf {
        let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "vmlord-agent-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("a temporary directory");
        path
    }

    #[test]
    fn a_tree_is_copied_whole() {
        let source = temporary("copy-source");
        let destination = temporary("copy-destination").join("staged");
        fs::create_dir(source.join("include")).unwrap();
        fs::write(source.join("dkms.conf"), b"PACKAGE_NAME=dxgkrnl\n").unwrap();
        fs::write(source.join("include/d3dkmthk.h"), b"header\n").unwrap();

        let changed = copy_tree(&source, &destination).expect("a copied tree");

        assert!(changed);
        assert_eq!(
            fs::read(destination.join("include/d3dkmthk.h")).unwrap(),
            b"header\n"
        );
    }

    #[test]
    fn copying_the_same_tree_again_changes_nothing() {
        // A reconnect must not rewrite the tree DKMS is registered against:
        // rewriting it is what would make DKMS rebuild on every session.
        let source = temporary("idempotent-source");
        let destination = temporary("idempotent-destination").join("staged");
        fs::write(source.join("dkms.conf"), b"PACKAGE_NAME=dxgkrnl\n").unwrap();

        assert!(copy_tree(&source, &destination).unwrap());
        assert!(!copy_tree(&source, &destination).unwrap());
    }

    #[test]
    fn a_changed_file_is_copied_over() {
        let source = temporary("changed-source");
        let destination = temporary("changed-destination").join("staged");
        fs::write(source.join("dkms.conf"), b"PACKAGE_VERSION=2.0.3\n").unwrap();
        copy_tree(&source, &destination).unwrap();

        fs::write(source.join("dkms.conf"), b"PACKAGE_VERSION=2.0.4\n").unwrap();

        assert!(copy_tree(&source, &destination).unwrap());
        assert_eq!(
            fs::read(destination.join("dkms.conf")).unwrap(),
            b"PACKAGE_VERSION=2.0.4\n"
        );
    }
}
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p vmlord-agent gpu_kernel::`
Expected: FAIL — `copy_tree` does not exist.

- [ ] **Step 3: Write the implementation**

In `crates/agent/src/gpu_kernel.rs`, above the tests:

```rust
use std::{
    fs,
    io,
    os::unix::fs::FileTypeExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use vmlord_agent_protocol::v1::{GpuRecipeStage, GpuRecipeStep};

use crate::{
    command::{self, Outcome},
    gpu_recipe::{
        Applicability, DkmsPackage, GuestFacts, Report, applicability, dkms_reports_installed,
        module_is_loaded, parse_dkms_conf, parse_os_release, parse_payload_target, recipe_for,
    },
    gpu_targets::PAYLOAD,
};

/// The kernel module this recipe exists to deliver.
const MODULE: &str = "dxgkrnl";

/// The device node the module creates, and the point of the whole recipe.
const DEVICE: &str = "/dev/dxg";

/// Where the module is asked for on every boot.
///
/// A module loaded only by `modprobe` is gone after the next reboot, and
/// GPU-PV then breaks silently on a VM that was fine yesterday.
const MODULES_LOAD: &str = "/etc/modules-load.d/vmlord-dxgkrnl.conf";

/// Where DKMS expects to find the sources of a package.
const DKMS_SOURCES: &str = "/usr/src";

/// Where DKMS leaves the log of a build that failed.
const DKMS_TREE: &str = "/var/lib/dkms";

const APT_BUDGET: Duration = Duration::from_secs(300);
const BUILD_BUDGET: Duration = Duration::from_secs(900);
const SHORT_BUDGET: Duration = Duration::from_secs(30);

/// Applies this guest's GPU recipe and says what happened, stage by stage.
///
/// Called once per session, after the shares of the same session were
/// mounted. Most calls do almost nothing: a guest whose module is already
/// built, installed and loaded short-circuits before the first stage that
/// would run a program.
pub fn apply(stopping: &AtomicBool) -> Vec<GpuRecipeStage> {
    let mut report = Report::new();
    let reason = match run_stages(&mut report, stopping) {
        Ok(()) => "the recipe did not need this stage".to_owned(),
        Err(reason) => reason,
    };
    report.finish(&reason)
}

/// The stages, in order, stopping at the first one that ends the recipe.
///
/// `Err` carries what the stages that never ran are reported with.
fn run_stages(report: &mut Report, stopping: &AtomicBool) -> Result<(), String> {
    let guest = guest_facts()?;
    if recipe_for(&guest.distribution).is_none() {
        let reason = format!(
            "vmlord-agent has no GPU recipe for {} {}",
            guest.distribution, guest.release
        );
        report.skipped(GpuRecipeStep::Distribution, reason.clone());
        return Err(reason);
    }
    report.ok(
        GpuRecipeStep::Distribution,
        format!(
            "{} {} {} on kernel {}",
            guest.distribution, guest.release, guest.architecture, guest.kernel_release
        ),
    );

    let package = payload_stage(report, &guest)?;
    halted(stopping)?;

    if module_is_loaded(&read(Path::new("/proc/modules")), MODULE) && device_is_usable() {
        let already = format!("{MODULE} is already loaded and {DEVICE} answers");
        for step in [
            GpuRecipeStep::BuildDependencies,
            GpuRecipeStep::ModuleSource,
            GpuRecipeStep::ModuleBuild,
        ] {
            report.skipped(step, already.clone());
        }
    } else {
        dependencies_stage(report, &guest)?;
        halted(stopping)?;
        source_stage(report, &package)?;
        halted(stopping)?;
        build_stage(report, &package, &guest)?;
        halted(stopping)?;
    }

    load_stage(report)?;
    device_stage(report);
    Ok(())
}

/// Stops the recipe when the guest is going down.
///
/// A kernel build is minutes long, and systemd is holding the guest open for
/// this process to exit.
fn halted(stopping: &AtomicBool) -> Result<(), String> {
    if stopping.load(Ordering::Relaxed) {
        return Err("the guest is shutting down".to_owned());
    }
    Ok(())
}

/// What this guest is, from its own files.
fn guest_facts() -> Result<GuestFacts, String> {
    let (distribution, release) = parse_os_release(&read(Path::new("/etc/os-release")))
        .ok_or_else(|| "/etc/os-release names no distribution".to_owned())?;
    let (kernel_release, machine) = uname()?;

    Ok(GuestFacts {
        distribution,
        release,
        // Debian's name for the machine, because that is what a payload
        // target and an apt package name are written in.
        architecture: match machine.as_str() {
            "x86_64" => "amd64".to_owned(),
            "aarch64" => "arm64".to_owned(),
            other => other.to_owned(),
        },
        kernel_release,
    })
}

/// The running kernel's release and machine.
fn uname() -> Result<(String, String), String> {
    let mut information = std::mem::MaybeUninit::<libc::utsname>::uninit();
    // SAFETY: `uname` fills the `utsname` it is given and touches nothing
    // else; the pointer is to a live, correctly sized allocation.
    let result = unsafe { libc::uname(information.as_mut_ptr()) };
    if result != 0 {
        return Err(format!("uname failed: {}", io::Error::last_os_error()));
    }
    // SAFETY: `uname` returned success, so the structure is initialized.
    let information = unsafe { information.assume_init() };

    Ok((field(&information.release), field(&information.machine)))
}

/// One NUL-terminated C string out of a `utsname`.
fn field(bytes: &[libc::c_char]) -> String {
    bytes
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8 as char)
        .collect()
}

/// Checks the mounted payload and reads what module it carries.
fn payload_stage(report: &mut Report, guest: &GuestFacts) -> Result<DkmsPackage, String> {
    let root = Path::new(PAYLOAD);
    let sources = read(&root.join("sources.json"));
    if sources.is_empty() {
        let reason = format!("no GPU payload is mounted at {PAYLOAD}");
        report.skipped(GpuRecipeStep::Payload, reason.clone());
        return Err(reason);
    }

    let target = parse_payload_target(&sources).map_err(|error| {
        report.failed(GpuRecipeStep::Payload, error.clone());
        error
    })?;
    let note = match applicability(&target, guest) {
        Applicability::NotApplicable(reason) => {
            report.skipped(GpuRecipeStep::Payload, reason.clone());
            return Err(reason);
        }
        Applicability::Applies { kernel } => kernel,
    };

    let module = root.join("content").join(MODULE);
    let package = parse_dkms_conf(&read(&module.join("dkms.conf"))).map_err(|error| {
        let reason = format!("{PAYLOAD}/content/{MODULE}: {error}");
        report.failed(GpuRecipeStep::Payload, reason.clone());
        reason
    })?;

    let mut message = format!("{} {} from the payload", package.name, package.version);
    if let Some(note) = note {
        message.push_str("; ");
        message.push_str(&note);
    }
    report.ok(GpuRecipeStep::Payload, message);
    Ok(package)
}

/// Installs what the build needs, and only what is missing.
///
/// A guest that already has a compiler, DKMS and its own kernel's headers
/// never reaches apt, which is what makes the second start of a VM work with
/// no network at all.
fn dependencies_stage(report: &mut Report, guest: &GuestFacts) -> Result<(), String> {
    let headers = format!("linux-headers-{}", guest.kernel_release);
    if dependencies_are_present(&guest.kernel_release) {
        report.skipped(
            GpuRecipeStep::BuildDependencies,
            format!("dkms, a compiler and {headers} are already installed"),
        );
        return Ok(());
    }

    let install = |report: &mut Report| {
        command::run(
            "apt-get",
            &["install", "-y", "dkms", "build-essential", &headers],
            &[("DEBIAN_FRONTEND", "noninteractive")],
            APT_BUDGET,
        )
    };

    let mut outcome = install(report);
    if !outcome.succeeded() {
        // A cloud image's package lists are as old as the image, and a stale
        // list is the ordinary reason an install of a kernel-specific package
        // fails on a VM's first boot.
        let _ = command::run(
            "apt-get",
            &["update"],
            &[("DEBIAN_FRONTEND", "noninteractive")],
            APT_BUDGET,
        );
        outcome = install(report);
    }

    if outcome.succeeded() {
        report.ok(
            GpuRecipeStep::BuildDependencies,
            format!("installed dkms, build-essential and {headers}"),
        );
        Ok(())
    } else {
        let reason = failure("apt-get install", &outcome);
        report.failed(GpuRecipeStep::BuildDependencies, reason.clone());
        Err(reason)
    }
}

/// Whether the guest can already build a module for its own kernel.
fn dependencies_are_present(kernel_release: &str) -> bool {
    let headers = PathBuf::from(format!("/lib/modules/{kernel_release}/build"));
    headers.exists()
        && command::run("dkms", &["--version"], &[], SHORT_BUDGET).succeeded()
        && command::run("cc", &["--version"], &[], SHORT_BUDGET).succeeded()
}

/// Stages the module sources where DKMS can write beside them.
///
/// A copy rather than a symlink: the payload is mounted read-only over 9p,
/// and DKMS writes its build tree next to the sources it is given.
fn source_stage(report: &mut Report, package: &DkmsPackage) -> Result<(), String> {
    let source = Path::new(PAYLOAD).join("content").join(MODULE);
    let destination =
        Path::new(DKMS_SOURCES).join(format!("{}-{}", package.name, package.version));

    match copy_tree(&source, &destination) {
        Ok(true) => {
            report.ok(
                GpuRecipeStep::ModuleSource,
                format!("staged {} sources at {}", package.name, destination.display()),
            );
            Ok(())
        }
        Ok(false) => {
            report.skipped(
                GpuRecipeStep::ModuleSource,
                format!("{} already holds these sources", destination.display()),
            );
            Ok(())
        }
        Err(error) => {
            let reason = format!("{} could not be staged: {error}", destination.display());
            report.failed(GpuRecipeStep::ModuleSource, reason.clone());
            Err(reason)
        }
    }
}

/// Builds and installs the module for the running kernel.
fn build_stage(
    report: &mut Report,
    package: &DkmsPackage,
    guest: &GuestFacts,
) -> Result<(), String> {
    let status = command::run("dkms", &["status"], &[], SHORT_BUDGET);
    if dkms_reports_installed(&status.output, package, &guest.kernel_release) {
        report.skipped(
            GpuRecipeStep::ModuleBuild,
            format!(
                "{} {} is already installed for kernel {}",
                package.name, package.version, guest.kernel_release
            ),
        );
        return Ok(());
    }

    // `dkms add` fails when the package is already registered, which is the
    // ordinary state of a guest whose sources were staged by an earlier
    // session. The build is what decides, so its failure is the only one
    // reported.
    let _ = command::run(
        "dkms",
        &["add", "-m", &package.name, "-v", &package.version],
        &[],
        SHORT_BUDGET,
    );

    for (arguments, budget) in [
        (["build"], BUILD_BUDGET),
        (["install"], SHORT_BUDGET),
    ] {
        let outcome = command::run(
            "dkms",
            &[
                arguments[0],
                "-m",
                &package.name,
                "-v",
                &package.version,
                "-k",
                &guest.kernel_release,
            ],
            &[],
            budget,
        );
        if !outcome.succeeded() {
            let mut reason = failure(&format!("dkms {}", arguments[0]), &outcome);
            if let Some(log) = make_log(package) {
                reason.push_str("\nmake.log:\n");
                reason.push_str(&log);
            }
            report.failed(GpuRecipeStep::ModuleBuild, reason.clone());
            return Err(reason);
        }
    }

    report.ok(
        GpuRecipeStep::ModuleBuild,
        format!(
            "built and installed {} {} for kernel {}",
            package.name, package.version, guest.kernel_release
        ),
    );
    Ok(())
}

/// The tail of the log a failed DKMS build leaves behind.
///
/// An exit code from a compiler is not a diagnosis, and the host's log is
/// where this is read.
fn make_log(package: &DkmsPackage) -> Option<String> {
    let log = Path::new(DKMS_TREE)
        .join(&package.name)
        .join(&package.version)
        .join("build/make.log");
    let text = fs::read_to_string(log).ok()?;
    Some(
        text.lines()
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// Loads the module now, and asks for it on every boot after this one.
fn load_stage(report: &mut Report) -> Result<(), String> {
    if let Err(error) = write_if_different(Path::new(MODULES_LOAD), &format!("{MODULE}\n")) {
        let reason = format!("{MODULES_LOAD} could not be written: {error}");
        report.failed(GpuRecipeStep::ModuleLoad, reason.clone());
        return Err(reason);
    }

    let outcome = command::run("modprobe", &[MODULE], &[], SHORT_BUDGET);
    if outcome.succeeded() {
        report.ok(
            GpuRecipeStep::ModuleLoad,
            format!("{MODULE} is loaded and listed in {MODULES_LOAD}"),
        );
        Ok(())
    } else {
        let reason = failure("modprobe", &outcome);
        report.failed(GpuRecipeStep::ModuleLoad, reason.clone());
        Err(reason)
    }
}

/// Looks at the device node the module exists to create.
fn device_stage(report: &mut Report) {
    if device_is_usable() {
        report.ok(GpuRecipeStep::Device, format!("{DEVICE} is a usable device"));
    } else {
        report.failed(
            GpuRecipeStep::Device,
            format!("{DEVICE} is missing, is not a character device, or will not open"),
        );
    }
}

/// Whether `/dev/dxg` is there and answers.
///
/// Opened rather than merely stat'd: that is what separates a node the kernel
/// created from one left behind by a module that is no longer there.
fn device_is_usable() -> bool {
    let Ok(metadata) = fs::metadata(DEVICE) else {
        return false;
    };
    metadata.file_type().is_char_device() && fs::File::open(DEVICE).is_ok()
}

/// Copies `source` onto `destination`, and says whether anything changed.
///
/// Files that are already byte-for-byte identical are left alone, so a
/// reconnect does not rewrite the tree DKMS is registered against.
pub fn copy_tree(source: &Path, destination: &Path) -> io::Result<bool> {
    fs::create_dir_all(destination)?;
    let mut changed = false;

    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            changed |= copy_tree(&from, &to)?;
            continue;
        }
        let wanted = fs::read(&from)?;
        if fs::read(&to).is_ok_and(|present| present == wanted) {
            continue;
        }
        fs::write(&to, &wanted)?;
        changed = true;
    }

    Ok(changed)
}

/// Writes `content` only when the file does not already hold it.
fn write_if_different(path: &Path, content: &str) -> io::Result<()> {
    if fs::read_to_string(path).is_ok_and(|present| present == content) {
        return Ok(());
    }
    if let Some(directory) = path.parent() {
        fs::create_dir_all(directory)?;
    }
    fs::write(path, content)
}

/// A file that may not be there, as the empty string.
///
/// Every caller treats "missing" and "empty" the same way -- as a fact that
/// is not there to be read -- and an `io::Error` here would be a second way
/// of saying the same stage did not apply.
fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
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

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-agent gpu_kernel::`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/agent/src/gpu_kernel.rs crates/agent/src/main.rs
git commit -m "TASK-95: Build and load dxgkrnl from a mounted payload"
```

---

### Task 6: The guest answers the apply request

**Files:**
- Modify: `crates/agent/src/session.rs`
- Modify: `crates/agent/src/main.rs:164-173`

**Interfaces:**
- Consumes: `gpu_kernel::apply`, task 1's messages.
- Produces: `session::run` takes a sixth argument,
  `apply: impl FnMut() -> Vec<GpuRecipeStage>`.

- [ ] **Step 1: Write the failing tests**

In the `tests` module of `crates/agent/src/session.rs`, add a helper beside
`refuse_to_mount` and two tests modelled on the manifest ones:

```rust
    /// A recipe that does nothing, for the tests about message order.
    fn apply_nothing() -> Vec<GpuRecipeStage> {
        Vec::new()
    }

    #[test]
    fn an_apply_on_a_gpu_session_is_carried_out_and_reported_back() {
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
                5,
                request::Kind::ApplyGpuRecipe(ApplyGpuRecipeRequest {}),
            )),
        ]);

        let mut applied = 0;
        run(
            &mut stream,
            &secret,
            "test-agent",
            &mut None,
            refuse_to_mount,
            || {
                applied += 1;
                vec![GpuRecipeStage {
                    step: i32::from(GpuRecipeStep::Device),
                    state: i32::from(GpuRecipeStageState::Ok),
                    message: "/dev/dxg is a usable device".to_owned(),
                }]
            },
        )
        .expect("the host closes after its recipe was answered");

        assert_eq!(applied, 1);
        let frames = stream.written_frames();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[2].request_id, 5);
        let Some(envelope::Body::Response(response)) = &frames[2].body else {
            panic!("an apply needs a response");
        };
        let Some(response::Kind::ApplyGpuRecipe(report)) = &response.kind else {
            panic!("an apply needs a recipe report");
        };
        assert_eq!(report.stages.len(), 1);
        assert_eq!(report.stages[0].step(), GpuRecipeStep::Device);
    }

    #[test]
    fn an_apply_on_a_session_without_the_gpu_capability_is_refused() {
        // The capability is what says the two builds agreed this session may
        // carry a recipe at all. Building a kernel module for a session that
        // never agreed on one would make the negotiation decorative.
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
                5,
                request::Kind::ApplyGpuRecipe(ApplyGpuRecipeRequest {}),
            )),
        ]);

        let mut applied = 0;
        run(
            &mut stream,
            &secret,
            "test-agent",
            &mut None,
            refuse_to_mount,
            || {
                applied += 1;
                Vec::new()
            },
        )
        .expect("the host closes after its request was refused");

        assert_eq!(applied, 0, "a refused apply must not run the recipe");
        let frames = stream.written_frames();
        let Some(envelope::Body::Response(response)) = &frames[2].body else {
            panic!("a refusal is a response");
        };
        let Some(response::Kind::Error(error)) = &response.kind else {
            panic!("a refusal is an error");
        };
        assert_eq!(error.code(), ErrorCode::UnsupportedRequest);
    }
```

Extend the `use` list of the test module with `ApplyGpuRecipeRequest`,
`GpuRecipeStage`, `GpuRecipeStageState` and `GpuRecipeStep`, and add
`apply_nothing` as the sixth argument to every existing `run(...)` call in the
module.

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p vmlord-agent session::`
Expected: FAIL — `run` takes five arguments.

- [ ] **Step 3: Write the implementation**

In `crates/agent/src/session.rs`, extend the imports with
`ApplyGpuRecipeResponse` and `GpuRecipeStage`, then widen `run` and `serve`:

```rust
pub fn run<S, A, R>(
    stream: &mut S,
    secret: &Secret,
    version: &str,
    opened: &mut Option<Session>,
    attach: A,
    apply: R,
) -> Result<(), SessionError>
where
    S: Read + Write,
    A: FnMut(&[GpuShare]) -> (Vec<GpuMount>, bool),
    R: FnMut() -> Vec<GpuRecipeStage>,
{
    let mut buffer = Vec::new();
    let session = greet(stream, version, &mut buffer)?;
    authenticate(stream, secret, &mut buffer)?;
    let session = opened.insert(session);
    serve(stream, session, attach, apply, &mut buffer)
}
```

`serve` takes `mut apply: R` with the same bound, and gains one arm beside the
manifest's:

```rust
            // A recipe is minutes of work rather than seconds, and it is still
            // answered from here: the host sends nothing that needs an answer
            // meanwhile, and a second thread would be two conversations on one
            // socket for a report that was asked for.
            Body::Request(request::Kind::ApplyGpuRecipe(_))
                if session.capabilities.contains(&Capability::Gpu) =>
            {
                let stages = apply();
                let report = Envelope::response(
                    request_id,
                    response::Kind::ApplyGpuRecipe(ApplyGpuRecipeResponse { stages }),
                );
                frame::write(stream, &report, buffer).map_err(SessionError::Frame)?;
            }
```

Also extend `kind_name` with an arm for `request::Kind::ApplyGpuRecipe(_)` --
`"an apply-recipe request out of order"` -- if the compiler asks for it.

In `crates/agent/src/main.rs`, pass the recipe:

```rust
    match session::run(
        &mut stream,
        secret,
        AGENT_VERSION,
        &mut opened,
        gpu_mounts::attach,
        || gpu_kernel::apply(&STOPPING),
    ) {
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-agent`
Expected: PASS, whole crate.

- [ ] **Step 5: Verify the guest binary still cross-builds**

Run: `cargo agent`
Expected: success.

- [ ] **Step 6: Commit**

```bash
git add crates/agent/src/session.rs crates/agent/src/main.rs
git commit -m "TASK-95: Apply the GPU recipe when the host asks"
```

---

### Task 7: The host asks, once per session, and logs the answer

**Files:**
- Modify: `crates/platform/src/agent_session.rs`

**Interfaces:**
- Consumes: task 1's messages.
- Produces: nothing other modules call; the behaviour is internal to
  `agent_session::serve`.

- [ ] **Step 1: Write the failing test**

In the `tests` module of `crates/platform/src/agent_session.rs`, add:

```rust
    #[test]
    fn a_session_asks_for_the_recipe_once_the_shares_are_attached() {
        // The recipe follows the mounts and never precedes them: a module
        // built out of a payload that is not mounted yet would fail for a
        // reason that has nothing to do with the guest.
        let secret = Secret::generate();
        let mut guest = Guest::opening_with(
            Secret::from_base64(&secret.to_base64()).expect("the secret"),
            hello(ProtocolVersion::current(), &[Capability::Gpu]),
        );
        let session = open(&mut guest, &secret, VM).expect("a session that authenticated");
        let manifest = GpuShareManifest {
            shares: vec![vmlord_core::GpuShare::payload()],
        };
        // What a guest that mounted its payload and applied its recipe sends.
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

        serve(&mut guest, &session, Some(&manifest), VM).expect("a session the agent closed");

        let asked = guest.answer_to(super::APPLY_REQUEST_ID);
        let Some(envelope::Body::Request(ref request)) = asked.body else {
            panic!("the recipe should have been asked for as a request");
        };
        assert!(matches!(
            request.kind,
            Some(request::Kind::ApplyGpuRecipe(_))
        ));
        assert_eq!(
            guest
                .received
                .iter()
                .filter(|envelope| matches!(
                    envelope.body,
                    Some(envelope::Body::Request(ref request))
                        if matches!(request.kind, Some(request::Kind::ApplyGpuRecipe(_)))
                ))
                .count(),
            1,
            "one recipe per session"
        );
    }

    #[test]
    fn a_session_with_no_shares_asks_for_no_recipe() {
        // A guest with no GPU shares has no payload to build a module from.
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
                    if matches!(request.kind, Some(request::Kind::ApplyGpuRecipe(_)))
            )),
            "a VM with no manifest is asked for no recipe"
        );
    }
```

Both tests use the fixture that is already in the module -- `Guest::opening_with`,
`say`, `answer_to` and `received` -- rather than a second one. Extend the test
module's `use` list with `ApplyGpuRecipeResponse`, `GpuRecipeStage`,
`GpuRecipeStageState` and `GpuRecipeStep`.

- [ ] **Step 2: Run it to make sure it fails**

Run: `cargo test-windows -p vmlord-platform agent_session`
Expected: FAIL — the host sends only the attach.

- [ ] **Step 3: Write the implementation**

In `crates/platform/src/agent_session.rs`, add the id beside the others:

```rust
/// The id the host asks for the guest's GPU recipe with.
///
/// One recipe per session, after the manifest of the same session: the module
/// is built out of a payload the guest has just been told to mount.
const APPLY_REQUEST_ID: u32 = ATTACH_REQUEST_ID + 1;
```

extend the imports with `ApplyGpuRecipeRequest`, `ApplyGpuRecipeResponse`,
`GpuRecipeStageState` and `GpuRecipeStep`, keep a second pending id in
`serve`:

```rust
    let mut pending_manifest = attach_shares(stream, session, shares, vm_name, &mut buffer)?;
    let mut pending_recipe = None;
```

send the apply when the attach report arrives:

```rust
            Body::Response(response::Kind::AttachGpuShares(report))
                if pending_manifest == Some(request_id) =>
            {
                pending_manifest = None;
                report_mounts(&report, vm_name);
                pending_recipe = apply_recipe(stream, vm_name, &mut buffer)?;
            }
            Body::Response(response::Kind::ApplyGpuRecipe(report))
                if pending_recipe == Some(request_id) =>
            {
                pending_recipe = None;
                report_recipe(&report, vm_name);
            }
```

and add the two functions:

```rust
/// Asks the guest to apply its GPU recipe, and says which id asked.
///
/// After the mounts of the same session, because the module is built out of
/// the payload the guest has just mounted. Once per session, for the same
/// reason the manifest is sent once: the guest reconciles rather than
/// rebuilds, and a retry loop around a kernel build is how a guest ends up
/// compiling continuously.
fn apply_recipe<S: Read + Write>(
    stream: &mut S,
    vm_name: &str,
    buffer: &mut Vec<u8>,
) -> Result<Option<u32>, SessionError> {
    let request = Envelope::request(
        APPLY_REQUEST_ID,
        request::Kind::ApplyGpuRecipe(ApplyGpuRecipeRequest {}),
    );
    frame::write(stream, &request, buffer).map_err(SessionError::Frame)?;
    log::debug!("VMLord asked the agent of VM \"{vm_name}\" to apply its GPU recipe");

    Ok(Some(APPLY_REQUEST_ID))
}

/// Says what the guest's recipe did, at the volume each stage earns.
///
/// Nothing is kept and nothing is retried: the next session applies the
/// recipe again, and deriving a GPU status from these facts is the
/// application layer's work.
fn report_recipe(report: &ApplyGpuRecipeResponse, vm_name: &str) {
    for stage in &report.stages {
        match stage.state() {
            GpuRecipeStageState::Ok => log::debug!(
                "the agent of VM \"{vm_name}\" finished GPU recipe stage {:?}: {}",
                stage.step(),
                stage.message
            ),
            GpuRecipeStageState::Skipped => log::debug!(
                "the agent of VM \"{vm_name}\" skipped GPU recipe stage {:?}: {}",
                stage.step(),
                stage.message
            ),
            _ => log::warn!(
                "the agent of VM \"{vm_name}\" failed GPU recipe stage {:?}: {}",
                stage.step(),
                stage.message
            ),
        }
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-platform`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/platform/src/agent_session.rs
git commit -m "TASK-95: Ask a guest for its GPU recipe once per session"
```

---

### Task 8: Documentation

**Files:**
- Modify: `ARCHITECTURE.md` (the GPU sections, after "GPU: guest payload")

**Interfaces:**
- Consumes: everything above.
- Produces: nothing in code.

- [ ] **Step 1: Write the new section**

Add a section after "GPU: guest payload", titled **GPU: the guest's Ubuntu
recipe**, covering, in the register of the surrounding prose:

* what the recipe is for -- the payload mounted by #94 becomes a `/dev/dxg`;
* the `ApplyGpuRecipe` exchange, revision 1.3, empty request, stages and never
  a verdict, and that #96 and #97 add enum values rather than messages;
* that build dependencies come from the guest's apt rather than the payload,
  and the consequence: no network means a failed stage and a `Degraded` GPU,
  never a VM that does not start;
* that distribution, release and architecture are the gate and the kernel is
  a recorded fact, with DKMS `AUTOINSTALL` carrying the module across kernel
  upgrades;
* the seven stages and the already-satisfied short circuit;
* the time budgets, the shutdown check between stages, and why the recipe runs
  inline in the session;
* the payload layout the recipe expects.

Update the sentence in "GPU: guest payload" that says the production catalog
is "intentionally empty until tasks 95 and 96 produce a compiled and probed
Ubuntu recipe" so that it names what is still missing after this task: a
packed and published archive, which is what #96 and #97 build on.

- [ ] **Step 2: Check the whole workspace one more time**

Run: `cargo test -p vmlord-agent`, `cargo agent`, `cargo test-windows`,
`cargo check-windows`
Expected: all four succeed.

- [ ] **Step 3: Commit**

```bash
git add ARCHITECTURE.md
git commit -m "TASK-95: Document the Ubuntu GPU kernel recipe"
```

---

## Manual verification (not automatable here)

The build itself needs a real Hyper-V host, a GPU-PV adapter, a packed payload
and an Ubuntu guest, none of which exist behind `cargo test`. On such a host:

1. Start a `Mirror` VM with a prepared payload and watch the host log: an
   attach report, then a recipe report with seven stages.
2. In the guest, check `lsmod | grep dxgkrnl`, `ls -l /dev/dxg`,
   `cat /etc/modules-load.d/vmlord-dxgkrnl.conf` and
   `dkms status -m dxgkrnl`.
3. Restart the VM and confirm the second session's report short-circuits: the
   dependency, source and build stages skipped, the load and device stages OK.
4. Disconnect the VM's network, recreate it, and confirm the recipe reports a
   failed `BUILD_DEPENDENCIES` stage and that the VM still runs.
