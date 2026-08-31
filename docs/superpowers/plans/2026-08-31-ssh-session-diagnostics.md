# SSH Session Diagnostics Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Report how a finished interactive SSH session ended, telling authentication, a changed host key, a transport failure and other OpenSSH exits apart.

**Architecture:** The terminal stops hosting `ssh.exe` and hosts a new console helper, `vmlord-ssh.exe`, which runs `ssh.exe` as its child with the helper's own console, waits for it, and writes a small JSON report. `ssh.exe` is given `-E <log>` so the text that distinguishes its failures is captured; the helper classifies code plus log tail through a pure function in `vmlord-core`, and announces itself finished through named events, the way `vmlord-com1.exe` does. VMLord reaps the session registry on its refresh tick and turns each report into a diagnostic.

**Tech Stack:** Rust 2024, `vmlord-core` / `vmlord-platform` / `vmlord` crates, `serde` + `serde_json`, Windows named events (`crate::event::WindowsEvent`), `tracing` + `vmlord_core::diagnostic!`.

**Spec:** `docs/superpowers/specs/2026-08-31-ssh-session-diagnostics-design.md`

## Global Constraints

- Rust only. No C code, no FFI, no PowerShell or `cmd.exe` on any path this
  plan touches.
- `unsafe_code = "deny"` workspace-wide; `vmlord-platform` is the only crate
  with `unsafe`, and this plan adds none -- everything Windows-facing goes
  through the existing `WindowsEvent` wrapper.
- Log through `tracing`, never `log`. A record meant for the user goes through
  `vmlord_core::diagnostic!` with a `Subsystem`; SSH uses `Subsystem::Network`.
- The UI holds no business logic and calls no Windows API. Diagnostics do not
  go through `t!`, so no locale catalogues change.
- Commit subjects are `TASK-76: <comment>`.
- Build and test through the aliases: `cargo check-windows`, `cargo
  test-windows`. Never prefix them with `timeout`.
- Tests that need Windows run under `cargo test-windows`; they execute through
  WSL interop.
- Avoid unnecessary abstractions and traits with a single implementation.
- Comments explain *why*, in the voice of the surrounding modules.

---

### Task 1: Classifying how a session ended

**Files:**
- Modify: `crates/core/src/ssh.rs` (append after `SshEndpoint`)
- Modify: `crates/core/src/lib.rs:63` (the `pub use ssh::{…}` line)
- Test: `crates/core/src/ssh.rs`, in its existing `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub enum SshSessionOutcome { Ended { code: i32 }, AuthenticationFailed, HostKeyMismatch, TransportFailure, Unrecognized { code: i32 }, Terminated, NotStarted, WindowClosed }` -- `Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize`, `#[serde(rename_all = "snake_case")]`.
  - `pub fn classify_session(exit_code: Option<i32>, log_tail: &str) -> SshSessionOutcome`
  - `pub struct SshSessionReport { pub outcome: SshSessionOutcome, pub detail: String }` -- `Clone, Debug, PartialEq, Eq, Serialize, Deserialize`.
  - Both re-exported from `vmlord_core`.

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module at the end of `crates/core/src/ssh.rs`:

```rust
    #[test]
    fn a_remote_shell_status_means_the_session_happened() {
        assert_eq!(classify_session(Some(0), ""), SshSessionOutcome::Ended { code: 0 });
        assert_eq!(classify_session(Some(130), ""), SshSessionOutcome::Ended { code: 130 });
    }

    #[test]
    fn a_refused_credential_is_told_from_the_log() {
        assert_eq!(
            classify_session(Some(255), "machi@172.22.42.7: Permission denied (publickey)."),
            SshSessionOutcome::AuthenticationFailed
        );
        assert_eq!(
            classify_session(Some(255), "Received disconnect: Too many authentication failures"),
            SshSessionOutcome::AuthenticationFailed
        );
    }

    #[test]
    fn a_changed_host_key_is_decided_before_a_refused_credential() {
        // OpenSSH says both in one run: it refuses the key it was shown and
        // then reports that nothing could authenticate. The host key is the
        // one a person has to act on.
        let log = "@@@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @@@\n\
                   Host key verification failed.\n\
                   Permission denied (publickey).";
        assert_eq!(classify_session(Some(255), log), SshSessionOutcome::HostKeyMismatch);
    }

    #[test]
    fn a_guest_that_does_not_answer_is_a_transport_failure() {
        assert_eq!(
            classify_session(
                Some(255),
                "ssh: connect to host 172.22.42.7 port 22: Connection refused"
            ),
            SshSessionOutcome::TransportFailure
        );
        assert_eq!(
            classify_session(Some(255), "kex_exchange_identification: Connection closed by remote host"),
            SshSessionOutcome::TransportFailure
        );
    }

    #[test]
    fn an_unrecognised_failure_keeps_its_code_rather_than_being_guessed_at() {
        assert_eq!(
            classify_session(Some(255), "something OpenSSH has never said before"),
            SshSessionOutcome::Unrecognized { code: 255 }
        );
        assert_eq!(
            classify_session(Some(255), ""),
            SshSessionOutcome::Unrecognized { code: 255 }
        );
    }

    #[test]
    fn a_client_that_died_without_a_code_is_not_an_exit() {
        assert_eq!(classify_session(None, ""), SshSessionOutcome::Terminated);
    }

    #[test]
    fn a_report_survives_the_file_it_travels_in() {
        let report = SshSessionReport {
            outcome: SshSessionOutcome::HostKeyMismatch,
            detail: "Host key verification failed.".to_owned(),
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"host_key_mismatch\""), "{json}");
        assert_eq!(serde_json::from_str::<SshSessionReport>(&json).unwrap(), report);
    }
```

Add `classify_session, SshSessionOutcome, SshSessionReport` to the `use super::{…}` line of that test module.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-core ssh`
Expected: FAIL — `cannot find function classify_session in this scope`.

- [ ] **Step 3: Write the implementation**

Append to `crates/core/src/ssh.rs`:

```rust
/// How an interactive SSH session ended.
///
/// OpenSSH answers nearly every failure of its own with exit code 255, so the
/// code alone cannot tell a refused key from a changed host key: the text it
/// wrote is what separates them, and this is what that text is turned into
/// before it reaches a person. Anything else the client exited with is the
/// remote shell's own status -- the session happened, and what it ran decided
/// the number.
///
/// Serializable because it crosses a process boundary: the helper that hosted
/// the session classifies it and VMLord reads the answer, so the variant names
/// are a file format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SshSessionOutcome {
    /// The client ran and exited with the status of what it ran.
    Ended { code: i32 },
    /// The guest refused the credential VMLord offered, or the one that was
    /// typed.
    AuthenticationFailed,
    /// The guest's host key is not the one VMLord learned for this VM. Nothing
    /// resets it automatically: a key that changed is a decision for a person.
    HostKeyMismatch,
    /// The client never got as far as authenticating.
    TransportFailure,
    /// OpenSSH failed in a way this does not recognise. The code and the log
    /// tail travel with it rather than being guessed at.
    Unrecognized { code: i32 },
    /// The client died without an exit code.
    Terminated,
    /// The helper could not run the client at all.
    NotStarted,
    /// The session's window was closed, taking the helper with it before it
    /// could report anything. How people end shells, not a failure.
    WindowClosed,
}

/// OpenSSH's own exit code, meaning the session never ran.
const SSH_CLIENT_FAILURE: i32 = 255;

/// What a changed host key looks like in an OpenSSH log.
const HOST_KEY_MARKERS: [&str; 3] = [
    "REMOTE HOST IDENTIFICATION HAS CHANGED",
    "Host key verification failed",
    "differs from the key for the IP address",
];

/// What a refused credential looks like in an OpenSSH log.
const AUTHENTICATION_MARKERS: [&str; 3] = [
    "Permission denied",
    "Too many authentication failures",
    "No supported authentication methods",
];

/// What a connection that never reached authentication looks like.
const TRANSPORT_MARKERS: [&str; 8] = [
    "connect to host",
    "Connection refused",
    "Connection timed out",
    "Connection reset",
    "Could not resolve",
    "kex_exchange_identification",
    "Network is unreachable",
    "No route to host",
];

/// Turns what `ssh.exe` left behind into what is known about the session.
///
/// The order of the questions is the order of what has to be acted on: a
/// changed host key and a refused credential appear together in one log --
/// OpenSSH refuses the key it was shown and then reports that nothing
/// authenticated -- and the host key is the one that means something other
/// than "try again".
#[must_use]
pub fn classify_session(exit_code: Option<i32>, log_tail: &str) -> SshSessionOutcome {
    let Some(code) = exit_code else {
        return SshSessionOutcome::Terminated;
    };
    if code != SSH_CLIENT_FAILURE {
        return SshSessionOutcome::Ended { code };
    }
    let says = |markers: &[&str]| markers.iter().any(|marker| log_tail.contains(marker));

    if says(&HOST_KEY_MARKERS) {
        SshSessionOutcome::HostKeyMismatch
    } else if says(&AUTHENTICATION_MARKERS) {
        SshSessionOutcome::AuthenticationFailed
    } else if says(&TRANSPORT_MARKERS) {
        SshSessionOutcome::TransportFailure
    } else {
        SshSessionOutcome::Unrecognized { code }
    }
}

/// What the helper that hosted a session leaves behind for VMLord to read.
///
/// Small on purpose: the outcome, and the tail of what OpenSSH wrote. The full
/// log is deleted by the helper that wrote it -- a session's diagnostics are
/// meant for the diagnostics panel, not for a directory that grows a file per
/// shell.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshSessionReport {
    pub outcome: SshSessionOutcome,
    /// The tail of the session log, or empty when OpenSSH said nothing.
    pub detail: String,
}
```

Then extend `crates/core/src/lib.rs:63`:

```rust
pub use ssh::{
    SshAuthentication, SshAvailability, SshConfig, SshEndpoint, SshPort, SshSessionOutcome,
    SshSessionReport, classify_session,
};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-core ssh`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/ssh.rs crates/core/src/lib.rs
git commit -m "TASK-76: Classify how an SSH session ended"
```

---

### Task 2: Where a session's files live, and when they go

**Files:**
- Modify: `crates/platform/src/layout.rs` (after `ssh_public_key_path`)
- Modify: `crates/platform/src/delete.rs:157-190` (`remove_files`) and its docs above
- Test: `crates/platform/src/layout.rs` tests, `crates/platform/src/delete.rs` tests

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub(crate) fn ssh_sessions_directory(vm_directory: &Path) -> PathBuf` — `<vm>\ssh-sessions`
  - `pub(crate) fn ssh_session_log_path(vm_directory: &Path, session_id: Uuid) -> PathBuf` — `<vm>\ssh-sessions\<simple uuid>.log`
  - `pub(crate) fn ssh_session_report_path(vm_directory: &Path, session_id: Uuid) -> PathBuf` — `<vm>\ssh-sessions\<simple uuid>.json`

- [ ] **Step 1: Write the failing tests**

In `crates/platform/src/layout.rs` tests:

```rust
    #[test]
    fn a_session_keeps_its_log_and_its_report_under_one_directory() {
        let vm = Path::new(r"C:\VMs\dev-linux");
        let id = Uuid::from_u128(0x1234_5678_9abc_def0_1234_5678_9abc_def0);
        let directory = ssh_sessions_directory(vm);

        assert_eq!(directory, Path::new(r"C:\VMs\dev-linux\ssh-sessions"));
        assert_eq!(
            ssh_session_log_path(vm, id),
            directory.join("123456789abcdef0123456789abcdef0.log")
        );
        assert_eq!(
            ssh_session_report_path(vm, id),
            directory.join("123456789abcdef0123456789abcdef0.json")
        );
    }
```

In `crates/platform/src/delete.rs` tests, extend the fixture used by the
`delete_disks = false` test (the one asserting `keys/` and `known_hosts` are
gone — around `crates/platform/src/delete.rs:575`) by creating a session
directory in the fixture builder beside the keys directory:

```rust
        fs::create_dir_all(crate::layout::ssh_sessions_directory(&vm_directory))
            .expect("the session directory can be created");
```

and assert in both deletion tests:

```rust
        assert!(
            !crate::layout::ssh_sessions_directory(&fixture.vm_directory).exists(),
            "the session reports of a VM nobody can reach must not outlive it"
        );
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-platform layout::tests`
Run: `cargo test-windows -p vmlord-platform delete::tests`
Expected: FAIL — `cannot find function ssh_sessions_directory`.

- [ ] **Step 3: Write the implementation**

In `crates/platform/src/layout.rs` (it already imports `Path`/`PathBuf`; add
`use uuid::Uuid;` if the module does not import it yet):

```rust
/// Returns the directory holding what interactive SSH sessions leave behind.
///
/// A directory of its own, and one file per session rather than one per VM:
/// two shells into one guest is an ordinary thing to want, and the second one
/// must not overwrite what the first is still writing. Nothing here outlives
/// being read -- the helper deletes its log, VMLord deletes the report it
/// reported -- so the directory is normally empty.
pub(crate) fn ssh_sessions_directory(vm_directory: &Path) -> PathBuf {
    vm_directory.join("ssh-sessions")
}

/// Returns the path of one session's OpenSSH log.
pub(crate) fn ssh_session_log_path(vm_directory: &Path, session_id: Uuid) -> PathBuf {
    ssh_sessions_directory(vm_directory).join(format!("{}.log", session_id.as_simple()))
}

/// Returns the path of one session's report, which is how the helper that
/// hosted it tells VMLord how it ended.
pub(crate) fn ssh_session_report_path(vm_directory: &Path, session_id: Uuid) -> PathBuf {
    ssh_sessions_directory(vm_directory).join(format!("{}.json", session_id.as_simple()))
}
```

In `crates/platform/src/delete.rs`, inside `remove_files`, after the
`known_hosts` removal:

```rust
    if let Err(error) = remove_directory_if_present(
        &layout::ssh_sessions_directory(vm_directory),
        "what its SSH sessions left behind",
    ) {
        failures.push(error.to_string());
    }
```

Extend the doc comment above `remove_files`: the session directory goes with
the identity rather than with the record of what the VM did — it holds reports
about logging in, which nothing can do any more.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-platform layout::tests`
Run: `cargo test-windows -p vmlord-platform delete::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/platform/src/layout.rs crates/platform/src/delete.rs
git commit -m "TASK-76: Give SSH sessions a directory that goes with the VM"
```

---

### Task 3: The helper that hosts the session

**Files:**
- Create: `crates/platform/src/ssh_session.rs`
- Create: `crates/vmlord/src/bin/vmlord-ssh.rs`
- Modify: `crates/platform/src/lib.rs` (`mod ssh_session;` beside `mod ssh_port;`, and a `pub use`)
- Modify: `crates/vmlord/Cargo.toml` (a second `[[bin]]`)
- Test: `crates/platform/src/ssh_session.rs`, in its own `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `vmlord_core::{SshSessionOutcome, SshSessionReport, classify_session}` (Task 1); `layout::ssh_session_report_path` is *not* used here — the helper is told its paths.
- Produces:
  - `pub struct SshHelperOptions { pub report_path: PathBuf, pub log_path: PathBuf, pub finished_event_name: String, pub alive_event_name: String, pub vm_name: String, pub client: PathBuf, pub client_args: Vec<OsString> }`
  - `pub fn parse_ssh_helper_args(args: impl IntoIterator<Item = OsString>) -> Result<SshHelperOptions, RepositoryError>`
  - `pub fn run_ssh_helper(options: SshHelperOptions) -> Result<(), RepositoryError>`
  - `pub(crate) fn write_report(path: &Path, report: &SshSessionReport) -> Result<(), RepositoryError>`
  - `pub(crate) fn read_report(path: &Path) -> Option<SshSessionReport>` (used by Task 4)
  - `pub(crate) const SESSION_TAIL_LINES: usize = 40;`

- [ ] **Step 1: Write the failing tests**

Create `crates/platform/src/ssh_session.rs` with only its `tests` module for
now:

```rust
#[cfg(test)]
mod tests {
    use std::{ffi::OsString, fs};

    use vmlord_core::{SshSessionOutcome, SshSessionReport};

    use super::{finish, parse_ssh_helper_args, read_report, write_report};

    fn arguments() -> Vec<OsString> {
        [
            "--report",
            r"C:\VMs\dev\ssh-sessions\a.json",
            "--log",
            r"C:\VMs\dev\ssh-sessions\a.log",
            "--finished-event",
            r"Local\VMLord.Ssh.a.finished",
            "--alive-event",
            r"Local\VMLord.Ssh.a.alive",
            "--vm-name",
            "dev-linux",
            "--",
            r"C:\Windows\System32\OpenSSH\ssh.exe",
            "-p",
            "22",
            "-l",
            "machi",
            "172.22.42.7",
        ]
        .into_iter()
        .map(OsString::from)
        .collect()
    }

    #[test]
    fn the_client_and_its_arguments_are_everything_after_the_separator() {
        let options = parse_ssh_helper_args(arguments()).unwrap();

        assert_eq!(options.vm_name, "dev-linux");
        assert_eq!(
            options.client,
            std::path::Path::new(r"C:\Windows\System32\OpenSSH\ssh.exe")
        );
        assert_eq!(
            options.client_args,
            ["-p", "22", "-l", "machi", "172.22.42.7"].map(OsString::from)
        );
    }

    #[test]
    fn a_missing_client_is_refused_rather_than_run_as_nothing() {
        let mut arguments = arguments();
        arguments.truncate(arguments.len() - 6);
        let message = parse_ssh_helper_args(arguments).unwrap_err().to_string();
        assert!(message.contains("client"), "{message}");
    }

    #[test]
    fn an_unknown_or_repeated_flag_is_a_launcher_bug_not_a_guess() {
        let mut unknown = arguments();
        unknown.insert(0, OsString::from("--colour"));
        assert!(parse_ssh_helper_args(unknown).is_err());

        let mut repeated = arguments();
        repeated.insert(0, OsString::from("dev"));
        repeated.insert(0, OsString::from("--vm-name"));
        assert!(parse_ssh_helper_args(repeated).is_err());
    }

    /// What the helper does once the client is over: classify, report, and
    /// take the log with it.
    #[test]
    fn a_finished_client_leaves_a_report_and_no_log() {
        let directory = tempfile::tempdir().unwrap();
        let log = directory.path().join("session.log");
        let report_path = directory.path().join("session.json");
        fs::write(&log, "machi@172.22.42.7: Permission denied (publickey).").unwrap();

        finish(Some(255), &log, &report_path, "dev-linux");

        let report = read_report(&report_path).expect("the report is readable");
        assert_eq!(report.outcome, SshSessionOutcome::AuthenticationFailed);
        assert!(report.detail.contains("Permission denied"), "{}", report.detail);
        assert!(!log.exists(), "the helper owns the log and takes it with it");
    }

    #[test]
    fn a_session_that_said_nothing_still_reports_how_it_ended() {
        let directory = tempfile::tempdir().unwrap();
        let log = directory.path().join("session.log");
        let report_path = directory.path().join("session.json");

        finish(Some(0), &log, &report_path, "dev-linux");

        let report = read_report(&report_path).expect("the report is readable");
        assert_eq!(report.outcome, SshSessionOutcome::Ended { code: 0 });
        assert!(report.detail.is_empty(), "{}", report.detail);
    }

    #[test]
    fn a_report_that_is_not_there_is_not_an_error() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(read_report(&directory.path().join("absent.json")), None);
    }

    #[test]
    fn a_report_that_cannot_be_parsed_is_reported_as_absent() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("torn.json");
        fs::write(&path, "{ half a docum").unwrap();
        assert_eq!(read_report(&path), None);
    }

    #[test]
    fn a_client_that_could_not_be_started_says_so() {
        let directory = tempfile::tempdir().unwrap();
        let report_path = directory.path().join("session.json");

        write_report(
            &report_path,
            &SshSessionReport {
                outcome: SshSessionOutcome::NotStarted,
                detail: "the system cannot find the file specified".to_owned(),
            },
        )
        .unwrap();

        let report = read_report(&report_path).unwrap();
        assert_eq!(report.outcome, SshSessionOutcome::NotStarted);
    }
}
```

`tempfile` is already a dev-dependency of `vmlord-platform`; confirm with
`grep -n tempfile crates/platform/Cargo.toml` and add it under
`[dev-dependencies]` if it is missing.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-platform ssh_session`
Expected: FAIL — the module is not declared and the functions do not exist.

- [ ] **Step 3: Write the implementation**

Write the module above its tests, and declare it in
`crates/platform/src/lib.rs` (`mod ssh_session;` in alphabetical order, and
`pub use ssh_session::{SshHelperOptions, parse_ssh_helper_args, run_ssh_helper};`
beside the COM1 re-export at line 72):

```rust
//! The process a terminal hosts instead of `ssh.exe`, and the report it leaves.
//!
//! A session in a window of its own is a session VMLord cannot see the end of:
//! the terminal owns what it hosts, and OpenSSH answers nearly every failure of
//! its own with the same exit code. So the terminal hosts this instead. It runs
//! the client with its own console -- the session is interactive exactly as it
//! was, and every prompt still appears in the window -- waits for it, and turns
//! the exit code plus the log OpenSSH was told to write into one small report
//! for VMLord to read.
//!
//! The helper is the client's parent and nothing more. It does not read the
//! session, does not touch the guest, and holds nothing of the user's: what it
//! knows is a path, an exit code, and the last lines of a log.

use std::{
    ffi::{OsStr, OsString},
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};

use vmlord_core::{RepositoryError, SshSessionOutcome, SshSessionReport, classify_session};

use crate::event::WindowsEvent;

/// How much of the session log travels in the report.
///
/// The same cap as the readiness transcript's, for the same reason: what
/// reaches the diagnostics panel has to be readable in it.
pub(crate) const SESSION_TAIL_LINES: usize = 40;

/// Everything the helper is told on its command line.
///
/// Nothing secret is here, and nothing secret may be added: a command line is
/// readable by every process on the machine that can enumerate this one.
#[derive(Debug, PartialEq, Eq)]
pub struct SshHelperOptions {
    pub report_path: PathBuf,
    pub log_path: PathBuf,
    pub finished_event_name: String,
    pub alive_event_name: String,
    pub vm_name: String,
    pub client: PathBuf,
    pub client_args: Vec<OsString>,
}

/// The flags [`parse_ssh_helper_args`] accepts, in the order the launcher
/// writes them. Everything after `--` is the client and its own arguments.
const FLAGS: [&str; 5] = [
    "--report",
    "--log",
    "--finished-event",
    "--alive-event",
    "--vm-name",
];

/// Parses the helper's command line.
///
/// Exact flag/value pairs, then `--`, then the client: an unknown or repeated
/// flag is a launcher bug, and guessing at one would let a typo turn into a
/// session nobody can hear the end of.
pub fn parse_ssh_helper_args(
    args: impl IntoIterator<Item = OsString>,
) -> Result<SshHelperOptions, RepositoryError> {
    let mut values: [Option<OsString>; FLAGS.len()] = Default::default();
    let mut args = args.into_iter();
    let mut client_line: Vec<OsString> = Vec::new();

    while let Some(flag) = args.next() {
        if flag == OsStr::new("--") {
            client_line.extend(args.by_ref());
            break;
        }
        let index = FLAGS
            .iter()
            .position(|known| OsStr::new(known) == flag)
            .ok_or_else(|| argument_error(format!("unknown argument {}", flag.display())))?;
        if values[index].is_some() {
            return Err(argument_error(format!("{} was given twice", FLAGS[index])));
        }
        let value = args
            .next()
            .ok_or_else(|| argument_error(format!("{} has no value", FLAGS[index])))?;
        values[index] = Some(value);
    }

    let mut values = values
        .into_iter()
        .zip(FLAGS)
        .map(|(value, flag)| value.ok_or_else(|| argument_error(format!("{flag} is required"))));
    let mut next = || values.next().expect("one value per flag");

    let report_path = PathBuf::from(next()?);
    let log_path = PathBuf::from(next()?);
    let mut name = || -> Result<String, RepositoryError> { Ok(next()?.to_string_lossy().into()) };
    let finished_event_name = name()?;
    let alive_event_name = name()?;
    let vm_name = name()?;

    let mut client_line = client_line.into_iter();
    let client = client_line
        .next()
        .ok_or_else(|| argument_error("the SSH client must follow \"--\"".to_owned()))?;

    Ok(SshHelperOptions {
        report_path,
        log_path,
        finished_event_name,
        alive_event_name,
        vm_name,
        client: PathBuf::from(client),
        client_args: client_line.collect(),
    })
}

fn argument_error(detail: String) -> RepositoryError {
    RepositoryError::new(format!("VMLord SSH helper arguments are invalid: {detail}"))
}

/// Runs the session and reports how it ended.
///
/// The client inherits this process's console: the window a person types in is
/// this window, and the helper is between them only in the sense that it is the
/// parent waiting for the child.
pub fn run_ssh_helper(options: SshHelperOptions) -> Result<(), RepositoryError> {
    let finished = WindowsEvent::open(&options.finished_event_name)?;
    // Created here and held for as long as this process lives: a named object
    // exists while a handle to it does, so VMLord probing this name is asking
    // whether this helper is still there. It is the only question a report
    // cannot answer -- a window someone closes takes the helper with it.
    let _alive = WindowsEvent::create_named(&options.alive_event_name, true, false)?;
    // However this leaves -- returning, failing, or a panic unwinding through
    // it -- VMLord learns that the session is over.
    let _finish = SignalOnDrop(&finished);

    match Command::new(&options.client).args(&options.client_args).status() {
        Ok(status) => {
            finish(
                status.code(),
                &options.log_path,
                &options.report_path,
                &options.vm_name,
            );
            Ok(())
        }
        Err(error) => {
            let detail = error.to_string();
            tracing::error!(
                "the SSH client for VM \"{}\" could not be started: {detail}",
                options.vm_name
            );
            report(
                &options.report_path,
                &options.vm_name,
                &SshSessionReport {
                    outcome: SshSessionOutcome::NotStarted,
                    detail,
                },
            );
            Err(RepositoryError::new(format!(
                "{} could not be started",
                options.client.display()
            )))
        }
    }
}

/// Classifies a client that has exited, reports it, and takes the log with it.
///
/// The log is this process's to delete: it was written for this one question,
/// and a directory that grew a file per shell would be a slow leak nobody
/// asked for.
fn finish(exit_code: Option<i32>, log_path: &Path, report_path: &Path, vm_name: &str) {
    let detail = log_tail(log_path);
    let outcome = classify_session(exit_code, &detail);
    report(report_path, vm_name, &SshSessionReport { outcome, detail });

    if log_path.exists()
        && let Err(error) = fs::remove_file(log_path)
    {
        tracing::warn!(
            "the SSH session log {} of VM \"{vm_name}\" could not be removed: {error}",
            log_path.display()
        );
    }
}

fn report(path: &Path, vm_name: &str, report: &SshSessionReport) {
    if let Err(error) = write_report(path, report) {
        tracing::error!("the SSH session of VM \"{vm_name}\" could not be reported: {error}");
    }
}

/// The last lines of what OpenSSH wrote, or nothing when it wrote nothing.
fn log_tail(path: &Path) -> String {
    let mut text = String::new();
    let Ok(mut file) = File::open(path) else {
        return String::new();
    };
    if file.read_to_string(&mut text).is_err() {
        return String::new();
    }
    let kept: Vec<&str> = text.trim_end().lines().collect();
    let start = kept.len().saturating_sub(SESSION_TAIL_LINES);
    kept[start..].join("\n").trim().to_owned()
}

pub(crate) fn write_report(path: &Path, report: &SshSessionReport) -> Result<(), RepositoryError> {
    let document = serde_json::to_vec(report).map_err(|error| {
        RepositoryError::new(format!("the SSH session report cannot be written: {error}"))
    })?;
    if let Some(directory) = path.parent() {
        fs::create_dir_all(directory).map_err(|error| {
            RepositoryError::new(format!(
                "the SSH session directory {} cannot be created: {error}",
                directory.display()
            ))
        })?;
    }
    fs::write(path, document).map_err(|error| {
        RepositoryError::new(format!(
            "the SSH session report {} cannot be written: {error}",
            path.display()
        ))
    })
}

/// Reads a report, treating one that is absent or torn as none.
///
/// A helper that was killed mid-write leaves half a document, and half a
/// document is exactly as informative as no document: both mean the window is
/// gone and nothing was said.
pub(crate) fn read_report(path: &Path) -> Option<SshSessionReport> {
    let document = fs::read(path).ok()?;
    match serde_json::from_slice(&document) {
        Ok(report) => Some(report),
        Err(error) => {
            tracing::warn!(
                "the SSH session report {} could not be read: {error}",
                path.display()
            );
            None
        }
    }
}

/// Signals an event however the scope it guards is left.
struct SignalOnDrop<'a>(&'a WindowsEvent);

impl Drop for SignalOnDrop<'_> {
    fn drop(&mut self) {
        let _ = self.0.signal();
    }
}
```

Create `crates/vmlord/src/bin/vmlord-ssh.rs`, in the shape of `vmlord-com1.rs`:

```rust
//! The process a terminal hosts for an interactive SSH session.
//!
//! Nothing decides anything here: the process exists so that a terminal window
//! has something to host, and everything it does lives in `vmlord-platform`.

#[cfg(not(windows))]
compile_error!("vmlord-ssh currently supports Windows only");

fn main() {
    if let Err(error) = run() {
        eprintln!("VMLord SSH session failed: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    // Settings are loaded for the application log alone: what the session says
    // belongs to the person in this window, and what the helper did belongs in
    // `vmlord.log` beside everything else VMLord did.
    let settings = vmlord_core::SettingsStore::for_current_user()?.load_or_create()?;
    vmlord_core::initialize_logging(&settings)?;
    let options = vmlord_platform::parse_ssh_helper_args(std::env::args_os().skip(1))?;
    vmlord_platform::run_ssh_helper(options)?;
    Ok(())
}
```

And in `crates/vmlord/Cargo.toml`, beside the `vmlord-com1` entry:

```toml
[[bin]]
name = "vmlord-ssh"
path = "src/bin/vmlord-ssh.rs"
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-platform ssh_session`
Run: `cargo check-windows`
Expected: PASS, and the workspace compiles with the new binary.

- [ ] **Step 5: Commit**

```bash
git add crates/platform/src/ssh_session.rs crates/platform/src/lib.rs \
        crates/vmlord/src/bin/vmlord-ssh.rs crates/vmlord/Cargo.toml
git commit -m "TASK-76: Host SSH sessions in a helper that reports how they end"
```

---

### Task 4: The sessions VMLord is waiting on

**Files:**
- Create: `crates/platform/src/ssh_sessions.rs`
- Modify: `crates/platform/src/lib.rs` (`mod ssh_sessions;`)
- Test: `crates/platform/src/ssh_sessions.rs`, in its own `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `crate::event::WindowsEvent`, `crate::ssh_session::read_report` (Task 3), `crate::layout::{ssh_sessions_directory, ssh_session_report_path}` (Task 2), `vmlord_core::{SshSessionOutcome, SshSessionReport}` (Task 1).
- Produces:
  - `pub(crate) struct SshSessionHandle { pub(crate) id: Uuid, pub(crate) vm_name: String, pub(crate) report_path: PathBuf, pub(crate) finished: WindowsEvent, pub(crate) alive_name: String }`
  - `#[derive(Default)] pub(crate) struct SshSessions` with `insert(&self, session: SshSessionHandle)`, `forget(&self, id: Uuid)`, `reap(&self) -> Vec<SshSessionEnd>`, `sweep(&self, vm_directory: &Path)`
  - `pub(crate) struct SshSessionEnd { pub(crate) vm_name: String, pub(crate) report: SshSessionReport }`

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use uuid::Uuid;
    use vmlord_core::{SshSessionOutcome, SshSessionReport};

    use super::{SshSessionHandle, SshSessions};
    use crate::{event::WindowsEvent, ssh_session::write_report};

    struct Fixture {
        directory: tempfile::TempDir,
        sessions: SshSessions,
        id: Uuid,
        report_path: PathBuf,
        finished_name: String,
        alive: Option<WindowsEvent>,
    }

    fn fixture() -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4();
        let report_path = directory.path().join(format!("{}.json", id.as_simple()));
        let finished_name = format!(r"Local\VMLord.Test.Ssh.{}.finished", id.as_simple());
        let alive_name = format!(r"Local\VMLord.Test.Ssh.{}.alive", id.as_simple());
        let finished = WindowsEvent::create_named(&finished_name, true, false).unwrap();
        // Held the way a running helper holds it: while this handle exists, the
        // session counts as running.
        let alive = WindowsEvent::create_named(&alive_name, true, false).unwrap();

        let sessions = SshSessions::default();
        sessions.insert(SshSessionHandle {
            id,
            vm_name: "dev-linux".to_owned(),
            report_path: report_path.clone(),
            finished,
            alive_name,
        });

        Fixture {
            directory,
            sessions,
            id,
            report_path,
            finished_name,
            alive: Some(alive),
        }
    }

    #[test]
    fn a_session_still_running_is_not_reaped() {
        let fixture = fixture();
        assert!(fixture.sessions.reap().is_empty());
    }

    #[test]
    fn a_finished_session_is_reported_from_its_report_and_the_file_is_taken() {
        let fixture = fixture();
        write_report(
            &fixture.report_path,
            &SshSessionReport {
                outcome: SshSessionOutcome::HostKeyMismatch,
                detail: "Host key verification failed.".to_owned(),
            },
        )
        .unwrap();
        WindowsEvent::open(&fixture.finished_name).unwrap().signal().unwrap();

        let ended = fixture.sessions.reap();

        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].vm_name, "dev-linux");
        assert_eq!(ended[0].report.outcome, SshSessionOutcome::HostKeyMismatch);
        assert!(!fixture.report_path.exists(), "a report read once is a report gone");
        assert!(fixture.sessions.reap().is_empty(), "a session is reaped once");
    }

    #[test]
    fn a_helper_that_is_gone_without_a_report_closed_its_window() {
        let mut fixture = fixture();
        // The helper's process ending is exactly this: the last handle to the
        // name goes, and the name goes with it.
        fixture.alive.take();

        let ended = fixture.sessions.reap();

        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].report.outcome, SshSessionOutcome::WindowClosed);
    }

    #[test]
    fn a_forgotten_session_is_not_waited_for() {
        let fixture = fixture();
        fixture.sessions.forget(fixture.id);
        assert!(fixture.sessions.reap().is_empty());
    }

    #[test]
    fn a_sweep_removes_what_no_session_is_waiting_for() {
        let fixture = fixture();
        let stale = fixture
            .directory
            .path()
            .join("ssh-sessions")
            .join("deadbeefdeadbeefdeadbeefdeadbeef.json");
        fs::create_dir_all(stale.parent().unwrap()).unwrap();
        fs::write(&stale, "{}").unwrap();

        fixture.sessions.sweep(fixture.directory.path());

        assert!(!stale.exists(), "a report from a VMLord that is gone is nobody's");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-platform ssh_sessions`
Expected: FAIL — the module does not exist.

- [ ] **Step 3: Write the implementation**

```rust
//! The interactive SSH sessions VMLord is still waiting to hear the end of.
//!
//! A session runs in a window of its own and is nobody's child here: what
//! VMLord holds is not a process handle but a name to probe and an event to
//! wait on, the way it holds a COM1 reader. A session is over when its helper
//! signals that it has finished, or when the name only that helper holds is
//! gone -- which is what closing the window looks like from outside.
//!
//! Not keyed by VM: two shells into one guest is an ordinary thing to want.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};

use uuid::Uuid;
use vmlord_core::{SshSessionOutcome, SshSessionReport};

use crate::{event::WindowsEvent, layout, ssh_session::read_report};

/// One session being waited for.
pub(crate) struct SshSessionHandle {
    pub(crate) id: Uuid,
    pub(crate) vm_name: String,
    pub(crate) report_path: PathBuf,
    /// Signalled by the helper however it leaves.
    pub(crate) finished: WindowsEvent,
    /// The name the helper created and holds, so a helper that was killed can
    /// be told from one that is still hosting a shell.
    pub(crate) alive_name: String,
}

/// A session that is over, and what it says.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SshSessionEnd {
    pub(crate) vm_name: String,
    pub(crate) report: SshSessionReport,
}

/// The sessions in flight.
///
/// Interior mutability because the two sides run on different threads: a launch
/// worker inserts, and the UI's refresh tick reaps.
#[derive(Default)]
pub(crate) struct SshSessions(Mutex<Vec<SshSessionHandle>>);

impl SshSessions {
    pub(crate) fn insert(&self, session: SshSessionHandle) {
        self.lock().push(session);
    }

    /// Drops a session VMLord never managed to start a terminal for.
    pub(crate) fn forget(&self, id: Uuid) {
        self.lock().retain(|session| session.id != id);
    }

    /// Reports every session that is over, and forgets it.
    pub(crate) fn reap(&self) -> Vec<SshSessionEnd> {
        let mut sessions = self.lock();
        let mut ended = Vec::new();
        let mut index = 0;
        while index < sessions.len() {
            if !has_ended(&sessions[index]) {
                index += 1;
                continue;
            }
            let session = sessions.remove(index);
            ended.push(SshSessionEnd {
                vm_name: session.vm_name,
                report: take_report(&session.report_path),
            });
        }
        ended
    }

    /// Removes what earlier runs left in a VM's session directory.
    ///
    /// Called before a session is opened. The reports this VMLord is still
    /// waiting for are exactly the ones it holds, so anything else in there was
    /// written for a VMLord that is no longer running and nobody will ever read
    /// it.
    pub(crate) fn sweep(&self, vm_directory: &Path) {
        let directory = layout::ssh_sessions_directory(vm_directory);
        let Ok(entries) = fs::read_dir(&directory) else {
            return;
        };
        let waited_for: Vec<PathBuf> = self
            .lock()
            .iter()
            .map(|session| session.report_path.clone())
            .collect();

        for entry in entries.flatten() {
            let path = entry.path();
            if waited_for.contains(&path) {
                continue;
            }
            if let Err(error) = fs::remove_file(&path) {
                tracing::debug!(
                    "an SSH session file left behind, {}, could not be removed: {error}",
                    path.display()
                );
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, Vec<SshSessionHandle>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Whether a session is over: its helper said so, or its helper is gone.
fn has_ended(session: &SshSessionHandle) -> bool {
    if session.finished.is_signaled().unwrap_or(false) {
        return true;
    }
    match WindowsEvent::exists(&session.alive_name) {
        Ok(exists) => !exists,
        // Unprovable rather than proven: a session that may still be running is
        // left alone.
        Err(error) => {
            tracing::warn!(
                "could not tell whether the SSH session of VM \"{}\" is still running: {error}",
                session.vm_name
            );
            false
        }
    }
}

/// Reads a session's report and takes the file with it.
///
/// No report means the helper never got to write one: its window was closed,
/// which is how people end shells rather than something to report as a failure.
fn take_report(path: &Path) -> SshSessionReport {
    let report = read_report(path);
    if path.exists()
        && let Err(error) = fs::remove_file(path)
    {
        tracing::debug!(
            "the SSH session report {} could not be removed: {error}",
            path.display()
        );
    }
    report.unwrap_or(SshSessionReport {
        outcome: SshSessionOutcome::WindowClosed,
        detail: String::new(),
    })
}
```

Declare `mod ssh_sessions;` in `crates/platform/src/lib.rs`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-platform ssh_sessions`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/platform/src/ssh_sessions.rs crates/platform/src/lib.rs
git commit -m "TASK-76: Wait for the sessions VMLord opened"
```

---

### Task 5: Launching through the helper

**Files:**
- Modify: `crates/platform/src/ssh_terminal.rs` (the `SshLauncher::launch` body, `terminal_commands`, the tests)
- Modify: `crates/platform/src/ssh.rs` (`invocation` gains a session-log argument)
- Test: `crates/platform/src/ssh_terminal.rs` tests, `crates/platform/src/ssh.rs` tests

**Interfaces:**
- Consumes: `layout::{ssh_session_log_path, ssh_session_report_path}` (Task 2), `SshSessions`/`SshSessionHandle` (Task 4), `crate::event::WindowsEvent`.
- Produces:
  - `ssh::invocation(client, endpoint, vm_directory, connect_timeout, remote_command, session_log: Option<&Path>)` — the readiness wait and the port mover pass `None`.
  - `SshLauncher::launch(&self, mapping, vm_directory, sessions: &SshSessions) -> Result<SshInvocation, SshLaunchFailure>`
  - `SshLaunchFailure::NoHelper { detail: String }`

- [ ] **Step 1: Write the failing tests**

In `crates/platform/src/ssh.rs` tests, extend the invocation tests with:

```rust
    #[test]
    fn a_session_log_is_where_openssh_writes_what_only_it_can_say() {
        let invocation = invocation(
            Path::new(CLIENT),
            &endpoint(),
            Path::new(r"C:\VMs\dev-linux"),
            None,
            None,
            Some(Path::new(r"C:\VMs\dev-linux\ssh-sessions\a.log")),
        );
        let line = invocation.command_line();

        assert!(line.contains("-E"), "{line}");
        assert!(line.contains(r"ssh-sessions\a.log"), "{line}");
    }
```

(Reuse whatever the surrounding tests already use to build an endpoint; the
existing tests in that module show the pattern.)

In `crates/platform/src/ssh_terminal.rs` tests, replace the assertions that
expect `ssh.exe` to be the hosted program:

```rust
    #[test]
    fn a_session_is_hosted_by_the_helper_rather_than_by_the_client() {
        let attempts = Attempts::default();
        let launcher = SshLauncher::for_test_with_helper(
            |_| Ok(Some(address())),
            |_, _, _| Ok(()),
            |_| true,
            attempts.spawn(),
        );
        let sessions = SshSessions::default();

        launcher
            .launch(&mapping(), vm_directory(), &sessions)
            .expect("a session opens");

        let commands = attempts.commands.lock().unwrap();
        let windows_terminal = &commands[0];
        assert_eq!(windows_terminal.program, PathBuf::from("wt.exe"));
        let arguments: Vec<String> = windows_terminal
            .args
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        let separator = arguments
            .iter()
            .rposition(|argument| argument == "--")
            .expect("the client follows a separator");
        assert!(
            arguments[separator - 1].ends_with("vmlord-ssh.exe")
                || arguments.iter().any(|argument| argument.ends_with("vmlord-ssh.exe")),
            "{arguments:?}"
        );
        assert!(
            arguments[separator + 1].ends_with("ssh.exe"),
            "the client is what the helper is told to run: {arguments:?}"
        );
        assert!(arguments.iter().any(|argument| argument == "--report"), "{arguments:?}");
        assert!(arguments.iter().any(|argument| argument == "-E"), "{arguments:?}");
    }

    #[test]
    fn an_opened_session_is_one_vmlord_waits_for() {
        let attempts = Attempts::default();
        let launcher = SshLauncher::for_test_with_helper(
            |_| Ok(Some(address())),
            |_, _, _| Ok(()),
            |_| true,
            attempts.spawn(),
        );
        let sessions = SshSessions::default();

        launcher
            .launch(&mapping(), vm_directory(), &sessions)
            .expect("a session opens");

        assert!(
            sessions.reap().is_empty(),
            "a session that just opened is neither finished nor forgotten"
        );
    }

    #[test]
    fn a_session_no_terminal_would_host_is_not_waited_for() {
        let attempts = Attempts { failures: 2, ..Attempts::default() };
        let launcher = SshLauncher::for_test_with_helper(
            |_| Ok(Some(address())),
            |_, _, _| Ok(()),
            |_| true,
            attempts.spawn(),
        );
        let sessions = SshSessions::default();

        let failure = launcher
            .launch(&mapping(), vm_directory(), &sessions)
            .expect_err("no terminal would have it");

        assert!(matches!(failure, SshLaunchFailure::NoTerminal { .. }));
        // Nothing to wait for, and nothing that would ever be reaped: the
        // helper that would have signalled was never started.
        assert!(sessions.reap().is_empty());
    }
```

`for_test_with_helper` is `for_test` plus a helper path that exists as far as
the launcher is concerned; give the test constructor a fixed
`PathBuf::from(r"C:\Program Files\VMLord\vmlord-ssh.exe")` and have production
resolve it beside the running executable.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-platform ssh_terminal`
Run: `cargo test-windows -p vmlord-platform ssh::tests`
Expected: FAIL — `invocation` takes five arguments, `launch` takes two,
`for_test_with_helper` does not exist.

- [ ] **Step 3: Write the implementation**

In `crates/platform/src/ssh.rs`, add the parameter and use it:

```rust
/// `session_log` is where OpenSSH is told to write its own log with `-E`. Only
/// the interactive launcher passes one: it is the sole caller that cannot read
/// the client's standard error, because the client's window is not its own.
pub(crate) fn invocation(
    client: &Path,
    endpoint: &SshEndpoint,
    vm_directory: &Path,
    connect_timeout: Option<Duration>,
    remote_command: Option<&str>,
    session_log: Option<&Path>,
) -> SshInvocation {
```

and, before the `-p` argument:

```rust
    if let Some(log) = session_log {
        args.push(OsString::from("-E"));
        args.push(log.as_os_str().to_owned());
    }
```

Update the two other callers (`guest_ready.rs` and `ssh_port.rs`) to pass
`None`; `cargo check-windows` names them.

In `crates/platform/src/ssh_terminal.rs`:

- Add `const HELPER_FILE_NAME: &str = "vmlord-ssh.exe";` and a `helper:
  Option<PathBuf>` field resolved in `production()` the way `com1_terminal`'s
  `helper_path` resolves its own (beside `std::env::current_exe()`), with the
  test constructors supplying a fixed path.
- Add the failure variant:

```rust
    /// The helper that hosts a session and reports how it ended is not beside
    /// `vmlord.exe`. Refused rather than falling back to a bare client: a
    /// session nobody can hear the end of is the thing this path exists to
    /// stop being normal.
    NoHelper { detail: String },
```

  with a `Display` arm naming the missing file.
- In `launch`, after the key check and before spawning:

```rust
        sessions.sweep(vm_directory);

        let session_id = Uuid::new_v4();
        let log_path = layout::ssh_session_log_path(vm_directory, session_id);
        let report_path = layout::ssh_session_report_path(vm_directory, session_id);
        // The directory has to exist before OpenSSH is told to write into it:
        // `-E` does not create one, and a client that cannot open its log
        // writes nothing anybody could classify.
        if let Err(error) = std::fs::create_dir_all(layout::ssh_sessions_directory(vm_directory)) {
            return Err(SshLaunchFailure::NoHelper {
                detail: format!("the session directory could not be created: {error}"),
            });
        }

        let names = SessionEventNames::of(session_id);
        let finished = WindowsEvent::create_named(&names.finished, true, false)
            .map_err(|error| SshLaunchFailure::NoHelper { detail: error.to_string() })?;

        // No connect timeout: an interactive session is one a person is
        // watching, and a deadline VMLord invented would close their window
        // mid-handshake.
        let invocation = ssh::invocation(&client, &endpoint, vm_directory, None, None, Some(&log_path));
        let hosted = helper_invocation(helper, &invocation, &report_path, &log_path, &names, &mapping.vm_name);

        // Inserted before the terminal starts: a helper that reports the
        // instant it is up must find a session waiting for it.
        sessions.insert(SshSessionHandle {
            id: session_id,
            vm_name: mapping.vm_name.clone(),
            report_path,
            finished,
            alive_name: names.alive.clone(),
        });
        if let Err(failure) = self.spawn_somewhere(&hosted, &mapping.vm_name) {
            sessions.forget(session_id);
            return Err(failure);
        }
```

  `SessionEventNames::of` mirrors `com1_terminal::EventNames::of` with the
  prefix `Local\VMLord.Ssh.<simple uuid>` and the two names `.finished` and
  `.alive`. `helper_invocation` builds an `SshInvocation` whose program is the
  helper and whose arguments are the five flags, `--`, the client and its
  arguments; `terminal_commands` then wraps *that* for `wt.exe` and the
  console fallback exactly as it does today.
- The `Ok` value stays the client's `SshInvocation`, so the diagnostic the
  repository writes still shows what was asked of `ssh.exe` rather than the
  helper's line.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-platform ssh`
Expected: PASS (all of `ssh::`, `ssh_terminal::`, `ssh_port::` and
`guest_ready::`).

- [ ] **Step 5: Commit**

```bash
git add crates/platform/src/ssh.rs crates/platform/src/ssh_terminal.rs \
        crates/platform/src/guest_ready.rs crates/platform/src/ssh_port.rs
git commit -m "TASK-76: Open interactive sessions through the SSH helper"
```

---

### Task 6: Saying how the session ended

**Files:**
- Modify: `crates/platform/src/ssh_sessions.rs` (a pure `session_diagnostic`)
- Modify: `crates/platform/src/repository.rs` (the struct, `new`, `open_ssh_in_state`, `refresh`, and a new `report_ssh_sessions` beside `report_console_failures` at `crates/platform/src/repository.rs:2060`)
- Test: `crates/platform/src/ssh_sessions.rs` tests

**Interfaces:**
- Consumes: `SshSessions`, `SshSessionEnd` (Task 4), `SshLauncher::launch` (Task 5), `vmlord_core::SshSessionOutcome` (Task 1).
- Produces:
  - `pub(crate) fn session_diagnostic(end: &SshSessionEnd) -> (DiagnosticLevel, String)`
  - `ssh_sessions: Arc<SshSessions>` on `HcsVmRepository`
  - `fn report_ssh_sessions(sessions: &SshSessions)` in `repository.rs`

Nothing in `vmlord-platform` captures diagnostics in a test -- `diagnostic!`
writes through `tracing` to a sink the application owns -- so what is tested is
the pure function that decides the level and the wording, and the repository
does nothing with it but choose the macro arm.

- [ ] **Step 1: Write the failing tests**

In `crates/platform/src/ssh_sessions.rs` tests:

```rust
    fn ended(outcome: SshSessionOutcome, detail: &str) -> super::SshSessionEnd {
        super::SshSessionEnd {
            vm_name: "dev-linux".to_owned(),
            report: SshSessionReport {
                outcome,
                detail: detail.to_owned(),
            },
        }
    }

    #[test]
    fn a_changed_host_key_is_an_error_that_says_where_the_keys_are() {
        let (level, message) = super::session_diagnostic(&ended(
            SshSessionOutcome::HostKeyMismatch,
            "Host key verification failed.",
        ));

        assert_eq!(level, DiagnosticLevel::Error);
        assert!(message.contains("dev-linux"), "{message}");
        assert!(message.contains("known_hosts"), "{message}");
        assert!(message.contains("Host key verification failed."), "{message}");
    }

    #[test]
    fn a_refused_credential_is_an_error_and_a_transport_failure_is_a_warning() {
        let (level, message) =
            super::session_diagnostic(&ended(SshSessionOutcome::AuthenticationFailed, ""));
        assert_eq!(level, DiagnosticLevel::Error);
        assert!(message.contains("credential"), "{message}");

        let (level, _) =
            super::session_diagnostic(&ended(SshSessionOutcome::TransportFailure, ""));
        assert_eq!(level, DiagnosticLevel::Warning);
    }

    #[test]
    fn a_shell_that_ended_and_a_window_that_was_closed_are_the_same_quiet_line() {
        let (level, ended_message) =
            super::session_diagnostic(&ended(SshSessionOutcome::Ended { code: 0 }, ""));
        assert_eq!(level, DiagnosticLevel::Info);

        let (level, closed_message) =
            super::session_diagnostic(&ended(SshSessionOutcome::WindowClosed, ""));
        assert_eq!(level, DiagnosticLevel::Info);
        assert_eq!(ended_message, closed_message);
    }

    #[test]
    fn a_nonzero_shell_status_keeps_its_code() {
        let (level, message) =
            super::session_diagnostic(&ended(SshSessionOutcome::Ended { code: 130 }, ""));
        assert_eq!(level, DiagnosticLevel::Info);
        assert!(message.contains("130"), "{message}");
    }

    #[test]
    fn a_message_does_not_end_in_the_gap_an_empty_log_leaves() {
        let (_, message) =
            super::session_diagnostic(&ended(SshSessionOutcome::AuthenticationFailed, ""));
        assert_eq!(message.trim_end(), message, "{message:?}");
    }
```

Add `DiagnosticLevel` to the test module's imports from `vmlord_core`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-platform ssh_sessions`
Expected: FAIL — `cannot find function session_diagnostic`.

- [ ] **Step 3: Write the implementation**

In `crates/platform/src/ssh_sessions.rs`:

```rust
/// What a person is told about a session that ended.
///
/// A line per outcome rather than one line with a variable in it, because what
/// to do next differs: a changed host key is a file to look at, a refused
/// credential is a key or a password to check, and a transport failure is a
/// guest to look at. The tail of OpenSSH's log is appended where there is one:
/// it is the sentence the client itself wrote, and no paraphrase beats it.
pub(crate) fn session_diagnostic(end: &SshSessionEnd) -> (DiagnosticLevel, String) {
    let vm = &end.vm_name;
    let (level, message) = match end.report.outcome {
        // A window someone closed is how people end shells, and a shell that
        // exited zero is the same event seen from the other side.
        SshSessionOutcome::Ended { code: 0 } | SshSessionOutcome::WindowClosed => (
            DiagnosticLevel::Info,
            format!("the SSH session to VM \"{vm}\" ended"),
        ),
        SshSessionOutcome::Ended { code } => (
            DiagnosticLevel::Info,
            format!("the SSH session to VM \"{vm}\" ended with code {code}"),
        ),
        SshSessionOutcome::HostKeyMismatch => (
            DiagnosticLevel::Error,
            format!(
                "the SSH session to VM \"{vm}\" was refused: the guest's host key is not the \
                 one VMLord learned for this VM. Nothing is reset automatically; the keys are \
                 in the VM's own known_hosts file."
            ),
        ),
        SshSessionOutcome::AuthenticationFailed => (
            DiagnosticLevel::Error,
            format!(
                "the SSH session to VM \"{vm}\" was refused: the guest did not accept the \
                 credential."
            ),
        ),
        SshSessionOutcome::TransportFailure => (
            DiagnosticLevel::Warning,
            format!("the SSH session to VM \"{vm}\" never reached the guest."),
        ),
        SshSessionOutcome::Unrecognized { code } => (
            DiagnosticLevel::Warning,
            format!("the SSH client for VM \"{vm}\" exited with code {code}."),
        ),
        SshSessionOutcome::Terminated => (
            DiagnosticLevel::Warning,
            format!("the SSH client for VM \"{vm}\" was stopped without exiting."),
        ),
        SshSessionOutcome::NotStarted => (
            DiagnosticLevel::Error,
            format!("the SSH client for VM \"{vm}\" could not be started."),
        ),
    };

    let detail = end.report.detail.trim();
    if detail.is_empty() {
        (level, message)
    } else {
        (level, format!("{message} {detail}"))
    }
}
```

Import `vmlord_core::DiagnosticLevel` at the top of the module.

In `crates/platform/src/repository.rs`:

- Add the field `ssh_sessions: Arc<SshSessions>` beside `ssh_launches`, built
  as `Arc::new(SshSessions::default())` in `new`.
- In `open_ssh_in_state`, clone it into the worker closure and pass it to
  `launcher.launch(&mapping, &vm_directory, &sessions)`.
- In `refresh`, beside `report_console_failures`, call
  `report_ssh_sessions(&self.ssh_sessions);`.
- Write the reporting function beside `report_console_failures`:

```rust
/// Says how each finished SSH session ended.
///
/// The launch says what was asked of `ssh.exe`; this says what came of it. The
/// level and the wording are decided in `ssh_sessions`, where they can be
/// tested; the only thing that has to happen here is that a record reaches the
/// panel under the right level, which `diagnostic!` takes as a literal.
fn report_ssh_sessions(sessions: &SshSessions) {
    for end in sessions.reap() {
        let (level, message) = ssh_sessions::session_diagnostic(&end);
        let vm = end.vm_name.as_str();
        match level {
            DiagnosticLevel::Info => {
                vmlord_core::diagnostic!(Info, Subsystem::Network, vm = vm, "{message}");
            }
            DiagnosticLevel::Warning => {
                vmlord_core::diagnostic!(Warning, Subsystem::Network, vm = vm, "{message}");
            }
            DiagnosticLevel::Error => {
                vmlord_core::diagnostic!(Error, Subsystem::Network, vm = vm, "{message}");
            }
        }
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-platform ssh_sessions`
Run: `cargo test-windows -p vmlord-platform repository`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/platform/src/ssh_sessions.rs crates/platform/src/repository.rs
git commit -m "TASK-76: Report how each SSH session ended"
```

---

### Task 7: Shipping the helper, and writing it down

**Files:**
- Modify: `crates/xtask/src/main.rs:29-37` (`ARTIFACTS`)
- Modify: `installer/check.ps1:95` (the `Require-File` block)
- Modify: `ARCHITECTURE.md` (the native SSH section written for task 78)
- Modify: `README.md` only if it lists the shipped binaries — check with `grep -n "vmlord-com1" README.md`

**Interfaces:**
- Consumes: `vmlord-ssh.exe` (Task 3).
- Produces: nothing code depends on.

- [ ] **Step 1: Add the binary to what a release collects**

In `crates/xtask/src/main.rs`, widen `ARTIFACTS` to five entries:

```rust
const ARTIFACTS: [(&str, &str); 5] = [
    (APP_TARGET, "vmlord.exe"),
    (APP_TARGET, "vmlord-com1.exe"),
    // The process a terminal hosts for an interactive SSH session, which is
    // what makes the end of one reportable. Beside `vmlord.exe` because that
    // is where the launcher looks for it.
    (APP_TARGET, "vmlord-ssh.exe"),
    (APP_TARGET, "vmlord-display.exe"),
    (AGENT_TARGET, "vmlord-agent"),
];
```

In `installer/check.ps1`, beside the other launched binaries:

```powershell
Require-File 'vmlord-ssh.exe'
```

- [ ] **Step 2: Write it down in ARCHITECTURE.md**

In the native SSH section, after the paragraph describing the interactive
launcher, add what changed: the terminal hosts `vmlord-ssh.exe`, which runs
`ssh.exe` with its own console and `-E`, and reports the outcome through a
JSON file announced by named events; VMLord classifies nothing itself beyond
reading that file, because the classification is `vmlord_core::classify_session`
and is shared with nothing else; a session is still not VMLord's child, and
still outlives the process.

- [ ] **Step 3: Verify the whole workspace**

Run: `cargo check-windows`
Run: `cargo test-windows`
Run: `cargo clippy --target x86_64-pc-windows-msvc --all-targets -- -D warnings`
Expected: all pass, no warnings.

- [ ] **Step 4: Commit**

```bash
git add crates/xtask/src/main.rs installer/check.ps1 ARCHITECTURE.md
git commit -m "TASK-76: Ship the SSH session helper and document the path"
```

- [ ] **Step 5: Manual verification on Hyper-V (alongside task 78's scenario)**

Not automatable and not a gate for the branch, but the thing this task exists
for. On the Windows host, with a running Ubuntu cloud-image VM:

1. Open a session, type `exit`. Expect one Info diagnostic: the session ended.
2. Replace the VM's `keys\id_ed25519` with another key and open a session.
   Expect the authentication diagnostic, naming the credential.
3. Empty the VM's `known_hosts`, let VMLord relearn the key, then replace the
   stored line with another key's. Expect the host-key diagnostic.
4. Stop `sshd` in the guest (`sudo systemctl stop ssh`) and open a session.
   The preflight port probe catches this one first -- confirm it still does,
   and that the message is the preflight's, not the session's.
5. Close a session's window with the mouse. Expect the plain "ended" Info, not
   an error.

---

## Notes for the executor

- Task 5 is the one with a real risk in it: `wt.exe` hands everything after
  `--` to one command line, so the helper's own flags and the client's have to
  survive that trip. If a session opens with the client visibly getting the
  helper's flags, the separator handling in `helper_invocation` is what to look
  at first, not `parse_ssh_helper_args`.
- `-E` moves OpenSSH's own messages out of the window and into the log. That is
  intended: the window closes with the session, so those messages were not
  readable in practice, and the diagnostics panel is where they land now.
  Password and host-key *prompts* are written to the console directly and are
  unaffected -- if a manual run shows a prompt missing, that is a bug in this
  work, not an accepted cost.
