# ARCHITECTURE.md

# VMLord Architecture

## Overview

VMLord is a native Windows application for managing Linux virtual machines using the Windows Host Compute System (HCS).

The architecture is intentionally layered to separate user interface, business logic and platform-specific implementation.

During the initial development phase the project reuses the AppSandbox C backend through a thin FFI layer.

The long-term objective is a fully Rust-native implementation.

---

# Design Goals

The architecture should:

* isolate platform-specific code
* isolate unsafe Rust
* keep the UI independent from virtualization logic
* allow gradual replacement of the legacy backend
* remain testable
* support future CLI and automation APIs

---

# High-Level Architecture

Current architecture:

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
|      Rust Core API        |
+---------------------------+
              |
              v
+---------------------------+
|         FFI Layer         |
+---------------------------+
              |
              v
+---------------------------+
| AppSandbox C Backend      |
+---------------------------+
              |
              v
+---------------------------+
| Windows HCS / HNS / APIs  |
+---------------------------+
```

---

# Target Architecture

After migration:

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

No C code should remain.

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
profiles) and `provisioning` (what VMLord delivers into a Linux guest), plus
the request, summary and repository types.

---

## Platform

Contains:

* Windows API wrappers
* FFI
* unsafe Rust

This is the only layer allowed to interact with operating system APIs.

---

# Current Backend

The AppSandbox backend is currently responsible for:

* HCS integration
* VM lifecycle
* networking
* display
* GPU configuration

Rust communicates with it through FFI.

The backend is considered an implementation detail.

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
  -> legacy-backend (dynamic C FFI, transitional fallback)
  -> appsandbox_core.dll
```

`legacy-backend` is the only crate that may contain `unsafe` code, raw C
pointers, or Windows DLL loading for the temporary AppSandbox implementation.
It dynamically loads the prebuilt `appsandbox_core.dll` placed next to
`vmlord.exe`; no C types cross into `core`, `app`, or `ui`.

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
list and its deletions too over a service only the networked VMs need. Setting
`VMLORD_BACKEND=legacy` selects the AppSandbox backend instead, for as long as
the migration leaves it something the native backend cannot do; any other value
(including an unset one) selects the native backend, so a typo cannot silently
keep VMLord on the backend being retired.

The native backend deliberately reports less than AppSandbox did while the
remaining migration tasks land: GPU mode is `None`, guest agent status is
`Unknown`, and display and SSH connections report that the backend does not
support them. SSH availability is not among them: it is read from the VM's
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
event and queues it; the repository drains that queue in `take_diagnostics` on
every refresh, logs each event, surfaces the significant ones as diagnostics,
and releases the handle of a VM that is gone. The enumeration remains the sole
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
the `NetworkAdapters` section to match. GPU modes other than `None` are
rejected until their own task lands, as are the `External` and `Internal`
network modes.

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
`%LOCALAPPDATA%\VMLord\logs\vmlord.log` for logs and
`%LOCALAPPDATA%\VMLord\images` for downloaded distribution images. The
configuration directory and the default VM, log and image directories are
created on first launch. `image_cache_path` carries `#[serde(default)]` and is
filled in on load when absent, so a `settings.toml` written before the field
existed keeps loading without a migration.

`core::logging` installs the shared `log` backend after settings are loaded and
before the backend starts. It writes records at the configured `log_level` to
both standard output and the append-only `log_file_path`; all Rust crates use
the `log` facade to emit application records.

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
named pipe is the platform layer's business. Snapshots remain future
application-layer work.

Under `VMLORD_BACKEND=legacy`, the same actions reach AppSandbox's C API
instead: Start invokes `asb_vm_start`; Stop invokes the graceful
`asb_vm_shutdown`; Force stop invokes `asb_vm_stop`; Edit uses AppSandbox's
configuration setters; Connect invokes `asb_vm_open_display`, which opens or
focuses the temporary AppSandbox IDD window after the guest display driver is
ready. It calls `asb_detach` on exit so it never stops VMs.

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
is a table of data -- two URL templates, the name of the checksum file, the
guest's default user and its admin group -- rather than a trait with one
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

### VM update contract

The edit workflow follows these rules:

* A VM can be edited in any state; a running VM applies the change on its next
  start, and the application layer says so.
* RAM must be at least 512 MiB and aligned to 2 MiB steps.
* CPU core count must be at least 1.
* GPU modes other than `None` are rejected by the native backend until their own
  migration task lands.
* Network mode accepts `None` and `Nat`; `External` and `Internal` are rejected
  with a message naming the task that will add them.
* Disk size is read-only in the current backend contract and requires recreating
  the VM to change.
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
systemd units that carry its SSH daemon.

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

`Provisioning::ssh_config` is the one place a configuration is derived from what
cloud-init was asked to do, and `VmComputeSystemMapping::ssh` is where it is
kept: `None` there is a VM created with SSH switched off. The mapping validates
it on the way in and on the way back out, through the same user-name validator
the create form uses, so a name that reaches an `ssh -l` argument is one Linux
would accept whether it was typed a minute ago or read from an edited document.

Provisioning refuses an SSH server with neither a deployed key nor a password
for the same reason it refuses a VM with neither a password nor SSH: there is no
third credential to fall back on, so the guest would run a daemon nobody can get
through.

### The VM's SSH key pair

Every VM gets its own ed25519 pair rather than sharing one. AppSandbox kept a
single key under `%ProgramData%\AppSandbox\ssh\id_appsandbox`
(`legacy-backend/src/windows.rs:691`), where the compromise of one sandbox
reached every other one.

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
Deleting the VM deletes the key with the directory.

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
`/etc/default/keyboard`, and `growpart`/`resize_rootfs`. Growing the root
filesystem is a VMLord promise, so it is stated rather than left to cloud-init's
defaults. `SshAccess::Disabled` adds a `runcmd` that disables the SSH daemon: a
cloud image ships it enabled, and silence would make the choice void. The unit
names come from `DistroProfile::ssh_units` -- `ssh.socket`/`ssh.service` in the
Debian family, `sshd.service` elsewhere -- so the generator knows no
distribution by name.

`meta-data` carries `instance-id`, formatted from the VM's id, and
`local-hostname`, the VM name. The identifier never changes, which is what makes
it safe to leave the seed attached: cloud-init re-reads it on every boot and
skips the per-instance modules it has already run.

The documents are printed by hand, not serialised: they are small, fixed, and
the `#cloud-config` line is a comment to YAML and a format marker to cloud-init.
Every value from outside is printed as a single-quoted YAML scalar, where YAML
has no escape sequences at all; the keyboard layout is escaped a second time for
the shell, because `/etc/default/keyboard` is read with `source`, where `$` and
a quote are code. The tests read the result back with a YAML parser and assert
on meaning, the way cloud-init's PyYAML will.

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

One limitation, stated rather than forgotten: `/etc/default/keyboard` is
Debian-family. Fedora keeps the setting in `/etc/vconsole.conf` under different
keys, which is a different mechanism rather than a different value; every other
key in the document is a cloud-init module that works anywhere.

Creating a VM is now the native backend's alone. AppSandbox's model was "media
plus unattended answers", and the tool that carried out the answers was
`iso-patch.exe`: a host-side Ubuntu installer with its own ext4 writer, its own
squashfs reader, and a partition table written through
`IOCTL_DISK_SET_DRIVE_LAYOUT_EX`. Importing a cloud image leaves it nothing to
do, so VMLord stopped shipping it, and `AppSandboxBackend::create_vm` refuses
rather than letting the DLL fail on a missing executable. Everything the legacy
backend does with VMs it did not create -- list, start, stop, edit, delete --
still works, which is what the transitional path is for. What remains of
`iso-patch` here are citations to its C sources, which recorded Windows
behaviour worth keeping; only the binary is gone.

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
holds the plaintext; and the seed volume built from both. After that the branches
converge: `config.json` is written, the VM is granted access to its disk and to
its medium, the compute system is created, and the mapping is inserted last, so
a VM is known to VMLord only once it exists in HCS.

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
that looks like one. Deleting a VM without its disks removes `config.json`
alone, so the seed stays with the disk it belongs to.

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

A graceful stop leaves the session alone: the guest is still printing what it
does on the way down, and the pipe closing is what ends the capture. A force
stop, a delete, an HCS exit event, and VMLord's own shutdown all cancel it,
because in each of those cases nothing will ever close the pipe from the other
end. `take_diagnostics` reaps sessions that are over: one that finished with its
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
readiness wait as one operation; a failure is reported to the user through the
shared diagnostics buffer rather than through a return value nobody is waiting on.

### Waiting for the guest

A build no longer ends where HCS accepts a compute system. That moment is where
VMLord stops working and well before the VM does anything: cloud-init installs
the SSH key at its init stage and applies `packages:` later, at its config stage,
so a probe of port 22 answers "ready" in the middle of the work. `VmBuildCycle`
therefore carries on -- `BuildStep::Starting`, then `BuildStep::AwaitingGuest` --
and the build leaves the list only once the guest's own `cloud-init status
--wait` has answered.

`guest_ready::GuestReadiness` waits in three phases, each with its own timeout
and its own failure, because the three are different facts about the guest: the
endpoint has to be given an address (HNS assigns it, the DHCP server delivers
it), something has to answer a TCP connection on port 22, and cloud-init has to
report that it is done. The transport is `ssh.exe` from `%SystemRoot%\System32\
OpenSSH`, driven as a child process behind a seam: every maintained Rust SSH
client is async-only, and VMLord has no async runtime, while a second vendored C
build under MSVC would cost more than this does. Its absence -- OpenSSH Client is
an optional Windows feature -- is an outcome of its own, named in the message a
person acts on. The child's output goes to `cloud-init-status.log` beside
`com1.log` rather than to a pipe: `--wait` prints for as long as it runs, and a
pipe nobody drains fills and deadlocks the child against the loop polling it.

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
address, 300 for port 22, 1200 for cloud-init, 10 for one connection attempt --
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
calls `list_vms`, and `take_diagnostics` -- the `&mut self` call that follows it
-- is where finished threads are joined by `BuildRegistry::reap`.

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
dialog says only that the key lives with the VM.

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
    ffi/

app/

ui/
```

---

# Migration Strategy

Backend replacement happens incrementally.

Recommended order:

1. HCS
2. HNS
3. Networking
4. GPU
5. Display
6. SSH
7. Diagnostics

Each completed Rust module replaces its C equivalent.

No module should exist in both languages permanently.

---

# FFI Principles

The FFI layer should remain extremely small.

Responsibilities:

* data conversion
* handle conversion
* string conversion
* error conversion

Business logic must never be implemented in FFI.

---

# Unsafe Rust

Unsafe code belongs only inside:

* platform
* ffi
* Windows wrappers

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
