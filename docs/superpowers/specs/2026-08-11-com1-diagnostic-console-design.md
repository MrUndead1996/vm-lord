# TASK-62: COM1 diagnostic console design

## Goal

Give every HCS-backed Linux VM an observable serial console. HCS exposes guest
COM1 through a Windows named pipe; VMLord must read it, persist the bytes beside
the VM, and show the live stream in a terminal window. This is the first host
channel that makes early kernel and cloud-init failures visible before SSH is
available.

A normal explicit VM start replaces the previous COM1 log. Reconnecting after
VMLord itself restarts appends to the current log because the guest has not
started a new boot.

## Scope

The task includes:

- `ComPorts.0.NamedPipe` in every newly built HCS VM configuration;
- a native Rust helper that reads COM1, writes `com1.log`, and mirrors the bytes
  to its standard output;
- automatic presentation through Windows Terminal with PowerShell and `cmd.exe`
  fallbacks;
- integration with start, reconnect, stop, force-stop, exit, deletion, and
  repository shutdown;
- unit coverage for the HCS document and process/reader decisions;
- an ignored Hyper-V test proving that Ubuntu cloud-init appears in the log;
- operational logging at the appropriate `DEBUG` through `ERROR` levels.

The task does not add an interactive serial input path, a terminal embedded in
the VMLord UI, log rotation, parsing of guest output, or support for non-HCS
backends.

## HCS configuration

`HcsVmConfigBuilder` will receive the stable COM1 pipe path and serialize it
under the virtual machine devices:

```json
{
  "VirtualMachine": {
    "Devices": {
      "ComPorts": {
        "0": {
          "NamedPipe": "\\\\.\\pipe\\vmlord-<vm-id>.com1"
        }
      }
    }
  }
}
```

The pipe name is derived from the VM UUID/HCS identity rather than the
user-provided display name. It is therefore stable across starts and VMLord
restarts, unique between VMs, and restricted to characters safe in a Win32
named-pipe name.

Creation already chooses the UUID and HCS compute-system ID before building the
configuration. It will derive the pipe path at that point and pass it to the
builder. The persisted `config.json` remains the source of truth when HCS has
discarded a stopped compute system.

The existing minimal-document test will be updated, and a focused unit test
will assert the exact `/VirtualMachine/Devices/ComPorts/0/NamedPipe` value.

## VM file layout

The serial log is stored as:

```text
<storage-root>\<vm-name>\com1.log
```

`layout.rs` owns this path, as it already owns `config.json`, the system disk,
the seed, and the SSH key paths.

An explicit `start_vm` opens the log in truncate mode before HCS starts. A
startup reconnect to a VM HCS already reports as running opens it in append
mode. The distinction prevents stale output from a previous boot from being
mistaken for the current one without destroying the current boot log merely
because the VMLord GUI restarted.

The log is raw serial output. VMLord does not decode, normalize, or parse it;
this preserves kernel control bytes and avoids making successful capture depend
on UTF-8 validity.

## Console helper

A small Rust console helper executable, `vmlord-com1.exe`, owns the live stream.
It receives only non-secret arguments:

- named-pipe path;
- COM1 log path;
- truncate or append mode;
- parent VMLord process ID;
- per-session cancellation and readiness object names;
- display name used for diagnostics.

The helper uses Win32 APIs inside a platform-specific module to wait for and
open the HCS pipe, perform cancellable reads, and wait for cancellation or
parent-process exit. After opening the log and installing its cancellation
waits, it signals a per-session readiness event. The parent waits for that event
before starting HCS; this handshake matters because `wt.exe` only dispatches a
tab and does not return the eventual helper process handle. The helper writes
every successfully read byte to both the log file and stdout, flushing often
enough that cloud-init failures remain visible if the guest or host process
exits abruptly.

The reader ends when any of these occurs:

- HCS closes the COM1 pipe;
- the parent signals cancellation;
- the parent VMLord process exits;
- an unrecoverable pipe, file, or stdout write error occurs.

Expected shutdown and pipe EOF are logged at `DEBUG` or `INFO`. Recoverable
presentation fallback is `WARN`. Failures that prevent capture are `ERROR` and
produce actionable `RepositoryError` or repository diagnostics. Guest bytes are
not copied into the global `vmlord.log`; they belong in the per-VM COM1 log and
terminal.

The helper does not contain VM lifecycle or provisioning business logic. Its
single responsibility is transporting one byte stream to one file and stdout.

## Terminal presentation

The platform launcher tries these hosts in order:

1. `wt.exe`, opening a new tab titled `VMLord COM1 — <vm-name>`;
2. `powershell.exe` with `CREATE_NEW_CONSOLE` and no `-NoExit`;
3. `cmd.exe` with `CREATE_NEW_CONSOLE`.

Each host runs `vmlord-com1.exe`; PowerShell and cmd are only presentation
fallbacks. They do not read the pipe, tail the file, or implement VM logic. The
helper receives arguments as individual process arguments, with host-specific
quoting kept in one tested launcher module.

Because neither fallback uses a keep-open option, its window closes when the
helper exits. Windows Terminal is invoked so that a normally exiting helper
closes its tab according to the terminal's normal close-on-exit behavior.

If all three launch attempts fail, VMLord refuses the VM start before invoking
HCS. COM1 capture is a required diagnostic channel, not a best-effort feature.
The combined error names each failed host so the user can repair the local
terminal installation.

## Start and lifecycle flow

An explicit start proceeds as follows:

1. Resolve the VM mapping, directory, stable pipe path, and `com1.log` path.
2. Create a per-start cancellation signal.
3. Launch the terminal/helper in truncate mode.
4. Wait with a bounded timeout for the helper's readiness signal, sent after it
   has opened the log and installed cancellation handling.
5. Let the ready helper wait for HCS to expose the pipe.
6. Attach networking, grant file access, and start the HCS compute system through
   the existing start pipeline.
7. If any preparation or HCS start step fails, signal cancellation so the
   waiting helper and terminal close, then return the original error augmented
   only when cleanup also fails.
8. On success, retain the console session alongside the VM's held HCS
   connection.

Starting the helper before HCS prevents loss of the earliest kernel output. The
helper waits for the server side rather than treating a not-yet-created pipe as
failure.

At repository initialization, each successfully reconnected running VM gets a
helper in append mode. A stopped or absent VM gets none.

Lifecycle behavior is:

- graceful stop: do not close COM1 immediately; the helper remains until the
  guest/HCS actually closes the pipe;
- force-stop: signal cancellation after the force-stop operation;
- HCS exit event: remove/cancel the retained console session;
- delete: cancel any retained session before removing the VM directory;
- replacing a session for another start: cancel the old session first;
- VMLord shutdown or crash: the helper observes parent-process exit and closes;
- helper exit: the terminal closes automatically while `com1.log` remains.

The repository keeps console session state keyed by VM UUID, matching its HCS
connection ownership. Finished sessions are reaped rather than accumulating
process and synchronization handles.

## Error handling

Failure boundaries follow these rules:

- inability to launch any terminal/helper prevents VM start;
- inability to create or truncate the log, or a helper readiness timeout,
  prevents VM start;
- network preparation or HCS start failure cancels the pending console session;
- a reader failure after HCS has successfully started does not terminate the VM;
  it is logged and surfaced through repository diagnostics;
- cancellation and normal pipe EOF are not user-facing errors;
- one VM's reader failure cannot stop readers belonging to other VMs;
- no secret provisioning values appear in process arguments or either log.

## Testing

### Unit tests

Tests will cover:

- exact serialization of `ComPorts.0.NamedPipe`;
- the stable pipe and `com1.log` paths;
- truncate mode for explicit start and append mode for reconnect;
- Windows Terminal, PowerShell, and cmd fallback order;
- helper argument construction and quoting;
- byte-for-byte duplication to log and stdout using injectable I/O;
- cancellation and EOF classification;
- start failure cancelling a pending console session;
- lifecycle registry replacement and removal.

Windows API calls and process launching will sit behind small injected
functions where needed so default unit tests do not require Hyper-V, an actual
pipe, or Windows Terminal.

### Ignored Hyper-V test

A `#[ignore]` test in `crates/platform/tests/hyperv.rs` will create and start an
Ubuntu 24.04 cloud-image VM, wait with a bounded timeout for `com1.log`, and
assert that it contains cloud-init output. Cleanup remains best-effort so a
failed assertion does not intentionally leave the test VM behind.

Seeing cloud-init on COM1 is the factual verification that the selected Ubuntu
cloud image boots with a serial-console kernel command line such as
`console=ttyS0,115200`. The implementation will not modify the cloud image or
invent a second cmdline mechanism.

## Documentation impact

`ARCHITECTURE.md` will document COM1 as the first host-observable guest
provisioning channel, the per-VM log location, the helper/terminal boundary, and
the truncate-versus-reconnect behavior.
