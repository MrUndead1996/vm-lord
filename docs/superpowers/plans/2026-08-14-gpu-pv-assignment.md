# GPU-PV HCS assignment implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a safe, best-effort-ready Rust service that applies HCS GPU-PV Default or Mirror assignment to a running compute system.

**Architecture:** `hcs.rs` retains the single `unsafe` HCS call and exposes a safe modify-operation primitive whose failure keeps the HRESULT and optional HCS result document. New `gpu_assignment.rs` owns mode-to-request conversion and maps that primitive into `GpuFailure`; it is deliberately not wired into lifecycle because #98 must first persist desired GPU mode.

**Tech Stack:** Rust 2024, `windows` 0.61 Host Compute System bindings already enabled, `serde_json`, no new dependencies.

**Spec:** `docs/superpowers/specs/2026-08-14-gpu-pv-assignment-design.md`

## Global Constraints

* Keep all `unsafe` HCS calls in `crates/platform/src/hcs.rs`; the new assignment service contains no `unsafe`.
* Use HCS resource path `VirtualMachine/ComputeTopology/Gpu`, request type `Update`, and `Settings.AssignmentMode` `Default` or `Mirror`.
* Preserve the call/operation HRESULT and the unparsed HCS result document in every HCS operation failure.
* Do not change `hcs_config.rs`, `repository.rs`, `start.rs`, UI, persisted GPU state, or retry behaviour; #98 owns lifecycle integration.
* Use `cargo check-windows` and `cargo test-windows` for final verification. Commit subjects use `TASK-89: `.

---

## File structure

* Modify `crates/platform/src/hcs.rs`: safe modify operation and structured diagnostics around the existing HCS FFI boundary.
* Create `crates/platform/src/gpu_assignment.rs`: HCS GPU JSON builder and assignment service.
* Modify `crates/platform/src/lib.rs`: declare the service module and export its public service type.
* Modify `ARCHITECTURE.md`: describe the low-level assignment boundary and deferred lifecycle integration.

### Task 1: Retain HCS modify diagnostics

**Files:**

* Modify: `crates/platform/src/hcs.rs`
* Test: `crates/platform/src/hcs.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**

* Produces `pub(crate) struct HcsModifyFailure { pub(crate) hresult: u32, pub(crate) result_detail: Option<String> }` and `pub(crate) fn HcsModifyFailure::new(hresult: u32, result_detail: Option<String>) -> Self`.
* Produces `pub(crate) fn HcsSystem::modify(&self, document: &str, timeout: Duration) -> Result<String, HcsModifyFailure>`.
* Consumes `HcsModifyComputeSystem`, `HcsOperation`, and `HcsAllocatedString` already owned by this module.

- [ ] **Step 1: Write the failing unit tests**

Add pure formatting tests beside the existing detach-document tests. The exact production change the test detects is removal of the HRESULT or result text from an HCS diagnostic.

```rust
#[test]
fn modify_failure_keeps_the_hresult_and_result_detail() {
    let failure = HcsModifyFailure::new(0x8037_010D, Some(r#"{"Error":"bad GPU"}"#.into()));

    assert_eq!(failure.hresult, 0x8037_010D);
    assert_eq!(failure.result_detail.as_deref(), Some(r#"{"Error":"bad GPU"}"#));
}
```

- [ ] **Step 2: Run the focused test to verify it fails**

Run: `rtk cargo test-windows -p vmlord-platform hcs::tests::modify_failure_keeps_the_hresult_and_result_detail`

Expected: compilation failure because `HcsModifyFailure` does not exist.

- [ ] **Step 3: Implement the smallest safe HCS primitive**

In `hcs.rs`, add the structured failure and make the operation wait path preserve the optional result string before converting the operation HRESULT into an error. Add a `HcsSystem::modify` method that owns the operation handle, converts `document` to `HSTRING`, invokes `HcsModifyComputeSystem` with a null identity, and waits for the supplied timeout.

```rust
pub(crate) struct HcsModifyFailure {
    pub(crate) hresult: u32,
    pub(crate) result_detail: Option<String>,
}

pub(crate) fn modify(
    &self,
    document: &str,
    timeout: Duration,
) -> Result<String, HcsModifyFailure>
```

On an immediate `HcsModifyComputeSystem` failure, return `error.code().0 as u32` and `None`. When `HcsWaitForOperationResult` returns a failing HRESULT, first turn the allocated result pointer into `Option<String>` and carry it in `result_detail`; do not parse it. Keep the existing `remove_network_adapter` behaviour by making it call this primitive and map a failure back to `RepositoryError` for its caller.

- [ ] **Step 4: Run focused HCS tests to verify they pass**

Run: `rtk cargo test-windows -p vmlord-platform hcs::tests`

Expected: PASS, including existing detach and operation-result tests.

- [ ] **Step 5: Commit the HCS boundary**

```bash
rtk git add crates/platform/src/hcs.rs
rtk git commit -m "TASK-89: Preserve HCS modify diagnostics"
```

### Task 2: Add the GPU-PV assignment service

**Files:**

* Create: `crates/platform/src/gpu_assignment.rs`
* Modify: `crates/platform/src/lib.rs`
* Test: `crates/platform/src/gpu_assignment.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**

* Consumes `vmlord_core::{GpuFailure, GpuMode, GpuStatusCode}` and `crate::hcs::{HcsModifyFailure, HcsSystem}`.
* Produces `pub struct GpuAssignmentService` with `pub fn assign(&self, system: &HcsSystem, mode: GpuMode) -> Result<(), GpuFailure>`.
* Produces `pub(crate) fn assignment_document(mode: GpuMode) -> Result<Option<String>, GpuFailure>`.

- [ ] **Step 1: Write failing JSON and diagnostic tests**

Add tests that parse the generated JSON and assert every HCS contract field, plus tests for unsupported modes and failure rendering. The production changes they detect are a wrong resource path, request type, mode spelling, omitted detail, or calling HCS for `None`.

```rust
#[test]
fn default_mode_updates_the_gpu_resource() {
    let document = assignment_document(GpuMode::Default).unwrap().unwrap();
    let value: serde_json::Value = serde_json::from_str(&document).unwrap();
    assert_eq!(value["ResourcePath"], "VirtualMachine/ComputeTopology/Gpu");
    assert_eq!(value["RequestType"], "Update");
    assert_eq!(value["Settings"]["AssignmentMode"], "Default");
}

#[test]
fn mirror_mode_updates_the_gpu_resource() {
    let document = assignment_document(GpuMode::Mirror).unwrap().unwrap();
    let value: serde_json::Value = serde_json::from_str(&document).unwrap();
    assert_eq!(value["Settings"]["AssignmentMode"], "Mirror");
}

#[test]
fn none_mode_needs_no_hcs_request() {
    assert_eq!(assignment_document(GpuMode::None).unwrap(), None);
}

#[test]
fn hcs_failure_includes_hresult_and_result_detail() {
    let failure = assignment_failure(HcsModifyFailure::new(
        0x8037_010D,
        Some(r#"{"Error":"GPU unavailable"}"#.into()),
    ));
    assert_eq!(failure.code, GpuStatusCode::AssignmentFailed);
    assert!(failure.message.contains("HRESULT 0x8037010D"));
    assert!(failure.message.contains("GPU unavailable"));
}
```

- [ ] **Step 2: Run the new test module to verify it fails**

Run: `rtk cargo test-windows -p vmlord-platform gpu_assignment::tests`

Expected: compilation failure because module and functions do not exist.

- [ ] **Step 3: Implement the service without lifecycle coupling**

Create `gpu_assignment.rs`. Use `serde_json::json!` and `serde_json::to_string` to generate:

```json
{
  "ResourcePath": "VirtualMachine/ComputeTopology/Gpu",
  "RequestType": "Update",
  "Settings": { "AssignmentMode": "Default" }
}
```

Map `GpuMode::None` to `Ok(None)`. Map `GpuMode::Unknown(value)` to
`GpuFailure::new(GpuStatusCode::AssignmentFailed, format!("GPU mode {value} is not supported by this build"))`. For `Default` and `Mirror`, call `system.modify(&document, GPU_ASSIGNMENT_TIMEOUT)`, where the timeout is `Duration::from_secs(30)`. Convert `HcsModifyFailure` to `GpuFailure` with a message containing `HRESULT 0x{hresult:08X}` and append the result detail when it is non-empty.

Add `mod gpu_assignment;` to `lib.rs` and export only `GpuAssignmentService`; do not expose HCS diagnostic internals outside `platform`.

- [ ] **Step 4: Run focused service and platform tests to verify they pass**

Run: `rtk cargo test-windows -p vmlord-platform gpu_assignment::tests`

Expected: PASS.

Run: `rtk cargo test-windows -p vmlord-platform`

Expected: PASS with ignored real-host tests still ignored.

- [ ] **Step 5: Commit the assignment service**

```bash
rtk git add crates/platform/src/gpu_assignment.rs crates/platform/src/lib.rs
rtk git commit -m "TASK-89: Add GPU-PV assignment service"
```

### Task 3: Document the completed boundary

**Files:**

* Modify: `ARCHITECTURE.md`
* Test: `cargo check-windows` and `cargo test-windows`

**Interfaces:**

* Consumes `GpuAssignmentService` from Task 2.
* Produces architecture documentation that names its HCS JSON update, diagnostic guarantees, best-effort contract, and #98 lifecycle boundary.

- [ ] **Step 1: Add the architecture paragraph**

In the GPU-PV section, explain that `platform::gpu_assignment` maps `Default` and `Mirror` to `HcsModifyComputeSystem` GPU updates, waits for HCS, and preserves HRESULT plus raw result detail in its `GpuFailure`. State that the service is safe and best-effort-ready, while desired-mode persistence, invocation after start, facts, and UI status are deferred to #98.

- [ ] **Step 2: Verify the complete project**

Run: `rtk cargo check-windows`

Expected: exit status 0 with no compile errors.

Run: `rtk cargo test-windows`

Expected: exit status 0; no non-ignored test failures.

- [ ] **Step 3: Inspect the final change set**

Run: `rtk git diff --check HEAD~2..HEAD && rtk git diff --check && rtk git status --short`

Expected: no whitespace errors and no unexpected files.

- [ ] **Step 4: Commit documentation**

```bash
rtk git add ARCHITECTURE.md
rtk git commit -m "TASK-89: Document GPU-PV assignment boundary"
```

## Done when

* `GpuAssignmentService` maps Default and Mirror to the documented HCS GPU update request.
* The HCS layer retains a numeric HRESULT and optional raw result detail for modify-operation failures.
* `None` produces no HCS request and unknown modes fail locally.
* `start.rs`, `repository.rs`, `hcs_config.rs`, and UI remain unchanged; #98 is the sole lifecycle integration task.
* `cargo check-windows` and `cargo test-windows` pass, and no merge request is opened without explicit approval.
