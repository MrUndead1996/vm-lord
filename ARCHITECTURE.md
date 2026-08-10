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
Delete stays limited to stopped VMs. Snapshots remain future application-layer
work.

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
validation, including the user-name rules the UI used to hold; `core::distro`
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

The native backend builds a VM from a `CloudImage`; the legacy AppSandbox
backend refuses one outright and is given empty credentials for `LocalMedia`:
its own model was "media plus unattended answers", which the domain no longer
spells. That is a deliberate loss on a transitional path -- #66 removes the
iso-patch dependency it belongs to.

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

`CloudDiskImporter` is `Fn(&CloudImage, u64, &Path) -> Result<(),
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

What is deliberately absent is everything about time. The import is synchronous:
`create_vm` runs on the calling thread, so a download of several hundred
megabytes happens with the UI waiting on it. It is also silent -- the composition
root hands `open_cloud_image` a default `ProgressPublisher` nobody reads and an
`AtomicBool` nobody sets, which are the exact parameters #64 will fill in without
changing a signature. There is no background thread, no `Building` state and no
progress reporting until then; the seam for all three already exists, and this
task deliberately stops short of using it.

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
