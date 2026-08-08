# TASK-33: HCS state and event watching

Design for observing the HCS compute systems VMLord owns: a callback registered
on each held compute system, the events it delivers, and how those events reach
the log and the user.

## Goal

VMLord currently learns what happened to a VM only by asking: the UI refreshes
once a second and `HcsEnumerateComputeSystems` reports each system's state. That
answers *what state a VM is in* and nothing else. Three things it cannot answer
at all:

- **why** a VM stopped — a guest that powered itself off and a guest that
  crashed are both simply absent from the enumeration;
- that the guest **crashed**, and where its dump went;
- that the **Host Compute Service itself disconnected**.

A fourth problem is not about information but about hygiene: `VmConnections`
holds a compute-system handle for every VM VMLord started, and nothing releases
the handle of a VM whose guest shut itself down. The handle survives until the
process exits.

Watching HCS events closes all four.

## Scope

The watcher is an **event source layered on top of the existing one-second
poll**, not a replacement for it. The enumeration remains the single authority on
VM state; the watcher adds the facts the enumeration does not carry, and releases
handles for systems that are gone. Moving state itself onto the watcher stays a
later, separate decision — this design keeps the door open by making the event
queue independent of who reads it, but does not walk through it.

In scope: a `watch` module in the platform crate, callback registration on every
held compute system, the event queue, the translation of events into log records
and diagnostics, handle release for exited systems, unit tests, and an ignored
Hyper-V integration test.

Out of scope:

- **Guest readiness / `AgentStatus`.** HCS reports nothing about whether a guest
  finished booting, so `agent_status` stays `AgentStatus::Unknown`. That is
  agent work, not watch work.
- **`VmState` derivation.** `repository.rs::vm_state` keeps reading the
  enumeration.
- **`BackendStatus`.** A service disconnect does not move the backend to
  `Unavailable`; see "Service disconnect" below.
- **Process-level events.** `HcsEventProcessExited` concerns host compute
  *processes*, which VMLord does not create.

## The Windows API

`crates/platform/Cargo.toml` already enables `Win32_System_HostComputeSystem`,
so no new feature is needed.

```rust
pub unsafe fn HcsSetComputeSystemCallback(
    computesystem: HCS_SYSTEM,
    callbackoptions: HCS_EVENT_OPTIONS,
    context: *const c_void,
    callback: HCS_EVENT_CALLBACK,
) -> Result<()>;

#[repr(C)]
pub struct HCS_EVENT {
    pub Type: HCS_EVENT_TYPE,   // #[repr(transparent)] wrapper over i32
    pub EventData: PCWSTR,
    pub Operation: HCS_OPERATION,
}
```

Registration passes `HcsEventOptionEnableVmLifecycle` so VM lifecycle events are
delivered and not only operation callbacks.

The signature above is the one the crate documents for the sibling
`HcsSetProcessCallback`, read through Context7 against
`microsoft.github.io/windows-docs-rs`; the compute-system entry documents the
same parameters without spelling out the Rust types. The implementation
confirms the exact `HCS_EVENT_CALLBACK` alias against the crate as it compiles,
rather than trusting this transcription.

## Module: `crates/platform/src/watch.rs`

The sole home of `HcsSetComputeSystemCallback`, mirroring how `hcs.rs` is the
sole home of the other HCS entry points.

```rust
/// One thing HCS reported about a compute system, already translated out of FFI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HcsVmEvent {
    pub vm_id: Uuid,
    pub vm_name: String,
    pub kind: HcsEventKind,
    /// The event's `EventData`, verbatim and unparsed, when HCS supplied one.
    pub details: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HcsEventKind {
    Exited,
    CrashInitiated,
    CrashReport,
    ServiceDisconnect,
    /// An event VMLord deliberately does not act on, carried with its raw type
    /// so it can still be logged rather than silently dropped.
    Ignored(i32),
}

/// Translates an `HCS_EVENT_TYPE` value into what VMLord does about it.
fn classify(event_type: i32) -> HcsEventKind;
```

`classify` is a pure function over the raw `i32`, which is what makes the whole
mapping testable on any platform. It is the only place the event-type table
lives.

### The queue

```rust
/// The queue a compute system's callback writes into and the repository drains.
#[derive(Clone, Default)]
pub struct VmEventSink(Arc<Mutex<EventQueue>>);

struct EventQueue {
    events: VecDeque<HcsVmEvent>,
    /// Events dropped because the queue was full, since the last drain.
    dropped: usize,
}

impl VmEventSink {
    fn push(&self, event: HcsVmEvent);
    /// Takes every queued event and the number dropped since the last drain.
    pub fn drain(&self) -> (Vec<HcsVmEvent>, usize);
}
```

Capacity is 256 events. When full, `push` discards the **oldest** event and
increments `dropped`: a burst of events matters less than the most recent state
of the world, and the count means the loss is reported rather than hidden.

A poisoned mutex is recovered with `unwrap_or_else(|poisoned| poisoned.into_inner())`,
as `repository.rs` already does for its diagnostics buffer. Losing event
reporting because an unrelated thread panicked would be worse than reading a
buffer that a panic left intact.

### The registration guard

```rust
/// An active `HcsSetComputeSystemCallback` registration, removed on drop.
pub struct SystemWatch {
    /// The system the callback is registered on. A non-owning copy: the
    /// `HcsSystem` this was registered against still owns and closes the
    /// handle, and must outlive this watch (see `WatchedSystem` below).
    system: HCS_SYSTEM,
    /// The `Arc<WatchContext>` handed to HCS, as a raw pointer.
    context: *const WatchContext,
}

impl SystemWatch {
    pub fn register(
        system: &HcsSystem,
        vm_id: Uuid,
        vm_name: &str,
        sink: &VmEventSink,
    ) -> Result<Self, RepositoryError>;
}
```

`HcsSystem` gains a `pub(crate)` accessor for its raw handle so `watch.rs` can
register against it; the handle stays owned by `HcsSystem` and is still closed
only by its `Drop`.

## Callback lifetime: the one hard `unsafe` problem

HCS invokes the callback on a thread it owns, with the `context` pointer we
supplied. The pointer must stay valid for as long as a callback can run — and a
callback may already be executing when we remove the registration.

- The context is an `Arc<WatchContext>` (`vm_id`, `vm_name`, `sink`), handed to
  HCS as `Arc::into_raw`.
- The callback reconstructs a **borrow**, not ownership: `&*context.cast::<WatchContext>()`.
  It never rebuilds an `Arc`, so it can never drop the allocation.
- `SystemWatch::drop` first clears the registration (`HcsSetComputeSystemCallback`
  with a null context and no callback), and reclaims the `Arc` with
  `Arc::from_raw` **only if clearing succeeded**.
- If clearing fails, the context is deliberately leaked with `mem::forget` and
  the failure is logged at `ERROR`. Leaking one small `Arc` is not comparable to
  a thread inside HCS dereferencing freed memory.

Two properties of the callback body are mandatory, and both are about it running
on a thread that is not ours:

- **It must not panic across the FFI boundary.** The body is wrapped in
  `catch_unwind`; a panic crossing `extern "system"` is undefined behaviour. A
  caught panic is swallowed there and reported by the main thread as a dropped
  event.
- **It must return quickly and touch nothing but the queue.** HCS waits for it.
  Inside: `classify`, the `PCWSTR` → `String` conversion of `EventData`, and one
  locked push. No HCS calls, no filesystem access, and deliberately **no `log`
  macros** — writing to the log file takes a lock on the service's thread, which
  invites both a stall inside HCS and a lock-ordering deadlock against the main
  thread. Logging is the drainer's job.

Two things this design cannot verify from documentation, and which the manual
Hyper-V pass therefore checks: that clearing the callback with a null function
pointer is supported, and that no further callback arrives once clearing
returns. If clearing turns out to be unsupported, the leak branch becomes the
normal path instead of the failure path — the code does not change, only the log
level does.

## Where registration happens

Registration belongs wherever `VmConnections` takes ownership of a compute
system:

- `HcsVmRepository::hold_started_system`, after a successful start;
- `reconnect_known_vms`, for each system reclaimed at startup.

A created-but-never-started VM is not watched: the creation pipeline does not
retain its handle, and lifecycle events only concern a system that runs.

`VmConnections` stops storing a bare `HcsSystem` and stores both the system and
its watch:

```rust
struct WatchedSystem {
    /// Declared before `system` on purpose: fields drop in declaration order,
    /// and the callback must be removed before the handle it is registered on
    /// is closed.
    ///
    /// `None` when registration failed: the VM is held and usable, just
    /// unwatched (see "Errors").
    watch: Option<SystemWatch>,
    system: HcsSystem,
}
```

The drop-order comment is load-bearing. A future refactor that reorders these
fields closes the handle while a callback registration still points at it, and
nothing in the type system objects.

`VmConnections` holds the sink and registers the watch itself on insert, so no
caller has to remember to. Its `insert` therefore needs the VM's name as well as
its id — it takes the `VmComputeSystemMapping` both current callers already have
in hand, instead of a bare `vm_id`.

## Errors

A failed registration does not fail the operation that produced the handle. A
started VM runs whether or not VMLord can watch it; refusing the start would
turn a loss of observability into a loss of function. So registration failure is
a `WARN` plus a `Warning` diagnostic, exactly as `hold_started_system` already
treats a failure to hold the handle at all.

Queue overflow is reported: each drain that finds `dropped > 0` logs one `WARN`
naming the count. Silently losing events would make the log lie about what
happened.

## Draining: what the main thread does

`HcsVmRepository::take_diagnostics` drains the sink before returning
diagnostics. It is already `&mut self` and already called on every refresh
(`WorkspaceApp::refresh` → `collect_diagnostics`) immediately after `list_vms`,
so an event surfaces within one refresh interval — at most about a second.

| HCS event type | Log | Diagnostic | Side effect |
| --- | --- | --- | --- |
| `HcsEventSystemExited` (1) | `INFO` | `Info`: the VM stopped | release the handle |
| `HcsEventSystemCrashInitiated` (2) | `WARN` | `Warning`: the guest is crashing | — |
| `HcsEventSystemCrashReport` (3) | `ERROR` | `Error`, including `EventData` | — |
| `HcsEventServiceDisconnect` (0x0200_0000) | `ERROR` | `Error`: HCS disconnected | release the handle |
| `HcsEventSystemRdpEnhancedModeStateChanged` (4), `HcsEventSystemSiloJobCreated` (5), `HcsEventSystemGuestConnectionClosed` (6), `HcsEventProcessExited` (0x0001_0000) | `DEBUG` | — | — |
| any other value | `DEBUG` with the raw number | — | — |

Per the epic's convention: `DEBUG` through `ERROR`, no `TRACE`.

`EventData` is **not parsed**. HCS documents no stable schema for it, so it goes
to the log verbatim and into a diagnostic truncated to 200 characters. This
is the same rule already applied to the service-properties document in
`hcs.rs::parse_service_result`, which validates only that the JSON parses and
carries no error.

Handle release is the drainer's work, not the callback's: `VmConnections` lives
in the repository and is not `Sync`, and the event only records a fact. This is
what closes the leak of a handle to a guest that powered itself off.

### Service disconnect

A disconnect is reported as an error and releases the handle, but does not move
`BackendStatus` to `Unavailable`. The next poll — a second later at most — calls
`enumerate_systems`, and that call fails on its own if the service is really
gone, which already moves the backend to `Unavailable` through the existing path
in `WorkspaceApp::refresh`. Setting the status here instead would need a way
back out of `Unavailable`, and `WorkspaceApp` has none short of a restart, so a
transient disconnect would leave the UI wrongly dead.

## Application and UI

Neither `crates/app` nor `crates/ui` changes. Events reach the user through the
existing `take_diagnostics` → `WorkspaceApp::diagnostics` → `render_diagnostics`
path. This keeps the UI free of business logic, as the project rules require:
the UI is told what to show and decides nothing about it.

## Testing

Unit tests in `watch.rs`, none of which need Windows or a live HCS:

- `classify` covers every row of the table above, including an unknown value,
  which must become `Ignored` with the raw number preserved;
- the queue drains FIFO;
- a full queue discards the oldest event and counts the loss;
- `drain` reports the dropped count once and resets it;
- translating each `HcsEventKind` into a diagnostic produces the expected level
  and names the VM.

Unit test in `repository.rs`: an `Exited` event pushed directly onto the sink
results, after `take_diagnostics`, in an `Info` diagnostic naming the VM and in
the VM's handle no longer being held by `VmConnections`.

Integration test in `crates/platform/tests/hyperv.rs`, `#[ignore]` like the rest
of the file: create a VM, start it, register a watch, `terminate` it, and wait
with a timeout for an `Exited` event to appear in the sink. This is the only
check that proves the callback fires at all — every other test exercises code
that runs *after* the callback.

Manual verification on live Hyper-V, per the epic's testing decision:

- a guest that shuts itself down from inside produces `Exited`, and the handle
  is released;
- clearing the registration succeeds, and no callback arrives afterwards;
- stopping the Host Compute Service produces `ServiceDisconnect`.

## Documentation

`ARCHITECTURE.md` currently states that guest observability waits on the
watch/event work. That line is corrected rather than deleted: lifecycle events
now exist, guest readiness still does not.
