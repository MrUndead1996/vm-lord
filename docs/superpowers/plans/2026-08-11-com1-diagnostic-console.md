# COM1 Diagnostic Console Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Attach every native HCS VM's COM1 to a stable named pipe, persist its raw output in a per-VM log, and show the live stream in an automatically closing terminal window.

**Architecture:** The HCS document derives a stable pipe name from the VM UUID. A small `vmlord-com1.exe` helper owns cancellable Win32 pipe reads and mirrors bytes to `com1.log` and stdout; the platform launcher presents it through Windows Terminal, then PowerShell, then cmd. `VmStartPipeline` starts the helper before HCS and returns a live session to `HcsVmRepository`, which owns sessions across starts, reconnects, stops, exits, and deletion.

**Tech Stack:** Rust 2024, `windows` 0.61 Win32 APIs, HCS, `serde_json`, standard-library process and file I/O, existing `log` facade, Cargo workspace tests.

## Global Constraints

- Implement all new application code in Rust.
- Keep Windows APIs and all `unsafe` code inside `vmlord-platform` platform-specific modules.
- Do not add an async runtime or external crate for terminal/pipe management.
- Use `\\.\pipe\vmlord-<vm-uuid>.com1` as the stable COM1 endpoint.
- Write raw guest bytes to `<vm-directory>\com1.log`; do not parse or copy them into `vmlord.log`.
- Explicit VM start truncates `com1.log`; application reconnect appends to it.
- Present the helper through `wt.exe`, falling back to PowerShell and then cmd.
- PowerShell and cmd only host the Rust helper; they do not read or tail the log.
- Close the terminal automatically when COM1 closes, the VM is cancelled, or VMLord exits.
- Treat inability to establish the required console before a new VM start as a start failure.
- A reader failure after HCS has started must not terminate the VM; surface it as a repository diagnostic.
- Keep provisioning secrets out of process arguments, `config.json`, `com1.log`, and operational logs.
- Log platform operations at `DEBUG` through `ERROR`; do not introduce `TRACE` calls.
- Use task-prefixed commits in the form `TASK-62: comment`.

---

## File Structure

- Modify `crates/platform/src/hcs_config.rs`: derive and serialize the COM1 pipe path.
- Modify `crates/platform/src/create.rs`: pass the VM UUID into HCS configuration construction.
- Modify `crates/platform/src/layout.rs`: own the per-VM `com1.log` path.
- Modify `crates/platform/src/event.rs`: support named cross-process events and nonblocking status checks.
- Create `crates/platform/src/com1_reader.rs`: parse helper arguments and perform cancellable named-pipe transport.
- Create `crates/platform/src/com1_terminal.rs`: construct terminal commands, launch with fallback, and own/reap COM1 sessions.
- Modify `crates/platform/src/start.rs`: launch COM1 before HCS and return the active session.
- Modify `crates/platform/src/repository.rs`: retain sessions and integrate reconnect/lifecycle diagnostics.
- Modify `crates/platform/src/lib.rs`: register modules and expose only the helper entry point needed by the companion binary.
- Modify `crates/platform/Cargo.toml`: enable the Win32 named-pipe API feature.
- Modify `crates/vmlord/Cargo.toml`: declare the companion binary target.
- Create `crates/vmlord/src/bin/vmlord-com1.rs`: minimal console entry point delegating to `vmlord-platform`.
- Modify `crates/platform/tests/hyperv.rs`: add the ignored cloud-init COM1 scenario.
- Modify `ARCHITECTURE.md`: document the diagnostic channel and lifecycle.

---

### Task 1: Persist the COM1 contract in the HCS document and VM layout

**Files:**
- Modify: `crates/platform/src/hcs_config.rs:13-103,297-410,412-620`
- Modify: `crates/platform/src/create.rs:168-177`
- Modify: `crates/platform/src/layout.rs:14-70,72-140`

**Interfaces:**
- Produces: `pub(crate) fn com1_pipe_path(vm_id: Uuid) -> String`
- Produces: `pub(crate) fn com1_log_path(vm_directory: &Path) -> PathBuf`
- Changes: `HcsVmConfigBuilder::build(request, system_disk_path, seed_path, vm_id) -> Result<String, RepositoryError>`
- Consumed later by: `com1_terminal.rs`, `start.rs`, and the ignored Hyper-V test.

- [ ] **Step 1: Add failing layout and HCS serialization tests**

In `layout.rs`, extend the path test:

```rust
assert_eq!(
    com1_log_path(&directory),
    PathBuf::from("/vms").join("dev-linux").join("com1.log")
);
```

In `hcs_config.rs`, introduce a fixed UUID and assert both derivation and JSON:

```rust
const VM_ID: Uuid = Uuid::from_u128(0x91cb_8e9a_f2a1_43b3_a682_5724_6fb8_f31d);

#[test]
fn a_vm_exposes_com1_through_its_stable_named_pipe() {
    let document = HcsVmConfigBuilder::build(
        &request(),
        Path::new(r"C:\vms\test-vm\disks\system.vhdx"),
        Path::new(r"C:\vms\test-vm\seed.iso"),
        VM_ID,
    )
    .unwrap();
    let json: Value = serde_json::from_str(&document).unwrap();

    assert_eq!(
        com1_pipe_path(VM_ID),
        r"\\.\pipe\vmlord-91cb8e9af2a143b3a68257246fb8f31d.com1"
    );
    assert_eq!(
        json.pointer("/VirtualMachine/Devices/ComPorts/0/NamedPipe"),
        Some(&json!(com1_pipe_path(VM_ID)))
    );
}
```

Update existing builder calls in the test module with `VM_ID` so the compile failure is limited to the missing production signature and fields.

- [ ] **Step 2: Run the focused tests and verify RED**

Run:

```bash
cargo test -p vmlord-platform hcs_config::tests::a_vm_exposes_com1_through_its_stable_named_pipe
cargo test -p vmlord-platform layout::tests::a_plain_name_becomes_a_directory_under_the_storage_root
```

Expected: FAIL because `com1_pipe_path`, `com1_log_path`, the new `build` argument, and `Devices.ComPorts` do not exist.

- [ ] **Step 3: Add the paths and serialized structures**

In `layout.rs`:

```rust
pub(crate) const COM1_LOG_FILE_NAME: &str = "com1.log";

pub(crate) fn com1_log_path(vm_directory: &Path) -> PathBuf {
    vm_directory.join(COM1_LOG_FILE_NAME)
}
```

In `hcs_config.rs`:

```rust
pub(crate) fn com1_pipe_path(vm_id: Uuid) -> String {
    format!(r"\\.\pipe\vmlord-{}.com1", vm_id.as_simple())
}

#[derive(Serialize)]
struct ComPort {
    #[serde(rename = "NamedPipe")]
    named_pipe: String,
}
```

Add `com_ports: BTreeMap<String, ComPort>` to `Devices` with `#[serde(rename = "ComPorts")]`, and build it as:

```rust
com_ports: BTreeMap::from([(
    "0".to_owned(),
    ComPort {
        named_pipe: com1_pipe_path(vm_id),
    },
)]),
```

Pass the creation UUID from `create.rs`:

```rust
let configuration = HcsVmConfigBuilder::build(
    request,
    &system_disk_path,
    &seed_path,
    vm_id,
)?;
```

Update the exact minimal-document expectation to include `ComPorts`.

- [ ] **Step 4: Run all configuration and creation tests**

Run:

```bash
cargo test -p vmlord-platform hcs_config::tests
cargo test -p vmlord-platform create::tests
cargo test -p vmlord-platform layout::tests
```

Expected: PASS. Confirm `omits_request_secrets` still passes.

- [ ] **Step 5: Commit the HCS contract**

```bash
git add crates/platform/src/hcs_config.rs crates/platform/src/create.rs crates/platform/src/layout.rs
git commit -m "TASK-62: Add COM1 to HCS VM configuration"
```

---

### Task 2: Add cross-process events and the native COM1 reader

**Files:**
- Modify: `crates/platform/Cargo.toml:29-47`
- Modify: `crates/platform/src/event.rs:1-85`
- Create: `crates/platform/src/com1_reader.rs`
- Modify: `crates/platform/src/lib.rs:12-61`

**Interfaces:**
- Produces: `pub enum Com1LogMode { Truncate, Append }`
- Produces: `pub struct Com1HelperOptions` with pipe/log/parent/event fields.
- Produces: `pub fn parse_com1_helper_args(args: impl IntoIterator<Item = OsString>) -> Result<Com1HelperOptions, RepositoryError>`
- Produces: `pub fn run_com1_helper(options: Com1HelperOptions) -> Result<(), RepositoryError>`
- Produces internally: named `WindowsEvent::create_named`, `WindowsEvent::open`, `WindowsEvent::is_signaled`.
- Consumed later by: `vmlord-com1.rs` and `com1_terminal.rs`.

- [ ] **Step 1: Write failing tests for named events, argument parsing, and byte mirroring**

Add event tests using a unique name:

```rust
#[test]
fn a_named_event_can_be_opened_by_another_owner() {
    let name = format!(r"Local\VMLord.Test.{}", Uuid::new_v4());
    let created = WindowsEvent::create_named(&name, true, false).unwrap();
    let opened = WindowsEvent::open(&name).unwrap();

    assert!(!opened.is_signaled().unwrap());
    created.signal().unwrap();
    assert!(opened.is_signaled().unwrap());
}
```

In the new `com1_reader.rs`, put argument parsing and stream duplication behind portable helpers and test them without HCS:

```rust
#[test]
fn parses_every_non_secret_helper_argument() {
    let options = parse_com1_helper_args([
        "--pipe", r"\\.\pipe\vmlord-test.com1",
        "--log", r"C:\vms\dev\com1.log",
        "--mode", "truncate",
        "--parent-pid", "42",
        "--cancel-event", r"Local\VMLord.Com1.cancel.test",
        "--ready-event", r"Local\VMLord.Com1.ready.test",
        "--failed-event", r"Local\VMLord.Com1.failed.test",
        "--finished-event", r"Local\VMLord.Com1.finished.test",
        "--vm-name", "dev",
    ].map(OsString::from)).unwrap();

    assert_eq!(options.log_mode, Com1LogMode::Truncate);
    assert_eq!(options.parent_process_id, 42);
    assert_eq!(options.vm_name, "dev");
}

#[test]
fn mirrors_serial_bytes_without_utf8_conversion() {
    let bytes = b"cloud-init\r\n\xffkernel\0";
    let mut log = Vec::new();
    let mut terminal = Vec::new();

    mirror_chunk(bytes, &mut log, &mut terminal).unwrap();

    assert_eq!(log, bytes);
    assert_eq!(terminal, bytes);
}
```

Also test that a missing argument and an unknown mode produce messages naming the bad field.

- [ ] **Step 2: Run the tests and verify RED**

Run:

```bash
cargo test -p vmlord-platform event::tests::a_named_event_can_be_opened_by_another_owner
cargo test -p vmlord-platform com1_reader::tests
```

Expected: FAIL because the named-event API and module do not exist.

- [ ] **Step 3: Extend the event RAII wrapper**

Add named creation/opening while preserving the existing unnamed constructor:

```rust
pub fn create_named(
    name: &str,
    manual_reset: bool,
    initially_signaled: bool,
) -> Result<Self, RepositoryError>;

pub fn open(name: &str) -> Result<Self, RepositoryError>;

pub fn is_signaled(&self) -> Result<bool, RepositoryError> {
    Ok(matches!(self.wait(Duration::ZERO)?, EventWaitResult::Signaled))
}

pub(crate) fn raw_handle(&self) -> HANDLE {
    self.0
}
```

Use `HSTRING` plus `CreateEventW` and `OpenEventW(EVENT_MODIFY_STATE | SYNCHRONIZE, ...)`. Keep ownership in `WindowsEvent` and close every successful handle in `Drop`.

- [ ] **Step 4: Implement helper options and portable stream mirroring**

Define:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Com1LogMode {
    Truncate,
    Append,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Com1HelperOptions {
    pub pipe_path: PathBuf,
    pub log_path: PathBuf,
    pub log_mode: Com1LogMode,
    pub parent_process_id: u32,
    pub cancel_event_name: String,
    pub ready_event_name: String,
    pub failed_event_name: String,
    pub finished_event_name: String,
    pub vm_name: String,
}
```

Parse exact flag/value pairs, reject duplicates and unknown flags, and reject a zero parent PID. Implement:

```rust
fn mirror_chunk(
    bytes: &[u8],
    log: &mut impl Write,
    terminal: &mut impl Write,
) -> io::Result<()> {
    log.write_all(bytes)?;
    terminal.write_all(bytes)?;
    log.flush()?;
    terminal.flush()
}
```

Open the log with `create(true).write(true).truncate(true)` or `append(true)` according to `Com1LogMode`.

- [ ] **Step 5: Implement cancellable Win32 pipe reading**

Enable `Win32_System_Pipes` in `crates/platform/Cargo.toml`. In `run_com1_helper`:

1. Open the four named events and the parent process with `OpenProcess(SYNCHRONIZE, false, parent_pid)`.
2. Open the log before signaling readiness.
3. Create an overlapped I/O event and signal `ready_event` only after all resources are ready.
4. Retry `CreateFileW(pipe, GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE, ..., OPEN_EXISTING, FILE_FLAG_OVERLAPPED, ...)` while `ERROR_FILE_NOT_FOUND` or `ERROR_PIPE_BUSY`, using `WaitNamedPipeW` in short bounded intervals.
5. Between retries, check the cancel event and parent process handle.
6. Issue `ReadFile` with `OVERLAPPED`; wait on the I/O event, cancel event, and parent process through `WaitForMultipleObjects`.
7. On cancellation or parent exit, call `CancelIoEx`, drain completion, and return `Ok(())`.
8. Treat `ERROR_BROKEN_PIPE` as normal EOF; mirror every nonempty read to log and locked stdout.
9. On an operational error, signal `failed_event`, log at `ERROR`, and return the `RepositoryError`.
10. Use a scope guard so `finished_event` is signaled on every exit path.

Wrap every raw `HANDLE` in a local owned RAII type. Add a `// SAFETY:` explanation at each `unsafe` call; no raw handle may escape this module except the existing crate-private event accessor.

- [ ] **Step 6: Run reader/event tests and diagnostics**

Run:

```bash
cargo fmt --all -- --check
cargo test -p vmlord-platform event::tests
cargo test -p vmlord-platform com1_reader::tests
```

Expected: PASS.

Run project diagnostics for `crates/platform/src/com1_reader.rs`; expected: no errors.

- [ ] **Step 7: Commit the native reader**

```bash
git add crates/platform/Cargo.toml crates/platform/src/event.rs crates/platform/src/com1_reader.rs crates/platform/src/lib.rs
git commit -m "TASK-62: Add cancellable COM1 pipe reader"
```

---

### Task 3: Add the COM1 helper executable

**Files:**
- Modify: `crates/vmlord/Cargo.toml:8-20`
- Create: `crates/vmlord/src/bin/vmlord-com1.rs`

**Interfaces:**
- Consumes: `parse_com1_helper_args` and `run_com1_helper` from Task 2.
- Produces: sibling executable `vmlord-com1.exe` used by the terminal launcher.

- [ ] **Step 1: Add the explicit binary target and minimal main**

Add to `crates/vmlord/Cargo.toml`:

```toml
[[bin]]
name = "vmlord-com1"
path = "src/bin/vmlord-com1.rs"
test = false
bench = false
```

Create the binary:

```rust
#[cfg(not(windows))]
compile_error!("vmlord-com1 currently supports Windows only");

fn main() {
    if let Err(error) = run() {
        eprintln!("VMLord COM1 reader failed: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let settings = vmlord_core::SettingsStore::for_current_user()?.load_or_create()?;
    vmlord_core::initialize_logging(&settings)?;
    let options =
        vmlord_platform::parse_com1_helper_args(std::env::args_os().skip(1))?;
    vmlord_platform::run_com1_helper(options)?;
    Ok(())
}
```

Load settings only to initialize the existing append-only application logger, so helper operations reach `vmlord.log` at the configured level. Do not load the GUI, repository, image stack, or legacy backend. The logger's stdout lines may appear around the guest stream in the terminal, but guest bytes themselves are written directly and never sent through `log`.

- [ ] **Step 2: Build both binaries**

Run:

```bash
cargo build -p vmlord --bins
```

Expected: PASS and both `vmlord.exe` and `vmlord-com1.exe` exist in the same target profile directory.

Run the helper without arguments:

```bash
cargo run -p vmlord --bin vmlord-com1 --
```

Expected: exit code 1 with an error naming the first required argument; no GUI opens.

- [ ] **Step 3: Commit the helper binary**

```bash
git add crates/vmlord/Cargo.toml crates/vmlord/src/bin/vmlord-com1.rs
git commit -m "TASK-62: Add COM1 console helper"
```

---

### Task 4: Launch the helper through terminal fallbacks and own sessions

**Files:**
- Create: `crates/platform/src/com1_terminal.rs`
- Modify: `crates/platform/src/lib.rs:12-61`

**Interfaces:**
- Consumes: `Com1HelperOptions`, `Com1LogMode`, `WindowsEvent`, `hcs_config::com1_pipe_path`, and `layout::com1_log_path`.
- Produces: cloneable `pub(crate) struct Com1Launcher`.
- Produces: `Com1Launcher::production() -> Self`.
- Produces: `Com1Launcher::launch(&self, mapping: &VmComputeSystemMapping, vm_directory: &Path, mode: Com1LogMode) -> Result<Com1Session, RepositoryError>`.
- Produces: `pub(crate) struct Com1Sessions` with `insert`, `cancel`, `reap`, and `cancel_all`.
- Consumed later by: `start.rs` and `repository.rs`.

- [ ] **Step 1: Write failing command/fallback tests**

Represent a host attempt as data before spawning:

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
struct TerminalCommand {
    program: PathBuf,
    args: Vec<OsString>,
    create_new_console: bool,
}
```

Add tests asserting:

```rust
#[test]
fn terminal_commands_prefer_windows_terminal_then_powershell_then_cmd() {
    let commands = terminal_commands(
        Path::new(r"C:\VMLord\vmlord-com1.exe"),
        "dev",
        &helper_args(),
    );

    assert_eq!(commands[0].program, Path::new("wt.exe"));
    assert!(commands[0].args.iter().any(|arg| arg == "VMLord COM1 — dev"));
    assert_eq!(commands[1].program, Path::new("powershell.exe"));
    assert!(commands[1].create_new_console);
    assert!(!commands[1].args.iter().any(|arg| arg == "-NoExit"));
    assert_eq!(commands[2].program, Path::new("cmd.exe"));
    assert!(commands[2].create_new_console);
}
```

Inject `spawn: Fn(&TerminalCommand) -> io::Result<()>` and assert exact fallback behavior:

- wt success makes one attempt;
- wt failure then PowerShell success makes two attempts;
- wt and PowerShell failure then cmd success makes three attempts;
- three failures produce one error containing `wt.exe`, `powershell.exe`, and `cmd.exe`.

- [ ] **Step 2: Run launcher tests and verify RED**

Run:

```bash
cargo test -p vmlord-platform com1_terminal::tests::terminal_commands_prefer_windows_terminal_then_powershell_then_cmd
cargo test -p vmlord-platform com1_terminal::tests::falls_back
```

Expected: FAIL because `com1_terminal` does not exist.

- [ ] **Step 3: Implement command construction and fallback**

Locate the helper as a sibling of `std::env::current_exe()` named `vmlord-com1.exe`; fail before any spawn if it is absent.

Use these host forms:

```text
wt.exe -w 0 new-tab --title "VMLord COM1 — <vm>" -- <helper> <args...>
powershell.exe -NoLogo -NoProfile -Command & '<helper>' '<arg1>' ...
cmd.exe /D /S /C ""<helper>" "<arg1>" ..."
```

Keep PowerShell single-quote escaping and cmd quoting in separate pure functions with tests for spaces, apostrophes, ampersands, and parentheses. Use `std::os::windows::process::CommandExt::creation_flags(CREATE_NEW_CONSOLE)` only for PowerShell/cmd. Log each failed host at `WARN`; return a combined `ERROR` only after all three fail.

- [ ] **Step 4: Write failing readiness and registry tests**

Use fake events/spawn in `Com1Launcher::for_test` to assert:

```rust
#[test]
fn explicit_launch_uses_truncate_and_waits_until_ready() { /* mode + wait assertions */ }

#[test]
fn readiness_timeout_cancels_the_helper_and_fails_launch() { /* cancel signaled */ }

#[test]
fn replacing_a_session_cancels_the_previous_one() { /* same VM UUID */ }

#[test]
fn reap_reports_failed_sessions_and_removes_finished_ones() { /* failed + finished */ }
```

Define the result of reaping explicitly:

```rust
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Com1Failure {
    pub(crate) vm_id: Uuid,
    pub(crate) vm_name: String,
}
```

- [ ] **Step 5: Implement session events, readiness, and registry**

For each session derive unguessable local event names from a fresh UUID:

```text
Local\VMLord.Com1.<session-id>.cancel
Local\VMLord.Com1.<session-id>.ready
Local\VMLord.Com1.<session-id>.failed
Local\VMLord.Com1.<session-id>.finished
```

Create all four manual-reset events in the parent before spawning. Build helper arguments from the mapping, `com1_pipe_path(mapping.vm_id)`, `com1_log_path(vm_directory)`, `std::process::id()`, and the event names. Wait up to 10 seconds for readiness. On timeout or wait failure, signal cancellation and return an error naming the VM.

`Com1Session` owns cancellation/failed/finished events and VM identity. Its `Drop` signals cancellation best-effort. `Com1Sessions::insert` removes and drops an older session for the same UUID before inserting. `reap` checks `finished` without blocking, reports `Com1Failure` when `failed` is signaled, and removes finished entries. `cancel_all` drains the map so every session receives cancellation.

- [ ] **Step 6: Run terminal/session tests**

Run:

```bash
cargo fmt --all -- --check
cargo test -p vmlord-platform com1_terminal::tests
```

Expected: PASS.

- [ ] **Step 7: Commit terminal launching and ownership**

```bash
git add crates/platform/src/com1_terminal.rs crates/platform/src/lib.rs
git commit -m "TASK-62: Launch COM1 helper in a terminal"
```

---

### Task 5: Start COM1 before HCS and cancel it on start failure

**Files:**
- Modify: `crates/platform/src/start.rs:3-20,50-191,525-935`

**Interfaces:**
- Consumes: cloneable `Com1Launcher` and `Com1LogMode::Truncate`.
- Changes: `VmStartPipeline::production(com1: Com1Launcher) -> Self`.
- Changes: `VmStartPipeline::start(...) -> Result<Com1Session, RepositoryError>`.
- Guarantee: any error after launch drops/signals the pending session; a successful start transfers it to the repository.

- [ ] **Step 1: Add failing start-order and cancellation tests**

Extend `Calls.steps` with `"console"` and a cancellation counter. Inject a test `Com1Launcher` into `VmStartPipeline::for_test`.

Add:

```rust
#[test]
fn starts_the_console_before_network_and_hcs() {
    let fixture = fixture_with("console-order", NetworkMode::Nat, None);
    let calls = fixture.calls.clone();

    let _session = pipeline(&calls, Behavior::default())
        .start(&fixture.store, "dev", &fixture.vm_directory)
        .unwrap();

    assert_eq!(
        calls.steps.lock().unwrap().as_slice(),
        ["console", "endpoint", "dhcp", "grant", "grant", "start"]
    );
}

#[test]
fn a_failure_after_console_launch_cancels_the_session() {
    let fixture = fixture_with("console-cancel", NetworkMode::Nat, None);
    let calls = fixture.calls.clone();
    let error = pipeline(
        &calls,
        Behavior { fail_endpoint: true, ..Behavior::default() },
    )
    .start(&fixture.store, "dev", &fixture.vm_directory)
    .unwrap_err();

    assert!(error.to_string().contains("endpoint"));
    assert_eq!(calls.console_cancellations.load(Ordering::Relaxed), 1);
}
```

Keep malformed/missing configuration tests asserting no console launch: validate stored state before opening a terminal that cannot lead to a start.

- [ ] **Step 2: Run focused start tests and verify RED**

Run:

```bash
cargo test -p vmlord-platform start::tests::starts_the_console_before_network_and_hcs
cargo test -p vmlord-platform start::tests::a_failure_after_console_launch_cancels_the_session
```

Expected: FAIL because the start pipeline has no launcher and returns `()`.

- [ ] **Step 3: Integrate the launcher into `VmStartPipeline`**

Add `com1: Com1Launcher` to the pipeline. In `start`:

```rust
let stored = self.read_configuration(&mapping, vm_directory)?;
let session = self
    .com1
    .launch(&mapping, vm_directory, Com1LogMode::Truncate)?;
let (configuration, endpoint) = self.attach_network(/* existing arguments */)?;
// existing access grants, first start, endpoint-busy retry
Ok(session)
```

Do not manually cancel on every `?`: Rust drops the local `session` and its `Drop` signals cancellation. Return the same session after either first-attempt success or successful endpoint replacement. Preserve the original `HcsStartFailure` behavior and retry exactly once for `EndpointBusy`.

- [ ] **Step 4: Update existing start tests for returned sessions and ordering**

Bind successful results as `_session` where needed so the session remains alive through assertions. Update the NAT ordering expectation from:

```rust
["endpoint", "dhcp", "grant", "grant", "start"]
```

to:

```rust
["console", "endpoint", "dhcp", "grant", "grant", "start"]
```

Assert console mode is `Truncate` and the launch request contains the fixed mapping UUID and VM directory.

- [ ] **Step 5: Run the complete start test module**

Run:

```bash
cargo test -p vmlord-platform start::tests
```

Expected: PASS, including endpoint replacement, no-network, DHCP failure, malformed configuration, and access-grant tests.

- [ ] **Step 6: Commit start integration**

```bash
git add crates/platform/src/start.rs
git commit -m "TASK-62: Start COM1 capture before each VM"
```

---

### Task 6: Integrate COM1 sessions with repository reconnect and lifecycle

**Files:**
- Modify: `crates/platform/src/repository.rs:8-33,44-95,145-193,465-497,602-744`

**Interfaces:**
- Consumes: `Com1Launcher`, `Com1LogMode::Append`, `Com1Sessions`, and `Com1Session` returned by start.
- Produces behavior: append-mode reconnect for running VMs; session ownership by VM UUID; cancellation on force-stop/delete/HCS exit/drop; reap diagnostics.

- [ ] **Step 1: Add repository-level tests with a fake COM1 launcher**

Add a test constructor or fixture that injects `Com1Launcher::for_test`. Cover these behaviors with exact assertions:

```rust
#[test]
fn an_explicit_start_retains_a_truncating_console_session() { /* Truncate + registry contains vm */ }

#[test]
fn reconnect_launches_append_only_for_running_vms() { /* Running => Append; Created/Absent => none */ }

#[test]
fn graceful_stop_leaves_console_until_pipe_completion() { /* no cancel */ }

#[test]
fn force_stop_and_delete_cancel_console_sessions() { /* cancel count */ }

#[test]
fn an_hcs_exit_cancels_the_matching_console() { /* drained.released */ }

#[test]
fn a_failed_finished_reader_becomes_a_repository_diagnostic() {
    assert!(repository.take_diagnostics().iter().any(|diagnostic| {
        diagnostic.level == DiagnosticLevel::Error
            && diagnostic.message.contains("COM1")
            && diagnostic.message.contains("dev")
    }));
}
```

If direct `HcsVmRepository` construction is too coupled to HCS, extract the lifecycle decisions into small private functions taking `Com1Sessions` and test those functions without initializing HCS. Do not weaken production visibility merely to test it.

- [ ] **Step 2: Run the new tests and verify RED**

Run each new test by exact name. Expected: FAIL because the repository owns no launcher/session registry and ignores reader completion.

- [ ] **Step 3: Add launcher and session ownership to the repository**

Construct once and clone into the start pipeline:

```rust
let com1_launcher = Com1Launcher::production();
Self {
    start: VmStartPipeline::production(com1_launcher.clone()),
    com1_launcher,
    com1_sessions: Com1Sessions::default(),
    // existing fields
}
```

On explicit start:

```rust
let session = self.start.start(&self.store, name, &vm_directory)?;
let mapping = self.mapping(name)?;
self.com1_sessions.insert(session);
self.hold_started_system(&mapping);
```

Insertion must happen before the temporary session drops.

- [ ] **Step 4: Launch append-mode helpers only for running reconnects**

After `reconnect_known_vms`, call `list_known_vms` and filter exactly:

```rust
known.state == Some(HcsSystemState::Running)
```

For each running mapping, resolve its directory and call:

```rust
com1_launcher.launch(&mapping, &vm_directory, Com1LogMode::Append)
```

Insert successes. Log and push a `Warning` diagnostic for a reconnect launch failure, but continue initializing other VMs: the guest is already running and must not be terminated because its terminal could not be restored.

- [ ] **Step 5: Wire stop, exit, delete, reap, and drop behavior**

Apply the lifecycle rules:

- `stop_vm`: leave the session registered; pipe EOF will finish it.
- `force_stop_vm`: after successful force-stop, `com1_sessions.cancel(vm_id)`.
- `delete_vm`: cancel before removing the VM directory, after `refuse_if_live` succeeds.
- `take_diagnostics`: cancel sessions for `drained.released`, then call `reap`; turn each `Com1Failure` into an `Error` diagnostic.
- `Drop`: call `com1_sessions.cancel_all()` before joining build workers.

Use this user-facing failure form:

```rust
format!(
    "COM1 diagnostics for VM \"{}\" stopped unexpectedly; see {}",
    failure.vm_name,
    layout::com1_log_path(&vm_directory).display()
)
```

Log the detailed operation at `ERROR`, normal EOF/reap at `DEBUG`, reconnect launch at `INFO`, and fallback/failure at `WARN`/`ERROR` as appropriate.

- [ ] **Step 6: Run repository, reconnect, watch, and start tests**

Run:

```bash
cargo test -p vmlord-platform repository::tests
cargo test -p vmlord-platform reconnect::tests
cargo test -p vmlord-platform watch::tests
cargo test -p vmlord-platform start::tests
```

Expected: PASS.

- [ ] **Step 7: Commit lifecycle integration**

```bash
git add crates/platform/src/repository.rs
git commit -m "TASK-62: Manage COM1 sessions with VM lifecycle"
```

---

### Task 7: Prove cloud-init output and document the architecture

**Files:**
- Modify: `crates/platform/tests/hyperv.rs:1-142` and append the new ignored test near cloud-image tests.
- Modify: `ARCHITECTURE.md:194-223` and the cloud-image creation section.

**Interfaces:**
- Consumes: production repository, `layout` behavior through the known `<root>/<vm>/com1.log` contract.
- Produces: ignored end-to-end verification of Ubuntu serial console/cloud-init.

- [ ] **Step 1: Add the ignored Hyper-V test**

Add a bounded helper:

```rust
fn wait_for_cloud_init_log(path: &Path, timeout: Duration) -> Result<String, String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(bytes) = fs::read(path) {
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
```

Add:

```rust
#[test]
#[ignore = "requires an elevated Windows host with Hyper-V/HCS and downloads a cloud image"]
fn ubuntu_cloud_init_is_visible_on_com1() {
    let root = std::env::temp_dir().join(format!(
        "vmlord-com1-cloud-init-{}",
        std::process::id()
    ));
    fs::create_dir_all(&root).unwrap();
    let mut repository = cloud_repository(&root);
    repository.initialize().unwrap();
    let name = "com1-cloud-init";

    repository.create_vm(background_cloud_request(name)).unwrap();
    wait_until_build_finishes(&repository, name, Duration::from_secs(20 * 60)).unwrap();
    repository.start_vm(name).unwrap();

    let result = wait_for_cloud_init_log(
        &root.join(name).join("com1.log"),
        Duration::from_secs(10 * 60),
    );

    let _ = repository.force_stop_vm(name);
    let _ = repository.delete_vm(VmDeleteRequest {
        name: name.into(),
        delete_disks: true,
    });
    drop(repository);
    let _ = fs::remove_dir_all(&root);

    result.expect("Ubuntu cloud-init output should reach COM1");
}
```

Extract/reuse a build-wait helper from the existing background cloud test rather than duplicating its polling loop. Keep cleanup after capture and before the final assertion.

- [ ] **Step 2: Compile the ignored test without running Hyper-V work**

Run:

```bash
cargo test -p vmlord-platform --test hyperv --no-run
```

Expected: PASS. Do not claim the real COM1 scenario passed unless it is explicitly run on an elevated Hyper-V host.

- [ ] **Step 3: Update architecture documentation**

Document:

- `ComPorts.0.NamedPipe` and UUID-derived path;
- `com1.log` beside `config.json`;
- truncate on explicit start and append on process reconnect;
- `vmlord-com1.exe` as a platform helper with no business logic;
- terminal fallback order;
- session ownership and automatic close behavior;
- ignored cloud-init test as factual serial-cmdline verification.

Do not alter the layering diagram unless adding the helper changes an architectural boundary; it remains part of the platform/composition implementation.

- [ ] **Step 4: Run focused and full validation**

Run:

```bash
cargo fmt --all -- --check
cargo test -p vmlord-platform
cargo test -p vmlord --no-run
cargo clippy -p vmlord-platform -p vmlord --all-targets -- -D warnings
cargo build --target=x86_64-pc-windows-gnu
```

Expected: every command exits 0. The platform test suite should report the Hyper-V tests as ignored, not failed.

Then run:

```bash
git diff --check
git status --short
```

Expected: no whitespace errors; only intended Task 62 files are modified. The pre-existing untracked `.worktrees/` directory remains untouched.

- [ ] **Step 5: Commit tests and documentation**

```bash
git add crates/platform/tests/hyperv.rs ARCHITECTURE.md
git commit -m "TASK-62: Verify cloud-init over COM1"
```

---

## Optional Manual Hyper-V Verification

This is required before claiming the ignored end-to-end scenario itself passes, but it may be performed by the project owner if the development environment lacks elevated Hyper-V access.

- [ ] Build both application binaries in the same output directory:

```bash
cargo build -p vmlord --bins
```

- [ ] Run the ignored test elevated:

```bash
cargo test -p vmlord-platform --test hyperv -- --ignored --exact ubuntu_cloud_init_is_visible_on_com1 --nocapture
```

Expected observable behavior:

1. a Windows Terminal tab opens with title `VMLord COM1 — com1-cloud-init`;
2. early Ubuntu/kernel output appears before SSH is available;
3. cloud-init lines appear in the terminal and `<test-root>\com1-cloud-init\com1.log`;
4. the terminal tab closes after cleanup stops the VM;
5. the test passes and removes the disposable VM directory.
