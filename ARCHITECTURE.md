# ARCHITECTURE.md

# VMLord Architecture

## Overview

VMLord is a native Windows application for managing Linux virtual machines using the Windows Host Compute System (HCS).

The architecture is intentionally layered to separate user interface, business logic and platform-specific implementation.

The implementation is fully Rust-native: the AppSandbox C backend it started
from, and the FFI layer that reached it, have been removed from the
distribution.

---

# Design Goals

The architecture should:

* isolate platform-specific code
* isolate unsafe Rust
* keep the UI independent from virtualization logic
* remain testable
* support future CLI and automation APIs

---

# High-Level Architecture

```text
+---------------------------+
|         UI (Rust)         |
+---------------------------+
              |
              v
+---------------------------+
|     Application Layer     |
+---------------------------+
              |
              v
+---------------------------+
|      Rust Core            |
+---------------------------+
              |
              v
+---------------------------+
| Windows APIs              |
| HCS                       |
| HNS                       |
| GPU-PV                    |
| HvSocket                  |
+---------------------------+
```

No C code remains.

---

# Layers

## UI

Responsibilities:

* windows
* dialogs
* controls
* rendering
* user interaction

The UI never calls Windows APIs directly.

---

## Application

Coordinates user actions.

Examples:

* Start VM
* Stop VM
* Update stopped VM configuration
* Import image
* Connect display
* Open terminal

Business workflows belong here.

---

## Core

Contains all virtualization logic.

This layer exposes safe Rust APIs.

It knows nothing about the UI.

Its modules today: `settings`, `logging`, `progress`, `distro` (distribution
profiles), `provisioning` (what VMLord delivers into a Linux guest) and `gpu`
(what a VM asks of GPU-PV and what has been observed of it), plus the request,
summary and repository types.

---

## Platform

Contains:

* Windows API wrappers
* unsafe Rust

This is the only layer allowed to interact with operating system APIs.

---

# Current Backend

The native Rust backend owns HCS integration, VM lifecycle, networking, GPU-PV
and display, and it is the only backend: `vmlord-legacy-backend`,
`appsandbox_core.dll` and the `VMLORD_BACKEND` switch that reached them are
gone. VMs created by AppSandbox are not read, migrated or listed -- VMLord
manages the VMs it created itself.

## Implemented scaffold

The initial executable is a Windows x64 Cargo workspace with these dependencies:

```text
vmlord (composition root)
  -> ui (egui/eframe)
  -> app (workflows)
  -> core (safe domain models)
  -> platform (native HCS backend, default)
  -> seed (the NoCloud documents cloud-init reads)
  -> image (release resolution, download, qcow2)

agent-protocol (portable wire contract)
  <- vmlord (host side, through platform)
  <- vmlord-agent (guest side, Linux)
```

`vmlord-agent` is the only crate that is not part of `vmlord.exe`. It is a
Linux program that runs inside a guest, so it is excluded from the workspace's
`default-members` and built on its own with `cargo agent`, which targets
`x86_64-unknown-linux-musl`. Its `main` refuses to compile for non-Linux
targets rather than link-failing later.
`agent-protocol` is in both sets, because both ends of the connection speak it.

`platform` and `vmlord-agent` are the only crates that override the workspace's
`unsafe_code = "deny"`: platform for Windows APIs, and the agent's `vsock`
module for the Linux socket ABI. Nothing is loaded dynamically and no C types
exist anywhere in the workspace, so none reach `core`, `app`, or `ui`.

`platform` is the Windows-native foundation for the incremental replacement.
It depends on `core`, `keys` and `seed` -- all three portable and free of I/O,
so nothing about needing them changes what `platform`'s own tests require --
never on `app` or `ui`, and contains all direct `windows-rs` calls. It owns
HCS/HCN handles and Windows events through safe RAII wrappers, and converts
Windows failures to `RepositoryError` values that include the operation, VM
name when applicable, and HRESULT. It deliberately does not depend on `image`
at build time: `image` is where the network lives (`ureq`, TLS, HTTP), and
pulling it into the crate that already holds every `unsafe` HCS call and every
raw handle would be one more thing to hold in mind while reading `platform`,
for the sake of a single call. `image` stays a dev-dependency there instead,
used only by the `#[ignore]`d tests that exercise a real cloud image;
the composition root is where the two halves meet in production, described in
"Creating a VM from a cloud image" below.

`vmlord-com1.exe` is a second binary of the `vmlord` package rather than a
crate of its own: it is a console for a terminal window to host, and every line
it runs lives in `platform`. It is part of the composition, not a layer -- it
depends on `core` for settings and logging and on `platform` for the capture,
and nothing depends on it. See "The COM1 diagnostic console" below.

Windows-only HCS integration tests are intentionally ignored by default. They
require Hyper-V, the Host Compute Service, and a disposable existing HCS VM ID
provided through `VMLORD_TEST_VM_ID`.

`platform::MetadataStore` persists the mapping between a VMLord VM id/name and
its HCS compute-system id as a single JSON document. HCS lifecycle work
(create, enumerate/open, reconnect, delete) resolves a VM to its compute
system through this store instead of re-deriving the mapping.

`platform::list_known_vms` enumerates every compute system HCS currently
reports, with the state HCS gives for it, and reconciles that against
`MetadataStore`'s persisted mappings. A mapping whose compute system HCS no
longer reports keeps its place in the list with no state rather than being
dropped. `platform::open_by_vm_id`/`open_by_vm_name` resolve a known VM
to its `HcsSystem` handle through the same store.

`platform::reconnect_known_vms` reopens one compute-system handle per VM in
`MetadataStore` and hands them back in a `VmConnections` registry meant to live
for as long as the VMLord process: handles do not survive a restart, so this is
what makes a restarted VMLord the owner of its running VMs again rather than an
observer of them. Reconnect never fails as a whole because one VM is in a bad
state -- each VM is reported individually as `Reconnected`, `Absent` or
`Failed` -- and only an unreadable store aborts it, because then nothing is
known at all. A VM HCS does not report is `Absent` and keeps its mapping: a
stopped VM looks exactly like one deleted outside VMLord, and dropping the
mapping would turn every stop into a delete.

Before a reconnected VM's agent listener is restored, `platform::run_recovery`
rebuilds the payload offers that died with the previous process. It only reads
the already staged GPU generation and the display's published `active`
directory, validates them through the same export builders a start uses, and
records their manifests in `GpuRuns` and `DisplayRuns`. It never stages a new
generation or changes the running compute system: the Plan9 device is immutable
for that boot, and the recovery only restores the names the guest must mount.

`platform::HcnNetwork::ensure` opens the one NAT network VMLord shares across
the whole installation, creating it when the Host Network Service does not have
it. The network has no owner among the VMs -- the per-VM object is the endpoint
-- so nothing about it is written to `MetadataStore`: a constant identifier
(`platform::VMLORD_NETWORK_ID`) is the whole of what VMLord remembers, which is
what makes "open it, create it if missing" idempotent without reading a file.

Its subnet is picked from `172.22.42.0/24` and then `172.22.142.0/24`, skipping
any candidate that overlaps a subnet the host's own adapters are on, and is
fixed at creation: guest addresses come out of it, so re-picking it would move
every address anything already remembers. When both candidates are occupied the
first is used anyway, with a warning -- a VM without a network is worse than a
VM on a contested subnet, and the warning is the only thing that connects the
resulting routing failure (a corporate VPN losing its route, typically) to
VMLord.

`platform::HcnEndpoint` is the per-VM half of that network: one endpoint per
VM, created lazily on the VM's first start and kept until the VM is deleted --
across stops and across VMLord restarts. Re-creating it per start would hand
the guest a new address every time and break everything that remembered the
old one (SSH, display). Its settings name the shared network and ask for no
address of their own: the network's IPAM assigns one, so VMLord never becomes
a second allocator of guest addresses beside HNS.

An endpoint's identifier is not derivable from anything else, so it is
remembered in `VmComputeSystemMapping::endpoint_id`, a `#[serde(default)]`
field -- mappings written before endpoints existed read back as "no endpoint
yet", which is exactly how a never-started VM reads, and need no migration.
The identifier is allocated by the caller and recorded after the endpoint
exists, so a VMLord that dies in between leaves an orphan endpoint behind;
collecting those is the cleanup on `initialize`, not something the creating
path tries to make atomic.

`cleanup::remove_orphan_endpoints` is that collection. It enumerates every
endpoint HNS has -- containers', WSL's and everyone else's are on the same list
-- keeps the ones whose `HostComputeNetwork` is `VMLORD_NETWORK_ID`, and deletes
those that no `MetadataStore` mapping names. The mappings are the whole of what
VMLord knows about its endpoints, so one no mapping names is one no VM can ever
use again, holding an address out of the subnet for as long as HNS has it. It is
logged at info: routine housekeeping over VMs that no longer exist, with nothing
for the user to do about it.

The network is never deleted in return, not even when its last endpoint is gone.
Re-creating it would re-pick the subnet and move every guest's address, which is
the whole thing a long-lived endpoint exists to avoid; an empty network costs a
host adapter and nothing else.

Two cases can still put a live VM's endpoint on that list: a second VMLord
process creating one at that exact moment, and a store that is not the one the
endpoint was recorded in -- the VM storage directory is the user's to change, and
the mappings live under it. Both cost the same and no more, because a VM whose
recorded endpoint is gone gets a new one on its next start: the guest changes
address rather than losing its VM.

`platform::VmStartPipeline` starts a VM the creation pipeline produced. It
re-grants the VM access to every file its stored `config.json` attaches before
issuing `HcsStartComputeSystem`: Hyper-V opens those files under the VM's own
`NT VIRTUAL MACHINE\<id>` security principal rather than the caller's token, so
a start without the grants fails with `ERROR_ACCESS_DENIED`. The stored
configuration, not a re-derived path list, is the source of truth for which
files the VM will open.

The start is also where a VM gets on the network. A VM whose mapping records
`NetworkMode::Nat` is given its endpoint before anything is granted or started:
`HcnNetwork::ensure`, then the recorded endpoint or a freshly created one, then
`HcnQueryEndpointProperties` for the MAC address HNS assigned. Failing any of
those fails `start_vm` -- a VM that asked for a network and did not get one must
not come up silently without it.

The recorded `endpoint_id` is opened rather than trusted, the same way
`HcsSystem::open_if_present` treats a compute system id: an endpoint deleted
outside VMLord or lost to an HNS reset is replaced instead of failing the start.
That changes the guest's address, but a VM that can no longer start is worse.
Nothing is undone when a later step fails -- the endpoint outlives stops and
lives until the VM is deleted, and dropping it after a failed start would hand
the guest a new address on the next attempt.

The endpoint and its MAC are then written into the stored `config.json` as a
`Devices/NetworkAdapters` entry keyed by the endpoint's own identifier, using
the same point-edit `update_vm` applies to `SizeInMB`/`Count`. The document on
disk is what a compute system HCS has forgotten is rebuilt from, so an adapter
that lived only in memory would be lost the first time that happened. The
section is replaced whole, which makes every start after the first converge on
the same document.

`VmComputeSystemMapping::network_mode` is what that decision reads, another
`#[serde(default)]` field: the stored `config.json` describes the adapter a VM
already has, not the mode it was created with, and mappings written before the
field existed read as `None` -- which is what every VM created so far asked for.
Its variant names are therefore an on-disk format.

The endpoint has to come off the VM before the VM is destroyed.
`platform::VmForceStopPipeline` therefore hot-detaches the adapter through
`HcsModifyComputeSystem` -- `RequestType: "Remove"` against
`VirtualMachine/Devices/NetworkAdapters/<endpoint id>` -- before it terminates
the compute system. HNS keeps an endpoint attached to the compute system it was
handed to even after HCS has destroyed that system, so a termination with the
adapter in place leaves the endpoint occupied and the next start fails with
`HCN_E_ENDPOINT_ALREADY_ATTACHED` (0x803B0014). The resource path is built from
`hcs_config::adapter_key`, the same function that keys the section in
`config.json`: a spelling that drifted between them would detach nothing while
HCS still reported success.

A detach that fails does not keep the VM running. A forced stop is the last way
to stop a wedged VM, so it terminates anyway and reports the failed detach as a
warning naming its consequence. `platform::VmShutdownPipeline` detaches nothing
at all: `HcsShutDownComputeSystem` returns once the request reaches the guest,
not once the guest is down, so there is no moment at which the guest is still
running and no longer needs its network -- and a guest that refuses to shut down
would be left running without one. The legacy AppSandbox backend made the same
choice for the same reason.

What is left over is recovered on the next start. A guest that powers itself
off, a crash, or a VMLord restart leaves no compute system to detach from, so
`platform::VmStartPipeline` recognises `HCN_E_ENDPOINT_ALREADY_ATTACHED` -- from
either the re-creation or the start -- and retries exactly once with a replaced
endpoint: it reads the occupied endpoint's address, deletes it, and creates a
new one asking for that same address. This is the one place VMLord names a guest
address, and it names one HNS assigned rather than one it chose, so HNS's IPAM
remains the sole allocator. A second occupied endpoint fails the start: one
replacement is a recovery, a loop of them would create an endpoint per attempt.
When the old address cannot be read, the replacement is created without one and
the guest is warned that its address changed.

AppSandbox's `hcs_detach_network` is not the precedent it looks like: the
function exists but is never called, and its comment -- that a detach is what
lets HCS deliver `SystemExited` -- is an untested hypothesis. AppSandbox avoids
the collision by never reusing an endpoint at all: it creates one per start,
deletes it on every stop, and keeps addresses stable by requesting a static IP.
VMLord keeps its endpoints instead, so it has to release them explicitly.

An endpoint alone does not give a Linux guest an address. HNS NAT does not
answer the guest's DHCP Discover, and this network's `EnableDhcpServer` is
rejected as unsupported, so `platform::dhcp` answers instead: a UDP socket on
`0.0.0.0:67` and a worker thread, started with the first NAT VM and stopped with
the process. The protocol itself is `arcbox-dhcp`'s -- it takes a datagram and
returns the reply -- while VMLord owns the socket, the thread and the
reservations.

VMLord is not an allocator here either. Every address it offers is one HNS
assigned to an endpoint and reserved to that endpoint's MAC, and a packet from a
MAC that has no reservation is dropped before the server sees it. That check is
also what keeps the host's own LAN out: the socket has to be bound to `0.0.0.0`,
because a socket bound to the vNIC's unicast address receives no broadcast
Discover, so DHCP broadcasts from every host interface arrive at it. A stranger
is sent nothing at all -- not even a NAK, which would break its configuration --
and the server's pool never holds an address HNS did not hand out, which is what
keeps `reserve_ip` from panicking on an address already taken.

A start reserves the address of the VM it is starting, and the first start of
the process also reserves the address of every endpoint already recorded: a
VMLord that was restarted while its VMs kept running would otherwise drop their
renewals, and a guest that is not answered does not ask again. An endpoint HNS
reports no address for fails the start -- a VM that asked for a network must not
come up with an adapter nothing will configure.

The subnet, gateway and mask come from the endpoint's own address rather than
from a second query to HNS. The DNS servers do not: WinNAT runs no DNS proxy on
the gateway, so a guest pointed at it would resolve nothing. `platform::host_dns`
offers the host's own IPv4 resolvers instead, minus loopback, link-local and
anything inside the VMLord subnet, and falls back to 1.1.1.1 and 8.8.8.8 when
nothing usable is left.

The lease is a day long, and it outlives VMLord: the server stops with the
process, so a guest keeps its address after the application is closed but has
nothing to renew against. Moving the server into a Windows service, or keeping
VMLord in the tray, is left for later.

The server belongs to the process rather than to the pipeline that started it. A
process builds more than one `VmStartPipeline` -- the repository has one and its
build cycle another -- and a server per pipeline meant the second pipeline to
start a NAT VM bound a port VMLord itself was already serving, which failed
every start after a creation until the application was restarted.

UDP 67 being served already fails the start with a diagnosis naming Internet
Connection Sharing, the Hyper-V Default Switch and third-party DHCP servers.
`SO_REUSEADDR` is deliberately not set: on Windows it would let VMLord take over
a port another server is answering on, and two servers answering the same guests
is worse than a start that says why it failed.

The address a listing reports for a VM is that same one, read back from the VM's
endpoint with `HcnQueryEndpointProperties` rather than from the guest. The host
assigns it: HNS's IPAM picks it and the DHCP server offers the guest that address
and no other, so the endpoint is where it is known. It becomes the guest's own
address only once the guest has taken it in a DHCP ACK, and nothing in VMLord
observes that acknowledgement -- so `VmSummary::ip_address` is where the guest is
expected to answer, not proof that it does.

Only a running VM is listed with one. The endpoint keeps its address across stops
-- that is the point of keeping the endpoint -- but a stopped guest answers
nowhere, and an address shown beside a stopped VM would read as somewhere to
connect. Gating on the state also keeps a list of stopped VMs from asking HNS
anything at all.

No absence is an error. A VM with no endpoint yet, an endpoint HNS no longer has
or reports no address for, and text that does not parse as an IP address all list
the VM without an address, each logged at debug: dropping a VM from the list over
its address would be far worse than listing it without one, and a listing runs
once a second, so a louder log would repeat one unreadable endpoint forever.

`platform::VmShutdownPipeline` asks the guest of a known VM to shut down
through `HcsShutDownComputeSystem`. HCS parses that call's options as JSON and
rejects a null pointer with `HCS_E_INVALID_JSON`, unlike start and terminate,
so a document is always passed. A successful shutdown means HCS delivered the
request, not that the guest powered off, so forced stop remains a separate
action.

That request is delivered on a thread of its own. The call waits for HCS to
answer -- moments when the guest is listening, and up to the pipeline's
sixty-second bound when the Host Compute Service is wedged -- and `stop_vm` used
to spend that wait on its caller's thread, which is the one drawing the window:
a slow stop froze the whole UI until the answer came back.
`platform::shutdown_workers::ShutdownWorkers` now carries each request,
modelled on the build registry for the same reasons: a flag the worker sets
however it leaves, the answer parked for the main thread, and a join for every
thread before the process goes. `stop_vm` still refuses what it can refuse
cheaply -- an unknown VM, one still being built, one already being shut down --
so an obvious mistake is the return value of the click that made it rather than
a diagnostic a moment later.

What comes back is recorded as a marked event, and reaped in `refresh`, the
`&mut self` call the refresh already makes. A delivered request means the guest is on its way down,
so its agent connection and its HCS event watch are given up there; a failed one
means the opposite -- the guest never heard the request and keeps running -- so
both stay where they are and only the reason becomes an `Error` diagnostic. A
second Stop on a VM whose request is still in flight is refused rather than
queued behind the first: clicking again means "it is taking long", not "ask once
more". Once the request is over the VM can be asked again, which is what stops a
guest that ignored the first one.

A stop also opens the VM's COM1 console, unless a window is already showing it.
Everything a shutdown does after HCS has delivered the request -- services
stopping, filesystems unmounting, a unit hanging for its ninety seconds -- is
written to the serial console and nowhere else, so it is the only account there
is of a stop that stalls. The log is appended to, because this is the boot that
is ending. A console that cannot be opened is a `Warning` diagnostic and nothing
more: the user asked for the VM to stop, and a window is how they watch it, not
what they asked for.

Two things have to be right for that request to reach a guest at all, and until
#70 neither was, which made every stop an emergency one.

The first is the compute system's own configuration. Integration components
live in `VirtualMachine.Services`, a section the HCS schema introduced in 2.5;
VMLord asked for 2.1, a model that has no such section, and got whatever HCS
offers by default -- timesync, but no shutdown. Nothing in the guest can make
up for that: Linux' `hv_util` driver binds to the VMBus channel
`0e0b6031-5213-4934-818b-38d90ced39db` and answers `ICMSGTYPE_SHUTDOWN` with
`orderly_poweroff`, but only if the host offers the channel, and a VM built
from a 2.1 document does not. `HcsVmConfigBuilder` therefore writes 2.5 and
names `Shutdown` and `Timesync`; timesync is named beside shutdown because
naming any service replaces the default set, and a VM must not lose its clock
to gain a way to be turned off. Heartbeat and key-value exchange stay out until
something needs them: an offered channel is a guest-facing surface, not a free
courtesy.

The second is the options document, which schema 2.5 also gave that call.
`Mechanism` picks between the two ways HCS can reach a guest: `GuestConnection`,
the hvsocket channel a utility VM's in-guest agent serves, and
`IntegrationService`, the VMBus channel an ordinary guest's own drivers answer.
Left unnamed -- the empty object VMLord used to pass -- HCS reaches for the
guest connection VMLord's VMs have never had and fails the operation with
`ERROR_NOT_SUPPORTED`. `Force` stays false: a graceful stop is the request a
guest may take its time over, and stopping one that will not go is what
terminating is for.

That HRESULT is still reported as its own error naming a forced stop, because
no retry helps -- but it now means something narrower: a VM whose stored
`config.json` predates #70. That document is what a start re-creates the compute
system from, so such a VM keeps being built without services and has to be
re-created to become stoppable. Existing VMs are not migrated: VMLord has no
users yet.

The legacy AppSandbox backend never found this. It resolved
`HcsShutDownComputeSystem`, never called it, and implemented graceful shutdown
over its own in-guest agent instead, returning `ERROR_NOT_SUPPORTED` itself
whenever that agent was unreachable. VMLord needs no agent for this.

A guest that resets rather than powers off is the same subject seen from the
other side, and it is bug #110: `reboot` inside a guest used to leave the VM
stopped. Nothing in VMLord asked for that. HCS reported
`{"ExitType":"UnexpectedExit", ... "WorkerExit":{"ExitCode":255,"Detail":"ProcessUnexpectedExit"}}`,
which is the worker process dying rather than the orderly
`GracefulExit`/`PowerOff` a `poweroff` produces -- the two are distinguishable
in the log, and both were seen on the same VM. What HCS does not say is why the
worker died. Windows Error Reporting does, once per attempted reboot:

```text
vmwp.exe … vmuidevices.dll … c0000005 … 000000000004bd7e
```

`vmuidevices.dll` is where a worker keeps a VM's input and display devices, and
the document named `Keyboard` and `Mouse` without naming `VideoMonitor` -- the
device the other two hang off. Nothing composes a display while a machine is
only booting, so a cold start never touched it; a reset is what makes the worker
build its UI devices a second time, and it dereferenced what was not there.
`HcsVmConfigBuilder` therefore names a 1024x768 `VideoMonitor` for every VM,
although VMLord draws no framebuffer and offers no RDP: the section exists to
make the device real, not to be looked at. The legacy AppSandbox backend named
it for every VM it built, with a comment saying vmwp crashes without it.

The VM's own state has a home for the same reason a machine has to survive
being put back together: `VirtualMachine.GuestState` names
`GuestStateFilePath`, the `.vmgs` a machine's virtual firmware keeps its store
in -- the UEFI variables and the boot entries written into them -- and
`RuntimeStateFilePath`, the `.vmrs` its worker keeps the state of the running
machine in. Creation makes both through `HcsCreateEmptyGuestStateFile` and
`HcsCreateEmptyRuntimeStateFile` -- their format is Hyper-V's, and a compute
system pointed at anything else is refused -- grants the VM access to them for
the same reason it grants access to its disk, and names them in the document.
They live beside `config.json` rather than under `disks/`: they describe the
machine rather than being one of its disks. Without the section a VM still
boots, on state HCS holds for a machine starting from nothing, and what it
writes into its firmware store is forgotten by the next boot. That is not what
made a reboot fail -- a VM given the files still crashed in `vmuidevices.dll`
-- but a machine that reboots is one whose firmware state has to outlive a
boot, so both belong to the same fix.

A VM whose stored `config.json` predates #110 keeps being built without either
section and has to be re-created to become rebootable, exactly as one predating
#70 has to be re-created to become stoppable. Existing VMs are not migrated:
VMLord has no users yet.

`platform::VmForceStopPipeline` is that remaining option: it stops a known VM
through `HcsTerminateComputeSystem`, which needs nothing from the guest, so its
completion means the VM really has stopped.

`platform::VmDeletionPipeline` removes everything a VM is made of: its compute
system, its endpoint in the shared network, the `config.json` creation wrote,
its disks, and its `MetadataStore` mapping. Each step runs even if an earlier
one failed -- a resource left behind is no reason to leave the others -- and the
mapping is dropped last and only when nothing failed. The endpoint goes after
the compute system it may still be attached to, and it is the only network
resource a deletion touches: the network stays, whether or not this was the last
VM in it. That order is what keeps a partial failure recoverable: a
VM whose resources are still partly present stays known to VMLord, stays listed,
and can be deleted again, whereas dropping the mapping first would orphan files
and compute systems the application can no longer reach. A running VM is refused
rather than terminated under its guest, because deletion cannot be undone. The
disks can be kept at the user's request, which leaves the VM's directory in
place and therefore reserves its name; the image the VM was installed from is
never touched.

Keeping the disks keeps the disks and nothing else that only served the VM. The
`config.json` describes a compute system that is gone, the `.vmgs` and `.vmrs`
hold the firmware and runtime state of that same gone machine, and the VM's SSH identity
-- the `keys/` pair and the `known_hosts` VMLord learned for it -- belongs to a
guest nobody reaches through VMLord any more, so both go in either mode. A
private key with no owner is worth removing on its own, and host keys kept past
their VM would only pin a host that no longer answers; a guest booted from the
kept disks brings its own `authorized_keys`, so giving it a key again is the
user's to do. There is no separate choice about this: a checkbox for keeping an
identity nothing can use would be a way to get it wrong. `com1.log` and
`cloud-init-status.log` stay either way -- they record what the VM did rather
than how to log into it, which is what someone who kept the disks may still need
to read.

An HCS compute system is a runtime object, not a registered machine: it exists
only while it is created or running, and HCS destroys it as it exits -- whether
the guest powered itself off or a forced stop terminated it. Reopening a stopped
VM therefore fails with `HCS_E_SYSTEM_NOT_FOUND`, which is a statement about its
state rather than an error, and `HcsSystem::open_if_present` reports it as
`None`. `ShouldTerminateOnLastHandleClosed: false` does not change this; it only
keeps a *created*, never-started system alive once VMLord's handles close.

What survives a stop is everything the VM is made of: its disks, the
`config.json` creation wrote, and its `MetadataStore` mapping. `VmStartPipeline`
therefore re-creates the compute system from that stored configuration whenever
HCS no longer knows it, under the same id, before starting it. Without that
step a stop would silently become a delete.

A system HCS *does* know is rebuilt too, but only when it is in `Created`. That
is the state a creation leaves behind, and the document creation used carries no
`NetworkAdapters` section: the endpoint does not exist yet, because one is made
on the first start so that a VM nobody ever starts takes no address. The start
creates the endpoint and writes the adapter into `config.json` -- and until
`plan_for_existing` was added, the freshly created system was then started
exactly as it stood, so the guest came up with no network card while HNS held an
endpoint with an address and nothing attached to it. Only the first start after
a creation was ever affected: every later one finds no system at all, because
HCS destroys one as it stops, and takes the re-creation path already. A system
in `Created` has executed nothing, so rebuilding it destroys no state; every
other state has a guest behind it and is started as it stands.

`platform::HcsVmRepository` is the `VmRepository` the composition root wires in
by default. It owns the process-wide `HcsClient`, the `MetadataStore` under the
configured VM storage directory, and the `VmConnections` registry, and maps each
repository operation onto the pipeline that implements it. Its `initialize` also
brings the shared NAT network up and runs the orphan-endpoint cleanup, so the
network exists -- with its host adapter, its subnet and its NAT -- from the
moment VMLord runs rather than from the first start that needs one. Neither can
fail the initialization: every start ensures the network again, and that is where
a host whose HNS is broken has to be told about it, rather than losing the VM
list and its deletions too over a service only the networked VMs need.

The native backend reports persisted GPU and display configuration together
with the facts observed from the running guest agent. SSH is the native
backend's alone, as
"Running the OpenSSH client" describes, and availability is read from the VM's
mapping, which records what its creation asked for. Network mode is reported from the VM's mapping, because the
edit form is filled from `VmSummary`: a summary that always said `None` would
make an unrelated edit switch a NAT VM off the network. The guest's address comes
from the VM's HNS endpoint, as above.

A VM's state comes from `HcsEnumerateComputeSystems`, not from the compute
system's mere presence: creation leaves behind a system that has never executed
anything, and only a `Running` one is running. The enumeration is the right
source because it is the only one that answers for a created system at all --
`HcsGetComputeSystemProperties` on one fails outright -- and because it is a
single call VMLord already makes for the whole list.

HCS writes `State` into an enumeration entry only once its compute system has
run: a VM created and never started is enumerated with an `Id`, a `SystemType`,
an `Owner` and a `RuntimeId` and nothing else, while a running one carries
`"State": "Running"`. A missing state therefore means `Created`, and that
absence is the only signal separating the two.

`platform::watch` registers an HCS event callback on every compute system
VMLord holds, which is the only source for what the enumeration cannot say: why
a VM stopped, that its guest crashed, and that the Host Compute Service
disconnected. The callback runs on a thread HCS owns, so it only classifies the
event and queues it; the repository drains that queue in `refresh` on every
refresh, records each event, marks the significant ones for the panel, and
releases the handle of a VM that is gone. The enumeration remains the sole
authority on VM state.

Each registration carries a generation, counted by the event sink the watches
report into, and a drain drops any event whose generation is no longer the one
held for its VM. Counting per sink rather than per `VmConnections` is what keeps
the generations unique among exactly the events one drain compares. HCS
delivers asynchronously, so an exit can arrive after the enumeration has already
reported the VM stopped and the user has started it again: without the
generation, that stale event would release the handle of the VM now running and
report that it had stopped. A generation is never reused, and an event for a VM
no handle is held for at all is not stale -- there is simply nothing left to
release.

A `ServiceDisconnect` releases every handle it names, and nothing outside
`initialize` reopens one or re-registers a callback, so a drain that saw one
also warns that VMLord reports no further HCS events until it is restarted.
HCS delivers the disconnect once per compute system and those deliveries can
fall on either side of a refresh, so the repository remembers having warned and
warns once per run rather than once per drain. The backend deliberately stays
`Ready`: `list_known_vms` succeeds again as soon as the service is back, and
`WorkspaceApp` has no way out of `Unavailable`.

Whether a running guest has finished booting is still unobservable -- HCS
reports nothing about it -- so `AgentStatus` stays `Unknown` until the guest
agent lands.

`platform::layout` decides where a VM's `config.json` and disks live, so
creation, start and the repository cannot disagree about it. A VM name is used
as a directory name and is rejected unless it is a single plain path component,
so it cannot escape the storage root.

An edit rewrites `SizeInMB` and `Count` in the VM's stored `config.json` and
changes nothing else, which is what editing a VM means once `VmStartPipeline`
rebuilds the compute system from that document. A running VM keeps the topology
it booted with, so the application layer accepts the edit and warns that it
applies after a restart rather than refusing it. An edit also carries the
network mode, which is recorded in the VM's mapping rather than in its
`config.json` and reaches the VM the same way: the next start writes or removes
the `NetworkAdapters` section to match. The GPU mode is recorded in the mapping
the same way, and may only be changed while the VM is stopped. The `External`
and `Internal` network modes are still rejected until their own task lands.

`VmSummary`'s memory and processor counts come from the same stored
configuration. `disk_gb` comes from the `MetadataStore` mapping, where creation
records it: the disk itself cannot be asked while its VM runs, because Hyper-V
holds the VHDX open exclusively and `OpenVirtualDisk` then fails with
`ERROR_ACCESS_DENIED` -- and a running VM is exactly what the VM list refreshes
against most. A mapping written before that field existed falls back to
`GetVirtualDiskInformation` once (a dynamically-expanding VHDX file is far
smaller than the disk it presents, so its file length cannot answer this) and
records the answer, so the fallback stops being needed.

A VM whose configuration cannot be read is still listed, with the unreadable
sizes zeroed and a warning diagnostic raised, because hiding a VM that exists
is worse than reporting it incompletely.

`core::settings` owns the UI-independent application settings model and TOML
persistence. The composition root initializes it before the backend. Settings
are stored per user at `%LOCALAPPDATA%\VMLord\settings.toml`; the initial file
uses `%LOCALAPPDATA%\VMLord\vms` for VM data,
`%LOCALAPPDATA%\VMLord\logs\vmlord.log` as the log-name template and
`%LOCALAPPDATA%\VMLord\images` for downloaded distribution images. The
configuration directory and the default VM, log and image directories are
created on first launch. `image_cache_path` carries `#[serde(default)]` and is
filled in on load when absent, so a `settings.toml` written before the field
existed keeps loading without a migration.

The main window's own geometry is kept apart from the settings, in
`%LOCALAPPDATA%\VMLord\window.ron`. `eframe` writes it and applies it before
the window is shown -- the place, the size and whether the window was
maximised, clamped to the monitors there are now -- which is why the only
decision here is where the file goes: `core::settings::window_state_path`
answers with VMLord's own directory rather than the roaming profile `eframe`
would pick from the application name, and the application layer hands that
path to the UI. It is a `NativeOptions` path and not a setting: nothing in it
is a choice the user made in a form, and a session with no `%LOCALAPPDATA%`
simply opens at 960x640 every time. Nothing else is persisted -- egui's own
memory holds scroll offsets and half-filled forms, and restoring those is not
what leaving the application and coming back means.

Distribution profiles are installed under
`{app}\distros\*.json` by the release build. Before loading the catalog, the
composition root passes that directory (derived from `current_exe`) to
`core::distro::sync_bundled_profiles`, which copies profiles into
`%LOCALAPPDATA%\VMLord\distros`. The ownership file
`%LOCALAPPDATA%\VMLord\distros\.bundled-profiles.json` records SHA-256 hashes
by filename: a new bundled profile is copied, and a changed bundle replaces
only a recorded bundled copy. Files not recorded there are user profiles and
are never replaced or deleted; profiles removed from a later bundle stay in
place. `core::distro::DistroCatalog` enumerates the per-user directory once at
startup, derives each profile identifier from its file name and deserializes
the complete catalog. Two profiles are shipped: `ubuntu.json` and
`arch.json`. `default_distro` in `settings.toml` selects the profile
used for new cloud-image VMs and defaults to `ubuntu` when an older settings
file has no such field. The settings dialog lists every loaded profile and
applies a new default immediately to subsequent VM creation. The creation
dialog lists the same catalog and opens on the profile `default_distro` names,
with that identifier prefilled as the VM name; choosing another distribution
there restarts the release from the new profile's first and replaces the
account name and the VM name too, unless either has been typed over, in which
case what someone wrote survives. Directory, file
and JSON errors retain the failing path; an unknown configured identifier names
the missing profile rather than silently falling back.

`core::logging` installs the shared `log` backend after settings are loaded and
before the backend starts. It writes records at the configured `log_level` to
both standard output and a file; all Rust crates use the `log` facade to emit
application records. `log_directory` names the folder those files land in
rather than any one file -- see "One file per run" below.

The current UI initializes the backend, shows availability and diagnostics,
lists known VMs, can create Linux VMs from ISO images, and can edit them. The
creation form asks for the medium, the sizes and the modes, but not for a user
or a password: an installation medium is installed by hand, and those fields
return with the cloud-image form in #65. It submits safe requests through the
application layer, which knows nothing about which backend serves them. Edit is
available whichever state the VM is in;
Delete stays limited to stopped VMs. `Open COM port` is enabled only while the
VM runs, and reopens the serial console described under "The COM1 diagnostic
console" -- the UI calls `WorkspaceApp::open_console` and nothing else, since a
named pipe is the platform layer's business. `Connect` follows the derived
display status rather than the VM's state -- a running VM whose desktop is
still installing has nothing to open a window on -- and shows that status's own
sentence while it cannot be pressed. `Update display` stands beside it, reads
the same status and starts a background update, described under "Display:
updating and rolling back".
Snapshots remain future application-layer work.

### Image download

`vmlord-image` fetches a distribution's cloud image over HTTPS into the
directory configured as `image_cache_path`. It is a separate crate rather than
part of `core` or `platform`: `core` carries no I/O dependencies, and `platform`
is the Windows-specific layer where `unsafe` is allowed, while downloading needs
neither. The qcow2 reader and the release resolver live there too.

The cache is addressed by content: an entry is named after the SHA256 it is
expected to have, so two releases cannot collide on a name and a file whose name
disagrees with its content cannot exist. The checksum is verified on every call,
cache hit included -- it is the only thing that tells a complete image from one
an interrupted download left truncated, and it costs seconds against re-fetching
hundreds of megabytes.

Trust comes from HTTPS. A checksum list downloaded from the same server as the
image proves nothing about authenticity, because whoever could swap the image
could swap the list. Certificates are checked against the platform verifier, so
a host behind a TLS-inspecting corporate proxy works instead of failing with
nothing the user can do. Signature checking is a later reinforcement.

The client is blocking (`ureq`, no async runtime, following the DHCP worker in
`platform::dhcp`). The `gzip` feature is deliberately off: transparent
decompression would put the byte count out of step with the file on disk and
send the resume offset astray. Downloads resume through HTTP `Range` when the
server allows it, fall back to a fresh transfer when the server ignores or
rejects the range, and report any other status as itself rather than as an
opaque transport failure.

Two downloaders of one image are separated by an exclusive operating-system lock
on the partial file (`std::fs::File::try_lock`). It covers two threads of one
process as well as two processes, and it is released when the handle closes,
including when the process dies -- unlike a marker file, which survives a crash
and forces a guess about whether it is stale. The second downloader is refused
with `AlreadyInProgress` rather than made to wait: queueing behind another
download is the caller's policy, and a wait buried in the fetch would need a
timeout invented out of nothing. Cancellation leaves the partial file intact so
the next attempt resumes it.

That lock behaves differently on the two platforms, and the difference is not
cosmetic. On Windows `LockFileEx` is **mandatory**: a second handle reading the
locked range fails with `ERROR_LOCK_VIOLATION`. On Linux `flock` is advisory and
the same read quietly succeeds. So the partial file is hashed through the very
handle that holds its lock, never by reopening the path -- reopening passes
every test on Linux and fails every download on Windows. Renaming the locked
file into its final name does work on Windows, which is verified rather than
assumed. Anything else added here must be exercised with
`cargo test --target=x86_64-pc-windows-gnu`, because a native Linux run cannot
see this class of bug at all.

Progress is published as a `core::progress::DownloadPhase` snapshot that a UI
thread can poll. It is a level rather than a queue of events, so only the latest
value is kept -- the opposite choice from `VmEventSink`, where each HCS event is
a distinct fact. Publishing is rate-limited, but a change of phase and the last
value within a phase are never held back. The widget that draws it is separate
work.

### Release resolution

`vmlord-image` also works out *which* image a release means. A `DistroProfile`
is a JSON-backed table of data -- supported releases, two URL templates, the
name of the checksum file, the guest's default user and its admin group -- rather than a trait with one
implementation per distribution, because that is what actually differs between
Ubuntu and Fedora. `resolve_image` validates the release version, reads the
checksum file published beside the image, and returns the image URL together
with the SHA256 the download must produce, in the lowercase hex the downloader
expects.

Releases are addressed by version number rather than codename. The server
answers `/releases/24.04/` with a 302 to `/releases/noble/`, so a table of
codenames would need a line added for every future release and buy nothing. The
version string is checked against a strict shape before it is pasted into a URL:
it is attacker-influenced input, and unchecked it walks the request into another
directory. The architecture is baked into the file name template, since Hyper-V
here is x86_64.

A body that parses as no checksum line at all is reported apart from a body that
parses but does not list the image: the first means the server sent something
else -- typically an HTML error page with status 200 -- and the second means the
distribution publishes no such build. The checksum list is never cached: it is
what says which build is current.

### Reading a qcow2 image

`Qcow2Image` turns a downloaded image back into the disk it holds: a
`Read + Seek` stream where offset zero is the guest's sector zero, the stream
ends at the disk's virtual size, and a hole reads as the zeros the guest would
see. That is the whole interface the VHDX importer needs.

Parsing the format is the `qcow` crate's work (MIT, panda-re/qcow-rs): headers,
L1 and L2 tables, zlib and zstd clusters. Writing that again was never worth it.
Mapping a guest offset onto those tables is ours, and the crate's own `Reader` is
deliberately not used: it opens the backing file named in the header -- any path
the image cares to name, opened on the host -- zero-fills a failed cluster read
instead of reporting it, and panics on several malformed inputs. On a file
fetched over HTTP none of that is acceptable, and the lookup that replaces it is
fifty lines.

The header is vetted before the parser is given the file, in
`image::qcow2::header`. Two things are being defended against. The first is
features: a qcow2 file states what a reader must understand, the spec requires
refusing an image whose incompatible bits one does not know, and the crate
discards the unknown bits while parsing -- so the field is read as the raw 64
bits it is. Exactly one bit is accepted, the one that says the compression type
field is present, and zstd is the only value behind it. Backing files,
encryption, internal snapshots, external data files and extended L2 entries are
each refused by name. The second is arithmetic: every count in the header becomes
an allocation in the parser, and an image claiming four billion L1 entries is a
few bytes to write and gigabytes to open, so the counts, the offsets and the
header extensions are all bounded while the file is still nothing but bytes.
Afterwards the parser's reading of the header is compared against ours, because a
check passed against one reading protects nothing if reads are served against
another.

The size of the disk is settled at the same moment. An image whose virtual size
exceeds the capacity it is headed for -- the VM's `disk_gb` -- is refused on
opening rather than part way through writing a VHDX.

Tests read fixtures written by qemu-img and committed under
`crates/image/tests/fixtures/qcow2` (regenerate with `generate.sh`): a sparse
image whose first cluster is a hole, the same disk in zlib and in zstd clusters
with a different cluster size, an overlay with a backing file, and a legacy
version 1 image. Their guest content is a pattern the tests recompute rather than
store, so a disk that comes back plausible but misaligned still fails. A real
cloud image is read by the `#[ignore]`d test behind `VMLORD_TEST_CLOUD_IMAGE`.

### Importing an image into a VHDX

`platform::import` writes the disk a `Qcow2Image` holds into a new VHDX of the
size the VM will have. It lives in `platform` rather than beside the reader
because there is no API that writes into a VHDX file: the disk is created,
attached to the host, and written to as the `\\.\PhysicalDriveN` Windows
presents it as. `import_image` takes any `Read`, so the reader and the importer
never meet in the type system -- the qcow2 crate stays out of `platform`, and
the importer is testable with a stream of bytes.

That route runs the image past the volume manager, and this is where failure is
silent. Once a disk carries a partition table the volume manager recognises, it
may mount what it finds and take the disk exclusively, after which writes are
accepted and never arrive: a VHDX of the right size, in the right place, a log
full of successes, and a VM that does not boot. AppSandbox met this and left the
note in `tools/iso-patch/ubuntu_vhdx.c:204-208`, where
`IOCTL_DISK_UPDATE_PROPERTIES` is deliberately delayed to the end of the run.

Three things answer it. Nothing in the module calls
`IOCTL_DISK_UPDATE_PROPERTIES` at all, so the volume manager is never asked to
look. The chunk carrying sector zero is held back and written after every other
byte, so the disk does not look like a disk until there is nothing left to
write -- the same defence as AppSandbox's, one step earlier. And every chunk
written is read back off the drive and matched against a digest taken on the way
out, because this failure is otherwise indistinguishable from success. The
read-back is what `FILE_FLAG_NO_BUFFERING` buys beyond throughput: a cached read
would answer out of the same memory the write went into and agree with it
whether or not the disk ever saw it.

Holes are skipped rather than written. The reader hands out zeros for every
cluster the image never allocated, and writing them back would allocate the
whole disk -- a 600 MB image would land as a 64 GB file, which is the whole
point of a dynamic VHDX gone. Everything moves in 1 MiB chunks: a multiple of
every qcow2 cluster size in use and of the 4096-byte alignment an unbuffered
handle demands, which is a multiple of both sector sizes Hyper-V presents, so
the disk never has to be asked which one it has. Only the last chunk of an image
is ever short, and it is padded with zeros the disk already reads as zeros.

The disk is made the size the VM will have, not the size of the image, so the
image's backup GPT header ends up short of the end of the disk. Moving it and
growing the filesystem is the next subtask's business.

A failed import leaves nothing behind: the disk is detached and the VHDX
removed, because a half-written disk that looks complete is the failure the
whole module is written against.

The arithmetic -- filling a chunk from a reader that serves one cluster at a
time, recognising a hole, padding a short tail, digesting a chunk -- is tested
on its own, and the copy is tested against an in-memory disk that can be told to
accept writes and drop them, which is the production failure reproduced exactly.
What cannot be tested without a host is `#[ignore]`d in
`crates/platform/tests/import.rs`: a synthetic image with a hole in the middle,
whose VHDX must stay far smaller than the disk it presents, and a real cloud
image behind `VMLORD_TEST_CLOUD_IMAGE`. All of them need an elevated process,
because `AttachVirtualDisk` fails with `ERROR_PRIVILEGE_NOT_HELD` without one.

### GPU: desired mode and runtime status

The sections below say why GPU-PV is built the way it is. What a host, a guest
and a payload must *be* is `docs/gpu-pv-compatibility.md`, and what to do about
a status that is not what it should be is `docs/gpu-pv-troubleshooting.md`;
both are written for whoever is using VMLord rather than changing it, and the
stable `GpuStatusCode` values are what they are indexed by.

What a VM asks of the host's GPU and what GPU-PV is actually doing for it are
two types, not one field. `GpuMode` is desired state: chosen in the create or
edit form, stored with the VM, and unchanged by whatever a start makes of it --
a `Mirror` VM whose adapters could not be attached is still a `Mirror` VM.
`VmGpuStatus` is runtime state: derived per refresh, never stored, and thrown
away when the next refresh describes the next moment. GPU is applied best
effort and never blocks a start, so a VM routinely runs with less GPU than it
asked for, and a single field would have to lie about one of the two.

Between them sits `VmGpuFacts`, which is what a backend reports on `VmSummary`:
what the host did when it tried to attach adapters (`GpuAssignment`: complete,
partial with a reason, or failed), what the guest agent last said about the
device it was given (`GuestGpuReport`: the device is present, it renders, or it
cannot be used), and when the newest of those was observed. Facts only -- a
backend never names a state. `vmlord_app::gpu::derive_status` turns them into a
`VmGpuStatus`, and `WorkspaceApp` derives one per listed VM on every refresh
and answers for it by name. Deriving once per refresh rather than per read is
what lets a status keep the time its facts were taken, under a UI that redraws
sixty times a second.

A `VmGpuStatus` says three things at three levels of detail, so that the coarse
one does not have to grow a variant per reason:

* `GpuState` is what a person takes in at a glance -- `Disabled`,
  `WaitingForGuest`, `Assigned`, `GuestReady`, `Degraded`, `Failed`. `Disabled`
  covers both "this VM does not use GPU-PV" and "it is not running, so nothing
  is attached"; neither is a failure, and a stopped `Mirror` VM must not be
  painted like one whose assignment was rejected. `Degraded` is the state that
  makes best-effort GPU legible: it works, with less than the mode asked for.
* `GpuStage` says which step the reading came from: `Idle`, `Assignment` (the
  host choosing adapters, attaching them and exporting their drivers) or
  `Guest` (the guest bringing the GPU up).
* `GpuStatusCode` says exactly why, and is stable: it is what logs, tests and
  future automation match on, while the message beside it carries the
  host-specific detail and is free to be reworded.

The UI only displays this. It shows the desired mode and the runtime status as
separate rows, because a VM configured for `Mirror` whose guest has not come up
yet is not a VM without a GPU.

### GPU: the shape of a start

Every start of a VM that asks for a GPU runs the same six steps, in
`VmStartPipeline`, before and around the start it already performed:

1. **Staging.** The payload for the VM's guest is staged into the VM's own
   `gpu-payload` directory. A failure here removes exactly one share and
   nothing else; the shipped catalog is empty until a payload is published, so
   this is the path every host takes today.
2. **Exports.** The host's partition adapters are enumerated and turned into
   Plan9 shares. VM access is asked for on each and the answer is not acted on:
   these paths are under `System32`, where the grant is always refused, and a
   Plan9 share does not need one.
3. **Configuration.** The shares are written into the configuration the compute
   system is built from, in memory. They are never written back to the stored
   `config.json`: they name this host's paths and this run's staging directory,
   and a compute system's Plan9 section is fixed for the lifetime of a boot
   anyway.
4. **Start.** The compute system is prepared, the console is opened, and the
   system runs. Unchanged by GPU.
5. **Assignment.** `HcsModifyComputeSystem` attaches the adapters the mode asks
   for, once, against the running system. A failure is recorded and changes
   nothing else: the VM is running, and GPU never decides that.
6. **Manifest.** What the guest is to mount is left where the agent listener
   reads it, and every session of that run is offered the same manifest.

None of this is retried -- not staging, not assignment, not a partial outcome.
A second attempt at a modify HCS refused is a second refusal, and a loop around
one is how a VM spends its life asking for a GPU it will not get.

The steps live in the start pipeline rather than in the repository, because the
build cycle starts VMs through the same pipeline: a VM created with a GPU gets
one on its first boot as well as on every later start.

Staging unpacks an archive on a cold cache and hashes the staged tree on every
start, so a start became a thread of its own -- `StartRegistry`, modelled on the
`BuildRegistry` that background creation already uses. `start_vm` refuses
synchronously only what is cheap and certain (an unknown VM, a build or a start
already in flight) and hands the rest to the thread; a VM with a start in flight
lists as `VmState::Starting`, and what the thread produced -- the console
session and the compute-system handle -- is taken over on the next refresh.

### GPU: what a run knows about itself

`GpuRuns` is one in-memory map, keyed by VM id, holding what has been observed
about each running VM's GPU and the manifest its guest is to be offered. Three
threads meet there -- the one starting the VM, the one serving its agent, and
the refresh that lists VMs -- and one entry with one lifetime is what keeps
them from disagreeing. Every point that ends a run forgets its entry: stop,
force stop, delete, the HCS release event, and process shutdown.

Nothing is persisted. A `VmGpuStatus` describes a moment, and facts recorded by
a process that is gone are confirmed by nothing -- the VM may have crashed, the
guest may have lost the device. The staged directories do survive, so a process
that reopens a still-running compute system reconstructs its mount manifest
from those directories before accepting the first agent session. Re-observing
the facts is cheap: a reconnecting agent runs the same attach, recipe and probe
exchange within seconds.

What cannot be re-observed is the assignment, which happens once, right after
the system starts. A VM reclaimed from a previous process therefore reports
`GpuAssignment::Unknown` and the stable code `gpu-assignment-unknown` -- "this
VM was started before VMLord, so what is attached to it is not known" -- rather
than `gpu-assignment-pending`, which would be a lie about the stage. It lasts
until the guest's first report.

### GPU: where partial comes from

HCS reports nothing about partiality: it either accepted the update or it did
not. Partiality is therefore derived from export coverage, which is its only
honest source. With N adapters enumerated and M driver-package shares built,
`M < N` is a partial assignment -- some adapters are attached but the guest
cannot mount their drivers -- and a payload that could not be staged is partial
too, with its own wording. Full coverage is complete, and a host with no
partition adapter at all is a failure rather than something partly done.

A VM's guest triple -- distribution, release, architecture -- is recorded in its
mapping at creation, from the cloud image it was built from. A VM from
installation media has none, because VMLord promises nothing about the system
inside it, and gets the WSL and driver shares without a payload. The kernel is
deliberately not part of the key: the host chooses a payload before the guest
has booted, and the guest's own recipe treats the kernel as soft because DKMS
rebuilds the module for whatever kernel is running.

### GPU: what the host can do

Before any VM is offered a GPU, `HostGpuCapabilities` answers what this host is
capable of, on two independent axes. `assignment` is whether a GPU partition
can be offered to a VM at all; `linux_payload` is whether the Linux userspace a
guest needs is staged on the host. They are separate because a host with a
partition adapter but no WSL can assign a GPU that a Linux guest will not
render on -- a warning, not a refusal, and one field could not say both. Each
axis is `Available` or `Unavailable` with a `GpuFailure`, so a reason here
reads the way a per-VM failure does.

`vmlord_platform::gpu_enumerate` walks the GPU Partition Adapter device
interface class through SetupAPI and the Configuration Manager --
`SetupDiGetClassDevsW`, `SetupDiEnumDeviceInterfaces`,
`SetupDiGetDeviceInterfaceDetailW`, `SetupDiGetDevicePropertyW` and
`CM_Get_Device_IDW` -- and resolves each adapter's driver package with
`SetupGetInfDriverStoreLocationW`. No WMI and no spawned process is involved:
the properties that AppSandbox read through `Win32_PNPSignedDriver` and a
registry key come from `DEVPKEY_Device_DeviceDesc`, `DEVPKEY_Device_Service`
and `DEVPKEY_Device_DriverInfPath` instead. Each adapter is reported with its
name, instance id, interface path, driver package directory and kernel service,
and an adapter whose package could not be located is still reported: it is a
real device that simply has nothing to hand a guest.

`vmlord_platform::gpu_discovery` turns that, a check of both halves of the
Linux userspace and an HCS service query into the two verdicts. Both halves,
because a host with only `System32\lxss\lib` looks installed and cannot
render, and the failure names the half that is missing rather than saying
"install WSL" to someone who has. Nothing is cached -- the enumeration
is cheap, and a driver update or a WSL install changes the answer with nothing
to invalidate a cache on. A dead Host Compute Service outranks the adapter
question, since reporting "no adapters" when the service is not answering would
blame the wrong thing.

An adapter, a resolved driver package and a live HCS service are a
**precondition** for GPU-PV, never a guarantee of it. Assignment is proven only
by assigning, which needs a running compute system, and this report is read
before there is one.

`app` reads it through `VmRepository::host_gpu_capabilities`, never by calling
`vmlord_platform` directly, and the UI only displays what it is handed. The
method is defaulted on the trait and its default is an error rather than an
empty report: a backend that cannot inspect the host does not thereby know the
host has nothing, and "this backend cannot tell you" is a different answer from
"this host cannot do it".

`vmlord_platform::gpu_assignment` is the narrow boundary that proves
assignment on a running compute system. It maps `Default` and `Mirror` to an
`HcsModifyComputeSystem` `Update` of `VirtualMachine/ComputeTopology/Gpu` and
waits for HCS to finish it. The settings carry `AllowVendorExtension`, which is
what lets HCS attach a vendor's own partition extension: without it a host with
an NVIDIA adapter refuses the update with HRESULT 0xC0350008 and an empty result
detail. The service is safe: its only native call is behind
`HcsSystem::modify`, which retains the failed HRESULT and the raw HCS result
detail rather than guessing at a version-specific error schema. It returns a
`GpuFailure`, so its caller records an unsuccessful assignment without stopping
the VM or retrying it. `gpu_assignment::assign_to_system` names the compute
system by id rather than taking an open handle, so a start can attach a GPU
without holding one across the steps before it -- and so the step can be
substituted in the tests of a start, which have no compute system to open.

### GPU: what is exported to a guest

A GPU partition is useless to a Linux guest without the host's driver package
and the WSL Linux userspace beside it, and the way in is a Plan9 share.
`vmlord_platform::gpu_exports` decides what may be shared, and the answer is
three system directories plus one exact per-VM staging directory:
`System32\DriverStore\FileRepository`, for the driver packages behind the
host's adapters, and the two directories the Linux userspace is split across.

That userspace is one directory only on a host whose WSL is the inbox one.
Where WSL comes from the Store or the standalone installer, `System32\lxss\lib`
holds what the GPU driver puts there -- the vendor's libraries, and on the
first real host nothing else -- while the Microsoft half the renderer actually
links against, `libd3d12.so`, `libd3d12core.so` and `libdxcore.so`, is
installed beside the package as `Program Files\WSL\lib`. A guest given only
the first half has vendor libraries with nothing to drive them, which is
exactly what the first real host produced. So there are two roots and two
roles, and the second is checked against `Program Files` for the same reason
the others are checked against `System32`. `Program Files` is asked of the
shell rather than read from `%ProgramFiles%`: the environment variable is
inherited and this decides what a VM is shown.

Every candidate is canonicalized before it is judged -- opened as a directory
handle, without `FILE_FLAG_OPEN_REPARSE_POINT`, and resolved with
`GetFinalPathNameByHandleW`. That is what collapses `..` and what turns a
junction into its target, so a link leading out of an allowed root fails the
root check instead of quietly exporting whatever it points at. The root check
is component-wise and case-insensitive, because `...\FileRepositoryEvil` passes
a string prefix and is a different directory. A root that itself resolves
outside `System32` admits nothing at all. What is exported afterwards is the
canonical path, not the one discovery reported, and the set is deduplicated by
it: two adapters from one vendor usually share a `FileRepository` folder.

A candidate that fails any of this is dropped with a log line and the rest are
still offered, and a set with nothing in it is `None` rather than an error: GPU
is applied best effort and never blocks a start. `HcsGrantVmAccess` runs only
after a path has passed -- a grant before the check is what makes the check
decorative -- but its answer is not acted on. Every one of these paths lives
under `System32`, whose DACLs belong to TrustedInstaller, so the grant is
refused there however elevated VMLord is, and a Plan9 share does not need it:
the share is served by the host's own Plan9 server rather than opened by the
VM's security principal, which is what makes it different from the VHDX files a
start grants separately. The AppSandbox backend asks for the same grants on the
same paths and ignores the answer. Dropping a share over the refusal is what
this used to do, and it removed the guest's entire GPU userspace on every real
host.

What the guest is told is a `GpuShareManifest`: for each share, a name and a
role -- `WslLib`, `WslD3d12`, or `DriverPackage` with the package's folder
name. Never a
host path. Where a share is mounted is the guest's decision, taken from its own
allowlist, so the host cannot dictate a path into a guest filesystem and the
host's topology does not travel. Share names are `vmlord.gpu.wsl-lib` and
`vmlord.gpu.drv.<package>`, restricted to `[A-Za-z0-9._-]`, because a name ends
up both in the HCS document and in a comma-separated `mount` option string.

`hcs_config::apply_plan9_shares` writes the set into the stored configuration
under `Devices/Plan9`, each share carrying port 50001 and the read-only flag;
`remove_plan9_shares` takes the section away again for a VM whose GPU was
switched off. Read-only is therefore stated twice and independently: by the
share's flag on the host and by the guest's own `MS_RDONLY` mount. The set is
computed once per start and written before the compute system is prepared,
which is what makes it immutable for the lifetime of a boot -- changing a GPU
mode takes a full VM restart.

### GPU: guest payload

`vmlord-gpu-payload` is portable: it validates an exact guest target tuple,
verifies content-addressed ZIP archives, and exposes only an opaque
`ReadyGpuPayload` after every cache hit has been rehashed. It has no network in
it and no catalog compiled into it. Release tooling creates sorted ZIP content
and deterministic provenance without making the archive digest
self-referential.

A payload reaches a host as a pair of files beside `vmlord.exe`, both named by
the payload's own ID:

```
gpu-payload/ubuntu-26.04-amd64-7.0.0-28-v2.json
gpu-payload/ubuntu-26.04-amd64-7.0.0-28-v2.zip
display-payload/display-ubuntu-24.04-amd64-0.1.0.json
display-payload/display-ubuntu-24.04-amd64-0.1.0.zip
```

The display pair travels the same way and is staged by
`cargo dist --display-payload`; the two kinds keep separate directories because
each catalog reads its own.

`cargo dist --gpu-payload <directory>` takes what `pack` wrote -- `payload.zip`
beside `catalog-entry.json` -- re-reads the entry through
`CatalogEntry::from_json`, hashes the archive against the `archive_sha256` that
entry claims, and copies both files into place. `release.rs` states the layout
once, as `local_archive_path` and `local_entry_path`, for the build tool
placing the files and the application reading them alike.

`PayloadCatalog::from_release_directory` assembles the catalog at runtime from
that directory, which is derived from `current_exe` and from nothing else --
never from a user directory and never from configuration. Each `*.json` is one
entry document with a `schema_version` of its own, currently `2`, because an
entry is now a release artifact rather than a fragment waiting to be pasted
into a larger document. A file must be named for the `payload_id` it contains
and must have its archive beside it.

**A missing catalog is an empty catalog.** No `gpu-payload` directory, an empty
one, or one that cannot be listed: the catalog has no entries, selection
answers `NoPayloadForGuest`, and the VM starts without GPU support. A build
without a payload is a build without GPU, and GPU assignment is best effort. A
file that *is* there and is wrong -- unreadable JSON, a failed validation, a
name that is not its `payload_id`, a missing archive -- fails the catalog,
because that is a broken release and a silent absence is the worst way to learn
of one. An archive no entry claims is ignored.

`PrepareRequest::archive` is required: there is no second source, and a file
that is not there is an error. Verification is the archive's `archive_sha256`,
the expansion limits, and the `payload.json` and `sources.json` cross-check
against the entry's provenance. The archive's length is measured rather than
claimed -- a digest pins a length as surely as it pins content, so the entry
does not state one, and the measured length is what caps the sum of the
members' compressed sizes.

The trust model changed deliberately with this layout. An entry is no longer
trusted for being compiled into the executable; it is trusted because whoever
can write into the installation directory can equally replace `vmlord.exe`
itself. The boundary does not move -- it becomes visible.

Ready content is materialized below a VM's exact `gpu-payload` child as
`generations/<digest>` followed by `ready/<digest>.json`. The third logical
share, `GpuPayload` / `vmlord.gpu.payload`, exports only that canonical child;
it never broadens either System32 root or exposes the cache. The guest mounts
that share at `/opt/vmlord/gpu-payload`; what the guest makes of it is the
recipe below, and task 98 owns lifecycle and UI orchestration.

`platform::gpu_staging` is what fills that child: given the executable's
directory, the shared cache root, a VM directory and the guest triple recorded
with the VM, it selects the entry, prepares the generation and stages it into
`layout::gpu_payload_staging_directory`. `gpu_prepare` calls it as the first
step of every start of a VM that asks for a GPU.

What is exported is the **generation** staging produced, not the staging root:
the root also holds the `ready` markers and lock files that make a swap atomic,
while the guest reads `sources.json` at the root of the share it mounts.
Offering the root gives a guest a directory it finds no payload in, which is
what the first real host reported as nine skipped recipe stages. The export is
accepted only if it canonicalizes to something strictly inside this VM's
staging root.

The catalog is selected by distribution, release and architecture, and never by
kernel: the host chooses a payload before the guest that will run it has
booted, so the exact `kernel_release` cannot be known. Where a triple has
several entries the newest proven kernel wins. This is safe because the guest
does the same thing from the other side -- its recipe treats distribution,
release and architecture as the hard gate and the kernel as soft, since DKMS
builds against the running kernel's headers.

### GPU: the guest's Ubuntu recipe

A mounted payload is a directory of sources, and what a guest actually needs is
`/dev/dxg`. The recipe is what turns one into the other: the host asks for it
once per session, right after the attach report, and the agent answers with a
list of stages. `ApplyGpuRecipeRequest` is empty on purpose -- everything the
guest needs to decide is in the guest or in the payload it was told to mount
one message earlier, and a field here would be the host dictating something it
cannot know better. The schema gains messages and enum values only, so the
revision moved to **1.3** with the kernel stages and to **1.4** with the
userspace ones below.

`ApplyGpuRecipeResponse` is stages and never a verdict, for the same reason
`VmGpuFacts` is not a `VmGpuStatus`: "the module built and `/dev/dxg` never
appeared" and "the headers would not install" are one word apart in a summary
and are different problems. Every step is reported, including the ones that
never ran -- a report that stopped at the failure would leave the host guessing
whether the rest was skipped or the agent hung up. The userspace stages add
values to `GpuRecipeStep` rather than messages of their own; the probe that
follows them is a message of its own, because it answers a different question
-- what was done, against what works.

The build dependencies -- DKMS, a compiler and the running kernel's headers,
whatever the guest's own manager calls them -- come from the guest's own
package manager, not from the payload. AppSandbox did the opposite, staging an
apt closure per (release, kernel) pair into the rootfs, and it cost an apt
resolver on a Windows host, hundreds of megabytes per kernel, and a payload
that goes stale the moment the guest upgrades its kernel. VMLord provisions its Ubuntu guests through
cloud-init over NAT, so a guest that cannot reach `archive.ubuntu.com` is
already a guest that did not finish provisioning. The consequence is stated
rather than hidden: no network means a failed `BUILD_DEPENDENCIES` stage and a
`Degraded` GPU, never a VM that fails to start.

Distribution, release and architecture are the hard gate; the kernel is not.
DKMS builds against the headers of the *running* kernel, so the payload's
`kernel_release` records what the recipe was proven on rather than what it
requires -- requiring it would mean the unattended kernel upgrade Ubuntu
performs on its own kills GPU-PV until someone repacks a payload on the host.
DKMS's own `AUTOINSTALL=yes` is what carries the module across that upgrade
with VMLord not involved at all.

The stages run in order, and the first failure ends the run:
`DISTRIBUTION` (a guest with no recipe is skipped, with the reason),
`PAYLOAD` (the mount, its `sources.json` target and the `dkms.conf` that names
the package), `BUILD_DEPENDENCIES`, `MODULE_SOURCE` (copied to `/usr/src`,
because the payload is read-only over 9p and DKMS writes beside its sources),
`MODULE_BUILD`, `MODULE_LOAD` (`/etc/modules-load.d/vmlord-dxgkrnl.conf` and
`modprobe`, because a module loaded only by hand is gone after the next reboot)
and `DEVICE`. A guest that already has the module loaded and a `/dev/dxg` that
opens short-circuits the three build stages, which is what makes every start
after the first cost nothing and need no network.

The guest's package manager, `dkms` and `modprobe` are the three programs the
agent runs beside `ldconfig`, all distribution-owned operations with no library
form. Each runs through one helper with a wall-clock budget -- 300 s for an
install, 900 s for a build, 30 s for the rest -- in a process group of its own, so a budget that
runs out takes the whole tree with it rather than waiting on a child holding
the pipe. Between stages the recipe checks the shutdown flag and abandons the
rest as skipped: systemd is holding the guest open for this process to exit.
The whole thing runs inline in the session, where the attach already does; the
host tolerates the silence, because its read timeout is an `Idle` it keeps
waiting through, and a background thread would be two conversations on one
socket for a report that was asked for.

The userspace half is three more stages of the same report. `USERSPACE` honours
the payload's own `mesa_policy`, which it reads itself rather than through the
payload stage: a policy from a payload built newer than the agent must fail the
stage it belongs to, not one after which a kernel module would have built.
Under `distro` it installs the `Mesa` package set -- `libgl1-mesa-dri`,
`mesa-vulkan-drivers` and `libvulkan1` under apt, and whatever the guest's own
manager calls them otherwise -- and only when the d3d12 DRI module and the
Vulkan loader are not already there; the distributions in the matrix do not
build Mesa with
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

What the recipe expects inside a payload is therefore fixed: `sources.json`
beside `licenses/`, `content/dxgkrnl/` holding `dkms.conf`, `Kbuild`, the
out-of-tree compat header and the sources vendored from
microsoft/WSL2-Linux-Kernel, and, when the policy is `bundled`,
`content/mesa/` holding the prefix that is staged at `/opt/vmlord/wsl-mesa`.

### GPU: the guest probe

A recipe says what was done, and every one of its stages can report `OK` on a
guest where nothing draws. The probe is the other question: the host asks for
it once per session, right after the recipe report, and the guest answers with
one verdict and a list of checks. The schema gains `ProbeGpuRequest` and
`ProbeGpuResponse`, so the revision moved to **1.5**.

The verdict is the guest's, and it is the one thing on this message the host
does not re-derive: the guest is the only side that saw the output of the
programs it ran. `RENDERS` needs one hardware renderer from either API and not
both -- Ubuntu does not build Mesa with `microsoft-experimental`, so under the
`distro` policy Vulkan is lavapipe and GL is the only hardware path such a
guest has. `DEVICE_ONLY` is a `/dev/dxg` that opens with nothing above it, and
`NO_DEVICE` is the one check that ends the probe early.

Hardware is decided by a deny list -- `llvmpipe`, `softpipe`, `swrast`,
`lavapipe`, `SwiftShader` -- and never an allow list of the drivers this build
knows: an allow list reports "no hardware renderer" on the first stack nobody
wrote code against, and a new software rasteriser counting as hardware once is
the milder failure. Vulkan adds one fact of its own: a `deviceType` of
`PHYSICAL_DEVICE_TYPE_CPU` is software whatever the device calls itself.

The operation on the hardware is an external program, because the agent is a
static musl binary that can neither link nor `dlopen` `libEGL`. The programs
are Mesa's and Khronos's own -- `eglinfo` from `mesa-utils` and
`vulkaninfo --summary` from `vulkan-tools` -- installed present-first by the
`TOOLS` check and run through `/etc/profile.d/vmlord-gpu.sh`, the same file a
person gets over SSH: setting those variables again inside the agent would be
a second copy of the recipe's decision, and running through the file is what
proves the file. Vendor tools are quoted when the mounted WSL userspace
carries one and never decide anything, which is the difference between a probe
that is vendor-neutral and one that only knows one vendor.

The checks are `DEVICE`, `KERNEL_MODULE`, `LIBRARIES` (including the
`libd3d12.so` and `libdxcore.so` that `d3d12_dri.so` opens out of the host's
mounted userspace), `TOOLS`, `OPENGL`, `VULKAN` and `VENDOR`. Only a failed
`DEVICE` ends the run: a missing library is a fact worth reporting and never a
veto over a guest that turns out to draw anyway. The host logs the verdict and
every check and keeps nothing -- the next session probes again, and deriving a
`VmGpuFacts` from a verdict is the application layer's work.

### GPU: what only a host can answer

Everything the start pipeline decides on its own is tested beside it with the
GPU steps substituted -- that a failed attachment never fails a start, that a
GPU is attached exactly once and never retried, that a host with nothing to
hand over is not asked to attach anything, that a start that failed still
leaves what it found out. None of it needs a GPU.

What no fake can answer is whether a guest ends up rendering, and that is
`#[ignore]`d in `crates/platform/tests/gpu_e2e.rs`, kept apart from
`hyperv.rs` because it asks a different question. Its subjects are the three
modes told apart by adapter coverage, a restart described by its own run rather
than the one before it, a VM reclaimed by a second process reporting
`GpuAssignment::Unknown`, a mode change applying at the next start rather than
merely being stored, a host with no adapter and a host with no payload each
leaving a running VM with the right half of the reason recorded, and a deleted
VM leaving no staged payload behind. Driver drift is two-phase and manual: the
exports name the DriverStore folder of the driver installed at the time of the
start, so only an actual driver update between the phases can produce it.

These tests need what the code needs: an elevated host, a partition adapter,
and the payload pair beside the *test binary*, since the catalog is read from
`current_exe` and under `cargo test` that is `deps\`. A host missing any of
those is what two of the tests are about, and each of them says so rather than
passing quietly.

### VM update contract

The edit workflow follows these rules:

* A VM can be edited in any state; a running VM applies the change on its next
  start, and the application layer says so.
* RAM must be at least 512 MiB and aligned to 2 MiB steps.
* CPU core count must be at least 1.
* The GPU mode may only be changed while the VM is stopped, and only the mode:
  the refusal fires when the requested mode differs from the stored one under a
  VM that is not stopped, so an edit of RAM or CPU on a running VM is unaffected.
  The mode is applied while the compute system is prepared and started, so a
  change under a live VM would leave a stored mode that does not describe the
  GPU the guest actually has. The UI disables the control and gives the reason,
  rather than offering a change the backend will refuse.
* Network mode accepts `None` and `Nat`; `External` and `Internal` are rejected
  with a message naming the task that will add them.
* The system disk may only grow, and only while the VM is stopped. The size is
  the one part of an edit that changes a file rather than a document: the VHDX
  is resized there and then, through `ResizeVirtualDisk`, so nothing about it
  waits for the next start. Shrinking is refused because it would cut the disk
  off underneath a filesystem that still believes in the old size, and a
  running VM is refused because Hyper-V holds its VHDX open exclusively. The
  new size is recorded in the mapping, which is what the summary reads back
  once the VM is running and its disk can no longer be opened. The guest grows
  its root filesystem onto the new space on its next boot, through the
  `growpart`/`resize_rootfs` cloud-init already seeds every cloud-image VM
  with; a guest installed from local media is left to its own tools.
  The UI opens the field on the size the VM has, refuses to be dragged below
  it, and is disabled with the reason while the VM is not stopped.
* The VM name is treated as the guest hostname and also requires recreating the
  VM to change safely from VMLord.

### VM creation contract

A VM's system comes from one of two sources, and they are different in kind:

* `VmSource::LocalMedia` is installation media. The system is installed by
  hand, so VMLord promises nothing about the user inside it.
* `VmSource::CloudImage` is a distribution's cloud image, provisioned by
  cloud-init from a seed VMLord writes. It carries the provisioning contract:
  user name, optional password, SSH access, locale, keyboard layout and
  timezone.

Provisioning lives inside the cloud variant rather than beside it, so "a local
medium with a password" is a state that cannot be spelled rather than one that
has to be rejected at run time. `core::provisioning` owns the types and their
validation, including the user-name and VM-name rules the UI used to hold and
the `GuestDefaults` a create form starts from; `core::distro`
owns `DistroProfile`, the table of where a distribution publishes its images
and what the guest inside them looks like -- including the admin group and the
`SshDaemon` description below.

A password travels as `Password`, whose `Debug` prints `<redacted>` and which
has no `Display`: until it is hashed, the plaintext sits inside a request that
several call sites log with `{:?}`.

### The guest password

`platform::password_hash` turns a `Password` into a `$6$` SHA-512-crypt entry on
the host. That is where the plaintext's journey ends: the seed, `config.json`,
the VM metadata and the log see the hash or nothing at all, and the tests in
`hcs_config` and `create` assert that neither the plaintext nor a `$6$` marker
reaches the compute system's configuration.

The digest comes from RustCrypto's `sha-crypt` rather than from a second
translation of the specification -- AppSandbox wrote its own in C
(`src/backend_win/disk_util.c:2235`), and every step of that algorithm is a
chance to be subtly wrong. What VMLord does keep from AppSandbox is the source
of the salt: twelve bytes from `BCryptGenRandom`, which encode to the sixteen
crypt-base64 characters SHA-512-crypt reads. Ninety-six bits, each byte used
once -- AppSandbox aimed for the same and cycled its twelve bytes over sixteen
characters, so several of them repeated. The `sha-crypt` `getrandom` feature is
off; the salt has one source. The entry names its cost explicitly
(`$6$rounds=5000$...`), which is the specification's default and changes nothing
about the digest.

The module's unit tests pin the algorithm against the specification's published
`$6$` vectors, not merely against itself: a hash that agrees only with this
implementation would pass every other test and still be rejected by the guest.

### The SSH contract

`core::ssh` owns what VMLord knows about logging into a guest. It used to be a
number beside a VM -- `VmSummary::ssh_port: Option<u32>` -- where a missing port
meant "SSH is off", "the VM is not running yet" and "this backend cannot answer"
at once, and every reader picked one of the three.

Four types replace it:

* `SshAuthentication` is `VmlordKey` or `Password`, and there is no third. The
  user's own keys and any running agent are deliberately absent: a login that
  succeeded through some other credential is a login VMLord cannot reproduce.
  Key mode does not fall back to a password either.
* `SshPort` wraps a `NonZeroU16`, so `1..=65535` is the type rather than a check
  at every use -- port 0 means "any free port" to a listener and nothing at all
  to a client. It serialises as a plain number, and a stored `0` fails to parse.
* `SshConfig` is what a VM's creation settled: user name, port and mode. No
  address, because the guest takes a new one from HNS on every start; no
  password, because a password on disk is a password leaked -- `Password` the
  variant records only that a person will type one.
* `SshAvailability` is what a listed VM has: `Disabled`, or `Enabled` with the
  configuration. A capability rather than a number, so the SSH button and the
  detail row read the same fact.
* `SshEndpoint` is one running guest as a client would have to be told about it:
  a configuration plus an address plus the VM's id. `SshEndpoint::new` is the
  only way to build one and validates the configuration first, so metadata
  edited by hand is refused while it is still data rather than after a process
  has been handed its arguments. The id -- not the name, not the address -- is
  the guest's `HostKeyAlias`: both of the others change over a VM's life, and a
  host key filed under either would be lost on a rename or matched against a
  different guest that inherited the address.

The port a guest listens on is chosen once, when the VM is created, and it lives
inside `SshAccess::Enabled { deploy_key, port }` rather than beside it: a port
for a guest that runs no daemon cannot be spelled, and neither can one for
installation media, since provisioning lives inside `VmSource::CloudImage`. The
create form holds it as a plain `u16` -- a field being edited passes through
values nobody means, 0 among them, on the way from 22 to 2222 -- and turns it
into an `SshPort` on submit, so a zero typed past the widget's `1..=65535` clamp
is refused by the domain rather than by the dialog. There is no list of
forbidden ports: a port some other service on the host happens to use is still
one a guest can listen on. Changing the port of a VM that already exists is a
different act from choosing one -- it reconfigures an installed guest rather
than describing a guest to be built -- and it is described under "Moving the
SSH port of an installed guest" below.

`Provisioning::ssh_config` is the one place a configuration is derived from what
cloud-init was asked to do, and `VmComputeSystemMapping::ssh` is where it is
kept: `None` there is a VM created with SSH switched off. It takes the port from
the same `SshAccess` the seed is printed from, so what is recorded and what the
guest listens on cannot drift apart. The mapping validates
it on the way in and on the way back out, through the same user-name validator
the create form uses, so a name that reaches an `ssh -l` argument is one Linux
would accept whether it was typed a minute ago or read from an edited document.

Provisioning refuses an SSH server with neither a deployed key nor a password
for the same reason it refuses a VM with neither a password nor SSH: there is no
third credential to fall back on, so the guest would run a daemon nobody can get
through.

### Running the OpenSSH client

`platform::ssh` is the single place a VM becomes a run of `ssh.exe`. It answers
two questions and nothing else: which endpoint a VM currently is -- its stored
`SshConfig` joined to the address HNS gives its endpoint, `None` when the VM has
no SSH access at all -- and what argument vector connects to that endpoint.

The readiness wait and the interactive launcher connect to the same guests with
the same key, the same host-key file and the same rules about what may prompt;
they differ only in what they ask the guest to run, which is one `Option<&str>`.
Two copies of these arguments would drift, and a drift here is not cosmetic: it
is one client learning a host key the other refuses, or one of them falling back
to a credential VMLord did not choose.

Every invocation carries `HostKeyAlias=<vm id>`, the VM's own
`UserKnownHostsFile` under its directory, and
`StrictHostKeyChecking=accept-new`. A key is therefore learned once, filed under
something a rename and a new address cannot move, and a key that changes
afterwards stops the login. Nothing here deletes or rewrites such a key: that is
a decision for a person who has been shown it. Key mode adds `-i`, the VM's own
private key, `IdentitiesOnly=yes` -- no agent key may stand in for it -- and
`BatchMode=yes`, so a key that stopped working is an error rather than a prompt.
Password mode adds `PubkeyAuthentication=no` and
`PreferredAuthentications=keyboard-interactive,password`, and no `BatchMode`,
because the prompt is what password mode is.

Nothing here builds a command *line*. `ssh.exe` is spawned with an argument
vector, so no user name, path or address is ever a substring something a shell,
PowerShell or `cmd` would parse; the user name is `-l user host` rather than
`user@host`, so a name containing an `@` cannot be split in the wrong place. The
values that do land inside a larger string are the `-o` options carrying a path,
and those are quoted and `%`-doubled -- for OpenSSH's own configuration parser,
which splits on white space and expands `%d`-style tokens, not for a shell. A VM
under `C:\Virtual Machines\` therefore keeps its known-hosts file, and one whose
directory contains a `%` points where it says.

### Moving the SSH port of an installed guest

The edit form can change the port of a VM that already exists, and that change
is unlike every other field it carries. RAM, CPU, the GPU mode and the network
mode are documents a later start reads; the SSH port is a file inside the guest.
Nothing on the host can write it -- Hyper-V holds the VHDX open while the VM
runs, and a guest whose file system was written behind its back would not read
the change anyway. So `platform::ssh_port` makes it where the file lives: over
the same OpenSSH client everything else uses, on the port the daemon answers on
*now*.

Four things are therefore refused before anything is attempted, each with its
own reason: a host with no `ssh.exe`, a VM created without SSH, a VM whose guest
VMLord did not provision, and a VM that logs in by password -- nobody is at a
prompt when VMLord runs a command of its own, and key mode is the only
credential it can present unattended. A VM with no address is refused as "start
it first". The UI shows the same four as a disabled field with the reason on
hover, so the refusal arrives before the click rather than after it.

`VmComputeSystemMapping::ssh_daemon` is what makes the change possible at all.
The drop-ins the seed writes come from the image's `DistroProfile`, and by the
time anyone edits a VM that profile is gone: the seed was consumed on the first
boot and nothing else on the host remembers which distribution answered. So the
`SshDaemon` is recorded with the VM at creation, and a move writes the same
files, at the same paths, with the same contents the seed wrote -- a guest
created on a port and a guest moved to one must not be two different
configurations.

It takes two commands rather than one, because the second cuts the branch it
sits on. Writing the drop-ins and running `daemon-reload` is safe and its exit
status means what it says; `set -e` is what makes it mean anything, since
without it the status would be `daemon-reload`'s alone and a drop-in that could
not be written would be reported as a change that was made. Restarting the
daemon is the opposite: on a socket-activated guest the session giving the order
*is* `ssh.service`, so stopping it kills the connection that asked, and
`ssh.exe` then reports a broken session rather than the command's status --
whether or not the guest did as it was told. That half is handed to
`systemd-run --collect --no-block`, which runs it in a transient unit outside
the session's cgroup: a shell backgrounded inside the session would be killed
along with the session and leave a guest with a stopped socket and no daemon at
all. Its exit code is deliberately not read. The question is settled by the only
thing that can settle it -- whether the guest answers on the new port
afterwards, probed for ten seconds.

Which is what the two successful outcomes are. `Applied` means the guest answers
there now; `RestartNeeded` means the drop-ins are in place and the running
daemon has not caught up. Both write the new port into the mapping, because the
guest's own configuration is the authority and everything VMLord does next --
the readiness wait, the `VmSummary` row, the endpoint the SSH button connects to
-- has to name the port the guest is configured for. Only a refused *write* is
an error, and it leaves the stored port exactly where the daemon still answers.
The difference between the two reaches the user as a diagnostic rather than as
an error: there is nothing to undo and nothing to retry, only a VM to restart.

Everything the remote command carries -- drop-in paths, unit names -- comes from
a document on disk and is single-quoted for the guest's shell, and the script as
a whole is quoted again as `sudo -n /bin/sh -c`'s single argument. `-n` rather
than a prompt: a guest whose passwordless `sudo` has been taken away refuses the
change instead of sitting there waiting to be typed at. The host-key file is
untouched by any of this, and deliberately so: `HostKeyAlias` is the VM's id, so
a guest that moved its port is still the same guest, and the key learned on 22
is the key expected on 2222.

### Opening an interactive session

`platform::ssh_terminal` puts a shell on screen. Everything knowable in advance
is established before the window opens: Windows has an OpenSSH client, VMLord
has its session helper, the VM has SSH access at all, its stored configuration
is one anything can connect with, HNS has given it an address, something answers
on the port it was created with, and -- in key mode -- the private key is still
where VMLord keeps it. Each of those is its own failure with its own message,
because each is a different thing for a person to do. What is left to OpenSSH is
what only OpenSSH can decide: the host key, the credential and the transport --
and what it decided is now reported too, which is what the helper below is for.

The port probe is one attempt with a three-second deadline, not the readiness
wait's patient loop: the VM is running and someone is holding a mouse button
down. Between that probe and the session there is a race a guest could lose its
daemon in; it is left alone, because the window is milliseconds wide and the
session that hits it lands in a terminal where OpenSSH explains itself. The
session itself carries no `ConnectTimeout` -- a deadline VMLord invented would
close a person's window mid-handshake -- and asks the guest to run nothing, so
what they get is the guest's own shell.

Two hosts are tried, in this order. Windows Terminal gets `-w new`, a titled
tab, and the helper and its argument vector after a `--`; `-w 0` is not used
here for the reason it is not used for COM1, and `--` is what keeps an `ssh`
option from being read as one of Windows Terminal's own. If that spawn fails
*synchronously* -- no `wt.exe` on the machine, or Windows refusing to start it
-- the fallback is the helper itself with `CREATE_NEW_CONSOLE`, because VMLord
is a windowed process whose console the session cannot inherit.
If both refuse, the failure quotes both. What the fallback does not promise is
anything about a Windows Terminal that *started*: a broken profile or an
unreadable settings file is reported in its own window, and there is nothing
left to fall back from. No `powershell.exe` and no `cmd.exe` appear anywhere on
this path.

A session is not VMLord's. No child handle is kept, nothing is killed when the
repository is dropped, and no registry counts sessions per VM, so a guest may
have as many shells as a person opens. That is the whole difference from the
COM1 console, which owns exactly one reader per VM because two readers on one
pipe would split the guest's output between two windows.

#### Hearing how a session ended

What a terminal hosts is `vmlord-ssh.exe`, not `ssh.exe`. The helper is the
same shape as `vmlord-com1.exe` -- a console program that exists so a window has
something to host, with all of its logic in `platform::ssh_session` -- and it
does one thing: it runs the client with its own standard input, output and error,
waits for it, and writes down how it ended. The session is interactive exactly as
it was before; every prompt still appears in the window, and the helper is
between the person and the guest only in the sense that it is the parent waiting
for the child.

Two facts make this necessary. A terminal owns what it hosts, so on the Windows
Terminal path the client is not VMLord's child and its exit code is not VMLord's
to read; and OpenSSH answers nearly every failure of its own with exit code
`255`, so even the code would not say which failure it was. The text does, which
is why the client is given `-E <vm>\ssh-sessions\<id>.log`. That moves OpenSSH's
own messages out of the window and into a file -- which loses nothing a person
could read, because the window closes with the session -- and gives the helper
something to classify.

`core::classify_session` does the classifying, as a pure function over the exit
code and the tail of that log: a code other than 255 is the remote shell's own
status and means the session happened; 255 is read as a changed host key, a
refused credential or a transport failure, in that order, because a log can say
two of those at once and the host key is the one that means something other than
"try again"; anything else keeps its code and its text rather than being guessed
at. Two outcomes never come from the client -- a helper that could not start it,
and a helper that is simply gone.

The answer crosses back as a small JSON report, `<vm>\ssh-sessions\<id>.json`,
announced by two named events derived from the session's own id: `finished`,
which the helper signals however it leaves, and `alive`, which it creates and
holds -- a named object exists while a handle to it does, so a name that is gone
is a helper that is gone, which is the one thing a report cannot say about a
window someone closed. `platform::ssh_sessions` holds the sessions VMLord is
waiting on, and the refresh tick reaps them beside the COM1 readers: a session
that is over becomes one diagnostic, whose level and wording come from
`session_diagnostic`, and whose report file is deleted as it is read.

File lifetimes are deliberate and dull. The helper creates the session
directory, and deletes its own log once it has taken the tail; VMLord deletes
the report it reported. The only file that can be orphaned is a report written
after the VMLord waiting for it exited, and the next launch into that VM sweeps
the directory of everything no live session is waiting for. The directory goes
with the VM when it is deleted, including a deletion that keeps the disks: it
records logging in, which is the one thing nobody can do any more.

`HcsVmRepository::open_ssh` is the way in. It asks HCS for the VM's state rather
than trusting the list the user clicked in and refuses anything but `Running` --
the network endpoint the guest answers on exists only while its compute system
does.

That refusal is all it answers, because the rest of the launch does not happen
on the caller's thread. Reaching a guest costs an HNS read, a port probe with a
three-second deadline and the start of a terminal host, and the caller is the
UI: a VM that had stopped answering froze the window for the whole probe.
`ssh_launches::SshLaunches` gives every launch a thread of its own, modelled on
`shutdown_workers::ShutdownWorkers` -- which exists for the same symptom -- minus
the part that carries an answer back, because a launch has none to carry. It is
not keyed by VM either: two shells into one guest is an ordinary thing to want,
and a second click while the first is still probing is a second session rather
than a duplicate to refuse. The threads are joined as later launches start and
again when the repository is dropped.

So `open_ssh` answers "a session is being opened", not "a session opened", and
the preflight failure that used to be its return value is now a diagnostic. The
person sees no difference: the UI ignored that return value and read the
diagnostics panel, which is where both outcomes have always gone.

### Offering a session

`WorkspaceApp::open_ssh` is the only thing the desktop shell calls, and it
collects the backend's diagnostics on both outcomes -- which is what makes it
different from the other actions beside it. Its own line says "Opening an SSH
session for VM …", in that tense deliberately: what it heard back is that the
request was accepted, and what became of it arrives moments later from the
thread that found out.

A refusal reaches the log unchanged: "Failed to open SSH session for VM …"
followed by which Windows feature is missing, which port did not answer, which
key is gone. A session that *opened* leaves the command it opened with:
`SSH session for VM "dev": C:\Windows\System32\OpenSSH\ssh.exe -o
HostKeyAlias="…" … -p 22 -l dev 172.30.0.5`. The launcher returns the
`SshInvocation` it spawned and the repository logs
`SshInvocation::command_line`, so what is shown is the argument vector that ran
rather than a reconstruction of it -- tokens holding white space are quoted for
reading, and the `-o` values keep the quotes OpenSSH's own parser needs.

Both exist for the same reason: what was asked of `ssh.exe` -- which key, which
known-hosts file, which port -- is knowable only at the moment of the spawn, and
it is the first thing anyone needs when a guest refuses a login. The reported
line is the client's own, not the helper's around it: the helper's paths and
event names say nothing about the login. What came of it arrives later, from the
helper, as the diagnostic described above. `command_line` is for reading, not for re-running: nothing
on this path goes through a shell, and this is the only place these arguments
ever become a single string.

The UI decides only what is worth offering, from the summary it drew the list
from. `ssh_offer` answers three things rather than a boolean, because "no
button" and "a button that cannot be pressed yet" are different things to see:

* `Absent` -- the VM was created without SSH, or from installation media, so no
  action appears at all.
* `Waiting(reason)` -- SSH is configured, but the VM is being built, is not
  running, or is running without an address on the VMLord network. The button
  stays on screen and its tooltip names what it waits for.
* `Ready` -- running, addressed, and worth pressing.

That summary is a refresh old, which is exactly why the check is repeated in the
repository against HCS when the button is pressed. The two are not redundant:
one decides what to draw, the other decides what to do.

The button reads "Open SSH" and names no terminal host, in the UI and in
`VmAction`'s own label: which of the two hosts ends up showing the session is
settled when it is launched, and a machine where the fallback answers would make
the other name wrong.

The details panel states the endpoint itself -- `user@address:port (key login)`
-- once the guest has an address, and the configuration without one before that,
saying that the address appears when the VM is running. A remembered address
would be a guess: HNS hands out a new one on every start.

### The VM's SSH key pair

Every VM gets its own ed25519 pair rather than sharing one. AppSandbox kept a
single key under `%ProgramData%\AppSandbox\ssh\id_appsandbox`, where the
compromise of one sandbox reached every other one.

`vmlord-keys` generates the pair and serialises it: an OpenSSH PEM document for
the private half, one `authorized_keys` line commented `vmlord@<vm>` for the
public one. It depends on `core` alone, so its tests run on any host. The pair
carries no passphrase -- VMLord connects to the guest unattended, and a
passphrase stored beside the key it protects protects nothing. `VmKeyPair` has
no `Debug`, for the reason `Password` and `Seed` have none.

`platform::vm_key` puts the pair under `keys/` in the VM's directory:
`id_ed25519` and `id_ed25519.pub`, both named by `platform::layout`. The private
file is created empty, its DACL is narrowed to SYSTEM, the Administrators group
and the user VMLord runs as -- who also becomes its owner -- and only then does
the key go in; the window between creating the file and setting its permissions
must not hold a private key. The user's own entry is what lets `ssh -i` work
from an unelevated console, and it is the exact shape Win32-OpenSSH accepts. A
VM that already has a key is never given another: the guest holds the public
half, and a new pair would leave it trusting a key the host no longer has.
Deleting the VM deletes the key: with the whole directory when the disks go, and
with `keys/` and `known_hosts` alone when they are kept.

### The cloud-init seed

`vmlord-seed` turns the provisioning contract into the two NoCloud documents
cloud-init reads on the first boot. It depends on `core` alone: no Windows API,
no filesystem, no network, so its tests run on any host. `build(&SeedRequest)
-> Seed` is infallible -- values arrive validated, and quoting handles the rest.
So is `image(&Seed)` below it: the crate does no I/O at all, so the first
failure any of this can produce belongs to the creation pipeline, where the
volume meets a disk -- "Creating a VM from a cloud image" below.

`SeedRequest` is flat rather than a borrowed `Provisioning`, and it deliberately
has no field for a plaintext password: what reaches the crate is the `$6$` hash
(#56, `platform::password_hash`) and the public key (#55). "The document contains no plaintext password"
is therefore a property of the types, not an outcome checked afterwards. `Seed`
has no `Debug` for the same reason -- `user_data` holds the hash.

`user-data` opens with the `#cloud-config` marker line and states: the user,
their `hashed_passwd` and `lock_passwd`, membership in the profile's admin
group, the sudo rule `ALL=(ALL) NOPASSWD:ALL` (cloud-init writes it into
`/etc/sudoers.d` itself, so `sudo` and `wheel` need no special case), the
authorized key, `ssh_pwauth`, `locale`, `timezone`, a `write_files` entry for
every keyboard file the profile names, and `growpart`/`resize_rootfs`. Growing
the root filesystem is a VMLord promise, so it is stated rather than left to cloud-init's
defaults.

SSH is the one thing in the document with two opposite shapes, and both come out
of `DistroProfile::ssh`, an `SshDaemon` -- a `config_drop_in` path and an
`SshUnits`, which is either `Service { unit }` for a daemon that opens its own
port or `SocketActivated { socket, socket_drop_in, service }` where a socket unit
owns it -- so that the generator knows no distribution by name. The two are a
choice rather than a list of unit names with an optional path beside it, because
the impossible combinations (a socket drop-in with no socket unit, a profile
naming no units at all) were states the generator had to check for and no
distribution could ever be in:

* `SshAccess::Disabled` adds a `runcmd` that disables every unit the profile
  names, socket first. A cloud image ships the daemon enabled, and silence would
  make the choice void. Nothing is configured: the drop-ins below would be
  settings for a daemon being switched off.
* `SshAccess::Enabled { port, .. }` writes those drop-ins -- `Port <n>` for the
  daemon, and `[Socket]` with an empty `ListenStream=` followed by
  `ListenStream=<n>` for the socket unit, where the distribution has one -- and
  then runs `systemctl daemon-reload` followed by one command that makes the
  running guest read them.

That last command is where the two shapes part. A daemon that opens its own port
is simply `try-restart`ed: that restarts a running daemon and does nothing to one
the release deliberately keeps stopped.

Where a socket owns the port, the service must not be restarted at all, and
`try-restart` is not enough to promise that. A socket-activated `ssh.service` is
inactive only until something connects -- and on a guest created with the default
port, something does: the image already listens on 22 from `sockets.target`, so
VMLord's own readiness probe and its `cloud-init status --wait` connect during
the first boot and activate the service before `runcmd` is reached. `try-restart`
then restarts a service that is now standalone, which binds the port out of
`sshd_config` while `ssh.socket` still holds it. That is how a VM created on port
22 ended up with `ssh.service` running as `sshd -D [listener]` beside its own
socket, answering on `::` alone and refusing every IPv4 connection VMLord made
(#105) -- while the same VM on port 222 was fine, because nothing could connect
early enough to activate anything.

So the socket-activated branch decides in the guest, as one `sh -c` command:

```sh
if systemctl is-active --quiet ssh.socket; \
   then systemctl stop ssh.service; systemctl restart ssh.socket; \
   else systemctl try-restart ssh.service; fi
```

If the socket is the listener, the service is *stopped* -- the next connection
brings it back through the socket, which is what socket activation is for -- and
only then is the socket restarted onto its new port. Stopping first is not an
ordering preference: restarting the socket while the service still holds the port
is the same collision from the other side. Releases that ship the socket unit
without enabling it -- Ubuntu 22.04 does -- take the `else` and have their
running service restarted, exactly as before. The decision is one command rather
than several because `runcmd` cannot spell "only if the previous one answered
yes", and the answer is knowable only in the guest.

Both files are written because either alone would be wrong somewhere: Ubuntu has
socket-activated the daemon since 22.10, and a socket-activated `sshd` is handed
a descriptor that is already bound, so `sshd_config`'s `Port` is read and then
ignored; a distribution whose daemon opens its own port names no socket unit,
and then the drop-in is the whole story. The empty `ListenStream=` is not a
stray line either -- systemd appends to list settings, so without it the guest
would answer on the distribution's port as well as the chosen one -- and the
drop-in then names one listener per address family, `0.0.0.0:<n>` and
`[::]:<n>`, because clearing the list threw away both of the entries Ubuntu's
own unit ships. That unit also sets `BindIPv6Only=ipv6-only`, which a drop-in
replacing only the addresses leaves in force, so a bare `ListenStream=<n>` binds
a single IPv6 socket and refuses every IPv4 connection -- which is what VMLord
makes, and what a guest created on port 22 refused (#105). Naming both is right
whichever way that setting goes: with `ipv6-only` they are the two listeners the
guest needs, and without it the IPv4 entry is the redundant half of a dual-stack
socket rather than a second one. The daemon's
drop-in is numbered `10-` because `sshd_config` reads `sshd_config.d/*.conf` in
name order and the *first* value of a keyword wins, which puts VMLord ahead of
cloud-init's own `50-cloud-init.conf`. The default port is written out like any
other: an image whose own configuration says something else would otherwise
decide where the guest listens.

A `runcmd` command that names a unit a release does not have makes `systemctl`
return non-zero, which cloud-init does not treat as fatal -- which is why the
restarts are one command per unit rather than one command listing all of them.

Arch is the second shipped profile and the first one to take the other branch
of every choice above (#158). Its `openssh` prepends
`Include /etc/ssh/sshd_config.d/*.conf` to `sshd_config` and ships no
`sshd.socket` at all, and the image `arch-boxes` builds enables `sshd`, so the
profile is `Service { unit: "sshd.service" }` and the daemon's own drop-in is
the whole story; `10-vmlord.conf` also sorts ahead of the package's own
`99-archlinux.conf`, which is what a first-value-wins parser needs. The keyboard
is two files rather than one, `/etc/vconsole.conf` as a shell assignment and
`/etc/X11/xorg.conf.d/00-keyboard.conf` as an Xorg string. The account is `arch`
in `wheel`. And it releases nothing of the form `NN.NN`: the one release it
offers is `rolling` (#154), and both URL templates spell no `{release}` at all,
because the image and its `.SHA256` sit together in a `latest` directory that
names none. Substituting a release into a template that carries no placeholder
is a no-op, so the resolver asks for the pair by release exactly as it does for
Ubuntu. The checksum file is one line of `<sha256>  <name>`, which is the
format `parse_sha256sums` already reads.

Nothing in the host or the agent branches on any of that. The profile is the
whole of what VMLord knows about Arch before the guest boots, and what the guest
turns out to be afterwards -- its package manager, its library layout, its
desktop -- is detected rather than declared, which is why adding this profile
added no line naming Arch outside `distros/arch.json`.

`meta-data` carries `instance-id`, formatted from the VM's id, and
`local-hostname`, the VM name. The identifier never changes, which is what makes
it safe to leave the seed attached: cloud-init re-reads it on every boot and
skips the per-instance modules it has already run.

The documents are printed by hand, not serialised: they are small, fixed, and
the `#cloud-config` line is a comment to YAML and a format marker to cloud-init.
Every value from outside is printed as a single-quoted YAML scalar, where YAML
has no escape sequences at all; the keyboard layout is escaped a second time for
the file it lands in, which has a reader of its own. The tests read the result
back with a YAML parser and assert on meaning, the way cloud-init's PyYAML will.

`vmlord-seed::image` packs both documents into the ISO9660 volume the guest
mounts: 2048-byte blocks, sixteen empty ones, a primary descriptor, a
terminator, two path tables, the root directory and one extent per file. No
Joliet, no Rock Ridge, no El Torito -- the volume is not bootable, and the names
Rock Ridge usually carries are written straight into the ISO9660 records
instead. The writer is ours rather than a crate's because the volume label is
the only thing cloud-init has to find the seed by, and roughly three hundred
lines of ECMA-119 can be verified byte for byte.

Two decisions are worth stating. File identifiers are written literally --
`user-data` and `meta-data`, lowercase, hyphenated, with no `;1` suffix -- even
though a hyphen is not an ISO9660 d-character at any level and Level 2 relaxes
only length. There is no conforming spelling of the names cloud-init requires,
so the deviation is made once, explicitly, where the bytes written are the bytes
the guest opens; `crates/seed/tests/mount.rs` proves it against a real Linux
kernel. And nothing is dated: the descriptor's date fields carry the "not
specified" form and directory records carry zeros, which keeps the image
reproducible and the crate free of calendar arithmetic `std` does not have.

The image is returned as bytes rather than written to a file, so `crates/seed`
still knows no filesystem; `platform::create` writes them into the VM's
directory as `seed.iso` and attaches it. The root directory grows by whole
sectors as records need them, which is what lets the same transport carry a
guest agent later without touching the writer.

The keyboard is the one guest setting cloud-init has no module for, so the seed
writes the files itself -- and which files those are is `DistroProfile::keyboard`,
a list of `KeyboardFile { path, form, template }` (#157). The template is the
whole file with `{layout}` where the layout goes, so the generator never learns
what `XKBMODEL` is, and `form` is what decides the escaping: `ShellAssignment`
for `KEY="value"` files, where Debian's `/etc/default/keyboard` is read with
`source` and `$`, a backtick or a quote is code; `XorgString` for
`Option "XkbLayout" "value"`, where Xorg's parser knows no escape sequences at
all and a quote is therefore dropped rather than escaped, since a backslash
before it would be a literal character and the string would still end. A list
rather than one path because the two are not one setting written twice: Debian
keeps the whole answer in one file, while Arch configures the console through
`/etc/vconsole.conf` and the graphical session through
`/etc/X11/xorg.conf.d/00-keyboard.conf`. Neither escaping handles control
characters and neither has to -- `Provisioning::validate` refuses them before a
seed is built. Every other key in the document is a cloud-init module that works
anywhere.

Creating a VM is now the native backend's alone. AppSandbox's model was "media
plus unattended answers", and the tool that carried out the answers was
`iso-patch.exe`: a host-side Ubuntu installer with its own ext4 writer, its own
squashfs reader, and a partition table written through
`IOCTL_DISK_SET_DRIVE_LAYOUT_EX`. Importing a cloud image leaves it nothing to
do, so VMLord stopped shipping it. What remains of `iso-patch` here are
citations to its C sources, which recorded Windows behaviour worth keeping;
only the binary is gone.

### Creating a VM from a cloud image

`VmCreationPipeline::create` is one transaction with a fixed order, and the
order is what makes it one. Everything that can refuse the request refuses it
before a single side effect: `VmCreateRequest::validate`, the duplicate-name
check against the metadata store, the "directory already exists" check, and
`HcsVmConfigBuilder::build`, which is called this early precisely because it is
where an unsupported GPU or network mode is rejected. The VM's id and its HCS
compute-system id (`vmlord-` followed by the id's 32 hex digits, undashed) are
minted just before the document is built, since neither leaves a trace on disk;
only then does the VM get a directory.

Inside the directory the cloud branch does two things the local-media branch
does not. `CloudDiskImporter` turns the release into the system disk -- the
VHDX is not created empty and then filled, it arrives already carrying the
image, sized for the VM rather than for the image. Then `write_provisioning`
writes what the first boot reads: the SSH key pair, when `deploy_key` asked for
one; the `$6$` hash of the password, made here so that nothing further down ever
holds the plaintext; and the seed volume. When the bundled agent binary is
available, it also mints the VM's agent secret, keeps the host copy in
`agent.secret`, puts the guest copy in the seed, and writes the tools ISO
carrying the binary. After that the branches converge: `config.json` is
written, the VM is granted access to its disk and to its medium, the compute
system is created, and the mapping is inserted last, so a VM is known to
VMLord only once it exists in HCS.

The configuration has two SCSI attachments, not three: slot 0 is the system
VHDX, slot 1 is an ISO -- the installer for local media, `seed.iso` for a cloud
image. `hcs_config::media_path` is the single place that decides which, and it
is asked twice, once by the configuration and once by the pipeline granting
access to the same file, so the two can never name different paths. A third
attachment would exist only to keep an empty slot for the source that does not
use it, and every VM would then carry a device that is a hole in one of the two
cases.

`seed.iso` lives at `<vm>/seed.iso`, beside `config.json` rather than under
`disks/`. It is a configuration medium, not a disk: `disks/` is what a person
means when they ask VMLord to delete a VM but keep its disks, and a seed left in
there would be either deleted with the disks it provisioned or kept as something
that looks like one. Deleting a VM without its disks removes `config.json` and
the VM's SSH identity, so the seed stays with the disk it belongs to.

The seed carries the password's SHA-512-crypt entry, which makes it the second
secret VMLord keeps in a file, and it is written the way the first one is:
`vm_key::restrict_to_owner` narrows it to SYSTEM, Administrators and the owner,
and the file is created empty and narrowed before the bytes go in, so they never
sit under permissions wider than the ones they end up with. The storage root is
the owner's to choose, and one carrying an inherited `Users:(R)` would otherwise
hand the hash to every account on the machine -- while the private key beside it
was locked down. The DACL is protected, which cuts off what the parent hands
down but not what is added explicitly afterwards, so `HcsGrantVmAccess` still
puts the VM's own SID on the file and the VM goes on reading its seed.

Rollback needs to know none of this. Any failure after the directory exists
tears down the compute system if one was created and then calls
`cleanup::remove_vm_directory` on the whole directory -- disk, seed and private
key together. Nothing enumerates the files a half-built VM might have left, so
adding a file to the VM's directory later cannot leave one behind.

`CloudDiskImporter` is `Fn(&CloudImage, u64, &Path, &BuildMonitor) -> Result<(),
RepositoryError>`, injected rather than called. It is the layering boundary in
executable form: fetching the image is HTTPS, TLS and qcow2, which know nothing
of Windows and live in `vmlord-image`; writing it into a VHDX has no API and
must go through an attached `\\.\PhysicalDriveN`, which is `vmlord-platform`'s
business and nobody else's. The composition root joins the halves in
`vmlord::cloud_disk_importer`, which is what keeps the network out of the crate
that holds every `unsafe` HCS call. The importer is required by
`VmCreationPipeline::production` rather than optional, because a pipeline that
silently cannot build a cloud VM is a state better left unspellable; and because
it is a closure, the pipeline's own tests exercise every rollback path without a
network or a Hyper-V host.

The seed stays attached for the life of the VM rather than being ejected after
the first boot. This is safe because `meta-data` carries an `instance-id`
derived from the compute-system id, which never changes: cloud-init re-reads the
volume on every boot, recognises the instance it has already provisioned, and
skips the per-instance modules -- the user, the key, the password -- rather than
re-running them. Ejecting would mean rewriting the configuration document and
recreating the compute system on a schedule nobody owns, for no gain the guest
can observe.

### The COM1 diagnostic console

Every VM VMLord creates is wired to a serial port, and everything the guest
writes to it is kept. `Devices.ComPorts.0.NamedPipe` in `config.json` names
`\\.\pipe\vmlord-<vm-uuid>.com1`, derived from the compute system's own
identity by `hcs_config::com1_pipe_path`: the endpoint has to survive a rename
and stay distinct between VMs, and the UUID is the only thing about a VM that is
both. The capture lands in `<vm>/com1.log`, beside `config.json` for the same
reason the seed does -- it describes what the VM did, not what it is made of, so
a deletion that keeps the disks must not be what decides whether the last boot's
output survives. Nothing from the stream is copied into `vmlord.log`: guest
output is bytes, not events, and mixing the two would make both harder to read.

`vmlord-com1.exe` ships beside `vmlord.exe` and is the only thing that reads the
pipe. It holds no business logic: it parses its arguments, opens the log, and
mirrors every byte to the log and to its own stdout with no decoding, so a
partial UTF-8 sequence or a control byte reaches the file exactly as the guest
sent it. It exists as a separate process because a terminal window has to host
something, and because a cancellable overlapped read of a named pipe belongs
where the rest of `platform`'s Win32 code already is. The reader owns nothing
the GUI owns: it is told a pipe, a log, a mode, its parent's process id and four
event names, and none of those is ever a secret -- a command line is readable by
anything on the machine that can enumerate the process.

The console is two-way. The pipe is opened `GENERIC_READ | GENERIC_WRITE` --
HCS serves it duplex -- and a second thread in the helper carries its standard
input into the guest, byte for byte, on the same handle: HCS serves one pipe
instance, so a second open would be refused as busy or take the stream away from
the reader, and one handle with an `OVERLAPPED` per operation is what overlapped
I/O is for. This is the only way into a VM that has no network -- `network_mode:
None`, a network that did not come up, a broken `sshd` -- and it is the reason
`com1_input` exists at all.

Typing works because the helper takes its console out of cooked mode for the
life of the capture: line input, echo and processed input off, virtual terminal
input and output on. Without that, keystrokes would be held until Enter, every
character would be echoed twice, a password would be shown, Ctrl-C would kill
the helper instead of the command running in the guest, and what a full-screen
guest program draws would arrive as escape codes. The modes the helper found are
restored on every path out, including a panic, so the window it leaves behind
behaves as it did before. A standard handle that is not a console -- input from
a pipe, output redirected to a file -- is left alone and the bytes still travel.

What is typed goes to the pipe and nowhere else. It is never written to
`com1.log`: the guest echoes what it means to echo, and a password is
deliberately not echoed, so recording input would put it in a file beside the
VM. Ctrl-C now belongs to the guest, which leaves the helper without a keyboard
interrupt of its own -- the console is closed by closing its window, or by
stopping the VM, which breaks the pipe. The input thread is never joined: a
blocking console read cannot be woken, so it stays blocked and process exit
collects it, and its `Arc` on the pipe handle is what guarantees the handle
cannot be closed under a write in the meantime.

A guest created without a password cannot be logged into here at all: cloud-init
turns password authentication off and the user has no password to type. The
creation form says so where the password is left empty.

`Com1Launcher` puts the reader on screen through the first terminal host that
starts: `wt.exe -w new new-tab --title "VMLord COM1 - <vm>"`, then
`powershell.exe -NoLogo -NoProfile -Command`, then `cmd.exe /D /S /C`. Neither
shell reads or tails the log -- they host the helper and nothing else -- and
neither is given `-NoExit`, so the window closes when the reader does. A host
that refuses is logged at `WARN` and the next is tried; only when all three
refuse is there an error, and it names all three.

The window Windows Terminal is asked for is a new one, never the current one.
`-w 0` names a window VMLord neither owns nor can see, and delivering an action
into it is not exactly-once: one launch was observed hosting the helper twice,
in two tabs of one window. Two readers on one COM1 pipe are not two views of the
same output -- the pipe serves one client, so the second reader sits in its
connect loop and takes the stream over the moment the first window is closed,
which is what a person sees as a second, empty console.

The four events are created by VMLord before anything is spawned, under
unguessable `Local\VMLord.Com1.<session-id>.*` names: `ready`, which the reader
signals once its log is open and it can be cancelled; `cancel`, which ends the
capture; `failed`, which distinguishes a reader that stopped for the wrong
reason; and `finished`, which is signaled on every path out of the reader,
including a panic. What VMLord keeps afterwards is a `Com1Session` holding those
events, not a process handle: the terminal owns the reader, and the session is
how VMLord speaks to it.

A fifth name, `alive`, works the other way round: the helper creates that event
itself and holds it for as long as its process lives, and VMLord only ever
probes whether it exists, never holding a handle -- one would keep the object
alive past the process it stands for. This is how a reader that said nothing is
noticed. `finished` covers every way the helper can leave on its own two feet,
and closing the console window is not one of them: the terminal kills the
process where it stands, and a killed process signals nothing. Before this,
such a session looked alive forever, and the VM whose window had been closed
could never be given another console. A probe that fails for any reason other
than "no such object" is a `WARN` and counts as alive: a reader that may still
be reading is left alone. `Com1Sessions::reap` asks this question, so both the
refresh that drains diagnostics and `open_console` see the same answer.

Ownership follows the VM. An explicit start opens the console once the compute
system is in its final shape and before it executes anything, and truncates
`com1.log`, because that boot's output replaces the previous one. Both halves of
that order are load-bearing. Not earlier: preparing the system may destroy and
re-create it, and the named pipe COM1 is served through goes with it, leaving a
console reading a pipe that has stopped existing -- an empty `com1.log` and a
terminal window that closes itself. Not later: the output that explains a failed
boot is written in the first seconds of one. A start whose console cannot be
opened fails: a VM running without diagnostics is the case this feature exists
for. Any failure after the
launch drops the pending session, and dropping one signals cancellation, so no
window survives a start that did not happen. A reconnect at startup is the other
direction: for each VM HCS still reports as `Running`, a console is opened in
append mode, because the boot it is in the middle of is the same boot. A
reconnect launch that fails is a `Warning` diagnostic and nothing more -- the
guest is already up, and no diagnostic is worth taking it down for.

A console opened in append mode -- a reopen, or a reconnect at startup -- shows
the end of `com1.log` before the live stream, under a one-line banner that is
written to the window and never to the log. Without it such a window is empty
until the guest prints its next byte, and a guest sitting at `login:` prints
nothing at all: it wrote that prompt once, minutes ago, and a `getty` repeats it
only when something is typed. The replay is the last 64 KiB, cut back to a line
boundary so that no half-finished escape sequence colours everything after it,
and it is read by seeking rather than by loading a log that holds a whole boot.
What the replay does not carry is the questions. A boot log holds the probes the
guest's own tools made -- `ESC[6n` for the cursor position, `ESC[c` for the
terminal's identity -- and a terminal answers those on its *input*, which here
is the helper's stdin, which goes into the guest's tty. Replayed unfiltered they
made the terminal type `^[[30;1R` at the login prompt of a guest that had asked
nothing. So the replay drops the sequences that solicit an answer, and keeps
everything that only paints: history without its colours is history that is hard
to read. The live stream is never filtered -- a guest that asks has asked, and
answering it is what a serial terminal is for.

A replay that cannot be read is a `WARN` and an empty window, never a failed
console: the window exists for the bytes the guest is about to write. A
truncating start replays nothing -- it is throwing the previous boot's output
away, and this one is about to print itself from the first line.

A console can also be asked for. `VmRepository::open_console` -- `Open COM port`
in the list, enabled only while the VM runs -- opens one for a VM that has none,
in append mode, for the same reason a reconnect does: the boot it joins is the
boot already being logged, and truncating would throw away the output the
console is usually reopened to read. This is the only way a closed window comes
back, and the way in when the guest is unreachable over the network. It asks HCS
for the VM's state rather than the application layer's cached list, which can be
a refresh out of date by the time the user clicks: only `Running` is accepted,
because the pipe belongs to the compute system and outlives it by nothing. A VM
that still has a session is refused with a message rather than given a second
reader, for the reason above -- one pipe, one client. Sessions that are over are
reaped first, so a window a person closed is not mistaken for one that is still
reading; a reader that stopped for the wrong reason still becomes its `Error`
diagnostic on the way through.

A graceful stop leaves the session alone -- and opens one when there is none:
the guest is still printing what it does on the way down, that output is the
only account there is of a stop that stalls, and the pipe closing is what ends
the capture. A force
stop, a delete, an HCS exit event, and VMLord's own shutdown all cancel it,
because in each of those cases nothing will ever close the pipe from the other
end. `refresh` reaps sessions that are over: one that finished with its
pipe, and one whose window was closed, are `DEBUG` lines, and one that signaled
`failed` becomes an `Error` diagnostic naming the VM and the `com1.log` to look
in. A killed helper cannot signal `failed`, so a window closed by hand and a
helper killed some other way read alike -- as the ordinary end of a capture,
which is what closing that window means.

`ubuntu_cloud_init_is_visible_on_com1` is the factual check, ignored like every
other test that needs Hyper-V: it builds a real Ubuntu cloud image, starts it,
and waits for the string `cloud-init` to appear in the VM's `com1.log`. That is
the claim worth verifying -- that the serial console the guest was given is the
one being captured, before SSH exists to ask it anything.

`the_com_port_of_a_running_vm_is_opened_once` covers what the action must not
do, against a real compute system: no console before the VM runs, no second
console while one is open, and none again once it is stopped. The case the
action exists for -- reopening a window a person closed -- stays a manual check,
because closing that window is what ends the session and no test can close it.

`a_guest_can_be_logged_into_over_com1` is the same kind of check for the other
direction: it builds a VM with a password, starts the compute system directly
rather than through the repository -- a repository start opens its own helper,
and the test has to be the pipe's only client -- then answers `login:`, types the
password, runs a command and waits for its output to come back.

### Creating a VM in the background

`create_vm` is asynchronous with respect to its caller, and `VmRepository` is
still a synchronous trait: the two are reconciled inside `platform`, which owns
the thread. `HcsVmRepository::create_vm` refuses what can be refused cheaply and
certainly -- validation, a name the store or the build registry already knows, a
directory that already exists -- and then hands the request to
`build::BuildRegistry::start`, which spawns one `std::thread` per VM and returns.
The thread runs `cycle::VmBuildCycle::run`, which is creation, start and the
readiness wait as one operation; a failure is reported to the user as a marked
event rather than through a return value nobody is waiting on.

### Waiting for the guest

A build no longer ends where HCS accepts a compute system. That moment is where
VMLord stops working and well before the VM does anything: cloud-init installs
the SSH key at its init stage and applies `packages:` later, at its config stage,
so a probe of port 22 answers "ready" in the middle of the work. `VmBuildCycle`
therefore carries on -- `BuildStep::Starting`, then `BuildStep::AwaitingGuest` --
and the build leaves the list only once the guest's own `cloud-init status
--wait` has answered.

`guest_ready::GuestReadiness` waits in phases, each with its own timeout and its
own failure, because they are different facts about the guest: the endpoint has
to be given an address (HNS assigns it, the DHCP server delivers it), something
has to answer a TCP connection on the guest's SSH port, and cloud-init has to
report that it is done. The transport is `ssh.exe` from `%SystemRoot%\System32\
OpenSSH`, its arguments built by `platform::ssh` like every other connection to a
guest, driven as a child process behind a seam: every maintained Rust SSH
client is async-only, and VMLord has no async runtime, while a second vendored C
build under MSVC would cost more than this does. Its absence -- OpenSSH Client is
an optional Windows feature -- is an outcome of its own, named in the message a
person acts on. The child's output goes to `cloud-init-status.log` beside
`com1.log` rather than to a pipe: `--wait` prints for as long as it runs, and a
pipe nobody drains fills and deadlocks the child against the loop polling it.

Everything about the connection comes from `VmComputeSystemMapping::ssh`, the
configuration the creation pipeline recorded, rather than from the request the
VM was built out of: the wait runs against the VM that exists. The port it probes
is therefore the one cloud-init configured the daemon with. Probing 22 instead
would answer during the very window the wait exists to sit through -- a cloud
image ships its daemon on 22 and the seed moves it -- and would then ask the
question on a port nothing listens on. A configuration that cannot be connected
with is refused before the first phase rather than after the address timeout: it
is not going to become connectable in ninety seconds.

A VM created on port 22 is the case where that reasoning runs out: the port the
seed configures is the port the image already listens on, so the probe answers
from `sockets.target` onwards and the wait sits through the first boot inside
`cloud-init status --wait` instead. That is honest -- `--wait` is the question,
the probe only avoids a pointless first attempt -- but it does mean VMLord's own
connection activates a socket-activated `ssh.service` mid-boot, which is why the
seed's restart command must never restart that service (#105, above).

How far the wait can get depends on how the VM lets VMLord in, so the two
authentication modes end differently and say so:

* **Key mode** asks the guest's own `cloud-init status --wait --long` over
  `ssh.exe` and reports what it answered.
* **Password mode** ends where the port opens. The password is in the head of
  whoever created the VM, and a build is not a moment anyone is at a prompt;
  nothing here tries to type one non-interactively, and no other credential may
  stand in. The build succeeds with a warning naming what was not checked, which
  is `GuestReady::Unverified` -- an outcome of its own rather than a `Ready` that
  quietly means something weaker. A VM created with SSH switched off gets the
  same answer for the same reason: there is no daemon to reach and nobody to ask.
  Password mode needs no OpenSSH client on the host, because it runs none; a
  missing client is the interactive launcher's problem to report.

The probe and `ssh.exe` connect separately, so a guest can accept the probe and
refuse the connection a moment later. That race is left alone deliberately: the
probe exists to avoid a pointless first attempt, not to guarantee the second,
and OpenSSH's own message is the better account of what went wrong anyway.

Exit codes decide readiness. `0` is done, `2` is done-but-degraded -- reported as
a warning with what `--long` said, because one broken cloud-init module must not
turn a working VM into a failed build -- and `1` is a cloud-init failure. `255`
is OpenSSH's own code and says nothing about cloud-init, so it is reported as an
unreachable guest.

The rollback rules differ by what failed, and deliberately so. A start that fails
rolls the whole creation back: a VM nobody has ever started is debris. A wait
that fails does not: the VM exists, it runs, and its `com1.log` is the only
account of what went wrong inside it, so the build ends with an error diagnostic
carrying the tail of that log and the VM stays for a person to look at. A
cancellation rolls everything back, running VM included -- force-stop, then
delete -- because the downloaded image survives in the cache and repeating the
creation is cheap. Installation media has no cloud-init and no key of VMLord's,
so there is nobody in that guest to ask: the cycle ends at a started VM.

The four timeouts live in `AppSettings::guest_readiness` -- 90 seconds for the
address, 300 for the SSH port, 1200 for cloud-init, 10 for one connection attempt --
under `#[serde(default)]`, so a `settings.toml` written before they existed keeps
loading. They are file-only: four second-counts for a case that arises once in
the life of an installation would dilute a dialog that shows a path, a language
and log settings.

A start produces a `Com1Session` and a compute-system handle, both of which
belong to the repository behind `&mut self` -- which a build thread does not
have. The thread therefore parks them in its `BuildRegistry` entry, and
`take_started` hands them over on the next refresh, where `adopt_started` inserts
the session and holds the system. `take_started` is separate from `reap` on
purpose: `reap` runs inside queries taking `&self`, and a session dropped there
would silently cancel the console reader of a running VM. Passing a session
across threads is what `unsafe impl Send for WindowsEvent` is for -- an event
handle is a process-wide kernel object whose whole purpose is signalling between
threads, and the type owns its handle rather than sharing it.

`list_vms` is where the two halves are joined: the VMs `MetadataStore` knows
about, plus `BuildRegistry::summaries` for the ones still being built. A build
therefore appears in the list as `VmState::Building { progress }` from the moment
it is accepted, with its sizes taken from the request because nothing of the VM
is on disk yet to read them from; and it stops appearing the moment its thread is
over, because a failed build rolled itself back and never reached the store. The
UI needs no new plumbing for any of this: the existing one-second refresh already
calls `list_vms`, and `refresh` -- the `&mut self` call that follows it -- is
where finished threads are joined by `BuildRegistry::reap`.

Progress is a level in two slots joined at the moment of reading, not a stream:
`BuildMonitor` holds a `ProgressPublisher<BuildStep>` written by the pipeline and
a `ProgressPublisher<DownloadPhase>` written deep inside `vmlord-image`, which
knows nothing of VMs. `BuildMonitor::snapshot` shows the byte counts only while
the step is `Downloading`, so a stale count can never appear beside a later step.
Whoever runs a step reports it, which is why `CloudDiskImporter` takes the
monitor: fetching and writing the disk are one call from outside the closure.

Cancellation is one `AtomicBool` in the same monitor, polled at the pipeline's
checkpoints and once per chunk inside `copy_image`, and it fails the build with
an ordinary `RepositoryError`. That is deliberate: a cancelled build then takes
the same rollback every other failure takes, instead of a second cleanup path
that can drift away from the first. `VmRepository::cancel_create` is the contract
for asking, defaulted to a refusal so that a backend creating VMs in the
foreground says so honestly. An interrupted build that carries no error at all --
a panic on the worker thread -- is caught by `CreationGuard`, a drop guard the
success and failure paths both disarm; `catch_unwind` is not an option, because
the pipeline's seams are boxed closures and `AssertUnwindSafe` would assert what
needs proving.

`HcsVmRepository::drop` cancels every build and joins it. Without that, leaving
VMLord either kills a thread in the middle of writing a VHDX -- leaving behind
the directory it was told to remove -- or hangs on one that was never told to
stop. Concurrency reaches one more place: `MetadataStore::insert` and `::remove`
are a read-modify-write over one document, so a process-wide lock serializes
them; two builds finishing together would otherwise both write, and one of the
two VMs would be gone from a file that reported success twice.

There is still no async runtime anywhere in VMLord, and `VmRepository` remains
synchronous. `std::thread`, `Arc`, `Mutex` and `AtomicBool` are the whole of the
machinery, modelled on `platform::dhcp`, the project's other background thread.

### The create form and what a build looks like on screen

The dialog's first control is the one that decides the shape of the rest: a
cloud image or the user's own installation ISO. It mirrors `VmSource` without
its payload -- `CreateVmForm` keeps the fields of both modes side by side, so
switching to media and back does not lose a typed password, and
`create_vm_source` reads only the fields the chosen mode has. A password typed
before the mode was switched therefore cannot travel with an ISO, which is a
property of the function rather than of a clearing routine.

The form states no rules. `create_vm_request` builds the request and calls
`VmCreateRequest::validate`, showing what comes back: the user-name rules, the
password rules, the guest settings and -- since this task -- the VM name, which
is the guest's host name and moved out of the dialog into
`core::provisioning::validate_vm_name` for the reason the user-name rules did.
The one check that stays in the UI is the duplicate name, because it is about
the list on screen and not about the request; the repository checks it again
against the metadata store, where it is authoritative.

Three fields are filled from the host: locale, keyboard layout and timezone.
`platform::host_guest_defaults` reads them once at startup --
`GetUserDefaultLocaleName`, `GetKeyboardLayoutNameW` and
`GetDynamicTimeZoneInformation` -- and maps each into what the guest names the
same thing: a POSIX locale, an XKB layout, an IANA zone. The timezone is mapped
by the CLDR table in `windows-timezones` from `TimeZoneKeyName`, the invariant
registry key, rather than from the localized `StandardName` -- which on a
Russian Windows reads «Русское стандартное время» and appears in no table. The
keyboard has no such crate anywhere, so it has a table of its own: the frequent
KLIDs, then the layout without its variant, then the language alone, then `us`.
`GuestDefaults` carries the three from the composition root through
`WorkspaceApp::with_guest_defaults`, and its `Default` -- `en_US.UTF-8`, `us`,
`Etc/UTC` -- is what each field falls back to on its own, so a host whose
keyboard is unrecognised still hands the guest its own timezone, and a VM is
created in a state that works rather than not created at all.

Leaving the password empty is a choice rather than a missing value: the guest
gets no password at all, cloud-init turns password authentication off, and the
field is not trimmed, because a space is a character of a password. The key-pair
toggle shows where the private key will be, and the path is answered by
`VmRepository::ssh_key_path` rather than composed in the dialog -- the on-disk
layout of a VM is the platform layer's, and the label has to be right before the
file exists. A backend that gives VMs no keys of its own answers `None` and the
dialog says only that the key lives with the VM. The port sits with the key
toggle, inside the group the SSH checkbox disables, so a guest that runs no
daemon has nothing to be asked about -- and installation media has no SSH
controls at all, because the whole provisioning grid belongs to the cloud-image
mode.

A build is a row in the list from the moment it is accepted, labelled with the
step it has reached; the selected-VM panel adds the download's byte counts,
which are the only counts any step publishes -- a percentage over the others
would need a denominator that does not exist. `percentage` never reports 100
before the last byte, because a bar that reads full while the work continues is
the one people wait on. `VmAction::CancelCreate` is enabled only while a VM is
building and is the only action then available: it calls
`WorkspaceApp::cancel_create`, the build rolls itself back, and the row leaves
the list on its own. Start, stop, edit and delete stay disabled meanwhile, since
what exists of the VM is a directory the build still owns.

---

## The guest agent protocol

Some things about a VM can only be known or done from inside it: whether the
GPU the host attached is the one the guest renders on, and where the userspace
that drives it has to be mounted. `vmlord-agent` is the program in the guest
that answers for those, and `vmlord-agent-protocol` is the contract it and the
host share. Both crates exist before either side of the connection does, so
that the wire format is designed once rather than twice.

The contract is Protobuf without gRPC. The transport is a single HvSocket
stream per VM, opened by the guest; a service definition would add an HTTP/2
stack to both ends of a socket that carries one connection and one peer. What
is left is a schema and a framing rule: `proto/vmlord/agent/v1/agent.proto`
holds the messages, and every one of them travels as a 4-byte little-endian
body length followed by an encoded `Envelope`. Protobuf messages are not
self-delimiting and a stream socket owes nobody message boundaries, so the
length has to come from somewhere; little-endian because both ends are x86-64.

Bodies are capped at one mebibyte, enforced before anything is allocated on
either side. The messages here are status reports and manifests, so the cap is
far above what the schema can produce and far below what a guest could use to
exhaust the host. A prefix that exceeds it leaves the stream at a body of
unknown length, which cannot be resynchronised: the connection is closed rather
than skipped past.

`Envelope` carries a request id and a `oneof` of `Request` or `Response`, and
is deliberately symmetric. The host asks the guest to do things, and the guest
reports to the host, so a client envelope and a server envelope would be two
things to keep in step for no gain. Ids are unique per originator rather than
globally; a response repeats the id of the request it answers.

Two agreements open a session, and they answer different questions.
`ProtocolVersion` is `major`/`minor`: a differing major means an existing
message changed meaning and there is nothing to negotiate, while a session
between differing minors runs at the lower one, so the older side decides how
new the conversation can be. Capabilities are not ordered and so are not a
version -- an agent on a VM with no GPU is not an older agent -- and only
capabilities both peers named may be used. A capability number this build has
never heard of is dropped rather than refused, which is what lets a newer agent
talk to an older host at all.

A session proves who it is with a secret the VM was created with. When a
cloud-image VM has the bundled agent binary available, VMLord mints 32 random
bytes in `write_provisioning`, beside the SSH key pair, and writes them twice:
into `<vm>/agent.secret` for itself, and into the seed as
`/etc/vmlord/agent.secret`, owned by root with mode `0600`. Both host files are
written the way the private key is -- created empty, narrowed by
`vm_key::restrict_to_owner`, and only then filled -- but only the seed is handed
to the VM by `HcsGrantVmAccess`: the guest has its own copy and has no business
reading the host's. One secret per VM rather than one for all of them, for the
reason each VM gets its own SSH key: the compromise of one guest must not reach
the next. `auth::GUEST_SECRET_PATH` is where the path is spelled, once, because
the crate that writes the seed and the crate that reads the file inside the
guest must not spell it separately.

The secret never travels on the protocol. What travels is a challenge: after
the hello exchange the host sends `AuthenticateRequest` with a nonce drawn
fresh for that session, and the agent answers `AuthenticateResponse` with
HMAC-SHA-256 over a fixed domain string and that nonce, keyed by the secret.
The host recomputes the tag and compares it in constant time -- an early return
on the first differing byte is how a tag gets forged a byte at a time. Freshness
is what makes a recorded answer worthless, so a reconnecting agent runs the
exchange again rather than replaying anything; `auth::allowed_unauthenticated`
is the rule for what a session may do before it has, and the answer is the hello
and the challenge and nothing else. Everything else is refused with
`ERROR_CODE_UNAUTHENTICATED`. Nothing rotates a secret: a VM's secret lives as
long as the VM, and replacing one means recreating the guest that reads it.
Deleting a VM removes the host's copy even when the disks are kept, for the
reason the SSH identity goes -- what it would authenticate no longer exists.

Both sides compute the tag with the same functions in `vmlord-agent-protocol`,
which is what keeps them from disagreeing about what is being signed. The
crypto is RustCrypto and `getrandom`: pure Rust, so the agent stays a static
musl binary built without a C toolchain.

### What the agent detects about its guest

The host has to declare what it cannot see: a cloud image is found and verified
before anything boots, and cloud-init runs before there is a system to ask, so
the image templates, the checksum file, the default user, the admin group, the
SSH units, the keyboard files and the packages that install a desktop are
profile data in `distros/*.json`. Everything else the agent asks the guest
itself, at the moment it acts, in `crates/agent/src/guest_platform.rs`.

Detection there is not merely cheaper than a profile field, it is truthful. A
profile records what VMLord *asked for* when the VM was created; a guest months
later is whatever it has *become* -- a kernel upgraded, a desktop replaced,
packages installed by hand. So `GuestFacts` carries identity -- the `ID` and
release of `/etc/os-release`, the Debian name of the machine, the running
kernel -- and beside it the platform: which package manager answers `--version`
(`apt-get`, `pacman`, `dnf` or `zypper`, in that fixed order), whether the
architecture's multiarch directory is there or the libraries are in `/usr/lib`,
and what logind says is on the screen -- the session's own `DESKTOP` and type,
and the unit `display-manager.service` is linked to.

Nothing in that module branches on the name of a distribution, which is the
point: a guest is what it answers, not what its `ID` implies. The identity half
still fails the call, because a guest that will not say what it is has no
recipe at all; the platform half never does -- a guest with no package manager
and no desktop is one whose stages skip themselves with a reason. Both recipes
report the detected platform in their `DISTRIBUTION` stage, since detection is
invisible when it works and undiagnosable from a host when it does not.

Every decision is a function of text or of a directory a test can point
elsewhere -- the shape `display-services`' `seat` module already uses -- so the
detection is covered by fixtures rather than by a live guest, and only the thin
gathering at the bottom of the module needs one.

### Installing a package

Detecting the manager is half the answer; the other half is what the thing is
called once it is found, and both live in
`crates/agent/src/guest_packages.rs`. Every stage in both recipes that needs
something installed -- the DKMS build dependencies of the display and GPU
recipes, the distribution's Mesa, the AppIndicator extension, the two render
probe programs -- goes through the one `install` there, and no recipe writes
the name of a package manager or of a package again.

A caller asks for a `Package`: what the thing is *for* -- `Dkms`,
`BuildTools`, `KernelHeaders`, `AppIndicator`, `Mesa`, `RenderTools`. The
program, the arguments that make it non-interactive and the environment it
needs come from the `PackageManager` that answered; the names come from a table
keyed by that manager rather than by a distribution. That key is the point:
`linux-headers-$(uname -r)` and `linux-headers` are apt's and pacman's
conventions, not Ubuntu's and Arch's, so Debian and Ubuntu share every name for
free and a second apt distribution adds no row. `DEBIAN_FRONTEND` is one such
row -- apt's way of being unattended, where the other three take a flag.

The install refreshes the package lists and tries once more when the first
attempt fails, because a cloud image's lists are as old as the image and a
stale list is the ordinary way an install of a kernel-specific package fails on
a VM's first boot. It costs nothing on the second start of a VM, since every
caller looks before it installs and a guest that has what it needs never
reaches the manager at all. A guest carrying no manager this build knows fails
the stage with that as the reason, rather than running a program that is not
there.

### Installing the guest agent

A cloud-image VM gets the agent on its first boot. At creation VMLord looks for
the statically linked `vmlord-agent` beside its own executable. When it is
present, the creation transaction writes its bytes to a per-VM `tools.iso`
beside `seed.iso`, labelled `VMLTOOLS`, and grants the VM access to it. The
secret stays separate: the host copy remains `<vm>/agent.secret`, while the
guest copy is in the secret-bearing NoCloud seed.

The seed's cloud-init document writes a root-owned systemd unit. Its first-boot
commands mount `VMLTOOLS` read-only, copy `vmlord-agent` into
`/usr/local/lib/vmlord`, unmount the medium, and enable the service. systemd
then runs the agent as root, which reads the guest secret and opens its first
authenticated HvSocket session to the host. The installed copy, rather than
the ISO, is what later boots run.

This gives a cloud VM three SCSI attachments: its system disk, the NoCloud
seed, and the tools ISO. A local-media VM receives no cloud-init configuration
and no agent, so it deliberately remains at two attachments. If the sibling
agent binary is absent, creation warns and follows that same no-agent path;
it does not create a tools ISO or agent secret. The installed agent is what
mounts a VM's GPU shares once the host hands it a manifest.

### Keeping the guest agent current

The agent is the one part of VMLord that lives inside the guest, and first-boot
installation alone would freeze it at the VM's creation date: every release
after that would ship a host talking to an older agent, and a feature whose
guest half lives in the agent would never reach a VM that already existed. Two
have been shipped that way -- the display audio daemon and the guest tray, both
carried by a display payload the agent that reads it had never heard of.

The tools volume is what answers it, because it is already what "the host hands
the guest to install" means and it stays attached for the life of the VM rather
than for its first boot. `tools_volume::refresh` runs in the start pipeline
while the VM is still down -- the only moment a volume attached to it can be
written -- and rewrites `tools.iso` when the agent beside the executable is not
the one already on it. The image build is deterministic and carries no
timestamps, so "does this volume already carry this agent" is a comparison of
the bytes that would be written rather than a version somebody has to maintain.
A VM with no `tools.iso` is left without one: the volume is an attachment
written into the compute system at creation, and a file nothing is attached to
is a file nobody would read.

`self_update::apply` is the guest half, and it runs before anything else the
agent does -- before it reads its secret, because none of that is the old
agent's work if a newer one is on the volume. It mounts `VMLTOOLS` read-only
where cloud-init mounts it, compares the file with the installed
`/usr/local/lib/vmlord/vmlord-agent`, and on a difference unlinks that binary,
writes the new one, makes it executable and exits. `Restart=always` starts the
replacement: the arrangement that recovers from a crash, used for a change that
is deliberate. Unlinking rather than overwriting is what `install_file` does for
the display services and for the same reason -- an open executable cannot be
written through, but it can be replaced, and the running process keeps the
inode it started with. Nothing loops: after the replacement the bytes match, so
the next start walks straight through the comparison. A volume that cannot be
mounted or read leaves the guest running the agent it has, which is what it
would have done anyway.

What this does not do is rescue a VM whose agent predates the mechanism: an
agent that never learned to look at the volume will not start looking. Those
guests are updated by hand, or recreated.

### Reconnecting

The agent outlives any one connection, and the reconnect is its own rather than
the unit's. `Restart=always` is what recovers from a crash; it is the wrong
instrument for a host that is not there, because a fixed `RestartSec` cannot
back off -- a VMLord closed for an hour would be polled seven hundred times --
and a process that exits because the host hung up is not a failure, however it
reads in `systemctl`. So `vmlord-agent` reads its secret once and then repeats:
connect, run a session, wait, connect again. The only thing that ends it is a
secret that cannot be read, because a VM's secret is minted at creation and
never rotated: nothing that happens while the guest runs can turn an unusable
one into a usable one.

The wait comes from `backoff`, which both ends of the socket share so that they
cannot drift apart: one second, doubling, capped at thirty. The rule is
deliberately not a table of error classes -- a refused connect, a host that hung
up mid-handshake, a revision that could not be negotiated and a tag the host
would not take are all "the peer is not talking to me", and the only question a
retry answers is how soon to ask again. **A session that authenticated is what
starts the wait over**, because it is the one thing that proves the other side
is there. The cap is therefore the bound on how long after a VMLord restart a
VM's agent comes back.

The host applies the same rule from its side. Its accept loop takes the next
connection as soon as a session ends, which is exactly right for an agent
reconnecting on that backoff, but a peer that connects and drops without ever
authenticating would otherwise be served as fast as the thread can loop. After
such a session the loop waits, on the same backoff, in `ACCEPT_POLL`-sized
slices with the running flag read between them -- stopping a VM joins that
thread, and must stay bounded by the poll rather than by the longest wait.

A reconnect re-binds nothing and modifies nothing. The listener belongs to the
VM's run and stays bound to the runtime id that run was given; no HCS call, no
device assignment and no configuration write is on this path. Nor is anything
resumed: a new connection is a new hello, a new nonce and a new challenge, and
since the challenge is what makes a recorded answer worthless there is nothing
from the previous session worth carrying over. VMLord restarting is the same
story from the other end, and is what `initialize` already does -- it puts the
standing offer back up for every VM that is running, and the agent inside each
one connects to it on the next turn of its loop.

### Confirming what the other side agreed to

The host picks the session's revision and capability set out of the two hellos;
the guest checks that what came back is something it offered. `confirm_version`
accepts the guest's own major with a minor no higher than the guest's, and
`confirm_capabilities` accepts a subset of what the guest announced -- an
unknown capability number included, because in an *agreed* set a number this
build cannot name is not a capability to drop but a peer expecting messages
nothing here answers. Both live beside `negotiate_version` and
`agreed_capabilities` in `handshake`, which is what keeps the two ends from
disagreeing about what was agreed. Either failure ends the connection: there is
no third round in this handshake, and the reconnect above is what a peer that
answered unofferably gets instead.

The guest keeps the confirmed pair for the life of its session, and that is
what decides whether it may act on a GPU manifest at all. Both ends now
announce `CAPABILITY_GPU` -- the host can send a manifest and the guest can
mount one -- so a session between two current builds agrees on it, and one with
an agent installed before it existed agrees on nothing and is sent no manifest.

### Handing a guest its GPU shares

The exports a VM was started with are in its compute system's configuration;
what the guest is told is `AttachGpuSharesRequest`, and it is the only way the
guest learns what those shares are for. The message carries the manifest whole
-- every share, not the ones that changed -- because the guest reconciles
against what it already has, and a delta would need both ends to agree about
what was sent last. Each share is a name and a role, never a host path, and a
driver package carries the DriverStore folder name that distinguishes it. The
answer is `AttachGpuSharesResponse`: one `GpuMount` per share saying `MOUNTED`,
`REFUSED` or `FAILED`, and whether the dynamic linker was told about the
result. Refused and failed are different facts -- the first says the two builds
disagree about what a share is, the second says the share is there and broken --
and `libraries_refreshed` is separate from the mounts because a set of mounts
that all succeeded is unusable if `ldconfig` did not run.

A manifest is delivered once per session rather than once per VM, after the
challenge and only on a session that agreed `CAPABILITY_GPU`. The host cannot
tell an agent that lost its socket from one whose VM rebooted, so it re-sends
on every session and lets the guest work out that there is nothing to do. That
path touches no HCS call: the `Devices/Plan9` section was written before the
compute system was started and is immutable for the lifetime of a boot, so
`AgentConnection` carries the manifest of the run and every session of that run
delivers the same one.

The guest decides where a share goes, from a table with one entry per role:
`WslLib` at `/usr/lib/wsl/host-lib`, `WslD3d12` at `/usr/lib/wsl/d3d12`,
`DriverPackage` at `/usr/lib/wsl/drivers/<package>` and `GpuPayload` at
`/opt/vmlord/gpu-payload`. The drivers are at WSL's own path, which is where a
vendor's DriverStore libraries are expected; the payload is VMLord's own and
lives under `/opt`.

`/usr/lib/wsl/lib` -- the path Mesa's D3D12 driver, the probe and anyone
running `eglinfo` by hand expect to find whole -- is not in that table and
cannot be claimed by a manifest. It is composed after the mounts, as a
read-only overlay whose lower layers are the two halves above, Microsoft's
first so that a name present in both resolves to the library a renderer links
against. An overlay rather than a directory of symlinks because every share is
mounted `MS_RDONLY` and nothing can be created inside one; a merged directory
rather than a second `ld.so.conf` line because a half-populated
`/usr/lib/wsl/lib` is what neither the probe nor a person finds complete. It is
remounted rather than repaired on each attach, so a half the manifest dropped
leaves it, and the linker is told about the merged directory rather than about
two fragments of one. It is also unmounted by name on shutdown, since the mount
table this agent reads holds 9p mounts and this one is an overlay. The package name is the only part a host
contributes to a path, and the guest validates it again -- non-empty, bounded,
neither `.` nor `..`, `[A-Za-z0-9._-]` throughout -- because a path assembled
from a peer's string is exactly where "the other side already checked it" stops
being true. A share that fails is refused and the rest of the manifest is still
mounted.

Each mount is the only way a Hyper-V Plan9 share can be mounted from Linux: an
`AF_VSOCK` connection to CID 2 on port 50001, where HCS's Plan9 server listens,
handed to the kernel's 9p client as `trans=fd,rfdno=N,wfdno=N` with
`aname=<share>` selecting the share. That is why a share name is restricted to
characters that cannot be read as structure in a comma-separated option string.
The flags are `MS_RDONLY | MS_NODEV | MS_NOSUID`, so read-only is stated twice
and independently -- by the share's flag on the host and by the mount in the
guest -- and the descriptor is closed once `mount` returns, because the kernel
took its own reference.

The attach is a reconcile against `/proc/self/mountinfo` rather than a mount. A
target already carrying the share the manifest names is left alone if it reads
back; one carrying a different share, or a mount that no longer reads back, is
lazily unmounted and mounted again at most once; a 9p mount at one of the
allowlisted targets that the manifest no longer names is unmounted. The health
check is a directory read rather than a `stat`, because a 9p mount whose
transport died still answers a `stat` from the dentry cache. Reading the mount table rather
than a list the process kept is also what lets an agent that was upgraded and
restarted clean up its predecessor's mounts.

Mounted directories that hold shared objects are written into
`/etc/ld.so.conf.d/vmlord-gpu.conf` and `ldconfig` is run. The file is
rewritten from the current set every time, which is what makes the attach
idempotent and what makes a share that went away lose its line. `ldconfig` is
the one external program the agent runs: there is no library form of it, and
writing `/etc/ld.so.cache` by hand would be a second implementation of a format
the distribution owns.

`SIGTERM` ends the loop rather than the process. The agent then unmounts every
9p mount under its allowlisted targets, unmounts the merged
`/usr/lib/wsl/lib` by name, removes its `ld.so.conf.d` file and runs
`ldconfig` once more, all best effort: a guest that is going down is not helped
by an agent that refuses to exit because a mount was busy. The handler itself
sets a flag and shuts down the connection the agent is on, because a signal
handler may call almost nothing and those are two of the things it may do. The
flag alone would not be enough: between requests the agent sits in a read on
that connection, `signal` installs a handler the kernel restarts a read across,
and the socket's own idle timeout only leads to another read. `shutdown` is
what gives the read something to return, and without it a stop waited out the
unit's stop timeout and took a minute and a half. The backoff wait is spent in
slices for the same reason, so that a shutdown between connections is not held
for the half minute a wait at the cap would take.

Failures are `Error { code, message }` rather than a string. The code is what a
peer branches on -- an unsupported version, an unauthenticated session, a
request this build has no arm for -- and the message is for the log. `Error` is
the first arm of the response `oneof` because every request can fail, including
one whose kind the responder cannot recognise.

`proto/agent.descriptor.bin` is the compiled schema, checked in beside the
`.proto`. It is what tools that read descriptor sets consume, and it makes a
change to the wire format visible in a diff; a test fails if it stops matching
what the `.proto` compiles to. The schema is compiled in-process by `protox`
rather than by `protoc`, so that neither the Windows nor the Linux side needs a
toolchain nothing else in the repository uses.

---

## The host end of the agent socket

The guest opens the connection and the host listens. A host that connected
would have to guess when the agent inside had started and keep trying until it
had; a guest knows exactly when it is ready, and a reconnect after a lost
connection is then the same code path as the first connect. What the host owns
is therefore not a connection but a standing offer.

An HvSocket address is a pair of GUIDs: which partition, and which service on
it. The partition is the compute system's *runtime id* -- the GUID Hyper-V
gives it for as long as it runs, not the `vmlord-<uuid>` name VMLord chose --
so it is read out of the HCS enumeration on every start rather than recorded:
a stored one would address the previous run. `hvsocket::AgentListener` binds
one VM's runtime id, which is why nothing on the host ever has to work out
which VM is speaking. A wildcard bind would accept from every partition on the
machine, WSL included, and would have to identify the peer before it could
refuse it. The service half is derived from a vsock port -- `0x564D4C41`,
`VMLA` in ASCII -- through the template Linux integration uses, because the
guest is Linux and spells this address as `AF_VSOCK` to the host's context on a
port. `Devices/HvSocket/HvSocketConfig/ServiceTable` in the VM's `config.json`
is what makes that service exist for the VM at all, with a bind descriptor of
SYSTEM and the administrators: the accounts that can drive HCS in the first
place, and nobody else, may take the socket the agent is about to connect to.
A VM created before that entry existed cannot reach its agent until it is
recreated, since `config.json` is what a start rebuilds the compute system
from.

Nothing can interrupt a blocking socket call, so every wait is bounded: the
accept waits a quarter of a second at a time and the read the same, and the
thread checks between waits whether it should still be there. The interval is
also how long stopping a VM takes to join the thread, on the thread that draws
the window, which is why it is a fraction of a second rather than one. A read
that times out is reported as `WouldBlock`, which the frame reader exposes as
idle only before the next frame has begun. The session uses that boundary to
service host work and its liveness timer; a timeout after a frame has begun
closes the unusable connection rather than resynchronising it. Once the
connection has been told to stop, that same timeout ends it instead. This is what makes
dropping an `AgentConnection` a bounded operation: it sets the flag and joins
the thread, and the socket never outlives the VM it was bound to.

`AgentSessions` in `HcsVmRepository` is where those connections live, keyed by
VM id beside the COM1 consoles and for the same reason: both belong to a VM's
run, and this is the one place that sees every way a run can end. A stop, a
forced stop, a deletion, a VM that exited on its own and VMLord itself going
away all end a connection the same way -- the entry is removed. A VM started
twice replaces its entry rather than gaining a second listener, because the
older one is bound to a partition that no longer exists.

`agent_session` is the conversation itself and knows nothing about Hyper-V: it
reads and writes any stream, which is how the order of the messages is tested
against a peer made of bytes. The guest says hello, the two settle on a
revision and the capabilities they share, and the host then sends the challenge
and waits for a tag over it. Requests that arrive before that tag is verified
are refused with `ERROR_CODE_UNAUTHENTICATED` rather than dropped -- an agent
waiting for an answer it will never get would sit there until its socket died
-- and which requests those are is `auth::allowed_unauthenticated`'s decision,
not the transport's. This build announces `CAPABILITY_GPU`, so a session with a
current agent agrees on it and is the one a share manifest may be sent on.
After the challenge the session hands the guest that manifest when the VM has
one, answers heartbeats, refuses a second hello, and ends when the guest hangs
up at a frame boundary, which is not a fault. A quiet guest normally speaks
every 30 seconds; after that interval the host sends its own heartbeat and
drops a session that does not answer promptly. This is necessary because a
guest reboot can leave HvSocket looking open without a FIN and otherwise keep
the listener from accepting the rebooted agent forever. The listener is also
polled between frames while a session is open. A candidate must complete hello
and authentication before it displaces that session; once it does, the old
socket and any request waiting on it are dropped and the authenticated
replacement is served immediately, without reconnect backoff. This remains
true while a long guest operation is in flight, when silence alone cannot
distinguish a reboot from legitimate work.

Whether a VM's agent has a session open is what `AgentStatus` in a `VmSummary`
now reports. A running VM VMLord is not listening for at all -- one whose
partition HCS would not name, one whose secret is missing -- is reported as
unknown rather than offline: an agent that was never offered a socket has not
failed to connect.

## The display protocol

The display stack that replaces AppSandbox's IDD has its own contract, and
`vmlord-display-protocol` is it: the schema, the framing, the authentication
and a transport-free session machine, written before either end of it exists
so that the guest services, the codec and the Windows viewer are all built
against the same wire. Like the agent's contract it is portable by
construction -- no Windows APIs, no Linux syscalls, no sockets -- and it knows
nothing about what a frame's bytes mean.

A session is five HvSocket services rather than one: `VMLD` for control,
`VMLF` for frames, `VMLI` for input, `VMLC` for the clipboard and `VMLS` for
sound, named the way `VMLA` is -- `S` rather than `A` for audio, because `VMLA`
is the agent's own service. **The guest
listens and the host connects**, which is the opposite of the agent socket and
is deliberate. The agent's connection is a standing report that lives as long
as the VM; a display session begins when a user presses Connect and ends when
they close the window, so making the socket's lifetime the session's lifetime
leaves no "is the stream currently on?" state in the protocol -- no viewer, no
connection, no capture. What the reversed direction would otherwise cost, the
knowledge of when the guest is ready, comes over the agent channel instead: the
guest reports its display readiness there and the UI keeps Connect disabled
until it does, which leaves the viewer only a short bounded retry for the race
of connecting while a guest service restarts.

Every record on every channel begins with the same 24-byte little-endian
header: header length, channel, type, payload length, sequence, base, CRC32C
and generation. The payload is Protobuf on control and input and for the frame
channel's own handshake, and raw codec bytes for keyframes, tile deltas and
cursors. That last part is why the frame channel is not Protobuf all the way
down: a 1440p keyframe is megabytes, and carrying it in a `bytes` field would
copy it through an encoder on the way out and another on the way in, every
frame, in the one place where this format meets real bandwidth. The frame
channel is also the least changeable part of the contract -- a frame is a
sequence, a base and some bytes -- while the changeable channels are the cold
ones. The first header byte is the header's own length rather than a magic
number: the version is settled in the handshake, and what a reader actually
needs is room for a later minor to append a field it can skip.

Control records are capped at 64 KiB and input records at 4 KiB, both fixed.
The frame cap is not a constant but `width * height * 4` plus 64 KiB of slack
for the geometry the session agreed on, recomputed when that geometry changes
and held under an absolute 64 MiB. A record larger than an uncompressed frame
of the agreed size is not a frame by definition, so "oversized" says something
about this session instead of naming a number. As in the agent protocol, a
record over its cap is unrecoverable -- the stream is parked on a body of
unknown length -- so the connection closes rather than skipping past.

Four records open a session, and the authentication is mutual because the
reversed direction demands it: any process inside the guest can squat the
service port before the real service binds it, so the host must be able to
tell the two apart, exactly as the guest must be able to tell VMLord from
anything else that reached the socket. The host sends `ClientHello` with a
session id and a nonce, the guest answers `ServerHello` with its own nonce and
what it supports, then proves itself with `ServerAuth`, and only then does the
host prove itself with `ClientAuth`. The guest goes first because the host must
not act on an unauthenticated peer, and a tag harvested by a fake host is worth
nothing: it is a MAC over a transcript whose nonces will differ next time.

The transcript is a SHA-256 over the two hello payloads *as they arrived on the
wire*. Protobuf does not promise that a message encodes to the same bytes
twice, so a transcript over a re-encoded message is one two correct
implementations can disagree about -- and that is also why the tags are their
own records rather than fields inside the hellos, which would force a side to
re-encode a message with the field cleared in order to hash it.

The key under those tags is derived from the per-VM secret the agent protocol
already mints, through HKDF with the nonces as salt and the session id in the
info. Nothing new is minted and nothing new is delivered. The unprivileged
capture process never holds the secret: the privileged broker, which is root
anyway because it needs DRM and uinput, derives the session key and hands only
that on, so compromising the capture process costs one session rather than the
VM's identity. The frame, input, clipboard and audio channels then get a key of their own
from that session key and the transcript hash, and prove it in a three-record
exchange of their own; because the channel key depends on the transcript, a socket cannot
be carried in from another session or offered by a process that took no part in
the control handshake.

Losing control ends the session -- the guest stops capturing, the host closes
the other two sockets, and the viewer starts again with a new session id.
Losing frame or input alone does not: that channel reconnects within the same
session at the next `generation`, and records still in flight from the previous
connection are rejected by the header before they reach a decoder or an input
device. A reconnected frame channel owes `StreamConfig` and a keyframe before
any delta, since a delta has nothing to apply to, and a reconnected input
channel owes a release-all -- which the guest also performs on its own the
moment the channel drops, because a key stuck down is worse than a lost
session.

The stream is neither encrypted nor authenticated per record after the
handshake, and that is a decision rather than an omission. This is a
point-to-point stream inside the hypervisor: injecting into an established
HvSocket stream requires a privilege under which everything else is already
lost, and confidentiality here comes from the partition boundary. A MAC on
every frame would cost a standing percentage of CPU in the hot path against a
threat this transport does not have. The agent protocol makes the same trade.
The CRC32C in each header is a corruption check, not a signature.

Nothing acknowledges a frame either. The guest regulates its own stream through
the encoder's bounded queue -- a newer frame displaces an older one that has
not been sent, so what is queued is always current state -- and a viewer that
falls behind receives one fresh frame rather than a backlog. The frame flow's
only back edges are `RequestKeyframe`, which is recovery for a decoder that
lost synchronisation, and `Ping`/`Pong`, which is how a slow viewer is told
from a dead one.

The control channel carries one record in the other direction. A
`CONTROL_RECORD_GUEST_COMMAND` is the guest's user raising one of the commands
the viewer's system menu offers -- full screen, the Secure Attention Sequence,
handing the keyboard back, mute, a quality, a mode from the host's list -- and
the guest raises rather than acts, because every one of those is a property of
the window on the host and there is nothing inside the guest for any of them
to change. The viewer answers it where it answers the menu: `ui_event_of` in
`crates/display-viewer/src/main.rs` turns a command into the `UiEvent` its
menu counterpart would have raised, and `handle_ui_event` -- which is the one
place any of them is answered from -- does the rest, the record having arrived
from `live.rs` as `Signal::GuestCommand`. Menu and guest cannot grow apart
that way: both halves of a session get the same answer to the same request
because both are answered by the same code. A command this build has no name
for is dropped the way an unknown record is, and so is a mode command whose
timing does not parse.

`MODE_AUTO`, `MODE_DESKTOP` and `MODE_MOTION` all exist in the contract, and
the MVP guest announces `MODE_DESKTOP` alone. `MODE_AUTO` names a host-side
policy that resolves to `MODE_DESKTOP` until a motion codec exists; a request
for `MODE_MOTION` is answered with `ERROR_CODE_UNSUPPORTED_MODE`.

Connect opens this native stack. Every VM's compute system
lists all five services beside the agent's -- an entry there is the
partition's permission for a service to exist, not a claim that the guest
binds it, so a headless VM lists them too and a VM created before they existed
has to be recreated rather than migrated. The repository refuses a session
before it starts one, a sentence per reason: a VM created without a desktop, a
desktop still installing, a VM that is not running, a guest that has not
offered its display. What gets past that is one viewer process per Connect.
VMLord distributes no AppSandbox host IDD or guest display/input artifact: the
display path is the native one described here, end to end.

### The guest display services

Five programs, one crate (`crates/display-services`), all static musl binaries
built by `cargo display-services`. The two described here are the capture
pair; the clipboard daemon, the audio daemon and the tray each have a section
of their own below. The pair is two rather than one because exactly one thing
on the guest side needs privilege: `DRM_IOCTL_MODE_GETFB2` will not hand a
framebuffer's handles to anything without `CAP_SYS_ADMIN`. Everything that
runs hot -- mapping, comparing tiles, encoding, writing sockets -- is the part
most likely to hold a bug worth exploiting, so it runs as `vmlord-display`
with `CapabilityBoundingSet=` and nothing to steal.

`vmlord-display-broker` is root and small. It holds the DRM device, the VM's
secret and the control channel, and it is an ordinary DRM client that never
takes master -- the compositor holds that. `vmlord-display-session` is
unprivileged and holds a read-only mapping and one channel key per socket, each
good for one session and no longer.

Between them is a root-owned `SOCK_SEQPACKET` socket at
`/run/vmlord/display-broker.sock`. `SO_PEERCRED` is checked on accept, so a
process that is not the service user is refused before it has said anything.
Pixels cross it as dma-buf descriptors over `SCM_RIGHTS`, exported without
`DRM_RDWR`: the unprivileged half maps the guest's own scanout buffer read-only
and root never copies a frame. Each buffer crosses once and is named by its
framebuffer id afterwards, since a descriptor costs a syscall and a slot in the
peer's table.

The five vsock ports are the protocol's: `VMLD` control, `VMLF` frames, `VMLI`
input, `VMLC` the clipboard, `VMLS` sound. The guest listens on all five and
the host connects, and four processes divide them -- the broker owns control,
the session process owns frames and input and never sees a device descriptor or
an ioctl of its own, the clipboard daemon owns the fourth, and the audio daemon
owns the fifth. The tray is the fifth process and owns none of them: it binds
no vsock and is never handed a channel key, because everything it has to say
crosses its own socket to the broker. What crosses the IPC socket after a
handshake is a `SessionParameters`: the session id, one channel key per socket,
the geometry, and whether the peer took the cursor stream. Not the secret. What
a compromised capture process could take from those bytes is one session, and
only while that session runs.

The three disconnect obligations are honoured where they are owed. A frame
channel that binds sends `StreamConfig` and then a keyframe before any delta,
because a decoder that has just been built has nothing to apply a delta to; a
reconnect binds at the next generation and starts again the same way, and the
loop notices a dropped socket by watching it for hangup rather than by
discovering it on the write that fails. Losing control ends the session: both
sockets are shut down and nothing more is asked for, because a process that
keeps asking for frames is one that never stopped capturing. An input channel
that is lost releases whatever the guest still holds before anything else,
because a key left down is worse than a session left broken.

Packaging runs from `cargo display-services` through `payloads/display/prepare.sh
--services`, which installs the five binaries and the five units into the
payload's `content/services/`. They are built by the host toolchain and not in
the payload container: a static musl binary is identical for 22.04, 24.04 and
26.04, and that container exists to prove the *module* compiles against a
release's headers. `pack` then refuses a recipe whose declared protocol range
does not contain the version this build speaks -- the range used to be a
placeholder, and the services in the archive are what make it a claim. Inside
the guest, `vmlord-agent`'s `SERVICES` and `SERVICES_START` recipe stages
install them by content digest, create the `vmlord-display` account, enable the
three system units for the boot and the two user ones -- the clipboard daemon's
and the tray's, which are wanted by a session rather than by the boot -- for
whichever session starts next, and wait for the socket between the capture
pair; a payload that carries no services is skipped rather than failed, because
every payload built before this is one of those.

### The native display viewer

`crates/display-viewer` produces `vmlord-display.exe`: one process per display
session, launched by VMLord with two anonymous pipes as its standard input and
output. It is a process of its own because a session outlives the application
that opened it -- closing or crashing VMLord leaves the desktop on screen, and a
viewer that crashes does not touch the VM.

The master secret stays in VMLord. The viewer holds the control socket and
VMLord holds the secret, so neither can run the handshake alone: the viewer
frames records off the socket and passes the bytes up the pipe without parsing
them, VMLord drives its own `Session` over them, and when the handshake
completes it hands over the session id, what was negotiated, the control
sequence and two `ChannelKey`s. Those keys are good for one session and no
longer, and nothing sensitive is ever on the command line or in the environment
-- the viewer takes no arguments at all. `Session::established_host` is the
protocol crate's side of that crossing: an established host session with no
secret and no session key, whose channel keys were given rather than derived.

From the hand-over on, the viewer is autonomous. It owns all three `AF_HYPERV`
connections -- control `VMLD`, frame `VMLF`, input `VMLI`, addressed by the
partition's runtime id and a service GUID derived from the vsock port -- pings
every five seconds, rebinds a frame channel at the next generation when a record
cannot be decoded, and asks VMLord for a fresh `ClientHello` (with the token it
was launched with, so only that VMLord can answer) when control is lost. Frames
are decoded by `vmlord-display-codec` and uploaded to a D3D11 texture as the
rectangles that changed, never as whole frames.

One window per VM. A named mutex `Local\VMLord.Display.{runtime-id}` answers
whether a viewer already exists before anything else happens, and a named pipe
`\\.\pipe\vmlord-display.{runtime-id}` -- authenticated by the launching
user's default DACL -- carries the only two things a later VMLord may ask for:
focus and close. Asking for a *new session* is deliberately not on that pipe.
So a repeated Connect focuses the window that is there rather than opening a
second one. The VMLord process makes that decision from the launch worker it
already owns. A newly started VMLord owns no such worker: its new viewer asks
the orphaned old one to close, waits for the mutex, and continues with the new
process's launch pipes. Focusing the orphan would leave it unable to request a
fresh handshake from either VMLord.

`unsafe` lives in `src/windows/{hvsocket, ipc, window, d3d, hook}.rs` and
nowhere else in the crate: the workspace denies it, and those five module
declarations are the only places it is re-allowed. Every other decision the viewer makes -- the
status machine and its thirty-second retry budget, the decode path, the launch
contract, the overlay's geometry -- is safe Rust with tests that need no
partition. Framebuffer and cursor content is never logged, and this build has no
screenshot feature.

VMLord's half of that crossing is `platform::display_session`: a state machine
over launch-pipe messages that holds the VM's secret and the protocol
`Session`, answers a relayed record with the record to send back, and answers
an established handshake with the hand-over. It is driven by
`platform::display_launches`, which starts the process, writes its launch
parameters and keeps a thread on its pipes -- a thread deliberately never
joined, because a display session outlives the application that opened it:
VMLord exiting closes the pipes, which costs the viewer the right to ask for a
fresh session and nothing else. The desktop remains visible while its existing
session lives; a Connect from a later VMLord replaces that viewer so the next
handshake has live pipes. Stopping a VM does close its window: the
`SystemExited` event names the VM, and `DisplayLaunches::close` asks the viewer
it opened for that VM to close over the command pipe, addressed by the runtime
id the launch recorded. The forced stop asks straight away rather than waiting
for the event it also has no guarantee of. The command pipe rather than the
launch pipe, because that is the channel the window itself reads -- and only an
exit, so a guest that reboots keeps both its compute system and its window,
which reconnects when the guest's services come back. A window this process
never opened is left to notice on its own: the partition goes and its next
connect fails with it. A repeated Connect is answered by the worker registry
and focuses the existing window without starting another process.

### Keyboard and mouse

The guest has two input devices, not one: `VMLord Keyboard` and `VMLord
Pointer`. libinput classifies a device by its capability bits, and a node
carrying keys, absolute axes and buttons at once is resolved by heuristics that
have changed between releases -- while this MVP has to behave the same on 22.04,
24.04 and 26.04. Two unambiguous nodes remove the question.

The broker creates them, because `/dev/uinput` is root's, and hands the
descriptors to the session process over the socket that already carries
framebuffers. That is not only cheaper than relaying every mouse movement
through a second hop: when the session process dies its descriptors close, the
kernel unregisters the device and releases every key it believed was held. "No
stuck keys after a crash" is then the kernel's property rather than our
diligence. A guest whose kernel has no uinput shows a read-only desktop and
reports it, rather than failing to show one at all -- the same rule the DRM side
follows.

The absolute axes are declared `0..32767` once and never again. Deriving them
from the resolution would mean recreating the device on every change of it,
which #120 makes an ordinary event, and the desktop would watch its pointer
disappear and come back; the session scales guest pixels onto the fixed range
instead.

That scale aims **inside** the pixel rather than at its edge, because the value
makes a round trip: libinput reads an absolute axis back as `value * size /
32768`, so each pixel owns an interval of the range, and a value on the
interval's boundary comes back as the pixel next door -- at 1920 wide, pixel 1
was arriving as pixel 0. Three eighths of the way in is far enough from either
edge that no rounding leaves the pixel, and short enough of a half that the
cursor plane's position, which the host measures the hotspot from, rounds down
whichever rule a compositor rounds by. The wheel travels at both resolutions: `REL_WHEEL_HI_RES` in the
hundred-and-twentieths the wire uses, and the whole detents they add up to, with
the remainder carried so slow scrolling is not lost.

On the host, deciding and catching are separate. `placement.rs` says where the
picture sits on the client area and maps a client point to a guest pixel;
`input/` holds the scan-code table and the policy -- focus, hover, what is held,
and every path that owes a release. Neither touches a Windows API, so the rules
are tested anywhere. `windows/window.rs` catches the mouse, the focus and the
system menu; `windows/hook.rs` is a `WH_KEYBOARD_LL` hook, installed on focus
and removed on its loss, which is the only way `Super`, `Alt+Tab` and `Ctrl+Esc`
reach GNOME rather than the Windows shell.

The cursor's **hotspot** is worked out on the host, because the host is the
only end that has it. Mutter subtracts the hotspot before capture sees
anything: the cursor plane sits at the pointer's position minus it, the plane
carries no hotspot property to read it back from, and the position record
therefore names the bitmap's corner. Windows anchors a cursor by its hotspot,
so a bitmap handed to it with zeros lands with its corner on the pointer and
every click goes a few pixels up and left of the arrow -- half a bitmap for the
I-beam. `cursor.rs` recovers it by subtraction: the position last sent to the
guest, minus the corner the guest reports, is the hotspot. The two are only the
same instant while the pointer stands still, so the subtraction is trusted on a
record with no motion sent since the previous one and the same corner as the
previous one, and a difference landing outside what the bitmap actually draws
is refused -- a cursor plane is 64x64 whatever sits on it, and the padding
points at nothing.

A hotspot belongs to a bitmap and is measured **once**, because a hotspot that
moves is a cursor that jumps. The subtraction is exact only in principle: this
end sends a guest pixel, the guest's pointer is a float that came back through
the absolute axes' fixed range, and the plane's corner is rounded, so the same
shape measures a pixel apart at two ends of the screen. The guest sends its
cursor with every frame whether or not it changed, so the bitmap is compared
with the one on the window: an identical one is neither a measurement nor an
icon rebuilt sixty times a second. Shapes already measured are remembered,
which keeps the arrow and the I-beam of an afternoon's editing to one movement
each.

Keys are carried as **scan codes**, not virtual keys. A virtual key has already
had the host's layout applied to it and the guest then applies its own, so a
host on a non-US layout sends the wrong keys and breaks `Ctrl`+letter; a scan
code is a position on the keyboard, and the layout stays entirely the guest's.
Three keys are the exception -- `Pause`, `NumLock` and `PrtScn` -- whose scan
codes Windows reports ambiguously and which carry no layout to get wrong.

While the hook is installed the keyboard is the guest's, `Alt+F4` and `F11`
included, so **`Ctrl+Alt+Left Shift`** is reserved: it hands the keyboard back,
and the guest is sent a release. `Ctrl+Alt+Del` is a system-menu action rather
than a shortcut, because the Secure Attention Sequence is routed by the kernel
and no documented hook sees it -- and undocumented ones are out of the
question.

### The clipboard

A selection exists only inside a compositor, which is what makes the clipboard
unlike everything else in this stack: frames come off a DRM device, input goes
to uinput devices, and both are blind to who is logged in. So the clipboard has
a component of its own -- `vmlord-display-clipboard`, a **user** unit that
starts with the graphical session, because only a process inside that session
can reach the session bus where mutter answers.

It reaches the clipboard through `org.gnome.Mutter.RemoteDesktop`, the
interface `gnome-remote-desktop` drives, which has carried a clipboard since
GNOME 42. Three of its properties decided the design and none are documented:
a session may be started with no ScreenCast beside it, so this is a small
daemon rather than a screen-sharing stack; `EnableClipboard` with mime types
makes a client the owner and mutter then refuses to let it read its own
selection, so the daemon listens until the host actually sends something; and
the descriptor `SelectionRead` returns is non-blocking, so reading a selection
is a poll loop -- which is where the size cap and the deadline live.

Two more properties shape what a user sees rather than the code. Mutter
inhibits session creation entirely while the screen is locked, so the daemon
comes and goes with the lock and retries rather than failing; and it announces a
selection only when ownership *changes*, so a daemon that attaches after a copy
has already happened learns nothing about it until the next one.

The daemon holds no secret and binds `VMLC` itself, with a key the broker sends
it over a second socket, `/run/vmlord/display-clipboard.sock`. That socket
cannot be owned by a group the way the capture process's is: the daemon runs as
whichever human logged in, and that account is not known when the VM is
provisioned. It is authorised by peer credentials instead -- the uid must be
the one logind reports for the active graphical session on `seat0`, looked up
at every accept. The clipboard therefore belongs to the person at the virtual
screen: a second user over SSH cannot take it, and a daemon left behind by a
user who has been switched away stops being authorised without anything having
to evict it.

Both ends run one state machine, `vmlord_display_protocol::clipboard`, which is
where the allowlist, the caps and the cancellation rules live. A limit only the
viewer enforced would be one a guest could ignore. The model is pull in both
directions: a side announces what its selection can produce and sends nothing
until the other asks, so a picture copied in a guest costs nothing until
somebody pastes it. Text, HTML and one picture are carried -- `image/bmp` in
preference to `image/png`, because a DIB is a BMP without its file header and
needs no codec. Arbitrary registered Windows formats are not passed through,
which is what AppSandbox did and what an allowlist exists to refuse.

**Files are carried, under a capability of their own.** Protocol 1.3 adds
`CAPABILITY_FILE_CLIPBOARD`, which is settled only beside the ordinary
clipboard capability and only at that revision, so an older peer never receives
a record it has no name for. File transfer is a second state machine,
`clipboard::files`, on the same channel: one transfer in each direction, its
own identifiers, its own cancellation, and no interaction with a text or
picture transfer running beside it. The viewer states the limits it was
configured with as soon as a file-capable channel binds, and the guest offers
nothing until it has heard them.

The model is still pull. Copying a directory announces one record; enumeration
and reading begin when the other side asks, and each side then sends one entry
or one 60-KiB chunk per turn round its loop, so a tree of a thousand files
cannot hold focus, the socket or an ordinary selection behind it. The sender
walks depth-first, sends each regular file in order, and finishes with a
`FileComplete` that is the only thing that publishes a tree: before it, nothing
partial is on anybody's clipboard.

Only regular files and directories cross. Linux opens every component from the
one above it with `O_NOFOLLOW`; Windows opens with `FILE_FLAG_OPEN_REPARSE_POINT`
and judges the handle's attributes, so a junction is refused rather than
followed. A symlink, reparse point, socket, FIFO or device ends the whole
transfer rather than producing a tree that is quietly not what was copied.
Every wire path is relative UTF-8 with `/` separators and is checked lexically
before anything is opened: no empty, `.` or `..` component, no NUL, control
character, colon, backslash or `<>"|*?`, no component ending in a dot or a
space, no Windows device name such as `CON` or `COM1` with or without an
extension, at most 1024 UTF-8 bytes and 64 components. Two entries that are one
file under Windows' case-insensitive comparison are refused, so a tree either
arrives whole on both platforms or does not arrive.

An arriving tree is staged in a fresh private directory --
`$XDG_RUNTIME_DIR/vmlord/clipboard/<session>/<transfer>` in the guest, which
has no `/tmp` fallback, and `%LOCALAPPDATA%\VMLord\Clipboard\<session>\<transfer>`
in the viewer -- with every destination created new and every component opened
without following what it might stand for. Containment comes from directory
handles and attributes, never from comparing the text of a path. When the tree
is whole, what is published is its top-level entries: `CF_HDROP` on Windows,
`text/uri-list` and `x-special/gnome-copied-files` in GNOME. Cancellation,
focus loss, a validation failure, a five-second silence or a lost channel
closes the handles and removes the partial tree at once.

Windows clipboard data outlives the process that wrote it, so a completed
Windows tree is not deleted when the viewer exits: the viewer removes stale
incomplete trees at startup and completed ones older than the configured
retention, without ever descending through a reparse point. Guest staging goes
with the user's runtime directory.

The limits are the host's settings, and both ends enforce the narrower of what
each holds:

```toml
[clipboard_files]
max_file_size = "1GB"
max_transfer_size = "4GB"
retention = "24h"
```

Those are the defaults, and an absent table or field takes them, so no settings
file needs migrating. Sizes are a positive integer and, case-insensitively,
`B`, `KB`, `MB` or `GB`, which are binary multiples -- the default `1GB` is
exactly one GiB. Durations are a positive integer and `s`, `m` or `h`.
Whitespace, fractions, compound values, unknown units, zero and overflow are
refused, and `max_file_size` may not exceed `max_transfer_size`. There is no
settings UI and no separate toggle: file transfer is on wherever ordinary
clipboard sync is, which means while the viewer has keyboard focus. Entry
count (4096), depth (64), wire-path length (1024 bytes) and one transfer per
direction are protocol constants rather than settings.

Two rules keep it honest. A selection crosses only while the viewer's window
has keyboard focus -- in *both* directions, so a VM in the background can
neither read what its user copies elsewhere nor replace what is on their
clipboard. Neither side loses what happened while the window was away: the host
announces its own clipboard when focus returns, and an announcement the guest
made in the meantime is held and handed on then. Holding the announcement holds
the selection with it, because the model is pull: nothing is asked for and so
nothing crosses. And each side suppresses its own
echo with what it already has -- the host with `GetClipboardSequenceNumber`
taken as it writes, the guest with `session-is-owner` out of mutter's
`SelectionOwnerChanged` -- because without that, applying the other side's
selection would immediately offer it back.

Nothing about a selection is logged, files included. A mime type, a byte count,
an entry count, a transfer id and an outcome are what a clipboard problem is
diagnosed from, on both sides; a file name and a file's contents are the two
things that must never be diagnosed from.

### Sound

What the guest's desktop plays is heard on the host, over `VMLS`, the fifth
channel. It is one-directional after the bind: there is no microphone, and the
one control a user has -- mute -- is answered entirely on the host, whichever
side raised it.

The guest end is `vmlord-display-audio`, and it is a **system** unit, which is
the opposite of the clipboard daemon and for the opposite reason. A selection
belongs to whoever is at the screen; sound belongs to the machine. What it
reads is an ALSA loopback: `snd-aloop` is loaded from `modules-load.d`, the
desktop's PipeWire plays into the loopback's playback half because it is the
only card a VM has, and the daemon reads the capture half. PipeWire is
therefore not in the hot path at all, and the daemon needs no session, no bus
and no login -- only membership of the `audio` group, which is what `/dev/snd`
is owned by. It takes its channel key from a third broker socket,
`/run/vmlord/display-audio.sock`, authorised by the socket's own permissions
rather than by a logind lookup.

It speaks to the kernel directly. `libasound` would be a system C library, and
the guest side is built for musl with no C toolchain, so the eight PCM ioctls
and three structures it needs are written out in `alsa/uapi.rs` the way the DRM
ABI is in `drm/uapi.rs`. A layout test holds the structures to the kernel's,
because a field that moved is an `EINVAL` with no explanation.

The desktop's own output is a **statically declared PipeWire node** bound to
`hw:Loopback,0,0`, not a card PipeWire discovered. That is not a preference:
PipeWire's ALSA monitor refuses a whole card while any of its PCM devices is
held by somebody else -- `card 0 pcm device pcm1c busy`, `ALSA card 0
unavailable (Device or resource busy)` -- and the daemon holds exactly that
device for the life of a session. A monitored card would therefore disappear
from the desktop the moment sound began working, leaving a Dummy Output with
nothing behind it. A declared node is opened by PipeWire itself, skips that
check, and removes the race between a login and the daemon's first capture.

**Whichever half of the loopback opens first fixes the other**, which is what
decides the format. The daemon starts at boot, so in the ordinary case it opens
first and pins S16_LE, 48 kHz, stereo, in 480-frame periods -- 192 KB/s on the
wire, one record every ten milliseconds, and a capture latency equal to the
period and nothing more. A daemon restarted into a desktop that is already
playing arrives second and is handed whatever PipeWire pinned; it does not
argue, it announces what it got in an `AudioFormat` record and the host
converts. That is the whole of the format negotiation: a statement of what the
kernel allowed rather than a conversation.

A period of pure silence is not sent, after ten consecutive ones. An idle
desktop therefore costs nothing rather than a megabit and a half, and neither
end needs a record to say so: a period's position in the stream travels in the
header's `base` field, the way a tile delta carries the sequence it builds on,
so a gap -- suppressed silence, or periods dropped under load -- is arithmetic
the host does rather than something it is told.

Nothing waits. The guest holds a 200-millisecond ring and drops its oldest
periods rather than blocking the ALSA reader on the socket; the host drops a
period that does not fit the endpoint's buffer rather than queueing it. Late
audio is worse than absent audio, and "audio does not delay a frame" is a
structural property this way rather than a promise about scheduling: a separate
process in the guest, a separate socket, a separate thread in the viewer.

On the host the viewer plays through WASAPI in shared mode, initialised with
the guest's format and `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM` so that the
endpoint converts rather than the guest. **A change of output device rebuilds
the endpoint and not the channel.** `IMMNotificationClient` reports a new
default, a disabled endpoint or a first endpoint appearing; the audio thread
tears down the client and activates the new one between periods, and the
channel stays bound throughout -- what a person plugged into their host is not
a fact the guest knows or should, and a rebind would cost a round trip that can
fail. AppSandbox restarted the whole socket here because its renderer and its
socket shared a loop; that was a consequence of its shape, not a decision.
If there is no endpoint at all the renderer parks, keeps draining the channel,
says so once rather than once a period, and revives on the next notification.

Mute is a system-menu item, remembered per VM beside the window's size. A muted
renderer keeps taking periods and discards them instead of stopping the stream,
which is what makes unmuting immediate and leaves the guest's pacing and the
pinned format alone.

`CAPABILITY_AUDIO` is announced by a guest whose payload ships the daemon,
whether or not anything is playing and whether or not anyone has logged in --
the clipboard's reasoning, for the same reason: a session commonly opens at the
login screen, and a capability cannot be renegotiated once the handshake has
settled it.

No sample is logged. A format, a frame count, a stream position and an outcome
are what an audio problem is diagnosed from, on both sides.

### The tray

The person at the guest's screen has no window of their own -- the viewer's
window is on the host -- so the commands they want are raised from a tray
icon. `vmlord-display-tray` is a **user** unit that starts with the graphical
session, like the clipboard daemon and for the same kind of reason: a tray
icon belongs to whoever is at the screen, and StatusNotifierItem is answered
on the session bus. Its SNI side is `ksni`, pure Rust on the same zbus the
clipboard daemon speaks mutter through, so the musl build stays
toolchain-free; the icon is a pixmap the binary carries rather than a theme
name, because a guest has no icon theme VMLord can count on.

GNOME shows a tray icon only through its AppIndicator extension. Installing
that is the recipe's business: the `SERVICES` stage asks the guest's own
package manager for the `gnome-shell-extension-appindicator` package, best
effort and
skipped where the desktop already has one on disk under either of the UUIDs
the supported releases ship it under, because what a guest is missing without
it is the icon and not the desktop. *Enabling* it is the session's business,
and deliberately so: the tray calls `org.gnome.Shell.Extensions.EnableExtension`
when it starts and on every reconnect to the broker, rather than the recipe
writing gsettings as root. `EnableExtension` adds one UUID and keeps the rest
of the user's own list, which no write from outside the session can do without
clobbering either the desktop's defaults or the user's choices -- and the
recipe runs as root before any session, where there is nothing to ask and no
list worth owning. Until the call succeeds the icon is merely absent, and the
next reconnect is the next attempt.

The menu mirrors the viewer's system menu, in its order: full screen, the
Secure Attention Sequence, release keyboard, the resolution list, mute, the
two qualities. The tray decides nothing. Each of those is forwarded over a
fourth broker socket, `/run/vmlord/display-tray.sock`, as a `UserCommand`
datagram whose kinds are the wire's `GuestCommandKind` one for one, and the
broker forwards the record whole rather than deciding it -- a test beside the
mapping is what notices the two schemas drifting apart. The socket is
authorised the way the clipboard daemon's is: the connecting uid must be the
one logind reports for the active graphical session on `seat0`, looked up at
every accept. The Resolution submenu is the one part that is an answer rather
than a command: the tray asks which modes the host has offered and is told,
the same list and the same selection the connector thread already holds --
one fact read by a second reader, not a second authority on it.

A command raised while no session is open is answered with a report rather
than kept, because the tray asked for something on the host and is told there
is nothing to ask; one that races a session's end is dropped by the epoch it
was raised under. An action the broker cannot carry out comes back the same
way and lands in the guest's journal, because a menu click that did nothing
is at least one that says so.

*Restart services* is the one item that is not a forwarded command, and it is
a split. The broker restarts the system units -- the capture process and the
audio daemon, over the system bus -- and never itself, because the process
answering the restart has to live through it. The clipboard daemon's unit
lives on the very bus the tray is already on, which the broker has no business
on, so the tray puts that one back itself, in the same click.

Nothing short of its own failure ends the tray. No StatusNotifierWatcher yet,
no broker yet, a broker that went away: each is waited through with a
backing-off reconnect rather than exited over, because a unit that restarts on
the ordinary shape of a session starting up spends its crash-loop budget on
nothing. The reconnects are also how a payload applied mid-session gets to
install what its first try missed -- the AppIndicator extension among it.

### Resizing the desktop

A size is never scaled into a rectangle it does not belong in: whichever end
the resolution was decided at, the other end takes it. Dragging the viewer's
edge changes the desktop inside it, and choosing a mode -- from the window's
*Resolution* submenu or from the settings inside the guest -- changes the
window around it. The whole path exists to make that certain rather than
likely.

**The host holds a size until it stops moving.** A drag is hundreds of
`WM_SIZE` messages and each one taken at face value would be a mode set, a
hotplug, a compositor commit and a keyframe; `resize.rs` waits 250 ms of
stillness before asking for one. What it sends is the *physical* client area:
the viewer calls `SetProcessDpiAwarenessContext` with per-monitor v2 before it
opens anything, because an unaware process is handed a virtualised rectangle --
1707x960 on a 2560x1440 panel at 150% -- and a viewer that set the guest's mode
from it would put a small desktop on a big screen and then blur it back up.

**The guest validates and applies.** The broker rounds the request to what
`drm_cvt_mode` builds (a width to a multiple of eight, a height to an even
number), refuses anything outside 640x480..2560x1440 with
`ERROR_CODE_RESOLUTION_REJECTED`, and writes what is left to
`/sys/module/vmlord_drm/parameters/mode` as `WIDTHxHEIGHT@HZ`. The module marks
that mode preferred and hotplugs the connector; it cannot commit a mode,
because the compositor holds DRM master and this is an ordinary client.

**The connector's list is the host monitor's.** The viewer enumerates the
screen its window sits on -- `MonitorFromWindow`, `GetMonitorInfoW`,
`EnumDisplaySettingsW`, and `DisplayConfigGetDeviceInfo` for the panel's own
preferred timing -- normalizes what it finds, and publishes it with
`SET_AVAILABLE_MODES`; the selection follows in `SET_DISPLAY_MODE`. The guest
writes the list to `/sys/module/vmlord_drm/parameters/modes` before the
selection, because a mode marked preferred while the connector still offers the
old list is a hotplug onto a mode about to be withdrawn. At most 32 modes and
512 bytes, parsed into a fixed array and swapped whole: a list the module
refuses leaves the guest offering the modes it already had.

**Modes are three integers and nothing else.** Width, height and whole hertz,
deduplicated by all three and sorted by pixel area, width, height, then refresh.
Refresh is an integer because `dmDisplayFrequency` is one -- Windows reports 60
for a 59.94 Hz panel, and pretending otherwise would be a precision the source
does not have. Nothing above 144 Hz is offered anywhere: that is the ceiling of
the wire format and of `drm_cvt_mode`'s caller. No EDID is synthesized in either
direction; the physical size stays at 96 DPI, which is what keeps a compositor
on scale 1 through a resize.

**Every size carries every rate it is offered at.** The ceiling that decides
this is the guest's encoder. A delta frame used to gather each tile twice -- the
captured one and the reference -- to find out whether they differed, which on a
desktop they almost never do; it now compares the rows where they lie and
gathers only a tile that changed. What made that the ceiling rather than merely
wasteful is that the guest links musl, whose `memcpy` and `memcmp` have no SIMD
and run at about a quarter of glibc's, so the copying was the whole workload.

`cargo display-bench --scene S --width W --height H` measures it. Inside a guest
with eight virtual processors, on the heaviest of the desktop scenes and in mean
milliseconds a frame:

| size      | before | after | share of the 6.94 ms a 144 Hz frame gets |
|-----------|--------|-------|------------------------------------------|
| 1280x720  |   2.28 |  0.55 |  8% |
| 1600x900  |   4.30 |  0.97 | 14% |
| 1920x1080 |   5.50 |  1.54 | 22% |
| 2560x1440 |  11.61 |  2.53 | 36% |

The before column is why 1920x1080 at 144 Hz looked worse than the same size at
60. It fit, at four fifths of the interval, until anything at all went wrong --
and an encode that misses one interval does not cost a frame here and there.
Capture is paced by the output's vblank, so it waits for the next one and the
stream alternates between one interval and two. Ninety uneven frames read worse
than sixty even ones. At a fifth of the interval that margin is no longer thin,
and 2560x1440, which did not fit at all, now does.

The largest size the viewer will publish uses a third of the interval, so the
menu is cut to nothing: what a monitor drives, the guest encodes.

Naming one scene is not optional when comparing builds. The scenes share a
process, and one that has just walked eight megabytes leaves the caches to the
next; measured together, this change reads as a regression on two of the scenes
it in fact improves by a third.

Scrolling and full-screen video are the exception, and stay so: they change
every tile, so the copies are the work rather than the waste, and 1920x1080
costs 14.5 ms whatever the mode is. That is a limit of the content and not of
the refresh -- 60 Hz does not carry it either -- so nothing about the mode list
would help, and it is what the delivered-frames diagnostic is for.

Damage would narrow this further and is deliberately not plumbed. The module
does advertise `FB_DAMAGE_CLIPS`, and mutter does populate it: instrumented over
a live GNOME session, 396 of 618 plane updates carried clips, most of them a few
thousand pixels against a frame of two million. It buys nothing worth its cost
now -- a desktop frame already fits four times over, and the scenes that do
overrun are exactly the ones whose damage is the whole frame.

**Selection is deterministic.** A selection the new list still offers is kept;
otherwise 1920x1080@60 when it is there; otherwise the greatest resolution and,
among its refresh variants, the greatest refresh; and when enumeration finds
nothing at all, a synthetic 1920x1080@60, because a connector with no modes is
an output no compositor lights. An explicit choice from the window's
*Resolution* submenu is remembered per VM in the viewer's state file and is
considered before every fallback. A monitor change -- `WM_DISPLAYCHANGE`, a
move onto another screen, a DPI transition -- is debounced for the same 250 ms
as a resize and re-published only when the normalized list actually differs.

The preferred mode is offered whether or not it is in the list, which is what
keeps a dragged window the authority on this output's size: a geometry nobody
enumerated is still a mode the connector will carry, at the refresh the host
selected. On a hotplug the compositor re-derives its configuration and finds
the preferred mode first and marked, which is what it moves to.

**Nothing is reported until it has happened.** A `SetResolution` is answered
with silence. The size the session runs at is read off the primary plane's
framebuffer by the thread that captures it, never off the mode that was asked
for, and only when that changes does anything move: the capture process is told
first and directly, ahead of the snapshot carrying the first buffer of the new
size, so its encoder is rebuilt before a frame of the new shape reaches it; the
host then gets a `DisplayState` from the control thread. A frame captured on
the wrong side of a commit is dropped rather than encoded -- a tile grid is
built on a geometry and cannot take another shape.

The refresh in that `DisplayState` is read the same way: off the CRTC's own
timing with `DRM_IOCTL_MODE_GETCRTC`, as the pixel clock over what a frame
spends, never off the mode that was written. Zero means the CRTC would not say,
which is a guest whose desktop has not come up yet.

**A picture that is too slow is said out loud.** The viewer counts frames that
both decoded and presented, one second at a time, against the refresh the guest
confirmed. Delivered FPS under `display.fps_gap_threshold_percent` of it (50 by
default, 1..100) for ten unbroken seconds is one `Warning` diagnostic naming the
mode, the rate and the share -- and only one: another needs ten unbroken seconds
back at or above the threshold first, so a session hovering on the line is one
line in the log rather than one every ten. Measurement pauses outside a running
visible session, because a minimised window and a session that is still coming
up are seconds of nothing rather than a guest failing to keep up.

**The picture never goes black or stretches.** `placement.rs` letterboxes:
scaled to fit, whole, centred, aspect ratio kept. Nothing is cropped, because a
desktop with a corner the user cannot reach is worse than one with bars, and
nothing is stretched, because a desktop whose circles are ovals is worse than
either. The renderer copies rather than samples whenever the picture is the
size of the rectangle it goes in, which is what a settled window and a settled
guest always are; the sampled path is the seconds in between. Across a
`StreamConfig` the old texture is kept and drawn scaled until the new one has
its first keyframe -- the alternative is a black window for as long as a
compositor takes to commit a mode.

**The loop is closed by construction.** The guest's answer is usually not the
request -- 1727 asked for, 1720 applied -- so a viewer that compared the answer
against its own window would ask again, forever. What is compared is request
against request: a size already asked for is never asked for twice, and the
difference between the answer and the window is exactly what the letterbox is
for. A new session forgets, because the guest that was told is not the guest
that is listening.

**A mode chosen rather than dragged moves the window.** Both arrive as the same
`StreamConfig`, so what tells them apart is arithmetic: the broker's rounding
rule is mirrored in `resize.rs`, and a geometry that is not what the last
request would have produced is the guest going its own way -- the submenu, or
GNOME's own display settings. Then the window takes that geometry as its client
area, because a 2560x1440 desktop letterboxed into the 1848x1048 the window
happened to be is the choice not having been honoured. Nothing before the first
request of a session counts: the guest is still on the mode the last one left,
and the window's own size is on its way to it.

The window that results is fitted rather than obeyed. It grows from where the
user left it and is pulled back only by however much it would hang off the work
area -- the taskbar's strip excluded, because a window sized to the whole
monitor has its bottom edge behind it. A mode larger than the monitor gets
every pixel there is and letterboxes the rest, which is the one case where a
picture scaled down is the right answer: the alternative is a desktop with
corners nobody can reach. Full screen, maximised and minimised windows are left
alone -- the first already covers a monitor, the second is sized by Windows and
un-maximising it would be the viewer overruling the user, and the third has no
client area to give. Whatever size the window ends up at is then taken as the
size last asked for, so that the `WM_SIZE` the move raises does not settle into
a request that asks the guest to undo what it just did.

**Full screen** is a system-menu action, borderless: a frame and a rectangle,
both given back on the way out. Exclusive would take the display for this
process, and a viewer that owns the screen is one the user cannot leave when
the guest stops answering; nothing here touches the monitor's own mode. It has
no key of its own. `F11` reaches the guest like any other key -- it is a real
key in a guest browser, and a hook that keeps the shell's hands off the
keyboard has no business keeping one of its own -- so full screen is offered
on the system menu, and the guest's tray offers the same from the far side.

**The frame is two words, not one** (`viewer::fullscreen`). Taking
`WS_OVERLAPPEDWINDOW` off `GWL_STYLE` is the caption and the sizing border;
`GWL_EXSTYLE` still carries `WS_EX_WINDOWEDGE`, which Win32 adds to any window
created with a caption and nobody ever asks for, and the edges beside it. Both
words are computed on the way in and *saved* rather than recomputed on the way
out: a frame restored by arithmetic is a frame that drifts. The rectangle is
the whole monitor the window is mostly on, read at each entry -- so a window
dragged to the second monitor fills the second monitor -- rather than that
monitor's work area, because the taskbar is what a full screen covers.

**A maximised window is put down first.** `WS_MAXIMIZE` is a state and not a
frame: Win32 sizes a window wearing it to its monitor's work area and does not
answer `SetWindowPos`, so a full screen entered from a maximised window would
be one with the taskbar still drawn on it. The placement is read before the
window is restored down, so `showCmd` remembers that it was maximised and
`SetWindowPlacement` maximises it again on the way out. For the same reason
focusing the window -- what a second Connect and a reconnect both do -- skips
`SW_RESTORE` while it is full screen: that would undo the state the user asked
for.

**The shell is told** (`ITaskbarList2::MarkFullscreenWindow`), both ways. A
foreground window the exact size of its monitor is usually enough for the
taskbar to get out of the way by itself, but that is detection rather than a
contract. Nothing in the viewer depends on the answer: a shell that refuses
costs a taskbar over the picture, not a session, so it is a warning in the log.

**What is remembered** is one small `key = value` file per VM under
`%LOCALAPPDATA%\VMLord\display`: the restored position, the client size,
whether it was full screen, and the quality the user picked. Not settings --
nobody edits it and losing it costs a window position -- so a file that is
missing, truncated or from a later version reads as the defaults, and the
default size is 1920x1080. The size kept is the window's, never the monitor a
full-screen one is covering, and the position is the *restored* one, so a
window closed maximised comes back where it was before it was.

**The position is reported rather than read back.** A window that is closing
has already been destroyed by the time the loop notices -- `WM_CLOSE` destroys
it inside the pump -- and a destroyed window has no placement to ask for. So
every `WM_MOVE` from a window that is neither full screen nor maximised is a
`Moved` event, the loop keeps the last one, and opening the window reports the
place Windows chose for it: a session that never drags the window still has a
place to remember. The coordinates are the *window* rectangle in virtual-desktop
pixels rather than a `WINDOWPLACEMENT`, whose rectangle is in workspace
coordinates and would walk the window by the height of the taskbar on every
restart. Nothing is scaled between sessions because the viewer is per-monitor
DPI aware: a coordinate means the same thing on a 100% monitor and on a 150%
one.

**A remembered position is checked against the monitors there are now**
(`viewer::monitors`): a desktop can lose the monitor a window was on, or be
rebuilt with the negative half on the other side. A window with a grabbable
strip of itself on some monitor's work area opens exactly where it was, hanging
off an edge included -- that is where the user put it. A window with less than
that showing opens centred on the monitor it is nearest to, and on the primary
one when it is near none. An enumeration that answers nothing moves nothing.

**Auto and Desktop** are the two entries on the system menu, and Motion is not
there: task #123 owns it, and a menu offering a mode the guest refuses is a
menu that lies. `Auto` is sent as `MODE_AUTO` rather than resolved on the host
-- the guest is what knows what it can encode, and it answers with what it
settled on, which in this build is always Desktop.

### Desktop profile and display provisioning

What a VM asks of its desktop, what installing that desktop came to, and what
the guest is doing with it right now are three things, and `core::display`
makes them three types rather than one field.

`DesktopProfile` is desired state -- `Headless` or `Gnome`, GNOME by default in
a create form. It lives inside `Provisioning`, beside the user name and the
locale, for the reason provisioning itself lives inside `VmSource::CloudImage`:
a desktop is something a cloud-init seed installs, installation media gets no
seed, and "a local ISO with GNOME" is therefore a state that cannot be spelled
rather than one to be rejected at run time. It is a creation-time decision;
installing a desktop into a guest that was built without one is #127.

`GuestDesktop` is the other half of that first type, and the reason it is a
second type rather than a second reading of the first: the profile is what was
*asked for* and the guest is what it *became*. The agent reads a session's own
name, its type and the unit `display-manager.service` is linked to out of the
guest it is running in (`agent::guest_platform`), the display recipe answers
with them, and the host keeps them in `VmDisplayFacts` beside the payload's
versions -- unstored, like every other runtime fact, because a desktop
observed before a stop says nothing about the one running after one. It is
reported at every outcome and not only at a working display: a VM created
asking for GNOME whose recipe stopped is exactly the VM somebody has to go
looking inside, and what the guest has is where that starts. Nothing derives
one from the other, and the seed keeps being filled from the profile alone,
because cloud-init writes it before there is a guest to ask anything of.

Names and never an enumeration on that side: a desktop this build has never
heard of arrives under its own name instead of reading back as the nearest
variant, and all three absent is a real answer -- a headless guest, and every
guest at the moment its display recipe runs, which is as root before anybody
has logged in.

`DisplayProvisioning` is how far installing it got -- `NotRequested`,
`Pending`, `Ready` or `Degraded` with a reason. It is *stored*, in the VM's
metadata mapping beside the GPU mode, and that is the point of it: the
installation happens once during the build, and every later run of VMLord has
to be able to read its outcome. A VM whose desktop packages could not be
downloaded is a working VM with a missing desktop -- it boots, SSH answers --
and this field is the only thing that knows, so it is also the only thing a
retry can be offered from. `Degraded` rather than `Failed` for the same reason:
the VM is not broken, the desktop is. A mapping written before these fields
existed reads back as `Headless` and `NotRequested`, which is what those VMs
are; that is deliberately not `DesktopProfile`'s own default, since a create
form starts from a desktop and a VM built before desktops existed did not.

Retry is a property of the cause, not of the button: `DisplayStatusCode`
answers `is_retryable`, and a download that failed, an install that was
interrupted and a service that is not running are worth another attempt while a
release that publishes no desktop packages is not. `DisplayProvisioning::retried`
moves a retryable failure back to `Pending` rather than to nothing, because
until the new attempt reports, what is known is still that the last one did not
finish.

Everything else is runtime state, and none of it is stored: `VmDisplayFacts` is
what the guest last reported, and `vmlord_app::display::derive_status` turns
the stored profile, the stored provisioning, the VM's state and those facts
into a `VmDisplayStatus` -- `DisplayState` for the glance (`Disabled`,
`Provisioning`, `WaitingForGuest`, `Ready`, `Degraded`), `DisplayStage` for
where the reading came from, `DisplayStatusCode` for exactly why, and a message
for the detail. The shape is the GPU's on purpose: a backend reports facts and
never names a state, the UI paints a state and never works one out.

Readiness is the display recipe's last stage. The guest marks `SERVICES_START`
`Ok` only once the system units are active and the socket between the capture
pair exists, which is exactly the fact a viewer needs, so the host reads it out
of the report the agent already delivers rather than asking a second question
over that channel. A recipe that skipped the stage -- a payload built before
the services existed -- is a display that will never arrive and is reported as
failed rather than waited for. The recipe is applied once per agent session, so
a guest that reconnects re-reports, and nothing survives the run: a readiness
observed before a stop says nothing about a guest that is not running.

That same report is what finally moves `DisplayProvisioning` off `Pending`.
Nothing on the host can watch cloud-init install a desktop -- it happens on the
first boot, long after the creation pipeline has finished, and no guest message
reports the package set -- but a guest running its display services is running
them on top of that desktop. The first time one reports, the stored
provisioning is written as `Ready`, which is also what keeps a stopped VM
reading as a VM whose desktop is not running rather than one whose desktop
never arrived.

The display payload offer is runtime state too, but its published directory
survives a VMLord restart. Reconnect validates `display-payload/active` through
the normal display export boundary and restores its fixed share name in
`DisplayRuns` before the agent listener starts. The first agent session can
therefore mount and report the desktop without waiting for a later reconnect.

The desktop itself comes from the distribution's own archives. `DistroProfile`
carries a `DesktopSetup`, and that setup is a package list and nothing else:
the seed prints it as cloud-init's `packages` block with `package_update`,
so Ubuntu installs `ubuntu-desktop-minimal` (GNOME Shell, GDM and the Wayland
session, without the office suite) from the archives the guest is already
configured with. Arch names the same session package by package --
`gnome-shell`, `gdm`, `gnome-control-center`, `gnome-console`, `nautilus` --
because it publishes no one metapackage for it. VMLord adds no repository,
downloads no desktop binary of its own and signs nothing.

A declared display manager unit sat in that setup and was deleted rather than
kept: a package list is what has to be declared, because it is needed before a
guest exists, while which unit owns the login screen is a link in the guest
that the agent reads at the moment it matters. Two copies of that answer can
disagree -- the guest's is the true one -- so there is now one.

What has to happen before those packages are installed is
`DistroProfile::package_refresh`, and it is a field because it is not a
preference. Debian and its family resolve a new package against refreshed lists
and leave everything installed where it is, which is `Lists` and prints
`package_update` alone. Arch resolves against one moving repository, so
installing into an image a month old pulls in libraries built for packages the
guest has not upgraded to -- the partial upgrade Arch documents as unsupported
and does not test. That is `FullUpgrade`, and it prints `package_upgrade`
beside `package_update`: on Arch cloud-init runs `pacman -Sy` for the first and
adds the `-u` for the second, so the guest is a full `-Syu` before a desktop
package is resolved against it. Neither key is printed for a headless VM --
nothing is being installed, so there is no partial upgrade to avoid and no
reason to touch a guest that asked for none. The field carries `#[serde(default)]`
at `Lists`, so a profile written before it existed keeps loading and keeps
meaning what it meant. A failure there does not stop the boot: cloud-init
reports it and carries on, which is exactly the outcome the stored `Degraded`
is for.

Small VMs are advised against and never refused. `VmCreateRequest::advisories`
is where that lives -- below 2 vCPU or 4 GiB a desktop is slow, and a desktop
with no password has nothing to log in with at a GDM screen, since the key
VMLord deploys logs in over SSH and not at a login screen. Both are sentences
the create form paints beside the fields; neither is a rule that stops a build,
because a machine that is small on purpose is a machine somebody meant.

No credential is added to any of this. The plaintext password exists while a VM
is being created, is hashed into the seed there, and the create form that held
it is dropped the moment the build is accepted; a display session authenticates
with a key derived from the per-VM agent secret, so nothing in the display
model has a field a password could be stored in.

### Display: the payload crates

`vmlord-payload` is what every VMLord payload is made of and nothing else: a
digest, a progress report, a prepared file, an error, ZIP expansion under
limits, the content-addressed host cache, per-VM staging, the layout of a
release directory and the four rules for reading one. It knows nothing about
what a payload carries, and it meets a kind of payload at one trait.

`PayloadEntry` is that trait, and it is deliberately small: identify a payload,
say what it may cost, and parse the two documents at its root. Everything a
kind decides for itself -- what a target is, which guest an entry applies to,
which of several entries wins -- has no method here, because a method here
would be the mechanism pretending to know. `PayloadSources` is the one hook the
mechanism owes back: expansion ends by handing the parsed provenance the
manifest, which is what the GPU payload's overlay cross-check needs and what
the display payload answers `Ok` to.

`vmlord-gpu-payload` and `vmlord-display-payload` are thin layers above it,
each with its own entry document, its own manifest and its own selection. The
GPU tests are what proved the extraction changed no behaviour; the shared half
is tested against a payload kind that exists only in tests, because no real
kind may be privileged by the mechanism that serves both.

### Display: the guest payload

A display payload carries the whole guest side of the display that a guest's
own package manager cannot provide: today the DKMS sources of `vmlord_drm`, and from task
#115 the guest display services. One artifact, one version and one declared
range of display protocol revisions, so that what the host talks to and what
the guest runs cannot drift apart unnoticed.

An entry states a semantic `version` of its own, a `target` of distribution,
release and architecture, the kernel it was `proven_on`, the `protocol` range,
both digests and the expansion limits. `proven_on` is a record and never a
selector: DKMS builds against the headers of the kernel a guest is running, and
Ubuntu upgrades kernels unattended, so requiring the kernel a payload was built
against would mean an upgrade kills the display until somebody repacks.

The `target`'s distribution and release are a record too. What a display
payload carries -- DKMS sources, static musl binaries, udev rules, units, audio
configuration -- knows nothing about a package manager, and the three shipped
specs under `payloads/display/` differ by base image and by nothing else. So
the pair says where a build was *proven*, not who it may serve, and a guest
nobody built a target for is served by a payload proven somewhere else rather
than by nothing at all. The container build stays what proves it: a module that
does not compile for 22.04, 24.04 or 26.04 is an artifact that never exists.

This is the display payload's asymmetry with the GPU payload, and it is
deliberate. A `mesa_policy: bundled` payload carries Mesa built against its
base image's glibc and is pinned to a kernel release, so its distribution and
release stay hard conditions.

Selection has two hard gates, both decided before the guest has booted, which
is why no kernel appears in either: the architecture, and a protocol range that
covers this build's revision. An entry outside that range is *passed over*
rather than failed: a payload may legitimately be built for a newer or an older
VMLord. What is left is ranked -- an entry built for exactly this guest above
one that was not, then the greatest version, then the payload ID, which is
arbitrary but total, so that two equally good candidates cannot make the answer
depend on the order a directory listed them. Ranking provenance first is what
keeps Ubuntu's selection exactly what it was. Nothing for this guest is
`NoPayloadForGuest`, which is a degraded display and a VM that starts.

Which payload a guest got, and what it was proven on, is logged at selection
(`crates/platform/src/display_staging.rs`) -- and at `info` rather than `debug`
when the two differ, because that is the only place a payload serving a guest
nobody built for is visible.

Several versions for one guest is the ordinary state of this catalog -- it is
what an update is made of -- so what a release may not do is carry the same
version twice, which would make selection depend on the order a directory
listed two identical candidates.

A release therefore carries *one* display payload, not one per Ubuntu release.
`rebuild_payload.sh` still builds inside all three containers, because each
build is the compile gate for that release's headers, and packs the 24.04 tree
alone: the three prepared trees differ in `recipe.json` and in nothing else, so
three archives would be one payload's content three times under three IDs. The
entry that ships says it was proven on 24.04, and every guest on `amd64` reads
it.

Verification happens twice, on both sides of the share. The host checks the
archive against the entry, the expansion limits, and `payload.json` against
what the entry claims; the guest, before it copies anything, checks
`payload.json` and hashes every file it declares. Both, because a 9p export is
a filesystem the host can rewrite between its own check and the guest's.

Twice means the *same* claim twice, and the guest's `PAYLOAD` stage therefore
gates on the architecture alone, exactly as selection does (`serves`,
`crates/agent/src/display_recipe.rs`). A stricter rule there would not be a
second opinion but a different one: it would move the refusal from selection,
which is allowed to fall back, to the mount, which is not -- which is what a
26.04 guest hit after #169 freed the host but not the guest. The distribution
and release the mount reports are appended to the stage's own line when they
are not this guest's, so a payload proven elsewhere says so where a person
reading the recipe will see it.

A VM exports one path, `<vm>/display-payload/active`, and versions are
*published into* it. That follows from HCS: a compute system's `Devices/Plan9`
section is written before the system is built and is immutable for the lifetime
of a boot, while a generation directory is named after its digest and is
therefore a different path per version. A publication writes every declared
file first, each through a rename; then `payload.json`, so a guest that reads a
manifest finds every file it names already there; and only then removes what
the new manifest does not declare, because a leftover file nothing declares is
ignored and a missing declared one is not.

The share is `vmlord.display.payload`, mounted at `/opt/vmlord/display-payload`,
and it is the display's own -- not a role inside the GPU manifest. So are the
agent's three display messages. A GPU attach that fails must not be able to
take the display with it, and the two stacks meet only where HCS makes them:
as entries in one Plan9 device.

### Display: the guest's recipe

Stages, in order, reported as a list and never as a verdict:
`DISTRIBUTION`, `PAYLOAD` (the mount, its manifest, and every declared file's
digest -- before anything is copied), `BUILD_DEPENDENCIES` (DKMS, a compiler
and the running kernel's headers, from the guest's own package manager),
`MODULE_SOURCE` (copied to `/usr/src/vmlord-display-<version>`, because 9p is
read-only and DKMS writes beside its sources), `MODULE_BUILD`, `INITRAMFS`
(`update-initramfs -u -k` for the running kernel after a successful DKMS
install), then `MODULE_LOAD` (`modules-load.d`, the modprobe options, the unit that unbinds
`simple-framebuffer`, the drop-in that keeps the compositor on the
distribution's Mesa, and `modprobe`), `DEVICE` (a `/dev/dri/card*` whose
driver is ours), and `SERVICES`/`SERVICES_START`, which are skipped with their
reason until task #115 fills `content/services`.

The modprobe options are written by the guest rather than copied out of the
payload, from the mode the host has stored for that one VM -- a size belongs to
a VM and a payload is shared by all of them. A VM with no stored mode gets
1920x1080, which is every VM until task #120 saves one. A mode that changed
under a module already loaded costs a `modprobe -r` and a `modprobe`, because a
module parameter is read once; a module that does not say what it was loaded
with is left alone, because a reload on a guess drops a working desktop.

The drop-in sits between the two: the same file for every VM, so it is copied
out of the payload, with one line the payload cannot write. It lands on
`org.gnome.Shell@.service`, which is a template, so it reaches the greeter's
compositor and a logged-in user's both, and it says two things -- the GPU
recipe's Mesa overrides unset, and `LD_LIBRARY_PATH` pointed at the
distribution's libraries. The second is appended on install, out of the library
layout the agent detected in the guest: a multiarch guest keeps that Mesa in
`/usr/lib/<triplet>` and a guest without a multiarch directory keeps it in
`/usr/lib`, and a shipped constant would be right on one of them. Both, because the
GPU recipe reaches a process by two paths: the environment it exports, and
`/etc/ld.so.conf.d/vmlord-wsl-mesa.conf`, which no environment can undo. A
compositor left on the payload's Mesa binds our device, fails to allocate a
buffer on it, and never finishes its modeset; applications, which is where the
GPU was wanted, keep the whole environment. A drop-in is read when a unit next
starts, and on a normal boot this recipe runs before the greeter does.

Idempotence is by fact and not by a flag: the payload's version installed, the
module loaded and a device that answers short-circuits the three build stages,
so every start after the first costs a few checks and needs no network. A
kernel upgrade is handled in the same place -- DKMS's `AUTOINSTALL=yes` carries
the module across it with VMLord not involved, and when it did not, the recipe
finds no loaded module and builds again. A build that fails on the new kernel
is a degraded display naming exactly that, and a VM that runs.

`vmlord_drm` itself is one CRTC, one connector, a primary plane, a cursor
plane, GEM shmem, an hrtimer vblank, atomic modesetting and PRIME export, and
nothing scans out: the framebuffer a compositor commits is the product, and
capture reads it as an ordinary DRM client. Three properties are decisions task
#111 measured -- a platform device under its own name (mutter's udev rules tag
`platform-vkms` on `ID_PATH`), no `DRIVER_CURSOR_HOTSPOT` (mutter hides the
cursor plane of drivers that declare it), and linear XRGB8888/ARGB8888 only (a
capture client that mmaps a buffer cannot detile anything else).

The cursor plane is what mutter puts the pointer on, which is why compositing
it back is capture's job rather than a convenience. The vblank is the output's
only clock: nothing here scans out, so a commit would otherwise complete in no
time at all and pace nothing. Each plane also exposes an immutable
`VMLORD_GENERATION` property whose driver-owned value advances in
`atomic_update`. Capture holds one outstanding frame request across vblanks
and reads the framebuffer only after one of those generations changes. An
older module with no property keeps the previous every-vblank behaviour, so a
service update cannot freeze a guest whose payload has not yet been updated.
Generation is sampled before and after the plane ioctls and the snapshot is
retried across a concurrent commit. It is acknowledged only after delivery to
the peer and host-session epoch that requested it; replacing either resets the
observation so a reconnect receives the current static frame immediately.

The connector offers the host monitor's modes, between 640x480 and 2560x1440 at
1..144 Hz, with the viewer's selection marked preferred, a physical size at 96
DPI and no synthesized EDID -- the name a monitor would carry costs a fifth
kernel version guard and is deferred. Until a host publishes a list it offers
the single mode its `width`, `height` and `refresh` parameters name. The module
declares its payload's version, which is what an update's verification compares
against.

Every mode it offers came from the host, and none was invented here -- see
*Resizing the desktop* below.

### Display: updating and rolling back

Installation is automatic and idempotent; a version change never is. A newer
version in the release becomes an offer in the status, and moving to it is an
action a person takes on a running VM.

The host refuses everything it can before the guest is asked: a VM that is not
running, a release with nothing newer, a payload that will not stage. Then the
new version is published into the directory the VM already exports, and the
request goes to the thread that owns that VM's agent session. That thread first
gets a heartbeat answer from the same socket and only then writes the update
between frames, because an HvSocket left apparently open by a reboot must not
consume the update's twenty-minute budget. A session ending closes admission
and drains its queue under one lock, so a request concurrent with that close is
either owned by the session or rejected, never inherited by the next one.

The guest builds, refreshes the running kernel's initramfs, reloads and then
*verifies*: the module loaded, its version the one that was asked for, and a
device that exists. Refreshing the initramfs is part of the build budget: the
image contains both `modules-load.d/vmlord-display.conf` and a copy of the DKMS
module, so leaving it untouched would make the next boot restore the old
version. A verification that fails rolls back one version, which costs a
`modprobe`, a `dkms remove` and another initramfs refresh because the previous
`/usr/src` tree was never deleted and DKMS still holds its build. One step and
no further: keeping two would be a version history, and there is nothing in an
MVP to build one from.

A compositor may hold the loaded module and make `modprobe -r` refuse it. That
is not a rollback and not a successful live update: the target remains
installed, the refreshed initramfs will load it at the next boot, and the
answer is `REBOOT_REQUIRED` with the installed and loaded versions reported
separately. VMLord warns the user to reboot the guest; it does not interrupt a
session by rebooting automatically.

A successful rollback is **not** a degraded display. The desktop works, on the
version that was working before, and `display-payload-update-rolled-back` says
exactly that. `display-payload-update-failed` is the other case: neither
version is running.

The offer reaches a person as `Update display`, a button beside Connect in the
selected VM's actions. The four facts the host will check are read off the
derived status *before* the click rather than out of a refusal after it: the VM
is running, its guest has reported the payload version it has, this release
carries a different one, and no update of that VM is already running. Which of
them is missing is what the disabled button says, and for a guest that has
reported nothing yet the sentence is the application layer's own -- installing,
waiting for the guest, and a desktop that failed are three different answers.
The details panel states both versions on a `Display payload` row beside the
desktop status, because whether there is an update to make is a fact about the
VM and not something to be found by hovering.

The press does not wait. An update is the longest thing a person can ask VMLord
for -- the guest builds a kernel module against its own running kernel, and the
recipe's budget for one is fifteen minutes -- so it runs on a worker, the way an
SSH launch and a start do, and for the same reason: the caller is the thread
that draws the window. What the click answers is only whether the update is
worth attempting, which is instant: a VM that is not running, one that records
no guest, one VMLord holds no agent session for, and one already being updated
all have nobody to ask and say so on the spot.

`display_updates::DisplayUpdates` is that worker registry, keyed by VM name.
Keyed, unlike `ssh_launches`, because two shells into one guest is an ordinary
thing to want while two updates of one VM would publish two versions into the
one directory that VM exports and ask one agent session twice. The key is also
what the list of VMs reads: a VM being updated carries `update_in_flight` in its
display facts, which becomes `updating` on the derived status, and that is what
disables the button and puts `updating to <version>` in the panel. It is beside
the display's state rather than inside it, because the desktop goes on working
while the guest builds -- Connect stays available throughout.

Asking the guest from a worker means not holding the session registry, which
lives behind the repository's `&mut self`. The connection hands out a
`DisplayUpdateChannel` instead -- the VM's name, whether a session is open, and
the sender the serving thread reads -- so every request still reaches the one
thread that owns that socket, whichever thread asked.

How the update ended is one line the worker pushes into the diagnostics the UI
already reads, decided by `display_update::report`, and the display's facts are
recorded before that line so a refresh arriving between the two reads the
versions the guest ended up on. An update that took is an ordinary line naming
the version; `display-payload-update-rolled-back` is a warning naming the
version the guest came back on -- not an error, because the display works -- and
`display-payload-update-failed` is an error, because nothing displays. A refusal
raised before anything was staged records no facts at all: an update that was
refused is an update that did not happen.

## The desktop codec

`vmlord-display-codec` is what turns a captured guest framebuffer into the
bytes the frame channel carries, and what turns them back. Like the protocol it
is portable by construction and has no dependencies at all: it is linked into a
Windows viewer and into a static musl guest binary built without a C toolchain,
and its output has to be the same bytes on both. It does not capture, does not
open a socket and knows neither DRM nor Windows.

A keyframe and a tile delta are the same container, distinguished by a flag in
an eight-byte header that also repeats the tile grid. The grid is derivable
from the session's `StreamConfig`, and repeating it costs four bytes and turns
a `StreamConfig`-against-frame mismatch from a silently wrong picture into a
named error. A keyframe carries every tile in raster order with no index at
all; a delta carries only the tiles that changed, each behind a varint index,
and the indices must strictly increase, which makes a delta canonical and
catches a shuffled payload.

Each tile is written in whichever encoding is shortest: `Raw`, `Zrle`, or --
deltas only -- `XorZrle`, which is the tile XORed with the one the far side
holds and then run-length coded. Every candidate is actually evaluated rather
than guessed at, ties going to the lower method number, which is what makes the
output deterministic: the same pixels produce the same bytes on every machine,
and the golden vectors can hold the wire still.

`Raw` carries no length field, and that is arithmetic rather than
parsimony. The protocol caps a frame record at `width * height * 4` plus 64 KiB
of slack, and the case that approaches the cap is a keyframe whose every tile
is incompressible. At tile size 16 a 2560x1440 frame is 14400 tiles, so a
four-byte length on each would spend 57 KB of that 64 KB before anything went
wrong. Deriving a raw tile's length from the grid leaves one byte of overhead
per tile. Edge tiles at the right and bottom edges are clipped, so a frame that
is not a multiple of the tile size is the normal case rather than an error.

The run-length coder works on 32-bit pixels rather than bytes -- a desktop
repeats whole pixels, and a byte-oriented coder would have to rediscover that
four times per pixel -- and under XOR a tile that changed in one corner is
mostly zeros, which is the shape it is best at. The cursor is a stream of its
own with its own two records and no shared state, because it moves far more
often than the desktop changes and must be able to overtake a frame that is
still being written.

### Why the queue precedes the encoder

The bounded queue keeps current state and discards what is stale, and *which*
frames may be discarded is the whole of the design. Dropping an already encoded
delta would be silent corruption: the next delta would be encoded against a
frame the viewer never received and applied to the wrong base, and nothing
anywhere would notice -- no error, no `RequestKeyframe`, just a picture that
drifts.

So the queue holds *captured* frames. `Encoder::submit` copies a frame into a
staging slot, displacing whatever had not been encoded yet, and
`Encoder::next_payload` encodes when the transport is ready to write. The
encoder's reference frame is therefore, by construction, the last payload the
caller was handed -- which is the last one written to the socket, since a
failed write ends the channel's generation and the next one opens with a
keyframe anyway. Damage hints accumulate across displaced frames for the same
reason: a frame dropped before it was encoded still changed pixels the newer
one keeps, and its hint is the only record of where they were.

Damage is a hint and never a fact. Tiles a hint covers are compared against the
reference; tiles it does not cover are not compared, and so are not advanced in
the reference either. A hint that under-reports therefore loses pixels until a
later hint or a keyframe covers them -- a bug in the capture backend -- but it
cannot desynchronise the stream, and that is the property the tests hold.

A keyframe is produced for two reasons only: the first frame of a stream, and
a viewer asking through `RequestKeyframe`. A request that arrives with nothing
newly captured is answered from the frame already staged rather than waiting
for the guest to repaint. Geometry never changes inside an encoder: a
resolution change is a new `StreamConfig`, hence a new encoder and a new
decoder, which is also what makes the reference frame's size an invariant
rather than a check.

There used to be a third reason -- a protective whole frame every
`keyframe_interval` encoded frames, 300 by default -- and it was a bad trade
once measured on a real desktop rather than on the synthetic scenes. That
period now drives a **protective sweep** instead, `refresh_interval` in
`EncoderConfig`. Every encoded frame compares a slice of the tile grid whatever
the damage hint said, and the slice advances, so the whole grid is checked
within the interval. The recovery property is the one the keyframe had: a tile
the hints keep missing cannot stay wrong for longer than `refresh_interval`
encoded frames. The cost is not: a tile that matches the reference is free to
check, so a desktop nobody is touching pays nothing at all, and a tile that
does not match is repaired by the delta already being written. Setting the
interval to zero turns the sweep off.

The measurement behind that is in "What a real desktop costs" below. The short
version is that a real 1920x1080 GNOME keyframe is 4.7 MB, not the 163 KB the
synthetic scenes report, and on a desktop with one small thing moving on it the
periodic keyframe was **98% of everything the stream sent**.

### What the benchmark measured

`cargo display-bench` runs five synthetic scenes -- a static desktop, typing, a
scrolling view, a moving window and fullscreen video -- and reports what each
costs. Timing names `submit` (copying the captured pixels into staging),
`encode` (`next_payload`) and the complete `frame` path separately; excluding
`submit` would hide the dominant cost of a guest framebuffer mapping. At
1920x1080 over 300 frames, per delta:

| scene | tile 16 | tile 32 | tile 64 |
| --- | --- | --- | --- |
| static desktop | nothing sent | nothing sent | nothing sent |
| typing | 172 B | 177 B | 178 B |
| moving window | 5.9 KB | 5.6 KB | 5.4 KB |
| scrolling | 8.0 MB | 7.9 MB | 7.9 MB |
| fullscreen video | 8.3 MB | 8.3 MB | 8.3 MB |

Keyframes on these scenes are 211 KB, 163 KB and 131 KB at the three tile
sizes -- a number that turned out to be worth nothing at all off the synthetic
bench, see below. Encoding a quiet frame costs one to two milliseconds and a
full one ten to twelve, and decoding is well under two.

Two conclusions and one caveat. **LZ4 does not earn its place in the MVP**: on
desktop-shaped frames ZRLE already compresses by three orders of magnitude, and
on the two heavy scenes nothing compresses at all, so a second general-purpose
compressor would be paying for itself in neither case. **The default tile size
stays 32**, the value the handshake already names: deltas are almost
independent of it, and the keyframe savings at 64 do not outweigh re-sending
four times the pixels for a small change on content that is not synthetic.

The caveat is that the scrolling and fullscreen scenes are noise, which is the
worst case any lossless codec can be given, and the moving window is a flat
rectangle, which is close to the best. The numbers here settle the codec's own
decisions and nothing beyond them.

### What a real desktop costs

The caveat above was load-bearing, and the keyframe row was the part that hurt.
`vmlord-display-guest-probe --record <path>` writes committed frames of the
live desktop straight off the `vmlord_drm` plane, packed, and
`cargo display-bench --raw <path>` runs that file through the same table as the
scenes. Measured on a GNOME 1920x1080 guest:

| capture | keyframe | mean delta | ratio |
| --- | --- | --- | --- |
| idle desktop (wallpaper) | 4.75 MB | nothing sent | 1.7 |
| scrolling terminal | 3.96 MB | 33 KB | 84 |
| fullscreen video | 0.86 MB | 678 KB | 12 |

A real keyframe is **4.7 MB, not 163 KB** -- twenty-nine times what the
synthetic scenes said, and the same within a percent at all three tile sizes
(4.78 / 4.75 / 4.73 MB). The reason is the wallpaper: it is a photograph, and
run-length coding does nothing to a photograph, so nearly every tile is stored
raw and the tile size only moves the per-tile overhead. The video frame
compresses *better* than the idle desktop, which looks backwards until one
notices that a video frame is one decoded picture while the desktop is a
photograph plus panels plus shadows.

That is what condemned the periodic keyframe. The counter only advanced on
frames that actually encoded something, so a perfectly still desktop never paid
it -- but a desktop with one 32x32 tile changing, which is a clock or a text
cursor, did. Over 600 frames of exactly that, built from the captured
wallpaper:

| | keyframes | mean bytes/frame | 600 frames |
| --- | --- | --- | --- |
| protective keyframe | 2 | 16.2 KB | 9.70 MB |
| protective sweep | 1 | 8.3 KB | 4.95 MB |

The steady-state delta is 339 bytes a frame -- 203 KB of the whole run. Every
other byte was keyframe. Over a ten-minute session at thirty frames a second
the periodic keyframe would send sixty of them, 285 MB, to protect a stream
whose actual content is about 6 MB; the sweep sends the one keyframe the stream
opens with and nothing further. On the scrolling capture the saving is smaller
but still real, 14% of all bytes sent, and on fullscreen video it is noise,
because a stream already sending whole frames does not care.

The sweep is not free, but it is cheaper in CPU than what it replaced, and for
the same reason it is cheaper in bytes. At 1920x1080 with tile 32 the grid is
2040 tiles, so an interval of 300 compares seven of them a frame -- 28 KB of
the frame against 28 KB of the reference, and a tile that matches is never
gathered. Over the near-idle capture above, per frame:

| | encode | whole frame path | bytes |
| --- | --- | --- | --- |
| no protection at all | 8.8 us | 291 us | 1.9 KB |
| protective sweep | 19.2 us | 297 us | 1.9 KB |
| protective keyframe | 37.3 us | 314 us | 16.2 KB |

The sweep doubles the encode step, which sounds worse than it is: the encode
step on a quiet desktop is nine microseconds, against a frame copy of nearly
three hundred. What it adds to the whole path is 2%, where the periodic
keyframe added 8% -- amortised, in bursts of twenty milliseconds every three
hundred frames rather than ten microseconds every frame, which is the shape
that matters for a frame deadline. Where the capture backend reports no usable
damage at all and every tile is compared anyway, the sweep costs nothing
measurable, because those tiles were already selected.


---

# Planned Modules

```
core/

    vm/
    workspace/
    images/
    networking/
    display/
    gpu/
    ssh/
    diagnostics/
    settings/

platform/

    hcs/
    hns/
    gpu/
    hvsocket/

app/

ui/
```

---

# Migration

The incremental replacement of the C backend -- HCS, HNS, networking, GPU,
display, SSH -- is finished, the backend it replaced has been removed from the
distribution, and diagnostics, the last piece, are done. They were Rust-native
work rather than a port: there was no C module left to stand beside.

---

# Diagnostics

Records go through `tracing`. The subscriber is a `Registry` with two layers,
both built in `core::logging`.

The **record layer** writes the log file, and standard output when there is a
console to write to. It is a `tracing` layer rather than
`tracing_subscriber::fmt` because the line format here is fixed and older than
the facade: `[1970-01-01T00:00:00.000Z] [INFO ] target: message`, with the
stamp spelled by `core::logging::timestamp` -- Howard Hinnant's civil-date
algorithm, written out because the leap rule is the whole difficulty. `fmt`
would need the `time` crate to say the same thing. An event's fields are
appended after its message, and a span's fields are appended before them, so a
reader scanning a column finds the VM in the same place on every line.

**One file per run.** The configured `log_directory` names no file that is ever
written. The folder is the person's choice and the name is the application's:
`core::logging::run_log_path` stamps it at startup, opening
`vmlord-20240229-010101-123.log` in that folder, so every
process start owns a transcript. One file held every launch VMLord had ever
made, and the launch worth reading is almost always a single one: a report now
carries the run that went wrong instead of the months around it, and `vmlord`,
`vmlord-com1`, `vmlord-ssh` and `vmlord-display` stop interleaving into each
other. The stamp is the same UTC instant `timestamp` writes, spelled
`YYYYMMDD-HHMMSS-mmm`: a colon is not a legal character in a Windows file name,
the two groups sort the directory into the order the runs happened, and the
millisecond is there because two helper processes can start within one second.
Nothing deletes an old file; retention is not implemented.

The **diagnostics layer** collects what is addressed to a person. An event
reaches the panel when it carries a `diagnostic` field, which
`vmlord_core::diagnostic!` writes and nothing else does. Not by level: a
warning that an image cache did not warm belongs in the file and not in the
user's face. Not by target: targets move when code moves between modules. The
macro exists so the marker cannot be misspelled -- `diagnostc = true` would
silently fail to show and would break no test -- and it picks the `tracing`
macro from the level literal, so a record's level and its event's level are one
decision.

A `Diagnostic` is a level, a `Subsystem`, an optional VM, an optional Windows
code, the moment it was observed, and a sentence. The subsystem is the list the
migration named -- HCS, HNS, networking, GPU, display, provisioning -- plus
`Image` and `App`; being a field rather than a guess from the wording is what
makes those six one stream instead of six conventions. The moment is stamped by
the layer, because when a thing was observed is a property of the observation.

Records wait in a `DiagnosticsSink`, a shared queue capped at a hundred, and
`WorkspaceApp::diagnostics` drains it. It drains on read rather than on refresh
so the answer to a click appears in the panel while the click is still being
drawn.

`VmRepository::refresh` is what the application calls after `list_vms` on every
refresh. It used to be `take_diagnostics`, which was never what it was for:
finished builds and starts are adopted there, answered shutdowns give up their
handles, desktops that appeared are written down, and HCS events are drained.
Only the return value went.

Two things follow from spans being thread-local. An event inside `start_vm`
inherits the VM from the operation's span and cannot lose it -- but an event on
a worker thread inherits nothing, so every report from a build, a start, a
display session or an SSH launch names its VM itself.

`log` remains a dependency, and not as a leftover: `eframe` and other
dependencies write through it, and `tracing_log::LogTracer` is what keeps their
records reaching the file.

### What never reaches a record

A secret has neither a `Display` nor a `Debug` that shows its value. `tracing`
records a field only through one of those two traits, so a secret does not
compile into a field at all: the guarantee comes from the compiler rather than
from anybody's vigilance.

This covers `Password`, `VmKeyPair`, `Seed`, `SeedRequest` and
`agent_protocol::auth::Secret` -- the last three because the user-data document
carries the password's crypt entry and the agent's secret. A guard test in
`seed::user_data` builds the real documents under a capturing subscriber and
asserts that neither the crypt entry nor the secret appears.

There is no pattern-matching scrubber and there should not be one: a leak can
only come from our own code, and a filter would cost something on every record
while promising more than it can keep.

---

# Localization

The UI reads every word it shows from a message catalogue. There are two:
`crates/ui/locales/en-US.toml` and `crates/ui/locales/ru-RU.toml`, embedded by
`rust-i18n` at compile time, so the shipped executable still needs nothing
beside it. A lookup that finds nothing in the active catalogue falls back to
English, and a test in `vmlord-ui` parses both files and refuses a key that
exists in one and not the other -- the fallback is a safety net for a running
program, not a place for a translation to go missing quietly.

`Language::code` in `vmlord-core` is the one place a locale tag is spelled.
`serde` writes it into `settings.toml` and the UI hands the same string to
`rust_i18n::set_locale`, so the file and the catalogue cannot drift apart.
`vmlord_ui::run` sets the locale from the settings before the window opens, and
the settings dialog sets it again when it saves. That is the whole of switching
language: egui rebuilds each frame from the catalogue, so the interface changes
under the Save button with no restart. Settings that failed to load leave the
locale at English, which is where a fresh installation starts anyway -- the
language belongs to the installer's choice, not to a guess at the host's
locale.

Translation stops at the UI boundary. A record raised through `diagnostic!` and
the `Display` of a `vmlord-core` error stay English: a diagnostic reaches the
log file in the same breath as it reaches the panel, and a log is more useful
in one language than in each reader's. Making them translatable would mean an
error no longer carries its own text -- `core` would return a code with
parameters and the UI would render it -- which is a larger change than a second
language needs.

The workspace list counts cores, and Russian inflects a counted noun three ways
(1 ядро, 2 ядра, 5 ядер). `plural_form` in `crates/ui/src/lib.rs` picks the
form; the catalogue holds all three. One function rather than a rule engine,
because that is the only string in the UI where a number stands before a noun
that bends -- and English is served by the same three keys, its rule being a
coarsening of the Russian one.

The two language names in the settings combo box, "English (US)" and
"Русский", are the one thing not translated: a user who cannot read the
language on screen has to find their own in that list.

---

# Unsafe Rust

Unsafe code belongs only inside:

* `platform`, for Windows API calls and wrappers
* `vmlord-agent::vsock`, for the Linux socket ABI

Everything above should expose safe Rust interfaces.

---

# Dependency Rules

Allowed dependency direction:

```
UI
↓

Application
↓

Core
↓

Platform
↓

Windows APIs
```

Reverse dependencies are forbidden.

---

# Build Targets

VMLord is one repository that produces two programs for two operating systems,
so every build names a target explicitly.

| Program | Target | Built on |
| --- | --- | --- |
| `vmlord.exe`, `vmlord-com1.exe` | `x86_64-pc-windows-msvc` | Windows |
| `vmlord-agent` | `x86_64-unknown-linux-musl` | Windows and Linux |
| the application, compile-checked and tested | `x86_64-pc-windows-gnu` | WSL |

MSVC is the release toolchain for the application: it is what Windows itself is
built against, and what the HCS bindings expect. The GNU target exists only so
that a Linux machine can tell whether the Windows code still compiles, and so
that its tests can run; it never produces a shipped binary.

The agent is built for musl rather than glibc because musl links statically
through `rust-lld`, using the libc that ships inside `rust-std`. No C toolchain
takes part, which is what lets Windows cross-compile the agent at all: the
`x86_64-unknown-linux-gnu` target calls an external `cc`, and a Windows host has
no Linux one. The result is a static binary that does not depend on the guest's
glibc version, and it is the same artifact whether it was built from Windows or
from WSL.

The cost is that the agent cannot link against system C libraries. Nothing it
does today needs to. Should that change -- PAM or NSS would be the likely
reason -- the way back is `cargo-zigbuild`, which supplies a Linux cross-linker
on Windows in exchange for a Zig installation on every developer's machine.

Windows tests run from WSL without Wine: WSL's binfmt interop hands a
`.exe` to Windows, so Cargo's default runner is enough.

---

# Packaging and Releases

Shipping VMLord adds no layer. Every piece of it lands in the layer that
already owns the concern, and the packaging itself is declarative:

| Piece | Lives in | Because |
| --- | --- | --- |
| release manifest types and validation, bundled-profile synchronization | `core` | deterministic rules over data and files, with no Windows API and no network |
| downloading release metadata and the verified installer | `image` | it is the layer that already speaks HTTP and verifies bytes against a digest |
| launching a verified installer through Windows | `platform` | every Windows API call is isolated there |
| the update state machine, and the first-run signal | `app` | it owns worker threads, diagnostics and what the UI is allowed to see |
| the Updates section and the install confirmation | `ui` | it draws state and collects a decision; it calls only application methods |
| staging, licence notices, the release manifest, workflow validation | `xtask` | build automation, reached through Cargo aliases and never linked into the product |
| file placement, shortcuts, uninstaller | `installer/vmlord.iss` | Inno Setup is packaging, not application logic |

The installer never writes or reads `settings.toml`, and never touches
`%LOCALAPPDATA%\VMLord`. It installs canonical distribution profiles into
`{app}\distros`; the application copies them into the current user's own
directory on startup, which is the only way an all-users installation can reach
a user who did not exist when it ran.

## The trust model of an unsigned release

There is no Authenticode certificate, and pretending otherwise would be the
worst possible outcome. What the release chain actually establishes is
*integrity*, not *authorship*:

* `cargo release-manifest` derives the installer's size and SHA-256 from the
  file's own bytes, never from a workflow variable, and puts the result through
  `vmlord-core`'s own validation before writing it. The generator and the
  validator therefore cannot disagree about what a valid manifest is.
* The release workflow recomputes the hash with a different tool and refuses to
  publish if the two disagree.
* The URL is built from one constant, `RELEASE_DOWNLOAD_PREFIX`, that core also
  validates against: a manifest pointing anywhere but this project's release
  assets is refused by every installation.
* VMLord verifies the downloaded installer against the manifest before handing
  it to Windows, and never launches anything without a person confirming it.

What none of that establishes is who built the bytes. A user is trusting
GitHub's control of the release assets and this repository's control of its
own tag. SmartScreen says so, in its own way, and will keep warning until an
installer has been run often enough -- that warning is accurate, and the
honest answer to it is a certificate, not a workaround.

The release workflow is held to the same standard as the product: default
permissions are `contents: read`, only the job that creates the release may
write, and every action is pinned to a commit SHA rather than a movable tag --
a tag is a name its owner can move onto other code, and that job holds a token
that can write to this repository. `cargo run -p xtask -- workflow-check` reads
the file back as data and asserts exactly those properties, so a later edit
that loosens one fails the branch rather than the release.

The release is two jobs because its outputs come from two operating systems.
The payloads are `docker build`s on Linux and the executables must come from
MSVC on Windows, so a `payloads` job builds the GPU and display pairs, hands
them over as an artifact, and the Windows job that needs them builds the
distribution around them. Releases used to carry neither: `cargo dist` treats
missing payload flags as a fact to report rather than an error, so 0.2.0 was
built, checked, drafted and published with no GPU-PV and no guest display, and
nothing in the run looked wrong. The flags are now asserted by
`workflow-check`, the arriving artifact is refused when it is missing either
kind, and `installer/check.ps1 -RequirePayloads` refuses a staged tree with no
payload pair in it -- three places, because the failure was silence and one
guard that is also silent when it breaks would not be an improvement.

The release states its version once, in `[workspace.package] version`. The tag
is checked against it in the workflow's first step, and the Inno Setup script
holds no version at all: it takes one through `/DAppVersion=` and refuses to
compile without it. This is a correction, not a preference -- the script used
to carry a copy synchronized by hand, and cutting 0.2.0 built
`VMLord-0.1.0-x86_64-setup.exe`, a name `cargo release-manifest` then could not
find, sixteen minutes into the run. `workflow-check` asserts the flag is still
passed and `installer/check.ps1` asserts the script has not grown a version of
its own again, so the copy cannot come back unnoticed.

The Forgejo mirror at `git.mrundead.org` is a pull mirror configured on Forgejo
itself, not a push from Actions. Forgejo's SSH is not reachable from GitHub's
runners, and rather than exposing it, the direction was inverted: Forgejo
fetches over HTTPS on its own schedule. GitHub therefore holds no credential
that can write to Forgejo, nothing has to be opened to the internet for it, and
the copy cannot write back to what it copies. The mirror lags by up to one
interval; a mirror is not a source of truth, so it may.

---

# Future Components

The architecture should support additional frontends without modifying the core.

Examples:

* Desktop GUI
* CLI
* REST API
* Automation tools
* LLM integrations

All of them should use the same application layer.

---

# Philosophy

VMLord is **not** intended to become another Hyper-V Manager.

The project focuses on providing an excellent Linux desktop workspace experience on Windows while keeping the internal architecture simple, modular and easy to evolve.
