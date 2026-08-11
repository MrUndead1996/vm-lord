# TASK-63: Guest readiness wait design

## Goal

Make "the VM is ready" an observable fact rather than a guess. Today creation
ends when the compute system has been registered, and a VM that is listed as
`Running` may still be halfway through cloud-init: the SSH key is installed at
the init stage, while `packages:` are applied later, at the config stage. A bare
probe of port 22 therefore reports "ready" in the middle of the work.

Creating a VM becomes a full cycle: build the disk, register the compute system,
start the VM, wait until the guest's cloud-init reports that it has finished,
and only then take the build off the list. Every way that cycle can fail names
its own cause, and a failure that happened inside the guest carries the tail of
the serial console that explains it.

## Scope

The task includes:

- a `guest_ready` module that waits for a guest in three phases -- address, TCP
  port, `cloud-init status --wait` -- each with its own timeout and outcome;
- the Windows OpenSSH client (`ssh.exe`) as the SSH transport, driven as a child
  process;
- `Starting` and `AwaitingGuest` build steps, and a creation flow that starts the
  VM and waits for it before the build finishes;
- returning the start's `Com1Session` from the build thread to the repository
  through the existing `BuildRegistry::reap` path;
- rollback rules for a failed start, a failed wait, and a cancelled build;
- four timeouts in `AppSettings`, defaulted and file-only;
- unit coverage for outcome parsing, per-phase timeouts, cancellation, and the
  diagnostics each outcome produces;
- an ignored Hyper-V test that creates a real Ubuntu cloud image VM and observes
  it become ready;
- operational logging at `DEBUG` through `ERROR`.

The task does not add: a guest agent, an interactive SSH session for the user, a
readiness wait for an ordinary `start_vm` (only the creation cycle waits),
`AgentStatus` polling on a schedule, or timeout fields in the settings dialog.

## Decisions

### The SSH transport is `ssh.exe`, not a library

The project has no SSH client. `ssh-key` in `vmlord-keys` handles the key format
only, and every maintained Rust SSH client (`russh`) is async-only, while the
epic rules out an async runtime: there is no `tokio` and no `.await` anywhere in
the tree.

That leaves `ssh2` -- libssh2 with `openssl-sys` -- or the OpenSSH client Windows
ships in `%SystemRoot%\System32\OpenSSH\ssh.exe`, present by default since
Windows 10 1809. `ssh2` gives a typed synchronous API at the cost of a second
vendored C build under MSVC; the first one, zlib-ng through CMake, already cost
a fifteen-line comment in the workspace manifest explaining a C-runtime
mismatch. `ssh.exe` adds no dependency and no build tooling, and driving an
external process behind a seam is what `com1_terminal` already does for the
console.

`ssh.exe` being an optional Windows component is handled as an outcome of its
own rather than ignored.

### Readiness is `cloud-init status --wait`, in three phases

The phases exist because their failures are different facts about the guest, and
a single timeout would report all three as "not ready":

1. **Address.** The VM has to be given an IP. HNS assigns it to the endpoint and
   the DHCP worker delivers it; this phase waits for the address to appear.
2. **Port.** `TcpStream::connect_timeout` against `ip:22`, retried until it
   succeeds. Plain `std`, no dependency, and it separates "the guest has not
   raised sshd yet" from "sshd refused us".
3. **cloud-init.** `ssh.exe ... cloud-init status --wait --long`.

### Exit codes decide readiness

`cloud-init status --wait` returns `0` when it is done, `1` on a fatal error, and
`2` when it finished degraded -- some module failed while the system came up.
Older cloud-init has no `2` and reports degraded as `0`.

- `0` -> ready, silently.
- `2` -> ready, with a `Warning` diagnostic quoting what `--long` said. One
  broken module does not turn a working VM into a failed build.
- `1` -> not ready, an `Error` diagnostic with the tail of `com1.log`.

### The transcript goes to a file, not a pipe

`cloud-init status --wait` prints a progress dot every second and may run for
twenty minutes. A `4 KiB` pipe fills long before that and deadlocks the child
against a parent that is polling `try_wait()`. The child's stdout and stderr are
therefore redirected to `<vm_directory>\cloud-init-status.log`, which also
leaves a second artifact beside `com1.log` for a person to read afterwards.

### A failed wait does not destroy the VM

Creation has been transactional so far, and it stays that way through the start:
a VM that could not start is nothing but debris, so a failed start rolls the
whole creation back.

A failed *wait* is different. The VM exists, it is running, and its `com1.log`
is the only record of what went wrong inside it. Removing the VM would remove
the evidence, so the wait's failures leave the VM in place and end the build
with an `Error` diagnostic.

### Cancellation rolls everything back

Cancelling a build cancels the creation, including a VM that has already
started: force-stop, tear down the compute system, release the endpoint, remove
the directory. The downloaded image survives in the image cache, so repeating
the creation is cheap -- which is what makes full rollback the affordable
answer.

### Timeouts live in settings, not in the dialog

Each phase gets a timeout, all four in `AppSettings` under `#[serde(default)]`
so an existing `settings.toml` keeps loading without a migration -- the way
`image_cache_path` was added. They reach the repository from the composition
root, so `HcsVmRepository::new` keeps its signature.

They are file-only. The settings dialog maps its fields by hand and today shows
a path, a language and log settings; four second-counts for a case that arises
once in the life of an installation would dilute it. The dialog already saves
the whole `AppSettings`, so a field it does not know about is preserved.

## Architecture

### `crates/platform/src/guest_ready.rs`

One entry point:

```rust
pub(crate) struct GuestReadiness { /* seams */ }

impl GuestReadiness {
    pub(crate) fn wait(
        &self,
        vm: &VmComputeSystemMapping,
        vm_directory: &Path,
        username: &str,
        monitor: &BuildMonitor,
    ) -> Result<GuestReady, ReadinessFailure>;
}

pub(crate) enum GuestReady {
    Ready,
    Degraded { detail: String },
}

pub(crate) enum ReadinessFailure {
    NoSshClient,
    NoAddress,
    Unreachable { last_error: String },
    CloudInitFailed { detail: String },
    TimedOut,
    Cancelled,
}
```

Seams follow the project's boxed-closure style with `production()` and
`for_test()`: the address source, the TCP probe, the SSH runner, and the clock
(`now` plus `sleep`) so that timeouts are tested without elapsing real time.
All of them are `Send + Sync`: the wait runs on the build thread.

Parsing an `ssh.exe` result is a free function -- `outcome(exit_code,
transcript_tail) -> Result<GuestReady, ReadinessFailure>` -- with no I/O of its
own. It carries most of the unit coverage.

`ssh.exe` is invoked as:

```
%SystemRoot%\System32\OpenSSH\ssh.exe
  -i <vm_directory>\keys\id_ed25519
  -o BatchMode=yes
  -o IdentitiesOnly=yes
  -o StrictHostKeyChecking=accept-new
  -o UserKnownHostsFile=<vm_directory>\known_hosts
  -o ConnectTimeout=<connect_timeout_secs>
  -l <username> <ip>
  cloud-init status --wait --long
```

`BatchMode=yes` guarantees no interactive prompt can hang the build.
`IdentitiesOnly=yes` keeps an agent's keys out of the attempt. The known-hosts
file is per VM, inside the VM directory, so VMLord never writes to the user's
own `known_hosts`.

The child is waited on by polling `try_wait()` every 500 ms; each poll checks
the deadline and `monitor.check_cancelled()`, and either one kills the child.

### Build flow

`BuildStep` gains `Starting` and `AwaitingGuest` after `Registering`. The build
closure runs create -> start -> wait, and the build leaves the list only when
the wait has resolved.

Two consequences in existing code:

- `VmStartPipeline` becomes `Send + Sync` (its seams are unbounded today, unlike
  `VmCreationPipeline`, whose bounds exist for exactly this reason) and moves
  behind an `Arc` in `HcsVmRepository`, beside `creation`.
- The start's result has to reach the main thread, because `Com1Session` and the
  held compute-system handle live behind `&mut self`. `BuildRegistry` keeps an
  outcome slot per build, and `reap()` -- which the repository already calls when
  refreshing the list -- returns it. The repository inserts the session into
  `com1_sessions` and calls `hold_started_system`. No new channel; existing reap,
  new payload.

### Outcomes

| Situation | VM | Build result |
|---|---|---|
| Start failed | rolled back | `Error` diagnostic naming the start failure |
| `GuestReady::Ready` | running | build succeeds |
| `GuestReady::Degraded` | running | `Warning` diagnostic quoting `status --long` |
| Any `ReadinessFailure` except `Cancelled` | left running | `Error` diagnostic: the cause in words plus the last ~40 lines of `com1.log` |
| `Cancelled` | force-stopped and removed | build reported as cancelled |

### Settings

```toml
[guest_readiness]
address_secs = 90          # the endpoint has to be given an address
ssh_port_secs = 300        # port 22 has to open once the address exists
cloud_init_secs = 1200     # the first boot installs `packages:` over the network
connect_timeout_secs = 10  # a single ssh connection attempt
```

The numbers are defaults with a comment explaining where each comes from,
mirroring `START_TIMEOUT` in `start.rs`. They are passed to the repository with
`.with_readiness_timeouts(...)` from `main.rs`.

## Testing

Unit:

- `outcome` for exit codes `0`, `1`, `2`, and for the stderr shapes `ssh.exe`
  produces (refused connection, host key failure, permission denied);
- each phase against a fake clock: the timeout fires exactly at its boundary and
  produces that phase's own failure;
- cancellation observed in each of the three phases;
- a missing `ssh.exe` produces `NoSshClient` before anything is spawned;
- rollback after cancellation stops a VM that had already started;
- each outcome maps to the diagnostic the table above specifies, and the
  `com1.log` tail is present when it should be.

Integration: an `#[ignore]`d test in `crates/platform/tests/hyperv.rs` that
creates a VM from a real Ubuntu cloud image, waits for `Ready`, and confirms a
key-based login works.

Logging: each phase transition at `DEBUG`, readiness at `INFO`, degraded at
`WARN`, every failure at `ERROR`.
