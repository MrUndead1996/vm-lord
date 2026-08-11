# TASK-68: Interactive COM1 console design

## Goal

Make the COM1 console two-way. Today `vmlord-com1.exe` reads the guest's first
serial port and mirrors it to `com1.log` and its terminal window; nothing the
user types reaches the guest. TASK-62 excluded interactive serial input on
purpose. It is needed now because it is the only way into a VM when the network
is not available: `network_mode: None`, a network that did not come up, a guest
without an address, or a broken `sshd`. The manual verification of TASK-63 hit
exactly that case — a `login:` prompt on screen and no way to answer it.

## Scope

The task includes:

- opening the COM1 named pipe for reading and writing;
- forwarding the helper's standard input to that pipe, unchanged;
- putting the helper's console into raw mode so that backspace, control
  characters and non-echoed password entry work;
- restoring the console modes the helper found, on every exit path;
- keeping today's behavior intact: `com1.log` is still written, cancellation
  still closes the helper, and a stopped VM still ends the capture quietly;
- extending the UI's password hint to say that a passwordless guest cannot be
  logged into over the console either;
- unit coverage of the input seams and an `#[ignore]` Hyper-V test that logs
  into a live guest over COM1 and runs a command;
- operational logging at `DEBUG` through `ERROR`.

The task does not add a terminal embedded in the VMLord UI, an escape sequence
for quitting the helper, a way to reopen a closed console (TASK-69), input
recording, or any interpretation of what the user types.

## Opening the pipe

`connect` opens the pipe with `GENERIC_READ | GENERIC_WRITE` instead of
`GENERIC_READ` alone. The HCS end is already duplex, so the change is on the
client side only. `FILE_FLAG_OVERLAPPED` stays.

There is exactly one handle. A second `CreateFileW` on the same path does not
give a second, independent channel: HCS serves one instance, so the second open
either fails with `ERROR_PIPE_BUSY` or takes the stream away from the reader.
Reading and writing therefore share one handle, with a separate `OVERLAPPED` and
event per operation — which is what overlapped I/O exists for.

## Input path

Input is a second thread. The main thread keeps the capture loop exactly as it
is: an overlapped `ReadFile` on the pipe, cancelled through
`WaitForMultipleObjects` over the I/O event, the cancellation event, and the
parent process handle. Once `connect` succeeds, the helper starts an input
thread that blocks in `ReadFile` on `STD_INPUT_HANDLE` and writes whatever it
gets into the pipe with an overlapped `WriteFile`.

That thread is deliberately detached, and this is the one design decision worth
recording. A blocking console read cannot be woken: waiting on the console input
handle instead does not help, because that handle is also signaled by focus,
mouse and buffer-resize records, which `ReadFile` discards before blocking
again. Rather than racing that, the helper lets the thread stay blocked and lets
process exit collect it — `run_com1_helper` returns, `main` returns, and
`ExitProcess` takes every thread with it.

What that costs is one invariant: the pipe handle must not be closed while a
detached thread might still write to it, or the write could land on a recycled
handle. The handle therefore lives in an `Arc`, and the input thread holds a
clone; the last owner to disappear closes it, and while the thread is alive that
is never the main thread.

An input write that fails with the codes `is_end_of_stream` already recognizes —
a stopped VM, a guest-side close, a cancelled operation — ends the thread
quietly at `DEBUG`. Any other write failure is logged at `WARN` and also ends
the thread: the capture direction is still useful, and a helper that keeps
retrying a broken write would spin.

Input bytes go to the pipe and nowhere else. They are not written to
`com1.log`: the guest echoes what the user types and that echo is captured as
before, but a password is deliberately not echoed, and copying stdin into the
log would put it in a file beside the VM.

## Raw mode

Before the input thread starts, the helper takes the console out of cooked mode
and restores it when it is done:

- on `STD_INPUT_HANDLE`, `ENABLE_LINE_INPUT`, `ENABLE_ECHO_INPUT` and
  `ENABLE_PROCESSED_INPUT` are cleared and `ENABLE_VIRTUAL_TERMINAL_INPUT` is
  set. Line input would hold every keystroke until Enter, echo would double
  every character the guest already echoes and would show a password, and
  processed input would keep Ctrl-C for the helper instead of passing it on.
  Virtual terminal input makes Windows deliver arrows and function keys as the
  VT sequences a Linux tty expects, so the helper has nothing to translate;
- on `STD_OUTPUT_HANDLE`, `ENABLE_VIRTUAL_TERMINAL_PROCESSING` is set, so that
  the colors and cursor movement in cloud-init output and in a full-screen guest
  program render instead of appearing as escape codes.

Both original modes are captured first and restored by a guard whose `Drop`
runs on every path out, including a panic. If `GetConsoleMode` fails — standard
input is redirected, so there is no console to configure — the helper logs it at
`DEBUG` and forwards bytes anyway.

Ctrl-C reaches the guest as `0x03` and interrupts the command running there.
That is the point of the feature, and it means the helper no longer has a
keyboard interrupt of its own: the console is closed by closing its window, or
by stopping the VM, which breaks the pipe and ends the helper as it does today.

## User interface

The VM creation form already warns that an empty password field means a
key-only login. That hint gains the consequence this task creates: without a
password there is no console login either, so a guest whose network fails cannot
be reached at all. No new widget: the console is the helper's, and reopening it
is TASK-69.

## Error handling

- opening the pipe for writing is not a separate failure mode: an open that
  fails, fails the capture as it does today;
- a console whose mode cannot be read or set never prevents capture;
- an input write that fails ends input only; the capture continues, and the
  helper's exit code still reflects the capture;
- cancellation, parent exit and a stopped VM stay non-errors in both
  directions;
- nothing typed appears in `vmlord.log` or in `com1.log`.

## Testing

### Unit tests

Tests will cover:

- the console input mode the helper computes from an arbitrary original mode:
  line, echo and processed input cleared, virtual terminal input set;
- the console output mode: virtual terminal processing set, everything else
  left alone;
- forwarding of input bytes to a sink unchanged, including `0x03`, `0x7f`, a
  NUL and an invalid UTF-8 byte;
- the pipe access mask including `GENERIC_WRITE`, so that the duplex open
  cannot be lost to a later edit;
- a broken pipe on write classified as the end of the stream rather than as a
  failure.

The Win32 calls stay behind the same kind of small pure functions the rest of
the module uses, so the default test run needs no console, no pipe and no
Hyper-V.

### Ignored Hyper-V test

A `#[ignore]` test in `crates/platform/tests/hyperv.rs` will create and start an
Ubuntu cloud-image VM with a known password, open its COM1 pipe duplex from the
test itself, wait for the `login:` prompt, log in, run a command, and assert its
output comes back. It drives the pipe rather than the helper process because
that is the path the helper uses; the terminal window adds nothing to verify
here. Cleanup stays best-effort, as in the neighboring tests.

## Documentation impact

`ARCHITECTURE.md` gains the second direction: the COM1 section will describe the
console as two-way, name raw mode as what makes it usable, and state that input
is never recorded.
