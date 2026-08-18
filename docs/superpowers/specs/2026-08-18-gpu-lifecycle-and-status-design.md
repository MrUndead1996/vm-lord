# GPU-PV lifecycle and status — design

TASK-98. Wires the GPU-PV parts that already exist into one working cycle:
a VM stores the mode it was created with, a start stages a payload, exports
the host's drivers, attaches adapters and tells the guest what to mount, the
guest's report travels back, and the UI shows desired mode and runtime status
as two separate things.

## What is already built

Every piece below exists, is tested, and is called by nothing:

| Piece | Where | State |
| --- | --- | --- |
| Host capability discovery | `platform/gpu_discovery.rs` | Reachable through `VmRepository::host_gpu_capabilities`, consumed by nobody |
| Partition adapter enumeration | `platform/gpu_enumerate.rs` | Called only by discovery |
| Plan9 export building | `platform/gpu_exports.rs` | `#[allow(dead_code)]` |
| Writing shares into a configuration | `platform/hcs_config.rs::apply_plan9_shares` | Called only by its own tests |
| Payload staging | `platform/gpu_staging.rs` | `#[allow(dead_code)]` |
| HCS assignment | `platform/gpu_assignment.rs` | Called by nobody |
| Guest conversation (attach, recipe, probe) | `platform/agent_session.rs` | Runs, but reports only to the log |
| Status derivation | `app/gpu.rs::derive_status` | Complete and tested; fed `VmGpuFacts::default()` forever |
| Desired mode and runtime status as separate rows | `ui/lib.rs` | Rendered; the status is always `Disabled` |

What is missing is the wiring, and four decisions that wiring forces. They are
the substance of this design.

## Decisions

### The mode is stored in the mapping

`GpuMode` becomes a `#[serde(default)]` field of `VmComputeSystemMapping`,
alongside `network_mode` and `endpoint_id`, which were added the same way. A
mapping written before the field existed reads back as `GpuMode::None`, which
is what every VM created so far actually has, so nothing needs migrating.

`create_vm` and `update_vm` stop refusing `Default` and `Mirror`. `summary()`
reads the mode out of the mapping instead of reporting the constant
`GpuMode::None` it reports today.

### The payload is chosen by distribution, release and architecture

`stage_for_vm` wants a `GuestTarget`, and a `GuestTarget` carries
`kernel_release` — the exact kernel of the guest. `PayloadCatalog::select`
compares targets for equality, so a caller must know that kernel. The host
cannot: the kernel is a property of a booted guest, and Plan9 shares have to be
in the compute system's configuration *before* it starts.

The circle breaks in the catalog, not in the protocol, because the guest
already treats the kernel as soft. `gpu_recipe.rs` says so outright:
distribution, release and architecture are the hard gate; the kernel is not,
because DKMS builds against the running kernel's headers, and a payload's
`kernel_release` records what the recipe was *proven* on rather than what it
requires. Requiring a match would break GPU on every unattended kernel upgrade.

So the catalog grows `select_for_guest(distribution, release, architecture)`,
which ignores `kernel_release` and, when a triple has several entries, picks the
one with the highest `kernel_release` — the most recently proven. The host
stages that entry before the start; the guest checks applicability itself and
rebuilds the module for whatever kernel it runs.

The triple is known at creation, from `VmSource::CloudImage { profile, release }`,
and is recorded nowhere today. It joins the mode in the mapping as
`guest_target: Option<GuestTargetKey>` — three strings. A VM created from
`LocalMedia` has `None`: VMLord does not know what system is inside it, and
`None` honestly means "there is nothing to select a payload from". Such a VM
gets the WSL and driver shares and no payload share.

**The shipped catalog is empty.** `crates/gpu-payload/catalog/catalog.json` has
`entries: []`, so on any real host today selection fails with
`UnsupportedTarget`, and it will keep failing until a payload is published.
That makes one requirement non-negotiable: **a staging failure fails neither
the start nor the GPU.** It removes exactly one share from the manifest; the
rest are exported unchanged and the facts record `Partial` with a reason. This
is the path that will actually execute until a payload ships, so it is tested
as the main path and not as an edge case.

### The start runs on a thread

Staging is not cheap. `stage_payload` hard-links files, which is cheap in
bytes, but `prepare` unpacks the local archive on a cold cache and
`verify_generation` hashes the whole tree on every staging. The first GPU start
unpacks hundreds of megabytes; every later one reads the payload back off the
disk. Neither belongs on the thread that draws the window.

`StartTasks` is introduced, mirroring the existing `Builds` registry: a
`HashMap<String, Start>` under a mutex, a thread per start, and a slot for what
a finished thread leaves for the main thread to take. `start_vm` performs
synchronously only the refusals that are cheap and certain — not initialized, a
build in flight, an unknown VM, a start already in flight, a VM already running
— and returns `Ok` having handed the rest to the thread. This is the contract
background creation already has: an obvious mistake is the return value of the
call that made it, and everything else is a diagnostic on the next refresh.

`vm_state` gains a third fact, "is a start in flight", and reports
`VmState::Starting` for a VM that has one. The native backend produces that
variant nowhere today, although `derive_status` has handled it from the start.

A finished start is collected on refresh the way a finished build is: the
`Com1Session` moves into `com1_sessions`, a failure becomes a `Diagnostic`.
The session must change hands because the thread opens it and the repository
owns it.

### Facts live in memory and are never stored

`VmGpuStatus` already documents itself as "derived from facts, never stored: it
describes a moment". The facts behind it are treated the same way. Facts
observed by a previous process are confirmed by nothing after a restart — the
VM may have crashed, the guest may have lost the device — and they are cheap to
re-observe, because `reconnect_known_vms` reopens the agent session and the
same attach → recipe → probe chain answers within seconds.

The cost is the assignment. It happens once, right after the compute system
starts, and cannot be repeated for a VM this process did not start — nor should
it be, since the topology is already applied. So a reconnected VM has no
`assignment` fact, and reporting `AssignmentPending` ("the host has not attached
the GPU yet") would be a lie about the stage.

`GpuStatusCode` therefore gains `AssignmentUnknown`: "this VM was started before
VMLord restarted, so what is attached to it is not known". No persistence, an
honest sentence, and it survives only until the guest's first report.

## Components and data flow

### The facts channel

`GpuFacts` is a small platform type over `Arc<Mutex<BTreeMap<Uuid, VmGpuFacts>>>`
with four operations:

- `record_assignment(vm_id, GpuAssignment)` — from the start thread
- `record_guest(vm_id, GuestGpuReport)` — from the agent session thread
- `forget(vm_id)` — from every lifecycle point that ends a run
- `snapshot(vm_id)` — from `summary()` on every refresh

`observed_at` is stamped on write, not on read: stamping on read would date the
refresh rather than the observation.

`agent_session::serve` stops being a function that only logs. `report_mounts`,
`report_recipe` and `report_probe` keep their logging and additionally hand a
`GuestGpuReport` to a callback passed into `serve`. The function stays testable
against a peer made of bytes — in tests the callback collects into a vector.

`ProbeGpuResponse` carries a `GpuProbeVerdict`, from which `Ready`,
`DevicePresent` and `Failed` follow; the render node and driver name become
`GuestGpuDetail`. A recipe that fails before the probe is a `Failed` naming the
first `GpuRecipeStage` that broke, not silence.

### Start stages

The thread publishes each stage into the facts slot before entering it:

1. **Staging** — when the mode is not `None` and the mapping has a
   `guest_target`, stage the payload. A failure interrupts nothing; the payload
   share simply never appears.
2. **Exports** — `partition_adapters()` then
   `GpuExports::build(adapters, vm_directory)`. The payload joins the set only
   if step 1 actually created the directory.
3. **Config** — `hcs_config::apply_plan9_shares` into the stored configuration.
4. **Starting** — the existing `VmStartPipeline::start`, unchanged.
5. **Assigning** — `GpuAssignmentService::assign` against the running system. A
   failure records a `GpuFailure` and leaves the VM alone: it is running.
6. **WaitingForGuest** — `listen_for_agent`, with the manifest instead of the
   `None` it passes today.

### Where `Partial` comes from

HCS reports nothing about partiality: it either accepted the `Update` or it did
not. Partiality is therefore derived from export coverage, which is its only
honest source. With N adapters enumerated and M driver-package shares built,
`M < N` under `Mirror` is `Partial` — some adapters are attached but the guest
cannot mount their drivers. A missing payload is `Partial` too, with its own
wording. Full coverage is `Complete`, and `NativeGpuDetail` carries the adapter
name (under `Default`, the only one) and the count.

## Lifecycle rules

**Deletion is stopped-only.** Already implemented: `delete_vm` refuses a live VM
through `refuse_if_live`. It gains a refusal for a start in flight.

**Mode changes are stopped-only, and only the mode.** `update_vm` refuses when
the VM is not stopped *and* the requested `gpu_mode` differs from the stored
one. RAM and CPU keep changing on a live VM as they do today. A start in flight
is refused for the same reason `refuse_if_building` refuses a build.

**Nothing is retried.** Neither staging, nor assignment, nor a `Partial`
outcome gets a second attempt — not inside the start thread and not in the
background afterwards. `GpuAssignmentService::assign` is called once. The one
retry that exists in the start path, replacing an occupied HNS endpoint, is
unrelated to GPU and stays.

**Agent cleanup is graceful.** `agent_sessions.cancel` is already called at five
points: stop, force stop, delete, the release event for a compute system, and
process shutdown. `gpu_facts.forget(vm_id)` joins it at all five. A stopped VM
must not show yesterday's `GuestReady`; `derive_status` would report
`VmNotRunning` for it regardless, but the facts underneath have to go, or they
would resurface on the next start before the first write.

## UI

The create and edit forms gain a warning line under the GPU combo box, shown
only when the mode is not `None`, built from `HostGpuCapabilities` read once at
application start. Reading it per frame is not an option: `discover_host_gpu`
walks SetupAPI and the filesystem, and a form redraws sixty times a second. The
host does not change between two openings of a dialog enough to justify polling.

The two axes produce their own sentences: no partition adapter means "this host
has no GPU Partition adapter; the VM will start without a GPU"; no Linux payload
means "the Linux payload is not installed, so the guest will see the device but
will not render on it". Both problems produce both lines.

A capability read that *fails* is not a host without a GPU. The legacy backend
answers "not supported by this backend", and in that case nothing is shown:
claiming the GPU is unavailable where we merely could not ask is exactly what
the trait method's own documentation forbids.

The edit form's GPU combo box is disabled while the VM is not stopped, with the
reason as its tooltip — the refusal is visible before the click rather than
after it. The current line "GPU and network are not wired to the native backend
yet" loses its GPU half, which stops being true.

In the detail panel the "GPU" (desired) and "GPU status" (runtime) rows already
exist and already run through `derive_status`; their structure does not change.
What is added is what there is nowhere to read today: the adapter name and
render node from `NativeGpuDetail`/`GuestGpuDetail` while the status is active,
and the stable code beside the message on `Failed` and `Degraded`, so the screen
and the log can be matched to each other.

## Testing

Everything checkable without a host is checked without a host, as elsewhere in
this project:

- Catalog selection by triple with the kernel ignored, the newest entry chosen
  among duplicates, and the empty catalog — pure tests in `gpu-payload`.
- Deriving `Complete` / `Partial` / `Failed` from adapter and share counts — a
  pure function in the platform, tested without SetupAPI.
- Turning guest answers into `GuestGpuReport`, including a recipe that fails
  before the probe — `agent_session` tests against a byte stream, as attach,
  recipe and probe are already tested.
- Stage order, and that neither a staging failure nor an assignment failure
  fails the start — start-thread tests over substituted steps, following the
  existing `VmStartPipeline` and `VmBuildCycle` tests.
- The `update_vm` and `delete_vm` refusals and the fact cleanup at all five
  points — repository tests.
- The form warnings and the disabled combo box — UI tests in the existing
  harness.

Real-host coverage — the ignored tests for None, Default, Mirror, reconnect,
restarts, driver drift and deletion leaks — is TASK-99 and is out of scope here.
This task stops at the boundary reachable through `cargo test-windows`.

## Out of scope

- Real-host end-to-end tests and the documentation sweep (TASK-99).
- Publishing an actual payload into the catalog.
- Any change to the legacy backend, which keeps serving the functions that have
  not been migrated.
