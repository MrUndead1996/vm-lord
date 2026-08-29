# AppSandbox Linux VM Import Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Discover completed AppSandbox Linux VMs and safely copy and convert one into a fully verified VMLord VM without changing the source.

**Architecture:** Add import domain contracts to `vmlord-core`, keep orchestration in `vmlord-app`, and implement AppSandbox parsing, Windows validation/copying, HCS bootstrap, SSH conversion, recovery, and final verification in focused `vmlord-platform` modules. The existing background-build and progress patterns carry long-running imports to the UI, while a durable import journal distinguishes recoverable conversion state from ordinary VM metadata.

**Tech Stack:** Rust 2024 workspace, serde/serde_json, native Windows FileSystem/VHD/HCS APIs, Windows OpenSSH executables invoked without a shell, egui/eframe, rust-i18n, tracing and `vmlord_core::diagnostic!`.

**Spec:** `docs/superpowers/specs/2026-08-29-appsandbox-linux-import-design.md`

## Global Constraints

- Import completed AppSandbox Linux VMs only; Hyper-V imports, Windows guests, AppSandbox templates, and VM export remain out of scope.
- Copy `disk.vhdx`; never move, link, adopt, rename, delete, start, or modify the AppSandbox source VM.
- Require `SshEnabled=1` and `SshDeployKey=1`; use the AppSandbox private key only for bootstrap and never copy it into VMLord storage.
- The first copied-guest boot uses NAT and SSH with VMLord GPU/display disabled; the second boot uses VMLord agent, display, and GPU integration.
- Application code is Rust; do not add C, FFI, PowerShell, WMI, or shell command construction.
- Keep Windows APIs and `unsafe` inside platform-specific modules; prefer native Windows APIs over external processes except the repository's established Windows OpenSSH integration.
- UI calls only the application layer, contains no business logic, and sends every new user-facing string through `t!` with entries in both `crates/ui/locales/en-US.toml` and `crates/ui/locales/ru-RU.toml`.
- User-facing events use `vmlord_core::diagnostic!`; ordinary records use `tracing`; secret values implement neither revealing `Display` nor revealing `Debug`.
- Do not add a dependency that links the Linux agent against system C libraries.
- Use `.cargo/config.toml` aliases: `cargo check-windows` and `cargo test-windows`.

---

## File Map

- `crates/core/src/appsandbox.rs`: import candidates, compatibility reasons, request validation, durable stage and progress models.
- `crates/core/src/lib.rs`: exports AppSandbox models and extends `VmRepository` with discovery/import/recovery operations.
- `crates/core/src/progress.rs`: import-specific progress values alongside existing build progress.
- `crates/app/src/appsandbox.rs`: application workflow, diagnostics, polling, cancellation, retry and cleanup commands.
- `crates/app/src/lib.rs`: owns `AppSandboxImportManager` and exposes it to the UI.
- `crates/platform/src/appsandbox/config.rs`: strict `vms.cfg` parser with no Windows dependencies.
- `crates/platform/src/appsandbox/discovery.rs`: default paths and candidate compatibility evaluation.
- `crates/platform/src/appsandbox/copy.rs`: native cancellable file copy and destination-space checks.
- `crates/platform/src/appsandbox/journal.rs`: atomic durable import journal and startup recovery discovery.
- `crates/platform/src/appsandbox/bootstrap.rs`: creates a VMLord compute system around the copied disk for the SSH-only first boot.
- `crates/platform/src/appsandbox/conversion.rs`: manifest-bound, idempotent SSH conversion stages.
- `crates/platform/src/appsandbox/verify.rs`: second-boot SSH, agent, display and GPU acceptance checks.
- `crates/platform/src/appsandbox/worker.rs`: end-to-end staged import transaction, rollback and needs-attention outcome.
- `crates/platform/src/appsandbox/mod.rs`: narrow public facade for the AppSandbox modules.
- `crates/platform/src/import_registry.rs`: background worker ownership, progress snapshots, cancellation and completed outcomes.
- `crates/platform/src/layout.rs`: import staging, marker, transcript and journal paths.
- `crates/platform/src/metadata.rs`: conversion from a completed journal into normal `VmComputeSystemMapping`.
- `crates/platform/src/repository.rs`: repository trait wiring, operation exclusion, listing and recovery integration.
- `crates/platform/src/lib.rs`: module declarations and test-visible exports.
- `crates/ui/src/appsandbox_import.rs`: import dialog state and rendering.
- `crates/ui/src/lib.rs`: toolbar action, dialog orchestration and regular polling.
- `crates/ui/locales/en-US.toml`, `crates/ui/locales/ru-RU.toml`: import text.
- `ARCHITECTURE.md`, `docs/appsandbox-import.md`: architecture and user documentation.

### Task 1: Domain Contract and Repository Boundary

**Files:**
- Create: `crates/core/src/appsandbox.rs`
- Modify: `crates/core/src/lib.rs`
- Modify: `crates/core/src/progress.rs`

**Interfaces:**
- Produces: `AppSandboxVmCandidate`, `AppSandboxCompatibility`, `AppSandboxImportRequest`, `AppSandboxImportStage`, `AppSandboxImportProgress`, `IncompleteAppSandboxImport`, `VmRepository::{discover_appsandbox_vms,start_appsandbox_import,cancel_appsandbox_import,incomplete_appsandbox_imports,retry_appsandbox_import,discard_appsandbox_import}`.

- [ ] **Step 1: Write validation and serialization tests**

Add tests covering a valid renamed request, an empty/invalid name, zero RAM/disk/CPU, a source identity with an empty path, progress serialization round-trips, and redacted candidate debug output.

```rust
#[test]
fn import_request_rejects_a_path_escaping_vm_name() {
    let mut request = valid_import_request();
    request.destination_name = "../ubuntu".into();
    assert!(request.validate().unwrap_err().to_string().contains("name"));
}

#[test]
fn source_private_key_is_not_part_of_candidate_debug() {
    let candidate = valid_candidate();
    assert!(!format!("{candidate:?}").contains("PRIVATE KEY"));
}
```

- [ ] **Step 2: Run the focused core tests and confirm failure**

Run: `cargo test -p vmlord-core appsandbox -- --nocapture`
Expected: FAIL because `vmlord_core::appsandbox` and its types do not exist.

- [ ] **Step 3: Implement the domain types and validation**

Use opaque source identity rather than accepting an arbitrary path back from the UI:

```rust
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppSandboxSourceId(String);

impl AppSandboxSourceId {
    pub fn from_stable_hash(hash: impl Into<String>) -> Result<Self, RepositoryError> {
        let hash = hash.into();
        (!hash.is_empty())
            .then_some(Self(hash))
            .ok_or_else(|| RepositoryError::new("AppSandbox source identity must not be empty"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppSandboxImportRequest {
    pub source_id: AppSandboxSourceId,
    pub destination_name: String,
}

impl AppSandboxImportRequest {
    pub fn validate(&self) -> Result<(), RepositoryError> {
        validate_vm_name(&self.destination_name)?;
        if self.source_id.as_str().is_empty() {
            return Err(RepositoryError::new("AppSandbox source identity must not be empty"));
        }
        Ok(())
    }
}
```

Define compatibility as `Compatible` or `Incompatible(Vec<AppSandboxIncompatibility>)`; define stages `Validating`, `Copying`, `Creating`, `BootstrapStarting`, `Converting`, `Restarting`, `Verifying`, `NeedsAttention`, and `Complete`. Add default repository methods that return an explicit unsupported error so existing fake repositories remain source-compatible until Task 7.

- [ ] **Step 4: Run core tests**

Run: `cargo test -p vmlord-core appsandbox -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit the contract**

```bash
git add crates/core/src/appsandbox.rs crates/core/src/lib.rs crates/core/src/progress.rs
git commit -m "TASK-21: Add AppSandbox import domain contract"
```

### Task 2: Strict AppSandbox Configuration Parser

**Files:**
- Create: `crates/platform/src/appsandbox/config.rs`
- Create: `crates/platform/src/appsandbox/mod.rs`
- Modify: `crates/platform/src/lib.rs`
- Test: `crates/platform/tests/fixtures/appsandbox/*.cfg`

**Interfaces:**
- Consumes: `AppSandboxSourceId` from Task 1.
- Produces: `parse_vms_cfg(input: &str) -> Result<Vec<ParsedVm>, RepositoryError>` and `ParsedVm` accessors used only inside `platform::appsandbox`.

- [ ] **Step 1: Add parser fixtures and failing tests**

Create fixtures for one Linux VM, two `[VM]` sections, duplicate `Name`, missing `VhdxPath`, malformed integers, CRLF input, a Windows VM, and unrelated `[Settings]` values.

```rust
#[test]
fn parses_two_vm_sections_without_leaking_settings_fields() {
    let parsed = parse_vms_cfg(include_str!("../../../tests/fixtures/appsandbox/two-linux.cfg"))
        .expect("fixture is valid");
    assert_eq!(parsed.iter().map(ParsedVm::name).collect::<Vec<_>>(), ["ubuntu", "fedora"]);
}
```

- [ ] **Step 2: Run the parser tests and confirm failure**

Run: `cargo test-windows -p vmlord-platform appsandbox::config`
Expected: FAIL because the parser is absent.

- [ ] **Step 3: Implement a small line-oriented INI parser**

Do not add an INI dependency. Track section name, VM ordinal and seen keys; preserve line numbers in errors; accept unknown keys but reject duplicate required keys in the same VM.

```rust
pub(super) fn parse_vms_cfg(input: &str) -> Result<Vec<ParsedVm>, RepositoryError> {
    let mut parser = Parser::default();
    for (index, raw) in input.lines().enumerate() {
        parser.consume(index + 1, raw.trim_end_matches('\r'))?;
    }
    parser.finish()
}
```

- [ ] **Step 4: Run parser tests**

Run: `cargo test-windows -p vmlord-platform appsandbox::config`
Expected: PASS.

- [ ] **Step 5: Commit the parser**

```bash
git add crates/platform/src/appsandbox crates/platform/src/lib.rs crates/platform/tests/fixtures/appsandbox
git commit -m "TASK-21: Parse AppSandbox VM configuration"
```

### Task 3: Discovery and Compatibility Evaluation

**Files:**
- Create: `crates/platform/src/appsandbox/discovery.rs`
- Create: `crates/platform/src/appsandbox/source.rs`
- Modify: `crates/platform/src/appsandbox/mod.rs`
- Modify: `crates/platform/src/repository.rs`

**Interfaces:**
- Consumes: `ParsedVm`, core candidate/compatibility models.
- Produces: `Discovery::default_windows()`, `Discovery::discover() -> Result<Vec<AppSandboxVmCandidate>, RepositoryError>`, `ValidatedSource`, and repository discovery implementation.

- [ ] **Step 1: Write compatibility matrix tests with injected probes**

Cover Linux success, Windows, incomplete install, SSH disabled, key undeployed, missing disk, disk path mismatch, unsupported network/GPU mode, invalid SSH port, and duplicate source IDs.

```rust
#[test]
fn windows_vm_is_visible_with_an_incompatibility_reason() {
    let candidates = discovery_with("OsType=Windows\n", existing_disk()).discover().unwrap();
    assert!(matches!(candidates[0].compatibility, AppSandboxCompatibility::Incompatible(_)));
}
```

- [ ] **Step 2: Run discovery tests and confirm failure**

Run: `cargo test-windows -p vmlord-platform appsandbox::discovery`
Expected: FAIL because `Discovery` is absent.

- [ ] **Step 3: Implement discovery with injected filesystem/disk probes**

Derive `%ProgramData%\AppSandbox\vms.cfg` and `%ProgramData%\AppSandbox\ssh\id_appsandbox` in `default_windows`. Generate a stable source ID by hashing the canonical VHDX path plus VM section ordinal; retain the resolved paths only in a platform-owned discovery snapshot so the UI cannot substitute them.

```rust
pub(crate) struct DiscoveryResult {
    pub candidates: Vec<AppSandboxVmCandidate>,
    pub sources: HashMap<AppSandboxSourceId, ValidatedSource>,
}
```

- [ ] **Step 4: Run discovery and repository tests**

Run: `cargo test-windows -p vmlord-platform appsandbox::discovery repository::tests::discovers_appsandbox_vms`
Expected: PASS.

- [ ] **Step 5: Commit discovery**

```bash
git add crates/platform/src/appsandbox crates/platform/src/repository.rs
git commit -m "TASK-21: Discover compatible AppSandbox Linux VMs"
```

### Task 4: Durable Journal, Paths and Recovery Classification

**Files:**
- Create: `crates/platform/src/appsandbox/journal.rs`
- Modify: `crates/platform/src/layout.rs`
- Modify: `crates/platform/src/appsandbox/mod.rs`

**Interfaces:**
- Produces: `ImportJournal::{create,load,save,remove,list}`, `JournalStage`, `layout::{imports_root,import_staging_directory,import_journal_path,import_transcript_path}`.

- [ ] **Step 1: Write journal atomicity and path-containment tests**

```rust
#[test]
fn journal_refuses_a_destination_outside_storage_root() {
    let journal = fixture_journal(PathBuf::from(r"C:\AppSandbox\ubuntu"));
    assert!(journal.validate_under(Path::new(r"C:\VMLord\vms")).is_err());
}
```

Test serialization at every stage, replacement through a sibling temporary file, corrupted journal reporting, and listing an interrupted import after constructing a fresh store.

- [ ] **Step 2: Run journal tests and confirm failure**

Run: `cargo test-windows -p vmlord-platform appsandbox::journal layout::tests::import`
Expected: FAIL because journal and paths are absent.

- [ ] **Step 3: Implement the journal**

Write JSON to `journal.json.new`, flush it, then replace `journal.json`. Persist source fingerprint, destination, requested resources, desired GPU, bootstrap SSH facts, current stage and last confirmed conversion step. Store no private-key bytes or agent-secret value in the journal.

- [ ] **Step 4: Run journal tests**

Run: `cargo test-windows -p vmlord-platform appsandbox::journal layout::tests::import`
Expected: PASS.

- [ ] **Step 5: Commit durable import state**

```bash
git add crates/platform/src/appsandbox/journal.rs crates/platform/src/appsandbox/mod.rs crates/platform/src/layout.rs
git commit -m "TASK-21: Persist recoverable AppSandbox imports"
```

### Task 5: Native VHDX Copy with Space, Progress and Cancellation

**Files:**
- Create: `crates/platform/src/appsandbox/copy.rs`
- Modify: `crates/platform/src/appsandbox/mod.rs`
- Modify: `crates/platform/Cargo.toml`

**Interfaces:**
- Produces: `copy_vhdx(request: CopyRequest<'_>) -> Result<CopySummary, RepositoryError>`; consumes `BuildMonitor` cancellation and reports `AppSandboxImportProgress` through an injected publisher.

- [ ] **Step 1: Add failing copy-policy and callback tests**

Test insufficient free bytes, cancellation before copy, cancellation from the progress callback, source identity changing between validation and open, partial target removal, and successful byte-for-byte copy of a sparse test file.

```rust
#[test]
fn cancellation_removes_only_the_staged_target() {
    let result = copy_fixture(cancelled_monitor());
    assert!(result.is_err());
    assert!(source_path().exists());
    assert!(!target_path().exists());
}
```

- [ ] **Step 2: Run copy tests and confirm failure**

Run: `cargo test-windows -p vmlord-platform appsandbox::copy`
Expected: FAIL because `copy_vhdx` is absent.

- [ ] **Step 3: Implement the native copy boundary**

Use `GetDiskFreeSpaceExW` and `CopyFileExW`/`CopyFile2` with a callback. Keep callback context allocation owned for the full call, translate cancellation to `ERROR_REQUEST_ABORTED`, and remove only the exact canonical staging target after a failed call. Add only the precise `windows` feature needed if bindings require one already not enabled.

```rust
pub(super) struct CopyRequest<'a> {
    pub source: &'a ValidatedSource,
    pub target: &'a Path,
    pub cancel: &'a AtomicBool,
    pub publish: &'a dyn Fn(u64, u64),
}
```

- [ ] **Step 4: Run copy tests**

Run: `cargo test-windows -p vmlord-platform appsandbox::copy`
Expected: PASS.

- [ ] **Step 5: Commit native copying**

```bash
git add crates/platform/src/appsandbox/copy.rs crates/platform/src/appsandbox/mod.rs crates/platform/Cargo.toml
git commit -m "TASK-21: Copy AppSandbox disks safely"
```

### Task 6: Bootstrap Compute System Around the Copied Disk

**Files:**
- Create: `crates/platform/src/appsandbox/bootstrap.rs`
- Modify: `crates/platform/src/hcs_config.rs`
- Modify: `crates/platform/src/metadata.rs`
- Modify: `crates/platform/src/create.rs`

**Interfaces:**
- Produces: `HcsVmConfigBuilder::build_import_bootstrap`, `BootstrapVm`, `VmComputeSystemMapping::from_completed_import`; reuses VMLord state-file creation, access grants, NAT endpoint and per-VM key generation.

- [ ] **Step 1: Add failing configuration tests**

Assert the bootstrap configuration attaches only the copied system VHDX, gives the VM VMLord agent/display service permissions, contains no installer/seed/tools ISO, contains no GPU Plan9 shares, uses fresh VMGS/VMRS paths, and sets the copied RAM/CPU values.

```rust
#[test]
fn import_bootstrap_has_no_appsandbox_or_gpu_exports() {
    let json = HcsVmConfigBuilder::build_import_bootstrap(&fixture()).unwrap();
    assert!(!json.contains("AppSandbox"));
    assert!(!json.contains("Plan9"));
}
```

- [ ] **Step 2: Run bootstrap tests and confirm failure**

Run: `cargo test-windows -p vmlord-platform appsandbox::bootstrap hcs_config::tests::import`
Expected: FAIL because the bootstrap builder is absent.

- [ ] **Step 3: Implement bootstrap creation using shared primitives**

Extract only the state-file/access/key helpers currently private to `create.rs`; do not duplicate HCS security descriptor or VMGS creation logic. Build an ephemeral mapping with NAT, desired SSH port/user and `GpuMode::None`; keep desired GPU/desktop in the journal until conversion completes.

- [ ] **Step 4: Run bootstrap tests**

Run: `cargo test-windows -p vmlord-platform appsandbox::bootstrap hcs_config::tests::import metadata::tests::completed_import`
Expected: PASS.

- [ ] **Step 5: Commit bootstrap support**

```bash
git add crates/platform/src/appsandbox/bootstrap.rs crates/platform/src/hcs_config.rs crates/platform/src/metadata.rs crates/platform/src/create.rs
git commit -m "TASK-21: Bootstrap copied AppSandbox guests"
```

### Task 7: Idempotent SSH Guest Conversion

**Files:**
- Create: `crates/platform/src/appsandbox/conversion.rs`
- Create: `crates/platform/src/appsandbox/bundle.rs`
- Modify: `crates/platform/src/ssh.rs`
- Modify: `crates/platform/src/appsandbox/journal.rs`
- Modify: `crates/platform/src/appsandbox/mod.rs`

**Interfaces:**
- Produces: `ConversionBundle::build`, `ConversionRunner::run`, `GuestIdentity`; adds a bootstrap-key variant to the existing argument-vector SSH builder without exposing private key material.

- [ ] **Step 1: Write manifest, invocation and replay tests**

Test deterministic manifest hashes, rejection of a missing bundled agent/payload, argument-vector use of the AppSandbox key, no secret in `Debug`, recording `/etc/os-release` facts, exact systemd unit disablement, and resumption after each confirmed conversion step.

```rust
#[test]
fn replay_skips_confirmed_steps_but_revalidates_their_postconditions() {
    let report = runner_with_stage(ConversionStep::VmlordAgentInstalled).run().unwrap();
    assert!(!report.commands.iter().any(|command| command.label == "install-agent"));
    assert!(report.commands.iter().any(|command| command.label == "verify-agent-files"));
}
```

- [ ] **Step 2: Run conversion tests and confirm failure**

Run: `cargo test-windows -p vmlord-platform appsandbox::conversion appsandbox::bundle ssh::tests::bootstrap`
Expected: FAIL because conversion types are absent.

- [ ] **Step 3: Implement shell-free host invocation and a fixed guest script**

Host-side values travel as separate `ssh.exe`/`scp.exe` arguments. The uploaded bundle contains a fixed root-owned script; user-controlled name/path values are stored in a JSON input file parsed by the script's bundled helper, never interpolated into a command string. Each stage verifies its postcondition before atomically advancing the journal.

The ordered stages are: observe guest, upload and verify bundle, deploy VMLord SSH key, install agent secret and service, install display payload, install GPU payload, disable AppSandbox units, validate replacements, remove obsolete AppSandbox files, request shutdown.

- [ ] **Step 4: Run conversion and secret-redaction tests**

Run: `cargo test-windows -p vmlord-platform appsandbox::conversion appsandbox::bundle ssh::tests::bootstrap`
Expected: PASS.

- [ ] **Step 5: Commit conversion**

```bash
git add crates/platform/src/appsandbox/conversion.rs crates/platform/src/appsandbox/bundle.rs crates/platform/src/appsandbox/journal.rs crates/platform/src/appsandbox/mod.rs crates/platform/src/ssh.rs
git commit -m "TASK-21: Convert AppSandbox guests to VMLord"
```

### Task 8: Second Boot Verification and Transaction Outcomes

**Files:**
- Create: `crates/platform/src/appsandbox/verify.rs`
- Create: `crates/platform/src/appsandbox/worker.rs`
- Modify: `crates/platform/src/appsandbox/mod.rs`
- Modify: `crates/platform/src/metadata.rs`
- Modify: `crates/platform/src/start.rs`

**Interfaces:**
- Consumes: copy, journal, bootstrap and conversion interfaces from Tasks 4-7.
- Produces: `ImportWorker::run(&BuildMonitor) -> ImportWorkerOutcome`, `Verification::run`, complete/needs-attention/rolled-back outcomes.

- [ ] **Step 1: Add failing transaction-table tests**

Use fakes for every side effect and cover failure before promotion, failure after promotion, cancellation during copy, cancellation after conversion begins, successful SSH/agent/display/GPU verification, and cleanup target containment.

```rust
#[test]
fn post_conversion_failure_preserves_copy_as_needs_attention() {
    let outcome = worker_failing_at(FailurePoint::AgentVerification).run(&monitor());
    assert!(matches!(outcome, ImportWorkerOutcome::NeedsAttention { .. }));
    assert!(fake_fs().destination_disk_exists());
    assert!(fake_fs().source_disk_exists());
}
```

- [ ] **Step 2: Run worker tests and confirm failure**

Run: `cargo test-windows -p vmlord-platform appsandbox::worker appsandbox::verify`
Expected: FAIL because the transaction does not exist.

- [ ] **Step 3: Implement the state machine**

Promote the staged disk only after copy validation. Before guest mutation, failures roll back the exact VMLord staging/final directory and ephemeral HCS object. Once conversion begins, failures persist `NeedsAttention` and retain the copy. On success, write ordinary metadata last, remove the journal, and hand the running HCS/session ownership back to the repository.

- [ ] **Step 4: Run transaction tests**

Run: `cargo test-windows -p vmlord-platform appsandbox::worker appsandbox::verify metadata::tests::completed_import`
Expected: PASS.

- [ ] **Step 5: Commit transaction and verification**

```bash
git add crates/platform/src/appsandbox/verify.rs crates/platform/src/appsandbox/worker.rs crates/platform/src/appsandbox/mod.rs crates/platform/src/metadata.rs crates/platform/src/start.rs
git commit -m "TASK-21: Verify and finalize imported VMs"
```

### Task 9: Background Registry, Repository Wiring and Recovery Commands

**Files:**
- Create: `crates/platform/src/import_registry.rs`
- Modify: `crates/platform/src/repository.rs`
- Modify: `crates/platform/src/lib.rs`
- Modify: `crates/platform/src/build.rs`

**Interfaces:**
- Consumes: Task 1 repository methods and Task 8 worker.
- Produces: repository discovery/start/cancel/list/retry/discard implementations and import summaries shown with `VmState::Building` progress.

- [ ] **Step 1: Add failing repository lifecycle tests**

Test duplicate destination refusal across builds/imports/stored VMs, background progress listing, cancellation, finished-worker reaping, startup journal discovery, retry, discard refusal for paths outside the storage root, and repository drop joining workers.

- [ ] **Step 2: Run repository tests and confirm failure**

Run: `cargo test-windows -p vmlord-platform import_registry repository::tests::appsandbox`
Expected: FAIL because import workers are not wired.

- [ ] **Step 3: Implement registry and repository integration**

Follow `BuildRegistry` ownership and poisoning recovery patterns, but store `AppSandboxImportRequest`, import progress and `ImportWorkerOutcome`. Share one name-reservation check between create and import. During `initialize`, load journals after metadata and before ordinary enumeration so incomplete imports cannot masquerade as healthy VMs.

- [ ] **Step 4: Run repository tests**

Run: `cargo test-windows -p vmlord-platform import_registry repository::tests::appsandbox`
Expected: PASS.

- [ ] **Step 5: Commit repository integration**

```bash
git add crates/platform/src/import_registry.rs crates/platform/src/repository.rs crates/platform/src/lib.rs crates/platform/src/build.rs
git commit -m "TASK-21: Run AppSandbox imports in background"
```

### Task 10: Application Workflow and Diagnostics

**Files:**
- Create: `crates/app/src/appsandbox.rs`
- Modify: `crates/app/src/lib.rs`

**Interfaces:**
- Consumes: Task 1 repository contract.
- Produces: `WorkspaceApp::{discover_appsandbox_vms,start_appsandbox_import,cancel_appsandbox_import,incomplete_appsandbox_imports,retry_appsandbox_import,discard_appsandbox_import}` and read-only import state for UI.

- [ ] **Step 1: Add failing application tests with `FakeRepository`**

Test ready-backend gating, discovery result retention, renamed request submission, error diagnostics, refresh after acceptance, cancellation diagnostics, retry/discard dispatch and no source path exposure.

```rust
#[test]
fn accepted_import_is_refreshed_into_the_vm_list() {
    let mut app = ready_app_with_candidate();
    app.start_appsandbox_import(request("ubuntu-copy")).unwrap();
    assert!(app.vms().iter().any(|vm| vm.name == "ubuntu-copy"));
}
```

- [ ] **Step 2: Run app tests and confirm failure**

Run: `cargo test -p vmlord-app appsandbox -- --nocapture`
Expected: FAIL because the application workflow is absent.

- [ ] **Step 3: Implement workflow and diagnostic boundaries**

Keep candidate selection and editable form values in application-owned state. Use `Subsystem::App` for form/request failures and `Subsystem::Hcs` for accepted platform operations. Diagnostics name VM and stage but never source private-key path contents or secret values.

- [ ] **Step 4: Run application tests**

Run: `cargo test -p vmlord-app appsandbox -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit application workflow**

```bash
git add crates/app/src/appsandbox.rs crates/app/src/lib.rs
git commit -m "TASK-21: Add AppSandbox import workflow"
```

### Task 11: Import Dialog and Localization

**Files:**
- Create: `crates/ui/src/appsandbox_import.rs`
- Modify: `crates/ui/src/lib.rs`
- Modify: `crates/ui/locales/en-US.toml`
- Modify: `crates/ui/locales/ru-RU.toml`

**Interfaces:**
- Consumes: Task 10 `WorkspaceApp` methods and core candidate/progress types.
- Produces: `AppSandboxImportDialog::{open,render}` and toolbar action.

- [ ] **Step 1: Add failing form and locale tests**

Test compatible/incompatible rows, displayed reason, default/editable destination name, submit disabled on invalid/conflicting name, cancel action by stage, needs-attention retry/discard controls, and catalogue parity.

```rust
#[test]
fn selected_candidate_prefills_but_does_not_lock_the_name() {
    let mut form = AppSandboxImportForm::from_candidate(&candidate("ubuntu"));
    assert_eq!(form.destination_name, "ubuntu");
    form.destination_name = "ubuntu-copy".into();
    assert_eq!(form.request().unwrap().destination_name, "ubuntu-copy");
}
```

- [ ] **Step 2: Run UI tests and confirm failure**

Run: `cargo test -p vmlord-ui appsandbox_import locale -- --nocapture`
Expected: FAIL because the dialog and locale keys are absent.

- [ ] **Step 3: Implement the dialog and toolbar flow**

Add a green-adjacent secondary toolbar button rather than overloading Create. Render stage labels and byte progress from application state. Incompatible candidates remain visible and selectable only for reading their reason. Confirmation names required disk space and states that the source remains unchanged.

- [ ] **Step 4: Run UI tests**

Run: `cargo test -p vmlord-ui appsandbox_import locale -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit UI and translations**

```bash
git add crates/ui/src/appsandbox_import.rs crates/ui/src/lib.rs crates/ui/locales/en-US.toml crates/ui/locales/ru-RU.toml
git commit -m "TASK-21: Add AppSandbox import dialog"
```

### Task 12: Documentation, Full Verification and Live End-to-End Import

**Files:**
- Modify: `ARCHITECTURE.md`
- Create: `docs/appsandbox-import.md`

**Interfaces:**
- Consumes: the complete import flow from Tasks 1-11.
- Produces: documented compatibility, ownership, recovery and operator procedure; no new code interface.

- [ ] **Step 1: Update architecture and user documentation**

Replace the statement that AppSandbox VMs are never migrated. Document discovery source, copy ownership, SSH prerequisites, two-stage conversion, journal recovery, needs-attention semantics, unsupported Windows/templates, capacity requirements, cancellation and cleanup.

- [ ] **Step 2: Run formatting and static verification**

Run: `cargo fmt --all -- --check`
Expected: exit 0.

Run: `cargo check-windows`
Expected: exit 0 with no compile errors.

- [ ] **Step 3: Run the complete Windows test suite**

Run: `cargo test-windows`
Expected: exit 0 with no failed tests.

- [ ] **Step 4: Capture source invariants before the live test**

With AppSandbox and its `ubuntu` VM stopped, record SHA-256, size and last-write time for `vms.cfg`, `ubuntu/disk.vhdx`, `ubuntu/vm.vmgs`, `ubuntu/vm.vmrs`, `ubuntu/vm_state.json`, and `ubuntu/display_settings.json`. Record the source directory tree separately. Store the evidence under a temporary directory outside both products' storage roots.

- [ ] **Step 5: Run the live import with explicit capacity confirmation**

Confirm the configured VMLord destination has at least the source VHDX file size plus 10 GiB free working headroom. Import `ubuntu` under a non-conflicting destination name. Verify copy progress, first SSH-only boot, conversion shutdown, second boot, VMLord SSH key, online `vmlord-agent`, display connection and requested GPU probe.

- [ ] **Step 6: Prove source preservation**

Recompute the Step 4 evidence and compare it byte-for-byte. Expected: hashes, sizes, timestamps and tree match; no AppSandbox file was created, removed or changed.

- [ ] **Step 7: Commit documentation and final verified state**

```bash
git add ARCHITECTURE.md docs/appsandbox-import.md
git commit -m "TASK-21: Document AppSandbox Linux import"
```

- [ ] **Step 8: Review the branch diff and history**

Run: `git status --short --branch`
Expected: clean `task-21-appsandbox-import` branch.

Run: `git log --oneline origin/main..HEAD`
Expected: the design commit followed by the task commits above.

Run: `git diff --check origin/main...HEAD`
Expected: exit 0.

Do not push or open a merge request without explicit user approval.
