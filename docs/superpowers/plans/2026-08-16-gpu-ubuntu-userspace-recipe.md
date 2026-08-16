# Ubuntu GPU userspace recipe Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give an Ubuntu guest that already has `/dev/dxg` a Mesa userspace, a Vulkan ICD and the environment that makes a process pick them, reported as three new stages of the existing GPU recipe.

**Architecture:** Three `GpuRecipeStep` values are appended to the wire (revision 1.4, enum values only). Everything that decides stays a pure function in `crates/agent/src/gpu_recipe.rs`; everything that touches the guest stays in `crates/agent/src/gpu_kernel.rs`, which gains three stages after `DEVICE` and makes `DEVICE` able to end the recipe. The host does not change: it logs stages by state and never branches on a step.

**Tech Stack:** Rust 2024, `prost` for Protobuf, the agent's own `command::run` for bounded external programs, no new dependencies.

**Spec:** `docs/superpowers/specs/2026-08-16-gpu-ubuntu-userspace-recipe-design.md`

## Global Constraints

* The agent is built for `x86_64-unknown-linux-musl` with `cargo agent`; **never** add a dependency that makes it link against system C libraries.
* Nothing in the recipe may fail as a whole. Every failure is a stage in the report and a VM that keeps running.
* Every external program runs through `crate::command::run` with a budget. Budgets already defined in `gpu_kernel.rs`: `APT_BUDGET` 300 s, `BUILD_BUDGET` 900 s, `SHORT_BUDGET` 30 s.
* Paths, verbatim: Mesa prefix `/opt/vmlord/wsl-mesa`, its linker file `/etc/ld.so.conf.d/vmlord-wsl-mesa.conf` (never the `/etc/ld.so.conf.d/vmlord-gpu.conf` that `gpu_mounts.rs` rewrites), ICD directory `/etc/vulkan/icd.d`, generator `/etc/systemd/user-environment-generators/50-vmlord-gpu` (mode 0755), profile script `/etc/profile.d/vmlord-gpu.sh`, WSL libraries `/usr/lib/wsl/lib` (`gpu_targets::WSL_LIB`), payload `/opt/vmlord/gpu-payload` (`gpu_targets::PAYLOAD`).
* Commands are run without a `timeout` prefix: `cargo test -p vmlord-agent`, `cargo agent`, `cargo test-windows`, `cargo check-windows`.
* Commit subjects are `TASK-96: <comment>` and end with the `Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>` trailer.

---

### Task 1: Three userspace steps on the wire

**Files:**
- Modify: `crates/agent-protocol/proto/vmlord/agent/v1/agent.proto:287-310` (the `GpuRecipeStep` enum)
- Modify: `crates/agent-protocol/src/handshake.rs:19` (`CURRENT_VERSION`)
- Test: `crates/agent-protocol/tests/recipe.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `GpuRecipeStep::{Userspace, VulkanIcd, Environment}` in `vmlord_agent_protocol::v1`; `CURRENT_VERSION` = `ProtocolVersion { major: 1, minor: 4 }`.

- [ ] **Step 1: Write the failing test**

In `crates/agent-protocol/tests/recipe.rs`, replace `the_recipe_report_belongs_to_revision_one_three` with:

```rust
#[test]
fn the_userspace_steps_belong_to_revision_one_four() {
    // Enum values only, so an agent from 1.3 simply never sends these and a
    // host from 1.3 logs a step it has no name for rather than misreading one.
    assert_eq!((CURRENT_VERSION.major, CURRENT_VERSION.minor), (1, 4));

    let report = Envelope::response(
        9,
        response::Kind::ApplyGpuRecipe(ApplyGpuRecipeResponse {
            stages: vec![
                GpuRecipeStage {
                    step: i32::from(GpuRecipeStep::Userspace),
                    state: i32::from(GpuRecipeStageState::Ok),
                    message: "staged mesa from the payload".to_owned(),
                },
                GpuRecipeStage {
                    step: i32::from(GpuRecipeStep::VulkanIcd),
                    state: i32::from(GpuRecipeStageState::Skipped),
                    message: "the payload carries no Vulkan driver".to_owned(),
                },
                GpuRecipeStage {
                    step: i32::from(GpuRecipeStep::Environment),
                    state: i32::from(GpuRecipeStageState::Ok),
                    message: "wrote the generator and the profile script".to_owned(),
                },
            ],
        }),
    );

    let decoded = Envelope::decode(report.encode_to_vec().as_slice()).expect("a decodable report");
    let Some(envelope::Body::Response(response)) = decoded.body else {
        panic!("a report is a response");
    };
    let Some(response::Kind::ApplyGpuRecipe(report)) = response.kind else {
        panic!("a report is a recipe report");
    };
    assert_eq!(report.stages[0].step(), GpuRecipeStep::Userspace);
    assert_eq!(report.stages[1].step(), GpuRecipeStep::VulkanIcd);
    assert_eq!(report.stages[2].step(), GpuRecipeStep::Environment);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p vmlord-agent-protocol --test recipe`
Expected: FAIL — `GpuRecipeStep` has no variant `Userspace`.

- [ ] **Step 3: Add the enum values**

Append inside `enum GpuRecipeStep` in `crates/agent-protocol/proto/vmlord/agent/v1/agent.proto`, after `GPU_RECIPE_STEP_DEVICE = 7;`:

```proto
  // The Mesa userspace the payload's policy calls for: the distribution's
  // own, or the tree the payload carries.
  GPU_RECIPE_STEP_USERSPACE = 8;

  // The Vulkan driver a payload may carry, registered where the loader
  // looks for it.
  GPU_RECIPE_STEP_VULKAN_ICD = 9;

  // What makes a process in this guest pick that userspace.
  GPU_RECIPE_STEP_ENVIRONMENT = 10;
```

- [ ] **Step 4: Move the revision**

In `crates/agent-protocol/src/handshake.rs:19`:

```rust
pub const CURRENT_VERSION: ProtocolVersion = ProtocolVersion { major: 1, minor: 4 };
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p vmlord-agent-protocol`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/agent-protocol
git commit -m "$(cat <<'EOF'
TASK-96: Carry three userspace steps on the wire

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: What the userspace stages decide

**Files:**
- Modify: `crates/agent/src/gpu_recipe.rs` (`STEPS`, plus new pure functions and their tests)

**Interfaces:**
- Consumes: `GpuRecipeStep::{Userspace, VulkanIcd, Environment}` from Task 1.
- Produces, all `pub` in `crate::gpu_recipe`:
  - `pub const STEPS: [GpuRecipeStep; 10]`
  - `pub enum MesaPolicy { Distro, Bundled }`
  - `pub fn parse_mesa_policy(json: &str) -> Result<MesaPolicy, String>`
  - `pub fn library_triplet(architecture: &str) -> Option<&'static str>`
  - `pub fn icd_documents(names: &[String]) -> Vec<String>`
  - `pub enum Shell { Generator, Profile }`
  - `pub struct Environment { pub library_paths: Vec<String>, pub icd: Option<String> }`
  - `pub fn environment_document(form: Shell, environment: &Environment) -> String`

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module of `crates/agent/src/gpu_recipe.rs` (and extend its `use super::{...}` list with `Environment, MesaPolicy, Shell, environment_document, icd_documents, library_triplet, parse_mesa_policy`):

```rust
    #[test]
    fn a_payload_names_the_mesa_policy_it_was_built_with() {
        assert_eq!(
            parse_mesa_policy(r#"{"mesa_policy": "bundled"}"#),
            Ok(MesaPolicy::Bundled)
        );
        assert_eq!(
            parse_mesa_policy(r#"{"mesa_policy":"distro","target":{}}"#),
            Ok(MesaPolicy::Distro)
        );
    }

    #[test]
    fn a_policy_this_build_does_not_know_is_an_error_and_not_a_guess() {
        // A payload built newer than this agent must fail the stage it belongs
        // to rather than be treated as one of the policies that exist today.
        for document in [r#"{"mesa_policy": "flatpak"}"#, "{}", "not json"] {
            assert!(parse_mesa_policy(document).is_err(), "{document}");
        }
    }

    #[test]
    fn every_architecture_with_a_recipe_has_a_library_path() {
        assert_eq!(library_triplet("amd64"), Some("x86_64-linux-gnu"));
        assert_eq!(library_triplet("arm64"), Some("aarch64-linux-gnu"));
        assert_eq!(library_triplet("riscv64"), None);
        assert_eq!(library_triplet(""), None);
    }

    #[test]
    fn the_icd_documents_of_a_directory_are_its_json_files_in_order() {
        // The names come from the payload rather than a constant: Mesa has
        // shipped this file under more than one name.
        let names = [
            "dzn_icd.x86_64.json".to_owned(),
            "README".to_owned(),
            "lvp_icd.x86_64.json".to_owned(),
            "notes.json.bak".to_owned(),
        ];

        assert_eq!(
            icd_documents(&names),
            vec![
                "dzn_icd.x86_64.json".to_owned(),
                "lvp_icd.x86_64.json".to_owned()
            ]
        );
        assert!(icd_documents(&[]).is_empty());
    }

    #[test]
    fn the_generator_prints_what_a_session_inherits() {
        let document = environment_document(
            Shell::Generator,
            &Environment {
                library_paths: vec![
                    "/opt/vmlord/wsl-mesa/lib/x86_64-linux-gnu".to_owned(),
                    "/usr/lib/wsl/lib".to_owned(),
                ],
                icd: Some("/etc/vulkan/icd.d/dzn_icd.x86_64.json".to_owned()),
            },
        );

        assert!(document.starts_with("#!/bin/sh\n"), "{document}");
        // The probe runs on every start: this file outlives a reboot and
        // /dev/dxg does not.
        assert!(document.contains("[ -e /dev/dxg ]"), "{document}");
        assert!(
            document.contains("[ -d /opt/vmlord/wsl-mesa/lib/x86_64-linux-gnu ]"),
            "{document}"
        );
        assert!(
            document.contains(
                "echo \"LD_LIBRARY_PATH=/opt/vmlord/wsl-mesa/lib/x86_64-linux-gnu:/usr/lib/wsl/lib\""
            ),
            "{document}"
        );
        // Both, always: the first is gallium selection on the GLX path, the
        // second is the DRI loader EGL and Wayland clients use.
        assert!(document.contains("echo \"GALLIUM_DRIVER=d3d12\""), "{document}");
        assert!(
            document.contains("echo \"MESA_LOADER_DRIVER_OVERRIDE=d3d12\""),
            "{document}"
        );
        assert!(
            document.contains("echo \"__GLX_VENDOR_LIBRARY_NAME=mesa\""),
            "{document}"
        );
        assert!(
            document
                .contains("echo \"VK_DRIVER_FILES=/etc/vulkan/icd.d/dzn_icd.x86_64.json\""),
            "{document}"
        );
    }

    #[test]
    fn the_profile_script_exports_and_never_exits_the_shell_it_is_sourced_by() {
        let document = environment_document(
            Shell::Profile,
            &Environment {
                library_paths: vec!["/usr/lib/wsl/lib".to_owned()],
                icd: None,
            },
        );

        // Sourced by /etc/profile: an `exit` here would end the login shell.
        assert!(!document.contains("exit"), "{document}");
        assert!(
            document.contains(
                "LD_LIBRARY_PATH=\"/usr/lib/wsl/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}\""
            ),
            "{document}"
        );
        assert!(document.contains("export LD_LIBRARY_PATH"), "{document}");
        assert!(document.contains("export GALLIUM_DRIVER=d3d12"), "{document}");
        // Nothing was registered, so nothing is pinned.
        assert!(!document.contains("VK_DRIVER_FILES"), "{document}");
    }

    #[test]
    fn the_same_environment_writes_the_same_document() {
        // What makes the second start of a VM report the stage as skipped.
        let environment = Environment {
            library_paths: vec!["/usr/lib/wsl/lib".to_owned()],
            icd: None,
        };

        assert_eq!(
            environment_document(Shell::Generator, &environment),
            environment_document(Shell::Generator, &environment)
        );
        assert_ne!(
            environment_document(Shell::Generator, &environment),
            environment_document(Shell::Profile, &environment)
        );
    }

    #[test]
    fn the_userspace_steps_a_failed_device_never_reached_carry_its_reason() {
        let mut report = Report::new();
        report.ok(GpuRecipeStep::Distribution, "ubuntu");
        report.failed(GpuRecipeStep::Device, "/dev/dxg is missing");

        let stages = report.finish("/dev/dxg never appeared");

        assert_eq!(stages.len(), STEPS.len());
        for stage in &stages[STEPS.len() - 3..] {
            assert_eq!(stage.state(), GpuRecipeStageState::Skipped);
            assert_eq!(stage.message, "/dev/dxg never appeared");
        }
        assert_eq!(stages[STEPS.len() - 3].step(), GpuRecipeStep::Userspace);
        assert_eq!(stages[STEPS.len() - 1].step(), GpuRecipeStep::Environment);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-agent gpu_recipe`
Expected: FAIL — `parse_mesa_policy`, `library_triplet`, `icd_documents`, `environment_document` are not defined.

- [ ] **Step 3: Extend `STEPS`**

In `crates/agent/src/gpu_recipe.rs`:

```rust
pub const STEPS: [GpuRecipeStep; 10] = [
    GpuRecipeStep::Distribution,
    GpuRecipeStep::Payload,
    GpuRecipeStep::BuildDependencies,
    GpuRecipeStep::ModuleSource,
    GpuRecipeStep::ModuleBuild,
    GpuRecipeStep::ModuleLoad,
    GpuRecipeStep::Device,
    GpuRecipeStep::Userspace,
    GpuRecipeStep::VulkanIcd,
    GpuRecipeStep::Environment,
];
```

- [ ] **Step 4: Write the pure functions**

Add to `crates/agent/src/gpu_recipe.rs`, after `parse_dkms_conf`:

```rust
/// Where a payload's userspace comes from.
///
/// The host has already checked this value against the catalog entry it
/// downloaded the payload for; the guest honours it rather than deciding
/// again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MesaPolicy {
    /// The distribution's own Mesa, installed from the guest's apt.
    Distro,
    /// The Mesa tree the payload carries.
    Bundled,
}

/// Reads `mesa_policy` out of a payload's `sources.json`.
///
/// Read here rather than folded into [`PayloadTarget`] on purpose: a policy
/// this build has never heard of must fail the userspace stage it belongs to,
/// not the payload stage after which a kernel module would have built and
/// `/dev/dxg` would have worked.
pub fn parse_mesa_policy(json: &str) -> Result<MesaPolicy, String> {
    let document: serde_json::Value = serde_json::from_str(json)
        .map_err(|error| format!("sources.json is unreadable: {error}"))?;
    match document.get("mesa_policy").and_then(serde_json::Value::as_str) {
        Some("distro") => Ok(MesaPolicy::Distro),
        Some("bundled") => Ok(MesaPolicy::Bundled),
        Some(other) => Err(format!(
            "vmlord-agent has no recipe for the mesa policy {other}"
        )),
        None => Err("sources.json names no mesa policy".to_owned()),
    }
}

/// The multiarch directory a Debian architecture's libraries live under.
///
/// Derived from the guest rather than written as a constant: an agent that
/// hard-codes one architecture's library path is one that silently installs
/// nothing on the other.
pub fn library_triplet(architecture: &str) -> Option<&'static str> {
    match architecture {
        "amd64" => Some("x86_64-linux-gnu"),
        "arm64" => Some("aarch64-linux-gnu"),
        _ => None,
    }
}

/// The Vulkan ICD documents among the names of a directory's entries.
///
/// Names from the payload and never a constant: AppSandbox's own notes record
/// a README promising `microsoft_icd.x86_64.json` where Mesa shipped
/// `dzn_icd.x86_64.json`, and a hard-coded name is a stage that reports
/// success on a file it never found.
pub fn icd_documents(names: &[String]) -> Vec<String> {
    let mut documents: Vec<String> = names
        .iter()
        .filter(|name| name.ends_with(".json"))
        .cloned()
        .collect();
    documents.sort();
    documents
}

/// Which of the two files the environment is written into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shell {
    /// A systemd user-environment generator, which prints `NAME=VALUE`.
    Generator,
    /// A `profile.d` script, which is sourced and therefore exports.
    Profile,
}

/// The userspace a process in this guest should be pointed at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Environment {
    /// Directories to put on `LD_LIBRARY_PATH`, in order. The first is also
    /// what the script checks for before setting anything.
    pub library_paths: Vec<String>,
    /// The ICD document to pin, when one was registered.
    pub icd: Option<String>,
}

/// The script that points a process at this userspace, when there is a GPU.
///
/// A script with the probe inside rather than a file of finished values: the
/// file outlives a reboot and `/dev/dxg` does not, and a VM restarted without
/// a GPU and a static `MESA_LOADER_DRIVER_OVERRIDE=d3d12` is a guest where GL
/// stops working entirely.
pub fn environment_document(form: Shell, environment: &Environment) -> String {
    let libraries = environment.library_paths.join(":");
    let guard = environment.library_paths.first().cloned().unwrap_or_default();

    let mut document = String::from("#!/bin/sh\n# Written by vmlord-agent. Do not edit.\n#\n");
    document.push_str("# The GPU is checked on every start: this file outlives a reboot and\n");
    document.push_str("# /dev/dxg does not.\n");
    document.push_str(&format!("if [ -e /dev/dxg ] && [ -d {guard} ]; then\n"));

    match form {
        Shell::Generator => {
            document.push_str(&format!("    echo \"LD_LIBRARY_PATH={libraries}\"\n"));
            for (name, value) in FIXED {
                document.push_str(&format!("    echo \"{name}={value}\"\n"));
            }
            if let Some(icd) = &environment.icd {
                document.push_str(&format!("    echo \"VK_DRIVER_FILES={icd}\"\n"));
            }
        }
        Shell::Profile => {
            document.push_str(&format!(
                "    LD_LIBRARY_PATH=\"{libraries}${{LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}}\"\n"
            ));
            document.push_str("    export LD_LIBRARY_PATH\n");
            for (name, value) in FIXED {
                document.push_str(&format!("    export {name}={value}\n"));
            }
            if let Some(icd) = &environment.icd {
                document.push_str(&format!("    export VK_DRIVER_FILES={icd}\n"));
            }
        }
    }

    document.push_str("fi\n");
    document
}

/// The variables that do not depend on what the payload turned out to carry.
///
/// `GALLIUM_DRIVER` and `MESA_LOADER_DRIVER_OVERRIDE` are both there because
/// the first is direct gallium selection on the GLX path and the second is the
/// DRI loader EGL and Wayland clients go through: setting one gives an
/// accelerated GLX and llvmpipe on EGL.
const FIXED: [(&str, &str); 3] = [
    ("GALLIUM_DRIVER", "d3d12"),
    ("MESA_LOADER_DRIVER_OVERRIDE", "d3d12"),
    ("__GLX_VENDOR_LIBRARY_NAME", "mesa"),
];
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p vmlord-agent`
Expected: PASS — including the existing `gpu_recipe` and `gpu_kernel` tests.

- [ ] **Step 6: Commit**

```bash
git add crates/agent/src/gpu_recipe.rs
git commit -m "$(cat <<'EOF'
TASK-96: Decide what a guest's GPU userspace should be

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: The three stages, in the guest

**Files:**
- Modify: `crates/agent/src/gpu_kernel.rs` (constants, `run_stages`, `device_stage`, three new stages, one new test)

**Interfaces:**
- Consumes: everything Task 2 produced, plus the existing `command::run`, `copy_tree`, `write_if_different`, `read`, `failure`, `halted`, `device_is_usable`, and `gpu_targets::{PAYLOAD, WSL_LIB}`.
- Produces: no new public API. `pub fn apply(stopping: &AtomicBool) -> Vec<GpuRecipeStage>` keeps its signature and returns ten stages.

- [ ] **Step 1: Write the failing test**

Append to the `tests` module of `crates/agent/src/gpu_kernel.rs` (extend its `use super::{...}` to `use super::{copy_tree, symlink_if_different};`):

```rust
    #[test]
    fn an_icd_symlink_is_written_once_and_then_left_alone() {
        // The stage is skipped on a second session only if re-registering an
        // ICD that is already registered changes nothing.
        let directory = temporary("icd");
        let target = directory.join("dzn_icd.x86_64.json");
        let link = directory.join("registered.json");
        fs::write(&target, b"{}\n").unwrap();

        assert!(symlink_if_different(&target, &link).unwrap());
        assert!(!symlink_if_different(&target, &link).unwrap());
        assert_eq!(fs::read(&link).unwrap(), b"{}\n");
    }

    #[test]
    fn an_icd_symlink_pointing_elsewhere_is_replaced() {
        let directory = temporary("icd-replaced");
        let old = directory.join("old.json");
        let new = directory.join("new.json");
        let link = directory.join("registered.json");
        fs::write(&old, b"old\n").unwrap();
        fs::write(&new, b"new\n").unwrap();
        symlink_if_different(&old, &link).unwrap();

        assert!(symlink_if_different(&new, &link).unwrap());
        assert_eq!(fs::read(&link).unwrap(), b"new\n");
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p vmlord-agent gpu_kernel`
Expected: FAIL — `symlink_if_different` is not defined.

- [ ] **Step 3: Add the constants and the file helpers**

In `crates/agent/src/gpu_kernel.rs`, beside the existing constants:

```rust
/// Where a bundled Mesa tree is staged, out of the payload that carries it.
///
/// A copy rather than the read-only 9p mount it came from: the mount lives as
/// long as the agent's session, and the linker cache, the `ld.so.conf.d` line
/// and the ICD symlink all outlive a reboot.
const MESA_PREFIX: &str = "/opt/vmlord/wsl-mesa";

/// Where the linker is told about a bundled Mesa.
///
/// Its own file, never the one `gpu_mounts` rewrites from the current set of
/// mounts: sharing one would mean that dropping a GPU share erases a line that
/// has nothing to do with shares.
const MESA_LD_CONF: &str = "/etc/ld.so.conf.d/vmlord-wsl-mesa.conf";

/// Where the Vulkan loader looks for the drivers of a system.
const VULKAN_ICD: &str = "/etc/vulkan/icd.d";

/// What a systemd user session and everything started from it inherits.
const GENERATOR: &str = "/etc/systemd/user-environment-generators/50-vmlord-gpu";

/// What a login shell picks up, which in an MVP guest means SSH.
const PROFILE: &str = "/etc/profile.d/vmlord-gpu.sh";
```

And, beside `write_if_different`:

```rust
/// Points `link` at `target`, and says whether anything changed.
fn symlink_if_different(target: &Path, link: &Path) -> io::Result<bool> {
    if fs::read_link(link).is_ok_and(|present| present == target) {
        return Ok(false);
    }
    if let Some(directory) = link.parent() {
        fs::create_dir_all(directory)?;
    }
    match fs::remove_file(link) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    std::os::unix::fs::symlink(target, link)?;
    Ok(true)
}

/// Writes an executable script only when the file does not already hold it.
fn write_script_if_different(path: &Path, content: &str) -> io::Result<bool> {
    if fs::read_to_string(path).is_ok_and(|present| present == content) {
        return Ok(false);
    }
    if let Some(directory) = path.parent() {
        fs::create_dir_all(directory)?;
    }
    fs::write(path, content)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(true)
}
```

Three import lines change at the top of the file: `use std::os::unix::fs::FileTypeExt;` becomes `use std::os::unix::fs::{FileTypeExt, PermissionsExt};`, the `gpu_recipe` list gains `Environment, MesaPolicy, Shell, environment_document, icd_documents, library_triplet, parse_mesa_policy`, and `use crate::gpu_targets::PAYLOAD;` becomes `use crate::gpu_targets::{PAYLOAD, WSL_LIB};`.

- [ ] **Step 4: Make `DEVICE` able to end the recipe, and run the three stages**

Replace `device_stage` and the tail of `run_stages` in `crates/agent/src/gpu_kernel.rs`:

```rust
    load_stage(report)?;
    device_stage(report)?;
    halted(stopping)?;

    let userspace = userspace_stage(report, &guest)?;
    halted(stopping)?;
    let icd = vulkan_stage(report, &userspace)?;
    halted(stopping)?;
    environment_stage(report, &userspace, icd)
}
```

```rust
/// Looks at the device node the module exists to create.
///
/// The stage that decides whether the userspace half runs at all: a guest
/// whose device never appeared must not be configured for a driver that
/// cannot open it.
fn device_stage(report: &mut Report) -> Result<(), String> {
    if device_is_usable() {
        report.ok(GpuRecipeStep::Device, format!("{DEVICE} is a usable device"));
        return Ok(());
    }

    let reason =
        format!("{DEVICE} is missing, is not a character device, or will not open");
    report.failed(GpuRecipeStep::Device, reason.clone());
    Err(reason)
}
```

- [ ] **Step 5: Write the userspace stage**

Add to `crates/agent/src/gpu_kernel.rs`:

```rust
/// The userspace this guest ended up with, and where it lives.
struct Userspace {
    policy: MesaPolicy,
    /// Where a bundled Mesa was staged; nothing under `distro`.
    prefix: Option<PathBuf>,
    /// The directories a process has to load libraries from, in order.
    library_paths: Vec<String>,
}

/// Installs or stages the Mesa the payload's policy calls for.
fn userspace_stage(report: &mut Report, guest: &GuestFacts) -> Result<Userspace, String> {
    let policy = parse_mesa_policy(&read(&Path::new(PAYLOAD).join("sources.json"))).map_err(
        |error| {
            report.failed(GpuRecipeStep::Userspace, error.clone());
            error
        },
    )?;
    let Some(triplet) = library_triplet(&guest.architecture) else {
        let reason = format!(
            "vmlord-agent has no library path for architecture {}",
            guest.architecture
        );
        report.failed(GpuRecipeStep::Userspace, reason.clone());
        return Err(reason);
    };

    match policy {
        MesaPolicy::Distro => {
            distribution_mesa(report, triplet)?;
            Ok(Userspace {
                policy,
                prefix: None,
                library_paths: vec![WSL_LIB.to_owned()],
            })
        }
        MesaPolicy::Bundled => {
            let prefix = bundled_mesa(report, triplet)?;
            Ok(Userspace {
                policy,
                library_paths: vec![
                    format!("{MESA_PREFIX}/lib/{triplet}"),
                    WSL_LIB.to_owned(),
                ],
                prefix: Some(prefix),
            })
        }
    }
}

/// Installs the distribution's own Mesa, and only what is missing.
///
/// Ubuntu's Mesa carries the d3d12 gallium driver and is not built with
/// `microsoft-experimental`, so Vulkan under this policy is lavapipe. That is
/// a fact for the host's log and not a refusal: the payload's author chose the
/// policy, and whether GL alone is enough is the probe's question.
fn distribution_mesa(report: &mut Report, triplet: &str) -> Result<(), String> {
    let driver = PathBuf::from(format!("/usr/lib/{triplet}/dri/d3d12_dri.so"));
    let loader = PathBuf::from(format!("/usr/lib/{triplet}/libvulkan.so.1"));
    if driver.exists() && loader.exists() {
        report.skipped(
            GpuRecipeStep::Userspace,
            format!("the distribution's Mesa is already installed; {} is present", driver.display()),
        );
        return Ok(());
    }

    let mut outcome = apt_mesa();
    if !outcome.succeeded() {
        let _ = command::run(
            "apt-get",
            &["update"],
            &[("DEBIAN_FRONTEND", "noninteractive")],
            APT_BUDGET,
        );
        outcome = apt_mesa();
    }

    if outcome.succeeded() {
        report.ok(
            GpuRecipeStep::Userspace,
            "installed the distribution's Mesa: d3d12 gallium for GL, and lavapipe for \
             Vulkan, because Ubuntu does not build the dzn driver"
                .to_owned(),
        );
        Ok(())
    } else {
        let reason = failure("apt-get install", &outcome);
        report.failed(GpuRecipeStep::Userspace, reason.clone());
        Err(reason)
    }
}

fn apt_mesa() -> Outcome {
    command::run(
        "apt-get",
        &[
            "install",
            "-y",
            "libgl1-mesa-dri",
            "mesa-vulkan-drivers",
            "libvulkan1",
        ],
        &[("DEBIAN_FRONTEND", "noninteractive")],
        APT_BUDGET,
    )
}

/// Stages the Mesa tree the payload carries, and tells the linker about it.
fn bundled_mesa(report: &mut Report, triplet: &str) -> Result<PathBuf, String> {
    let source = Path::new(PAYLOAD).join("content").join("mesa");
    let prefix = PathBuf::from(MESA_PREFIX);
    if !source.is_dir() {
        let reason = format!(
            "the payload's policy is bundled and {} is not there",
            source.display()
        );
        report.failed(GpuRecipeStep::Userspace, reason.clone());
        return Err(reason);
    }

    let changed = copy_tree(&source, &prefix).map_err(|error| {
        let reason = format!("{MESA_PREFIX} could not be staged: {error}");
        report.failed(GpuRecipeStep::Userspace, reason.clone());
        reason
    })?;

    let libraries = format!("{MESA_PREFIX}/lib/{triplet}");
    let line = format!("# Written by vmlord-agent. Do not edit.\n{libraries}\n");
    if let Err(error) = write_if_different(Path::new(MESA_LD_CONF), &line) {
        let reason = format!("{MESA_LD_CONF} could not be written: {error}");
        report.failed(GpuRecipeStep::Userspace, reason.clone());
        return Err(reason);
    }
    let _ = command::run("ldconfig", &[], &[], SHORT_BUDGET);

    if changed {
        report.ok(
            GpuRecipeStep::Userspace,
            format!("staged the payload's Mesa at {MESA_PREFIX} and told the linker about {libraries}"),
        );
    } else {
        report.skipped(
            GpuRecipeStep::Userspace,
            format!("{MESA_PREFIX} already holds this payload's Mesa"),
        );
    }
    Ok(prefix)
}
```

- [ ] **Step 6: Write the Vulkan and environment stages**

Add to `crates/agent/src/gpu_kernel.rs`:

```rust
/// Registers the Vulkan driver a payload carries, when it carries one.
///
/// A payload with GL and no Vulkan is a legitimate payload, so nothing here
/// fails on an absent ICD: whether a guest has enough of a renderer is the
/// probe's judgement, not a stage's.
fn vulkan_stage(report: &mut Report, userspace: &Userspace) -> Result<Option<String>, String> {
    let Some(prefix) = &userspace.prefix else {
        report.skipped(
            GpuRecipeStep::VulkanIcd,
            "the distribution registers its own Vulkan drivers".to_owned(),
        );
        return Ok(None);
    };

    let directory = prefix.join("share/vulkan/icd.d");
    let names: Vec<String> = fs::read_dir(&directory)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    let documents = icd_documents(&names);
    if documents.is_empty() {
        report.skipped(
            GpuRecipeStep::VulkanIcd,
            format!("the payload carries no Vulkan driver in {}", directory.display()),
        );
        return Ok(None);
    }

    let mut registered = Vec::with_capacity(documents.len());
    for document in &documents {
        let link = Path::new(VULKAN_ICD).join(document);
        if let Err(error) = symlink_if_different(&directory.join(document), &link) {
            let reason = format!("{} could not be registered: {error}", link.display());
            report.failed(GpuRecipeStep::VulkanIcd, reason.clone());
            return Err(reason);
        }
        registered.push(link.to_string_lossy().into_owned());
    }

    report.ok(
        GpuRecipeStep::VulkanIcd,
        format!("registered {} in {VULKAN_ICD}", documents.join(", ")),
    );
    Ok(registered.into_iter().next())
}

/// Writes what makes a process in this guest pick that userspace.
fn environment_stage(
    report: &mut Report,
    userspace: &Userspace,
    icd: Option<String>,
) -> Result<(), String> {
    let environment = Environment {
        library_paths: userspace.library_paths.clone(),
        icd,
    };

    let mut changed = false;
    for (path, form) in [
        (GENERATOR, Shell::Generator),
        (PROFILE, Shell::Profile),
    ] {
        match write_script_if_different(Path::new(path), &environment_document(form, &environment)) {
            Ok(written) => changed |= written,
            Err(error) => {
                let reason = format!("{path} could not be written: {error}");
                report.failed(GpuRecipeStep::Environment, reason.clone());
                return Err(reason);
            }
        }
    }

    let policy = match userspace.policy {
        MesaPolicy::Distro => "the distribution's Mesa",
        MesaPolicy::Bundled => "the payload's Mesa",
    };
    if changed {
        report.ok(
            GpuRecipeStep::Environment,
            format!("{GENERATOR} and {PROFILE} now point a process at {policy}"),
        );
    } else {
        report.skipped(
            GpuRecipeStep::Environment,
            format!("{GENERATOR} and {PROFILE} already point a process at {policy}"),
        );
    }
    Ok(())
}
```

`write_if_different` returns `io::Result<()>`; if the compiler objects to the `.is_ok()` shape above, keep the same behaviour with a `match` that reports the `io::Error` in the message.

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test -p vmlord-agent`
Expected: PASS.

- [ ] **Step 8: Build the agent for its real target**

Run: `cargo agent`
Expected: the musl build succeeds with no warnings.

- [ ] **Step 9: Commit**

```bash
git add crates/agent/src/gpu_kernel.rs
git commit -m "$(cat <<'EOF'
TASK-96: Install the GPU userspace an Ubuntu guest renders on

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: The recipe as documented

**Files:**
- Modify: `ARCHITECTURE.md` (the "GPU: the guest's Ubuntu recipe" section)

**Interfaces:**
- Consumes: the behaviour of Tasks 1-3.
- Produces: nothing code depends on.

- [ ] **Step 1: Extend the recipe section**

In `ARCHITECTURE.md`, in "GPU: the guest's Ubuntu recipe", change the sentence that says the schema moves to **1.3** so it reads that the userspace steps moved it to **1.4**, and add after the kernel stages:

```markdown
The userspace half is three more stages of the same report. `USERSPACE` honours
the payload's own `mesa_policy`, which it reads itself rather than through the
payload stage: a policy from a payload built newer than the agent must fail the
stage it belongs to, not one after which a kernel module would have built.
Under `distro` it installs `libgl1-mesa-dri`, `mesa-vulkan-drivers` and
`libvulkan1` from the guest's apt, and only when the d3d12 DRI module and the
Vulkan loader are not already there; Ubuntu does not build Mesa with
`microsoft-experimental`, so Vulkan under that policy is lavapipe, which the
stage says rather than hides. Under `bundled` it copies the payload's Mesa to
`/opt/vmlord/wsl-mesa` and names it in `/etc/ld.so.conf.d/vmlord-wsl-mesa.conf`
-- a copy, because the 9p mount lives as long as the agent's session while the
linker cache and the ICD symlink outlive a reboot.

`VULKAN_ICD` symlinks whatever `*.json` the payload's `share/vulkan/icd.d`
holds into `/etc/vulkan/icd.d`, by the names the payload uses; a payload with
no Vulkan driver is skipped and never failed. `ENVIRONMENT` writes
`/etc/systemd/user-environment-generators/50-vmlord-gpu` and
`/etc/profile.d/vmlord-gpu.sh` from one builder -- scripts that probe
`/dev/dxg` on every start rather than a file of finished values, because the
file survives a reboot into `GpuMode::None` and the device does not.

`DEVICE` is what gates all three: a guest whose device node never appeared is
one that must not be configured for a driver that cannot open it, so the
userspace stages are reported as skipped with that reason.
```

- [ ] **Step 2: Verify both builds still pass**

Run: `cargo test-windows` then `cargo check-windows`
Expected: PASS — the host was not changed, and this proves the protocol bump did not break it.

- [ ] **Step 3: Commit**

```bash
git add ARCHITECTURE.md
git commit -m "$(cat <<'EOF'
TASK-96: Document the Ubuntu GPU userspace recipe

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Verification on a real VM

The stages that touch apt, DKMS, the linker and `/etc` cannot run under `cargo
test`. After the tasks above, the recipe is proven by hand on an Ubuntu guest
with a GPU payload mounted:

* the host log shows ten stages, in order, with `USERSPACE`, `VULKAN_ICD` and
  `ENVIRONMENT` among them;
* `ls -l /etc/vulkan/icd.d` shows the payload's ICD under a payload name;
* a login over SSH has `GALLIUM_DRIVER=d3d12` in its environment, and a VM
  restarted with `GpuMode::None` has nothing of the sort;
* a second start of the same VM reports the three stages as skipped and runs no
  apt.
