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
  -> legacy-backend (dynamic C FFI, transitional fallback)
  -> appsandbox_core.dll
```

`legacy-backend` is the only crate that may contain `unsafe` code, raw C
pointers, or Windows DLL loading for the temporary AppSandbox implementation.
It dynamically loads the prebuilt `appsandbox_core.dll` placed next to
`vmlord.exe`; no C types cross into `core`, `app`, or `ui`.

`platform` is the Windows-native foundation for the incremental replacement.
It depends only on `core`, never on `app` or `ui`, and contains all direct
`windows-rs` calls. It owns HCS/HCN handles and Windows events through safe
RAII wrappers, and converts Windows failures to `RepositoryError` values that
include the operation, VM name when applicable, and HRESULT.

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
so an empty JSON object is always passed. A successful shutdown means HCS
delivered the request, not that the guest powered off, so forced stop remains a
separate action.

A VM whose guest exposes no shutdown channel HCS can use fails the shutdown
*operation* with `ERROR_NOT_SUPPORTED` while the call and its options are
accepted; that HRESULT is reported as its own error naming a forced stop as the
remaining option, because no retry helps. Whether a fully booted guest fares
better is not yet established: the legacy AppSandbox backend resolved
`HcsShutDownComputeSystem` but never called it, implementing graceful shutdown
over its own in-guest agent instead, so VMLord may need the same.

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
remaining migration tasks land: GPU mode and SSH port are `None`, guest agent
status is `Unknown`, and display and SSH connections report that the backend does
not support them. Network mode is reported from the VM's mapping, because the
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
uses `%LOCALAPPDATA%\VMLord\vms` for VM data and
`%LOCALAPPDATA%\VMLord\logs\vmlord.log` for logs. The configuration directory
and the default VM and log directories are created on first launch.

`core::logging` installs the shared `log` backend after settings are loaded and
before the backend starts. It writes records at the configured `log_level` to
both standard output and the append-only `log_file_path`; all Rust crates use
the `log` facade to emit application records.

The current UI initializes the backend, shows availability and diagnostics,
lists known VMs, can create Linux VMs from ISO images, and can edit them. It
submits safe requests through the application layer, which knows nothing about
which backend serves them. Edit is available whichever state the VM is in;
Delete stays limited to stopped VMs. Snapshots remain future application-layer
work.

Under `VMLORD_BACKEND=legacy`, the same actions reach AppSandbox's C API
instead: Start invokes `asb_vm_start`; Stop invokes the graceful
`asb_vm_shutdown`; Force stop invokes `asb_vm_stop`; Edit uses AppSandbox's
configuration setters; Connect invokes `asb_vm_open_display`, which opens or
focuses the temporary AppSandbox IDD window after the guest display driver is
ready. It calls `asb_detach` on exit so it never stops VMs.

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
