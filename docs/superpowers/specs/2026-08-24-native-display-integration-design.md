# Native display integration design

## Purpose

Task #121 connects the display stack to the application. Everything it needs
was built and left unwired: `#118` settled the protocol, `#115` made the guest
listen on three vsock ports, `#117` produced `vmlord-display.exe` and the
launch contract it is started with, `#119` and `#120` made the window
interactive. What is missing is the four things only the host application can
do -- list the three HvSocket services in the compute system's configuration,
run the control handshake with the VM's secret, start the viewer, and let a
person ask for all of it by pressing Connect.

Today `HcsVmRepository` has no `open_display` at all, so the trait's default
answers "display connections are not supported by this backend". After this
task the native backend opens a real desktop, and `asb_vm_open_display` is
reached only by the legacy backend, whose removal is #129's.

One thing the task description does not name has to be done here too: nothing
in the repository has ever set `VmDisplayFacts::guest`. The application derives
`DisplayState::WaitingForGuest` from its absence, `is_connectable()` is
therefore false for every VM that has ever run, and a Connect gated on it would
never be offered. Closing that gap is part of wiring Connect, not a separate
concern.

## Decisions

### The guest's readiness is the recipe's last stage

`ApplyDisplayRecipe` is already the guest's report over the agent channel, and
its final stage is exactly the fact Connect needs. `start_services` in
`display_kernel.rs` marks `SERVICES_START` `Ok` only after both units are
active *and* `/run/vmlord/display-broker.sock` exists -- the socket between the
broker and the session process, which is what proves the two halves met. A
guest that reports that stage is a guest listening on `VMLD`.

So the host reads readiness out of the report it already receives, and the
agent protocol does not change:

| what the recipe said | `VmDisplayFacts::guest` |
| --- | --- |
| `SERVICES_START` = `Ok` | `Ready(GuestDisplayDetail::default())` |
| `SERVICES_START` = `Skipped` | `Failed(PayloadInvalid, "this payload carries no display services")` |
| any stage `Failed` | `Failed(<the failure already derived from that stage>)` |
| no report yet | `None`, which the application reads as `WaitingForGuest` |

The recipe is applied once per agent session, so a guest that reconnects
re-reports, and `DisplayRuns::forget` drops it when the run ends. That is the
right lifetime: a readiness observed before a stop says nothing about a guest
that is not running.

The alternative -- a `ProbeDisplayRequest` of its own -- was rejected. It costs
a protocol minor, a guest implementation and a host poll, and it would carry
the same fact the recipe already carries at the same moment.

`GuestDisplayDetail` stays empty. Its two fields are documented as "when the
guest says", and today the guest says neither: the compositor is GNOME's
business and the DRM output name never leaves the device stage's message. An
invented value would be worse than an absent one.

### Three service entries, on every VM

`Devices/HvSocket/HvSocketConfig/ServiceTable` gains three entries beside the
agent's, keyed by the service GUIDs derived from `VMLD`, `VMLF` and `VMLI` and
carrying the same narrow SDDL -- `D:P(A;;FA;;;SY)(A;;FA;;;BA)`, SYSTEM and the
local administrators, who are the only accounts that can drive HCS anyway.

They are listed for every VM, not only for VMs with a desktop. A service table
entry is the partition's permission for a service to exist, not a claim that
anything inside the guest is listening: a headless guest simply never binds the
ports, and Connect is refused a step earlier by the desktop profile. Making the
entries conditional would mean a VM that changes profile after creation (#127)
has to have its `config.json` rewritten, which is a migration bought for
nothing.

A VM created before this task has no such entries and cannot get them: the
compute system is rebuilt from the stored configuration on every start. Those
VMs are recreated rather than migrated, which is the rule for the whole MVP.

### One viewer process per Connect, and no registry of them

The viewer already answers the "is one open?" question by itself: it takes
`Local\VMLord.Display.{runtime-id}` and, if the mutex is held, sends `Focus`
down `\\.\pipe\vmlord-display.{runtime-id}` and exits. So a repeated Connect is
a second process that lives for a few milliseconds, and VMLord needs no map of
open windows, no reference counting and no way to be wrong about what is on
screen.

That leaves one thing per launch: a thread that holds the launch pipes.

### The host half of the handshake

The viewer holds the socket and VMLord holds the secret, which is the whole
reason the launch pipes exist. `display_session.rs` is VMLord's side:

1. Read the VM's secret from `agent.secret` as a
   `vmlord_display_protocol::keys::Secret` -- the same 32 bytes the agent
   protocol minted, read through `Zeroizing`, as `agent.rs` already does.
2. Draw a 32-byte token. It is the right to ask for another session on these
   pipes, and it is compared, never logged.
3. Build `Session::host` with an `Offer`: `Mode::Auto`, tile size 32,
   `Capability::CursorStream` and `Capability::DynamicResolution` -- what the guest
   announces and what this viewer implements -- and the geometry from the VM's
   stored `display_mode`, or 1920x1080 when nothing has been saved.
4. Start `vmlord-display.exe` from the directory of the running executable,
   with piped standard input and output, and write `Message::Launch`.
5. Serve the pipe until it closes:
   * `RelayFromViewer(bytes)` -- read the record, `Session::handle`, write back
     `reply` and then `pending_auth` if the machine owes one; on
     `Event::ControlEstablished` send `Message::Handover` with the session id,
     the two derived channel keys, what was negotiated and the control
     sequence, and drop the session.
   * `RequestRelay { token }` -- compare the token, build a *new*
     `Session::host` and send its `ClientHello` as `RelayToViewer`. A mismatched
     token is refused and logged without its bytes.
   * anything else -- logged and ignored; a viewer that sends a `Launch` back
     is a build that disagrees with this one, and the revision check in
     `launch::decode` has already caught the ordinary form of that.

The geometry in the offer is a starting point and not an authority. The viewer
prefers the size it remembered for this VM and asks the guest to resize once
the session is up, which is #120's path and unchanged here.

`Limits::new(0, 0)` is what the control records are read and written with: the
frame caps depend on a geometry this side never carries frames for.

### The thread outlives nothing and is never joined

The launch thread lives as long as the viewer's pipes. It is deliberately not
joined when VMLord exits: a display session outliving the application is the
property the separate process was built for, and joining would either hang the
shutdown or force a `Close` that closes a window the user did not close.

What VMLord's exit does cost is the right to ask for a fresh `ClientHello`.
That is already handled on the far side -- `refresh` gives up after five
seconds and the window says so -- and the picture on screen is unaffected.

Stopping a VM sends nothing either. The partition disappears, the viewer's next
connect fails with partition-gone, and it exits quietly; a stopped VM is not a
failure to report in a window.

`SshLaunches` is therefore the wrong model to copy wholesale: its `join_all`
exists because a probe is bounded. This registry only tracks threads long
enough to join the ones that have finished, so a session opened an hour ago is
not still holding a handle.

### What Connect refuses, and how it says so

The preflight runs in the repository, where the facts are, and answers a
different sentence for each reason:

| state | what the user is told |
| --- | --- |
| VM not running | the VM has to be running before its display can be opened |
| `DesktopProfile::Headless` | this VM was created without a desktop |
| provisioning still pending | the desktop is still being installed |
| provisioning degraded / payload failed | the recorded failure's own message |
| guest has not reported | the guest has not reported its display services yet |
| guest reported `Failed` | the guest's failure message |
| HCS names no partition | VMLord cannot tell which partition this VM is running as |

Each is a `RepositoryError`, and the application already turns one into an
error diagnostic naming the VM. The UI keeps Connect disabled unless the status
is connectable, so these are the honest backstop for a click that raced a
refresh rather than the ordinary path.

### What reaches the diagnostics, and what stays in the viewer's log

Into the shared `Diagnostic` buffer, from the launch thread:

* the viewer was started (info, once);
* the handshake completed, with the geometry and mode agreed (info);
* the handshake failed, with the protocol error's own words -- an unsupported
  version, a tag that did not verify, a guest that closed the socket (error);
* a token that did not match (error);
* the viewer exited with a non-zero code (error), and quietly otherwise;
* the viewer could not be started at all -- no `vmlord-display.exe` beside the
  application (error).

Not into it: every retry, every reconnect of a frame channel, every ping. Those
are the viewer's own log, `%LOCALAPPDATA%\VMLord\display`, which stays a
per-VM file. Nothing formats a secret, a token, a channel key or a pixel: the
keys are never `Display`, the token is compared as bytes, and no frame ever
crosses this process.

### The UI change is one predicate

`render_selected_vm` already receives `Option<&VmDisplayStatus>`. Connect stops
depending on "the VM is running" and starts depending on
`VmDisplayStatus::is_connectable()`, with the status's own `message` as the
disabled tooltip -- which is how the button explains "still installing the
desktop" and "the guest has not reported yet" without the UI knowing what
either means. No Windows API appears in the UI, and no business logic with it:
the predicate and the sentence both come from the application layer.

The Update-display button #113 left out stays out. It is a second action with a
second set of failure states, and this task is Connect.

## Components

| where | what changes |
| --- | --- |
| `crates/platform/src/hcs_config.rs` | three service entries beside the agent's |
| `crates/platform/src/hvsocket.rs` | the three vsock ports and their service GUIDs |
| `crates/platform/src/display_session.rs` | new: the host end of one viewer |
| `crates/platform/src/display_launches.rs` | new: the launch threads in flight |
| `crates/platform/src/agent_session.rs` | the recipe report also yields readiness |
| `crates/platform/src/display_runs.rs` | records and returns the guest report |
| `crates/platform/src/repository.rs` | `open_display`: preflight, then launch |
| `crates/platform/Cargo.toml` | depends on `vmlord-display-viewer` for `launch` |
| `crates/ui/src/lib.rs` | Connect gated on the display status |
| `ARCHITECTURE.md` | the two "not wired yet" paragraphs become what it does |

The launch contract's encode/decode already lives in `vmlord-display-viewer`'s
library half, which builds anywhere; the host links it rather than growing a
second copy of a private schema.

## Testing

Everything but the partition is testable, and that is most of it.

* **The service table** -- the configuration builder's existing JSON test gains
  the three keys and their descriptor, and a test that the four service GUIDs
  are distinct.
* **The host half of the handshake** -- the session driver is written over a
  `Read + Write` pair and a message channel, not over a process, so a test
  drives a guest `Session` on the other side of an in-memory duplex and asserts
  that a hand-over arrives with a 16-byte session id and two 32-byte keys.
  `relay.rs`'s own test is the shape to follow, from the other side.
* **A refused token** -- a `RequestRelay` carrying the wrong bytes produces no
  hello and one diagnostic.
* **A second session** -- a `RequestRelay` with the right token produces a
  `ClientHello` that differs from the first, because the nonces do.
* **Readiness mapping** -- one test per row of the table above, over
  `ApplyDisplayRecipeResponse` values.
* **The preflight** -- one test per refusal, asserting the sentence names the
  reason and not another one.
* **The UI predicate** -- Connect's enabled state follows `is_connectable`.

What only a real partition can prove -- that the guest's `VMLD` listener is
reachable through the service entries, that a GDM greeter appears, that a
resize survives the round trip -- is #128's matrix, and this task does not
claim it.

## Out of scope

Removing `asb_vm_open_display` and the AppSandbox IDD artifacts (#129); the
E2E matrix and the performance gates (#128); the Update-display button and the
available-version line (#113's leftovers); post-create profile changes (#127);
audio, clipboard, multi-monitor, zero-copy and the Motion codec.
