# HCS State and Event Watching Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Register an HCS event callback on every compute system VMLord holds, so the log and the UI report why a VM stopped, that its guest crashed, and that the Host Compute Service disconnected — and so the handle of a VM that powered itself off is released.

**Architecture:** The callback runs on a thread HCS owns, so it does the minimum possible: classify the event and push it onto a shared, bounded queue. The main thread drains that queue inside `HcsVmRepository::take_diagnostics`, which already runs on every one-second refresh, turning events into log records and diagnostics and releasing dead handles. The existing enumeration stays the only authority on VM state.

**Tech Stack:** Rust 2024, `windows` 0.61 (`Win32_System_HostComputeSystem`, already enabled), `log`, `uuid`, `egui`/`eframe` in the UI (untouched by this plan).

**Spec:** `docs/superpowers/specs/2026-08-07-hcs-state-and-event-watch-design.md`

## Global Constraints

- Branch: `task-33-hcs-watch`. Commit subjects are prefixed `TASK-33: `.
- Commits must be authored as the agent:
  `GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local git commit -m "TASK-33: ..."`
- Host-side tests (`core`, `app`): `cargo test -p vmlord-core -p vmlord-app`
- Platform tests: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu`
- Whole workspace build: `cargo build --target=x86_64-pc-windows-gnu`
- Lints: `cargo clippy --target=x86_64-pc-windows-gnu --all-targets`
- Logging uses `log` at `DEBUG`, `INFO`, `WARN`, `ERROR` only. **`TRACE` is never used.**
- All `unsafe` stays inside `crates/platform`. Every `unsafe` block carries a `// SAFETY:` comment stating why it is sound. The workspace denies `unsafe_code`; `crates/platform/Cargo.toml` already allows it for this crate alone.
- `crates/app` and `crates/ui` are **not modified by this plan**. Events reach the user through the existing `take_diagnostics` → `WorkspaceApp::diagnostics` → `render_diagnostics` path.
- No new dependencies and no new `windows` crate features.
- Comments explain *why*, matching the density of the surrounding code. Do not add comments restating what a line does.

---

## File Structure

| File | Responsibility |
| --- | --- |
| `crates/platform/src/watch.rs` (create) | The whole watch: event vocabulary, classification, the bounded queue, the drain-and-report step, the FFI callback, and the RAII registration guard. Sole home of `HcsSetComputeSystemCallback`. |
| `crates/platform/src/hcs.rs` (modify) | Gains a `pub(crate)` non-owning accessor for a system's raw handle, so `watch.rs` can register against it. |
| `crates/platform/src/reconnect.rs` (modify) | `VmConnections` stores each held system together with its registration, and registers on insert. |
| `crates/platform/src/repository.rs` (modify) | Owns the sink, hands it to reconnect, drains it in `take_diagnostics`, releases handles the drain reports as dead. |
| `crates/platform/src/lib.rs` (modify) | Declares `mod watch` and re-exports its public items. |
| `crates/platform/tests/hyperv.rs` (modify) | Ignored live test proving HCS actually calls back. |
| `ARCHITECTURE.md` (modify) | Corrects the line saying guest observability waits on this work. |

Nothing outside `crates/platform` and `ARCHITECTURE.md` changes.

---

## Task 1: Event vocabulary and classification

Creates `watch.rs` with the domain types and the one function that decides what VMLord does about each HCS event type. Pure logic, no FFI yet.

**Files:**
- Create: `crates/platform/src/watch.rs`
- Modify: `crates/platform/src/lib.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `pub struct HcsVmEvent { pub vm_id: Uuid, pub vm_name: String, pub kind: HcsEventKind, pub details: Option<String> }`
  - `pub enum HcsEventKind { Exited, CrashInitiated, CrashReport, ServiceDisconnect, Ignored(i32) }`
  - `fn classify(event_type: i32) -> HcsEventKind` (private to `watch.rs`)

- [ ] **Step 1: Write the failing test**

Create `crates/platform/src/watch.rs` with only the test module and the imports it needs, so the test names the API before it exists:

```rust
//! Watching the HCS compute systems VMLord owns.
//!
//! VMLord otherwise learns about its VMs only by asking: the enumeration
//! reports what state a compute system is in. It cannot report why a VM
//! stopped, that its guest crashed, or that the Host Compute Service itself
//! disconnected. Those arrive as events, and only here.
//!
//! The enumeration remains the authority on VM state; this module adds the
//! facts the enumeration does not carry.

use uuid::Uuid;

#[cfg(test)]
mod tests {
    use super::{HcsEventKind, classify};

    /// The numbers are asserted, not just the names: these values come from the
    /// `windows` crate, and a constant renamed or re-valued upstream must fail
    /// here rather than silently reclassify a crash as noise.
    #[test]
    fn every_event_vmlord_acts_on_is_classified() {
        assert_eq!(classify(1), HcsEventKind::Exited);
        assert_eq!(classify(2), HcsEventKind::CrashInitiated);
        assert_eq!(classify(3), HcsEventKind::CrashReport);
        assert_eq!(classify(0x0200_0000), HcsEventKind::ServiceDisconnect);
    }

    /// RDP enhanced mode, silo-job creation, a closed guest connection and a
    /// host compute *process* exiting are all real events VMLord has nothing to
    /// do about. They stay `Ignored` so the drain can still log them.
    #[test]
    fn events_vmlord_does_not_act_on_keep_their_raw_type() {
        for event_type in [4, 5, 6, 0x0001_0000] {
            assert_eq!(classify(event_type), HcsEventKind::Ignored(event_type));
        }
    }

    #[test]
    fn an_unknown_event_type_is_ignored_and_not_lost() {
        assert_eq!(classify(9_999), HcsEventKind::Ignored(9_999));
    }
}
```

- [ ] **Step 2: Declare the module so the test is compiled**

In `crates/platform/src/lib.rs`, add `mod watch;` to the module list, keeping it alphabetical (after `mod vhd;`):

```rust
mod vhd;
mod watch;
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu watch`

Expected: FAIL to compile, with `cannot find type HcsEventKind in this scope` and `cannot find function classify in this scope`.

- [ ] **Step 4: Write the minimal implementation**

Add above the test module in `crates/platform/src/watch.rs`:

```rust
use windows::Win32::System::HostComputeSystem::{
    HcsEventServiceDisconnect, HcsEventSystemCrashInitiated, HcsEventSystemCrashReport,
    HcsEventSystemExited,
};

/// One thing HCS reported about a compute system, already translated out of FFI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HcsVmEvent {
    pub vm_id: Uuid,
    pub vm_name: String,
    pub kind: HcsEventKind,
    /// The event's `EventData`, verbatim and unparsed, when HCS supplied one.
    ///
    /// HCS documents no stable schema for it, so nothing here interprets it.
    pub details: Option<String>,
}

/// What VMLord does about an HCS event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HcsEventKind {
    /// The compute system stopped executing.
    Exited,
    /// The guest started crashing.
    CrashInitiated,
    /// The guest crashed and HCS wrote a report.
    CrashReport,
    /// The Host Compute Service dropped its connection to the system.
    ServiceDisconnect,
    /// An event VMLord deliberately does not act on, carried with its raw type
    /// so it can still be logged rather than silently dropped.
    Ignored(i32),
}

/// Decides what VMLord does about an `HCS_EVENT_TYPE` value.
///
/// The single place the event table lives, and a plain `i32` function on
/// purpose: that is what makes the whole mapping testable without HCS.
fn classify(event_type: i32) -> HcsEventKind {
    match event_type {
        value if value == HcsEventSystemExited.0 => HcsEventKind::Exited,
        value if value == HcsEventSystemCrashInitiated.0 => HcsEventKind::CrashInitiated,
        value if value == HcsEventSystemCrashReport.0 => HcsEventKind::CrashReport,
        value if value == HcsEventServiceDisconnect.0 => HcsEventKind::ServiceDisconnect,
        other => HcsEventKind::Ignored(other),
    }
}
```

If the compiler rejects any of those four constant names or the `.0` field access, the transcription of the `windows` 0.61 API in the spec is wrong for this version. Find the real names with `cargo doc -p windows --open` or the crate's `HostComputeSystem` module, and fix the `use`. Do **not** replace them with literal numbers: the test already pins the numbers, and the constants are what keep the code readable.

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu watch`

Expected: PASS, 3 tests.

`HcsVmEvent` is unused so far and will warn. That is expected and resolved in Task 2; do not add `#[allow(dead_code)]`.

- [ ] **Step 6: Commit**

```bash
git add crates/platform/src/watch.rs crates/platform/src/lib.rs
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
  git commit -m "TASK-33: Classify the HCS events VMLord acts on"
```

---

## Task 2: The bounded event queue

The queue the callback writes into from HCS's thread and the repository drains from the main thread. This is the whole thread boundary of the feature.

**Files:**
- Modify: `crates/platform/src/watch.rs`
- Modify: `crates/platform/src/lib.rs`

**Interfaces:**
- Consumes: `HcsVmEvent` (Task 1).
- Produces:
  - `pub struct VmEventSink` — `Clone + Default`
  - `pub(crate) fn VmEventSink::push(&self, event: HcsVmEvent)`
  - `pub(crate) fn VmEventSink::drain(&self) -> (Vec<HcsVmEvent>, usize)` — the queued events and the number dropped since the last drain

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `crates/platform/src/watch.rs`:

```rust
    use super::{EVENT_CAPACITY, HcsVmEvent, VmEventSink};
    use uuid::Uuid;

    fn event(vm_name: &str) -> HcsVmEvent {
        HcsVmEvent {
            vm_id: Uuid::new_v4(),
            vm_name: vm_name.into(),
            kind: HcsEventKind::Exited,
            details: None,
        }
    }

    #[test]
    fn the_queue_drains_in_arrival_order() {
        let sink = VmEventSink::default();
        sink.push(event("first"));
        sink.push(event("second"));

        let (events, dropped) = sink.drain();

        assert_eq!(dropped, 0);
        let names: Vec<_> = events.iter().map(|event| event.vm_name.as_str()).collect();
        assert_eq!(names, vec!["first", "second"]);
    }

    #[test]
    fn draining_twice_yields_nothing_the_second_time() {
        let sink = VmEventSink::default();
        sink.push(event("only"));

        assert_eq!(sink.drain().0.len(), 1);
        assert!(sink.drain().0.is_empty());
    }

    /// A full queue keeps the newest events: the most recent state of the world
    /// matters more than the start of a burst. The count is what keeps the loss
    /// reportable instead of silent.
    #[test]
    fn a_full_queue_discards_the_oldest_events_and_counts_them() {
        let sink = VmEventSink::default();
        for index in 0..EVENT_CAPACITY + 2 {
            sink.push(event(&format!("vm-{index}")));
        }

        let (events, dropped) = sink.drain();

        assert_eq!(dropped, 2);
        assert_eq!(events.len(), EVENT_CAPACITY);
        assert_eq!(events[0].vm_name, "vm-2");
        assert_eq!(
            events[EVENT_CAPACITY - 1].vm_name,
            format!("vm-{}", EVENT_CAPACITY + 1)
        );
    }

    #[test]
    fn the_dropped_count_resets_with_each_drain() {
        let sink = VmEventSink::default();
        for index in 0..EVENT_CAPACITY + 1 {
            sink.push(event(&format!("vm-{index}")));
        }

        assert_eq!(sink.drain().1, 1);

        sink.push(event("later"));
        assert_eq!(sink.drain().1, 0);
    }

    /// The callback and the drain hold the same sink from different threads;
    /// cloning must share the queue rather than copy it.
    #[test]
    fn a_cloned_sink_shares_the_same_queue() {
        let sink = VmEventSink::default();
        let clone = sink.clone();

        clone.push(event("through-the-clone"));

        assert_eq!(sink.drain().0.len(), 1);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu watch`

Expected: FAIL to compile — `cannot find type VmEventSink`, `cannot find value EVENT_CAPACITY`.

- [ ] **Step 3: Write the minimal implementation**

Add to `crates/platform/src/watch.rs`, after `classify`:

```rust
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, MutexGuard},
};

/// How many events are kept before the oldest are discarded.
const EVENT_CAPACITY: usize = 256;

/// The queue a watched compute system's callback writes into and the repository
/// drains.
///
/// Cloning shares the queue: the callback holds one clone on HCS's thread while
/// the repository drains through another on the main thread.
#[derive(Clone, Default)]
pub struct VmEventSink(Arc<Mutex<EventQueue>>);

#[derive(Default)]
struct EventQueue {
    events: VecDeque<HcsVmEvent>,
    /// Events discarded because the queue was full, since the last drain.
    dropped: usize,
}

impl VmEventSink {
    /// Queues `event`, discarding the oldest one if the queue is full.
    pub(crate) fn push(&self, event: HcsVmEvent) {
        let mut queue = self.lock();
        if queue.events.len() == EVENT_CAPACITY {
            queue.events.pop_front();
            queue.dropped += 1;
        }
        queue.events.push_back(event);
    }

    /// Takes every queued event, and how many were discarded since the last
    /// drain.
    pub(crate) fn drain(&self) -> (Vec<HcsVmEvent>, usize) {
        let mut queue = self.lock();
        let dropped = std::mem::take(&mut queue.dropped);
        (queue.events.drain(..).collect(), dropped)
    }

    /// Recovers a poisoned lock rather than propagating the panic.
    ///
    /// The queue holds plain owned data that a panic elsewhere cannot leave
    /// half-written, and losing all event reporting because an unrelated thread
    /// panicked would be worse than reading it.
    fn lock(&self) -> MutexGuard<'_, EventQueue> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu watch`

Expected: PASS, 8 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/platform/src/watch.rs
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
  git commit -m "TASK-33: Queue HCS events for the main thread to drain"
```

---

## Task 3: Reporting a drained event

Turns queued events into log records, diagnostics, and the list of VMs whose held handle is dead. Everything the main thread does with an event lives here, so it is all testable without HCS.

**Files:**
- Modify: `crates/platform/src/watch.rs`

**Interfaces:**
- Consumes: `HcsVmEvent`, `HcsEventKind` (Task 1), `VmEventSink::drain` (Task 2).
- Produces:
  - `pub(crate) fn drain_events(sink: &VmEventSink) -> (Vec<Diagnostic>, Vec<Uuid>)` — the diagnostics to surface, and the VM ids whose compute-system handle must be released

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `crates/platform/src/watch.rs`:

```rust
    use super::{DETAILS_LIMIT, drain_events};
    use vmlord_core::DiagnosticLevel;

    fn event_of(kind: HcsEventKind, details: Option<&str>) -> HcsVmEvent {
        HcsVmEvent {
            vm_id: Uuid::new_v4(),
            vm_name: "dev".into(),
            kind,
            details: details.map(str::to_owned),
        }
    }

    #[test]
    fn an_exit_is_reported_and_releases_the_handle() {
        let sink = VmEventSink::default();
        let exited = event_of(HcsEventKind::Exited, Some("{\"ExitCode\":0}"));
        let vm_id = exited.vm_id;
        sink.push(exited);

        let (diagnostics, released) = drain_events(&sink);

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].level, DiagnosticLevel::Info);
        assert!(diagnostics[0].message.contains("dev"));
        assert_eq!(released, vec![vm_id]);
    }

    #[test]
    fn a_starting_crash_warns_and_keeps_the_handle() {
        let sink = VmEventSink::default();
        sink.push(event_of(HcsEventKind::CrashInitiated, None));

        let (diagnostics, released) = drain_events(&sink);

        assert_eq!(diagnostics[0].level, DiagnosticLevel::Warning);
        assert!(released.is_empty());
    }

    #[test]
    fn a_crash_report_is_an_error_carrying_what_hcs_said() {
        let sink = VmEventSink::default();
        sink.push(event_of(
            HcsEventKind::CrashReport,
            Some("{\"DumpFile\":\"C:\\\\dumps\\\\dev.dmp\"}"),
        ));

        let (diagnostics, released) = drain_events(&sink);

        assert_eq!(diagnostics[0].level, DiagnosticLevel::Error);
        assert!(diagnostics[0].message.contains("dev.dmp"));
        assert!(
            released.is_empty(),
            "a crash report is written while the system still exists"
        );
    }

    /// The disconnect releases the handle because the service that backed it is
    /// gone. It deliberately does not touch `BackendStatus`: the next poll fails
    /// on its own if the service is really dead, and `WorkspaceApp` has no way
    /// back out of `Unavailable`.
    #[test]
    fn a_service_disconnect_is_an_error_and_releases_the_handle() {
        let sink = VmEventSink::default();
        let disconnected = event_of(HcsEventKind::ServiceDisconnect, None);
        let vm_id = disconnected.vm_id;
        sink.push(disconnected);

        let (diagnostics, released) = drain_events(&sink);

        assert_eq!(diagnostics[0].level, DiagnosticLevel::Error);
        assert_eq!(released, vec![vm_id]);
    }

    #[test]
    fn an_ignored_event_produces_no_diagnostic() {
        let sink = VmEventSink::default();
        sink.push(event_of(HcsEventKind::Ignored(4), Some("noise")));

        let (diagnostics, released) = drain_events(&sink);

        assert!(diagnostics.is_empty());
        assert!(released.is_empty());
    }

    /// `EventData` has no documented length bound, and the diagnostics panel is
    /// a few lines tall. Truncation counts characters, not bytes, so it cannot
    /// split a UTF-8 sequence.
    #[test]
    fn a_long_detail_is_truncated_in_the_diagnostic() {
        let sink = VmEventSink::default();
        sink.push(event_of(
            HcsEventKind::CrashReport,
            Some(&"д".repeat(DETAILS_LIMIT * 2)),
        ));

        let (diagnostics, _released) = drain_events(&sink);

        assert!(diagnostics[0].message.ends_with("..."));
        assert!(diagnostics[0].message.chars().count() < DETAILS_LIMIT * 2);
    }

    #[test]
    fn draining_reports_every_event_in_order() {
        let sink = VmEventSink::default();
        sink.push(event_of(HcsEventKind::CrashInitiated, None));
        sink.push(event_of(HcsEventKind::Exited, None));

        let (diagnostics, released) = drain_events(&sink);

        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.level)
                .collect::<Vec<_>>(),
            vec![DiagnosticLevel::Warning, DiagnosticLevel::Info]
        );
        assert_eq!(released.len(), 1);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu watch`

Expected: FAIL to compile — `cannot find function drain_events`, `cannot find value DETAILS_LIMIT`.

- [ ] **Step 3: Write the minimal implementation**

Add to `crates/platform/src/watch.rs`:

```rust
use vmlord_core::{Diagnostic, DiagnosticLevel};

/// The longest `EventData` excerpt a diagnostic carries.
const DETAILS_LIMIT: usize = 200;

impl HcsEventKind {
    /// Whether this event means the compute-system handle VMLord holds is dead.
    ///
    /// HCS destroys a compute system as it exits, and a disconnected service
    /// backs nothing at all, so in both cases the handle refers to nothing.
    fn releases_handle(&self) -> bool {
        matches!(self, Self::Exited | Self::ServiceDisconnect)
    }
}

/// Drains `sink`, logging every event, and reports what the caller must act on:
/// the diagnostics to surface and the VMs whose held handle is dead.
///
/// Releasing handles is left to the caller because `VmConnections` lives in the
/// repository and is not `Sync`; an event only records a fact.
pub(crate) fn drain_events(sink: &VmEventSink) -> (Vec<Diagnostic>, Vec<Uuid>) {
    let (events, dropped) = sink.drain();
    if dropped > 0 {
        log::warn!(
            "{dropped} HCS event(s) were discarded because VMLord's event queue was full"
        );
    }

    let mut diagnostics = Vec::new();
    let mut released = Vec::new();
    for event in events {
        if event.kind.releases_handle() {
            released.push(event.vm_id);
        }
        if let Some(diagnostic) = report(&event) {
            diagnostics.push(diagnostic);
        }
    }
    (diagnostics, released)
}

/// Logs `event` and returns the diagnostic the user should see, if any.
///
/// Noise HCS reports but VMLord has nothing to do about stays in the log only:
/// the diagnostics buffer holds 100 entries, and filling it with silo-job
/// notifications would push out the crash report that matters.
fn report(event: &HcsVmEvent) -> Option<Diagnostic> {
    let name = &event.vm_name;
    let vm_id = event.vm_id;
    let details = event.details.as_deref().unwrap_or("");

    match &event.kind {
        HcsEventKind::Exited => {
            log::info!("VM \"{name}\" ({vm_id}) exited; HCS reported: {details}");
            Some(diagnostic(
                DiagnosticLevel::Info,
                format!("VM \"{name}\" stopped"),
            ))
        }
        HcsEventKind::CrashInitiated => {
            log::warn!("the guest of VM \"{name}\" ({vm_id}) is crashing; HCS reported: {details}");
            Some(diagnostic(
                DiagnosticLevel::Warning,
                format!("The guest of VM \"{name}\" is crashing"),
            ))
        }
        HcsEventKind::CrashReport => {
            log::error!("the guest of VM \"{name}\" ({vm_id}) crashed; HCS reported: {details}");
            Some(diagnostic(
                DiagnosticLevel::Error,
                format!(
                    "The guest of VM \"{name}\" crashed; HCS reported: {}",
                    excerpt(details)
                ),
            ))
        }
        HcsEventKind::ServiceDisconnect => {
            log::error!(
                "the Host Compute Service disconnected from VM \"{name}\" ({vm_id}); \
                 HCS reported: {details}"
            );
            Some(diagnostic(
                DiagnosticLevel::Error,
                format!("The Host Compute Service disconnected from VM \"{name}\""),
            ))
        }
        HcsEventKind::Ignored(event_type) => {
            log::debug!(
                "VM \"{name}\" ({vm_id}) reported HCS event type {event_type}, \
                 which VMLord does not act on; HCS reported: {details}"
            );
            None
        }
    }
}

fn diagnostic(level: DiagnosticLevel, message: String) -> Diagnostic {
    Diagnostic { level, message }
}

/// Shortens `details` to something a diagnostics line can hold.
///
/// Counts characters rather than bytes: `EventData` is arbitrary text, and
/// slicing it by byte index would panic on a multi-byte boundary.
fn excerpt(details: &str) -> String {
    let trimmed = details.trim();
    if trimmed.chars().count() <= DETAILS_LIMIT {
        return trimmed.to_owned();
    }
    let head: String = trimmed.chars().take(DETAILS_LIMIT).collect();
    format!("{head}...")
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu watch`

Expected: PASS, 15 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/platform/src/watch.rs
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
  git commit -m "TASK-33: Report drained HCS events to the log and the user"
```

---

## Task 4: The callback and its registration

The only `unsafe` in the feature: the function HCS calls on its own thread, and the RAII guard that keeps its context alive exactly as long as the registration exists.

**Files:**
- Modify: `crates/platform/src/watch.rs`
- Modify: `crates/platform/src/hcs.rs`
- Modify: `crates/platform/src/lib.rs`

**Interfaces:**
- Consumes: `HcsVmEvent`, `classify` (Task 1), `VmEventSink::push` (Task 2).
- Produces:
  - `pub(crate) fn HcsSystem::raw_handle(&self) -> HCS_SYSTEM` — non-owning
  - `pub struct SystemWatch`, with `pub fn SystemWatch::register(system: &HcsSystem, vm_id: Uuid, vm_name: &str, sink: &VmEventSink) -> Result<SystemWatch, RepositoryError>`
  - `unsafe extern "system" fn on_hcs_event(event: *const HCS_EVENT, context: *const c_void)` (private)
  - `crates/platform/src/lib.rs` re-exports `HcsEventKind`, `HcsVmEvent`, `SystemWatch`, `VmEventSink`

- [ ] **Step 1: Write the failing tests**

The callback is an ordinary function pointer, so it can be called directly with a synthesised event — no HCS involved. Add to the `tests` module in `crates/platform/src/watch.rs`:

```rust
    use super::{WatchContext, on_hcs_event};
    use std::{mem, sync::Arc};
    use windows::{
        Win32::System::HostComputeSystem::{HCS_EVENT, HcsEventSystemExited},
        core::{HSTRING, PCWSTR},
    };

    fn context(sink: &VmEventSink, vm_id: Uuid) -> Arc<WatchContext> {
        Arc::new(WatchContext {
            vm_id,
            vm_name: "dev".into(),
            sink: sink.clone(),
        })
    }

    /// `HCS_EVENT` is zeroed rather than built field by field: only `Type` and
    /// `EventData` are read, and zeroing avoids depending on how the `windows`
    /// crate spells an idle `HCS_OPERATION`.
    fn hcs_event(event_data: PCWSTR) -> HCS_EVENT {
        // SAFETY: `HCS_EVENT` is a plain `#[repr(C)]` struct of a wrapped i32
        // and two pointers, for which an all-zero value is valid.
        let mut event: HCS_EVENT = unsafe { mem::zeroed() };
        event.Type = HcsEventSystemExited;
        event.EventData = event_data;
        event
    }

    #[test]
    fn the_callback_queues_the_event_it_is_handed() {
        let sink = VmEventSink::default();
        let vm_id = Uuid::new_v4();
        let context = context(&sink, vm_id);
        let data = HSTRING::from("{\"ExitCode\":0}");
        let event = hcs_event(PCWSTR(data.as_ptr()));

        // SAFETY: both pointers are to live values owned by this test for the
        // duration of the call, which is exactly HCS's own contract.
        unsafe { on_hcs_event(&event, Arc::as_ptr(&context).cast()) };

        let (events, dropped) = sink.drain();
        assert_eq!(dropped, 0);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, HcsEventKind::Exited);
        assert_eq!(events[0].vm_id, vm_id);
        assert_eq!(events[0].vm_name, "dev");
        assert_eq!(events[0].details.as_deref(), Some("{\"ExitCode\":0}"));
    }

    #[test]
    fn the_callback_accepts_an_event_without_data() {
        let sink = VmEventSink::default();
        let context = context(&sink, Uuid::new_v4());
        let event = hcs_event(PCWSTR::null());

        // SAFETY: as above; a null `EventData` is what HCS sends for an event
        // that carries no document.
        unsafe { on_hcs_event(&event, Arc::as_ptr(&context).cast()) };

        let (events, _dropped) = sink.drain();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].details, None);
    }

    /// Nothing documents that HCS never passes a null, and dereferencing one
    /// would take the whole process down from a thread VMLord does not own.
    #[test]
    fn the_callback_ignores_null_arguments() {
        let sink = VmEventSink::default();
        let context = context(&sink, Uuid::new_v4());
        let event = hcs_event(PCWSTR::null());

        // SAFETY: passing null is the case under test; the callback must return
        // without dereferencing either pointer.
        unsafe { on_hcs_event(std::ptr::null(), Arc::as_ptr(&context).cast()) };
        // SAFETY: as above, with the context pointer null instead.
        unsafe { on_hcs_event(&event, std::ptr::null()) };

        assert!(sink.drain().0.is_empty());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu watch`

Expected: FAIL to compile — `cannot find type WatchContext`, `cannot find function on_hcs_event`.

- [ ] **Step 3: Expose the raw handle from `HcsSystem`**

In `crates/platform/src/hcs.rs`, add to the `impl HcsSystem` block, after `terminate_and_wait` (the file currently has a blank line before the closing brace at `hcs.rs:281-282`; put the method there):

```rust
    /// The raw compute-system handle, for registering an event callback on it.
    ///
    /// Non-owning: this `HcsSystem` still closes the handle in `Drop`, so
    /// anything holding the returned value must not outlive it.
    pub(crate) fn raw_handle(&self) -> HCS_SYSTEM {
        self.handle
    }
```

- [ ] **Step 4: Write the callback and the guard**

Add to `crates/platform/src/watch.rs`:

```rust
use std::{ffi::c_void, panic, ptr};

use vmlord_core::RepositoryError;
use windows::Win32::System::HostComputeSystem::{
    HCS_EVENT, HCS_SYSTEM, HcsEventOptionEnableVmLifecycle, HcsEventOptionNone,
    HcsSetComputeSystemCallback,
};

use crate::{error::windows_error, hcs::HcsSystem};

/// What the callback needs in order to turn an `HCS_EVENT` into an
/// [`HcsVmEvent`].
struct WatchContext {
    vm_id: Uuid,
    vm_name: String,
    sink: VmEventSink,
}

/// An active `HcsSetComputeSystemCallback` registration, removed on drop.
pub struct SystemWatch {
    /// The system the callback is registered on.
    ///
    /// A non-owning copy: the [`HcsSystem`] this was registered against still
    /// owns and closes the handle, and must outlive this watch.
    system: HCS_SYSTEM,
    /// The `Arc<WatchContext>` handed to HCS, as the raw pointer HCS holds.
    context: *const WatchContext,
}

impl SystemWatch {
    /// Asks HCS to report `system`'s lifecycle events into `sink`.
    pub fn register(
        system: &HcsSystem,
        vm_id: Uuid,
        vm_name: &str,
        sink: &VmEventSink,
    ) -> Result<Self, RepositoryError> {
        let handle = system.raw_handle();
        let context = Arc::into_raw(Arc::new(WatchContext {
            vm_id,
            vm_name: vm_name.to_owned(),
            sink: sink.clone(),
        }));

        // SAFETY: `handle` is owned by `system`, which outlives this watch;
        // `context` is a live allocation this watch owns until it clears the
        // registration in `Drop`. `HcsEventOptionEnableVmLifecycle` is what
        // makes HCS deliver VM lifecycle events rather than only operation
        // callbacks.
        let registered = unsafe {
            HcsSetComputeSystemCallback(
                handle,
                HcsEventOptionEnableVmLifecycle,
                context.cast(),
                Some(on_hcs_event),
            )
        };

        if let Err(error) = registered {
            // SAFETY: HCS rejected the registration, so it holds no pointer to
            // the context and no callback can be running.
            drop(unsafe { Arc::from_raw(context) });
            return Err(windows_error(
                "set compute system callback",
                Some(vm_name),
                error,
            ));
        }

        log::debug!("watching the HCS events of VM \"{vm_name}\" ({vm_id})");
        Ok(Self {
            system: handle,
            context,
        })
    }
}

impl Drop for SystemWatch {
    fn drop(&mut self) {
        // SAFETY: `self.system` is still open here, because the `HcsSystem`
        // that owns it is dropped only after this watch.
        let cleared = unsafe {
            HcsSetComputeSystemCallback(self.system, HcsEventOptionNone, ptr::null(), None)
        };

        match cleared {
            Ok(()) => {
                // SAFETY: the registration is gone, so HCS holds no pointer to
                // the context and no further callback can observe it.
                drop(unsafe { Arc::from_raw(self.context) });
            }
            Err(error) => {
                // The `Arc` is deliberately never reclaimed on this path: not
                // calling `Arc::from_raw` is what leaks it, and leaking one
                // small allocation is not comparable to a thread inside HCS
                // dereferencing freed memory.
                log::error!(
                    "could not remove the HCS event callback of VM {:?}: {error}; \
                     its context is leaked deliberately rather than freed while \
                     HCS may still call into it",
                    // SAFETY: the context is still alive precisely because this
                    // branch is the one that does not free it.
                    unsafe { &(*self.context).vm_name }
                );
            }
        }
    }
}

/// The function HCS calls, on a thread it owns.
///
/// It must not panic across this boundary -- a panic crossing `extern "system"`
/// is undefined behaviour -- and it must return quickly, because HCS waits for
/// it. So it only classifies the event and queues it: no HCS calls, no
/// filesystem access, and deliberately no logging, because writing the log file
/// takes a lock on the service's thread and invites both a stall inside HCS and
/// a lock-ordering deadlock against the main thread. The drain logs instead.
unsafe extern "system" fn on_hcs_event(event: *const HCS_EVENT, context: *const c_void) {
    let queued = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        if event.is_null() || context.is_null() {
            return;
        }

        // SAFETY: HCS passes a valid event for the duration of this call, and
        // `context` is the `WatchContext` pointer registered alongside this
        // callback, which `SystemWatch` keeps alive for as long as the
        // registration exists.
        let event = unsafe { &*event };
        // SAFETY: as above. This borrows the context and never rebuilds an
        // `Arc` from it, so the callback can never free the allocation.
        let context = unsafe { &*context.cast::<WatchContext>() };

        let details = if event.EventData.is_null() {
            None
        } else {
            // SAFETY: a non-null `EventData` is a null-terminated UTF-16 string
            // owned by HCS and valid for the duration of this call.
            unsafe { event.EventData.to_string() }.ok()
        };

        context.sink.push(HcsVmEvent {
            vm_id: context.vm_id,
            vm_name: context.vm_name.clone(),
            kind: classify(event.Type.0),
            details,
        });
    }));

    // A panic here cannot be reported: logging is not allowed on this thread and
    // the queue is what panicked. Swallowing it is the only sound option, and
    // the loss shows up as an event that never arrives.
    let _ = queued;
}
```

- [ ] **Step 5: Re-export the public items**

In `crates/platform/src/lib.rs`, add the re-export after the `pub use start::VmStartPipeline;` line, keeping the list alphabetical by module:

```rust
pub use watch::{HcsEventKind, HcsVmEvent, SystemWatch, VmEventSink};
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu watch`

Expected: PASS, 18 tests.

If `HcsEventOptionEnableVmLifecycle`, `HcsEventOptionNone` or the `HCS_EVENT_CALLBACK` shape (`Option<unsafe extern "system" fn(*const HCS_EVENT, *const c_void)>`) does not compile, correct the names against the crate's `HostComputeSystem` module. The registration flags and the callback signature are the two things the spec transcribed from documentation rather than from a build.

- [ ] **Step 7: Verify the crate builds and lints clean**

Run: `cargo clippy -p vmlord-platform --target=x86_64-pc-windows-gnu --all-targets`

Expected: no warnings from `watch.rs`. `SystemWatch` is not yet constructed outside its tests, so a dead-code warning on `register` is expected here and resolved in Task 5.

- [ ] **Step 8: Commit**

```bash
git add crates/platform/src/watch.rs crates/platform/src/hcs.rs crates/platform/src/lib.rs
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
  git commit -m "TASK-33: Register an HCS event callback on a compute system"
```

---

## Task 5: Watch every system VMLord holds

`VmConnections` is the one place that owns compute-system handles, so it is the one place that registers watches — no caller has to remember to.

**Files:**
- Modify: `crates/platform/src/reconnect.rs`
- Modify: `crates/platform/src/repository.rs:130-144` (`hold_started_system`)

**Interfaces:**
- Consumes: `SystemWatch::register`, `VmEventSink` (Tasks 2, 4).
- Produces:
  - `pub fn VmConnections::with_events(events: VmEventSink) -> VmConnections`
  - `pub fn VmConnections::insert(&mut self, mapping: &VmComputeSystemMapping, system: HcsSystem) -> Result<(), RepositoryError>` — the system is held either way; `Err` means held but unwatched
  - `pub fn reconnect_known_vms(store: &MetadataStore, events: &VmEventSink) -> Result<ReconnectReport, RepositoryError>`

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `crates/platform/src/reconnect.rs`:

```rust
    use crate::watch::{HcsEventKind, HcsVmEvent, VmEventSink};

    /// Registration writes into whichever sink the connections were built with,
    /// so that sharing must be real rather than a copy. The registration itself
    /// needs a live HCS handle and is covered by the ignored Hyper-V test.
    #[test]
    fn connections_queue_their_events_into_the_sink_they_were_given() {
        let sink = VmEventSink::default();
        let connections = VmConnections::with_events(sink.clone());

        connections.events.push(HcsVmEvent {
            vm_id: Uuid::new_v4(),
            vm_name: "dev".into(),
            kind: HcsEventKind::Exited,
            details: None,
        });

        assert_eq!(sink.drain().0.len(), 1);
    }

    #[test]
    fn a_reconnect_reports_through_the_sink_it_was_given() {
        let dev = mapping("dev");
        let fixture = fixture("sink", std::slice::from_ref(&dev));
        let sink = VmEventSink::default();

        let report = reconnect_known_vms(&fixture.store, &sink)
            .expect("a reconnect with no live systems should succeed");

        assert_eq!(report.outcomes.len(), 1);
        assert!(report.connections.is_empty());
    }
```

Also add `use super::VmConnections;` to the existing `use super::{...}` line in that test module.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu reconnect`

Expected: FAIL to compile — `no function or associated item named with_events`, and `reconnect_known_vms` takes 1 argument but 2 were supplied.

- [ ] **Step 3: Store each system with its watch**

In `crates/platform/src/reconnect.rs`, replace the `VmConnections` struct and its `insert`:

```rust
/// The compute-system handles VMLord holds for the VMs it knows, each watched
/// for HCS events.
///
/// Dropping this closes every handle it holds, so it is meant to live for as
/// long as the VMLord process does.
#[derive(Default)]
pub struct VmConnections {
    systems: HashMap<Uuid, WatchedSystem>,
    events: VmEventSink,
}

/// A held compute system together with its event registration.
struct WatchedSystem {
    /// Declared before `system` on purpose: fields drop in declaration order,
    /// and the callback must be removed before the handle it is registered on
    /// is closed.
    ///
    /// `None` when registration failed: the VM stays held and usable, just
    /// unwatched.
    watch: Option<SystemWatch>,
    system: HcsSystem,
}

impl VmConnections {
    /// Creates connections that report their HCS events into `events`.
    #[must_use]
    pub fn with_events(events: VmEventSink) -> Self {
        Self {
            systems: HashMap::new(),
            events,
        }
    }

    /// Returns the open compute system of `vm_id`, if one is held.
    #[must_use]
    pub fn handle(&self, vm_id: Uuid) -> Option<&HcsSystem> {
        self.systems.get(&vm_id).map(|held| &held.system)
    }

    /// Starts holding `system` open for the VM in `mapping`, closing any handle
    /// already held for it, and asks HCS to report its events.
    ///
    /// The system is held either way. `Err` means it is held but unwatched: a
    /// running VM VMLord cannot watch is still a running VM VMLord owns, so
    /// refusing to hold it would turn lost observability into lost control. The
    /// caller reports the loss.
    pub fn insert(
        &mut self,
        mapping: &VmComputeSystemMapping,
        system: HcsSystem,
    ) -> Result<(), RepositoryError> {
        let registration =
            SystemWatch::register(&system, mapping.vm_id, &mapping.vm_name, &self.events);
        let failure = registration.as_ref().err().cloned();
        self.systems.insert(
            mapping.vm_id,
            WatchedSystem {
                watch: registration.ok(),
                system,
            },
        );
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
```

Keep `remove`, `is_connected`, `len` and `is_empty` as they are — `remove` still works on the map, and its DEBUG log still reads correctly.

Add the imports this needs at the top of the file:

```rust
use crate::watch::{SystemWatch, VmEventSink};
```

- [ ] **Step 4: Thread the sink through the reconnect**

In the same file, change `reconnect_known_vms` and `reconnect_with`:

```rust
pub fn reconnect_known_vms(
    store: &MetadataStore,
    events: &VmEventSink,
) -> Result<ReconnectReport, RepositoryError> {
    reconnect_with(store, events, |mapping| {
        HcsSystem::open_if_present(&mapping.hcs_compute_system_id, HCS_ACCESS_ALL)
    })
}

fn reconnect_with(
    store: &MetadataStore,
    events: &VmEventSink,
    open: impl Fn(&VmComputeSystemMapping) -> Result<Option<HcsSystem>, RepositoryError>,
) -> Result<ReconnectReport, RepositoryError> {
```

Replace `let mut connections = VmConnections::default();` with:

```rust
    let mut connections = VmConnections::with_events(events.clone());
```

And replace the direct map write at `reconnect.rs:147` (`connections.systems.insert(mapping.vm_id, system);`) with the public insert, reporting a lost watch:

```rust
                if let Err(error) = connections.insert(&mapping, system) {
                    log::warn!(
                        "reconnected to VM \"{}\" ({}) but cannot watch its HCS events: {error}",
                        mapping.vm_name,
                        mapping.vm_id
                    );
                }
```

The three existing `reconnect_with` tests each gain the new argument: pass `&VmEventSink::default()` as the second argument in `an_empty_store_reconnects_to_nothing`, `a_vm_hcs_no_longer_knows_is_reported_absent_and_stays_mapped` and `a_failure_to_open_one_vm_does_not_abort_the_others`. `an_unreadable_store_fails_the_reconnect` calls `reconnect_known_vms`, so it gains it too.

- [ ] **Step 5: Update the one other caller of `insert`**

In `crates/platform/src/repository.rs`, `hold_started_system` currently calls `self.connections.insert(mapping.vm_id, system)`. Replace the whole method:

```rust
    /// Reopens and holds the compute system of a VM that has just started, and
    /// starts watching its HCS events.
    ///
    /// A start that HCS accepted is not undone by a failure to hold or watch
    /// its handle, so this only warns: the VM runs either way.
    fn hold_started_system(&mut self, mapping: &VmComputeSystemMapping) {
        match HcsSystem::open_if_present(&mapping.hcs_compute_system_id, HCS_ACCESS_ALL) {
            Ok(Some(system)) => {
                if let Err(error) = self.connections.insert(mapping, system) {
                    log::warn!(
                        "VM \"{}\" ({}) started and is held, but VMLord cannot watch \
                         its HCS events: {error}",
                        mapping.vm_name,
                        mapping.vm_id
                    );
                    self.push_diagnostic(
                        DiagnosticLevel::Warning,
                        format!(
                            "VM \"{}\" started, but VMLord cannot report its HCS events",
                            mapping.vm_name
                        ),
                    );
                }
            }
            Ok(None) => log::warn!(
                "VM \"{}\" ({}) started, but HCS no longer reports its compute system",
                mapping.vm_name,
                mapping.vm_id
            ),
            Err(error) => log::warn!(
                "VM \"{}\" ({}) started, but VMLord could not hold a handle to it: {error}",
                mapping.vm_name,
                mapping.vm_id
            ),
        }
    }
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu`

Expected: PASS. The repository still fails to compile if `initialize` was not updated — that is Task 6; if the compiler stops here with `reconnect_known_vms` taking 2 arguments at `repository.rs:280`, fix it as the first step of Task 6 rather than patching it twice.

To keep this task independently green, make that one-line change now:

```rust
        let report = reconnect_known_vms(&self.store, &self.events)?;
```

and add the `events: VmEventSink::default()` field to `HcsVmRepository` and its `new`, which Task 6 then builds on:

```rust
    events: VmEventSink,
```

- [ ] **Step 7: Run the whole platform test suite and lints**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu`
Expected: PASS.

Run: `cargo clippy -p vmlord-platform --target=x86_64-pc-windows-gnu --all-targets`
Expected: no warnings.

- [ ] **Step 8: Commit**

```bash
git add crates/platform/src/reconnect.rs crates/platform/src/repository.rs
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
  git commit -m "TASK-33: Watch every compute system VMLord holds"
```

---

## Task 6: Surface events through the repository

Drains the sink on the main thread, so events become diagnostics the UI already renders and dead handles are released.

**Files:**
- Modify: `crates/platform/src/repository.rs`
- Modify: `ARCHITECTURE.md`

**Interfaces:**
- Consumes: `watch::drain_events` (Task 3), `VmConnections::remove` (existing), the `events` field added in Task 5.
- Produces: no new public API. `VmRepository::take_diagnostics` now also reports HCS events.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `crates/platform/src/repository.rs`:

```rust
    use uuid::Uuid;
    use vmlord_core::DiagnosticLevel;

    use crate::watch::{HcsEventKind, HcsVmEvent};

    /// The drain runs inside `take_diagnostics` because that is already called
    /// on every refresh, right after `list_vms`, so an event reaches the user
    /// within one refresh interval without any new machinery.
    ///
    /// Releasing the handle is asserted by `watch::drain_events`' own tests and
    /// by the ignored Hyper-V test; it cannot be asserted here, because holding
    /// a handle requires a live compute system.
    #[test]
    fn a_queued_exit_event_becomes_a_diagnostic() {
        let mut repository = repository();
        repository.events.push(HcsVmEvent {
            vm_id: Uuid::new_v4(),
            vm_name: "dev".into(),
            kind: HcsEventKind::Exited,
            details: None,
        });

        let diagnostics = repository.take_diagnostics();

        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic.level == DiagnosticLevel::Info
                    && diagnostic.message.contains("dev")
            }),
            "a VM that stopped on its own must be reported: {diagnostics:?}"
        );
    }

    #[test]
    fn a_queued_ignored_event_produces_no_diagnostic() {
        let mut repository = repository();
        repository.events.push(HcsVmEvent {
            vm_id: Uuid::new_v4(),
            vm_name: "dev".into(),
            kind: HcsEventKind::Ignored(5),
            details: Some("silo job created".into()),
        });

        assert!(repository.take_diagnostics().is_empty());
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu repository`

Expected: FAIL — `a_queued_exit_event_becomes_a_diagnostic` panics with "a VM that stopped on its own must be reported: []", because `take_diagnostics` does not drain the sink yet.

- [ ] **Step 3: Drain the sink in `take_diagnostics`**

In `crates/platform/src/repository.rs`, replace `take_diagnostics`:

```rust
    /// Reports everything the repository has to say since the last call,
    /// including the HCS events its watches queued.
    ///
    /// Draining here rather than in `list_vms` is deliberate: this is the
    /// `&mut self` call the application already makes on every refresh, right
    /// after listing, so it is where a released handle can actually be
    /// released.
    fn take_diagnostics(&mut self) -> Vec<Diagnostic> {
        let (from_events, released) = watch::drain_events(&self.events);
        for vm_id in released {
            self.connections.remove(vm_id);
        }

        let mut diagnostics: Vec<Diagnostic> = self
            .diagnostics
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain(..)
            .collect();
        diagnostics.extend(from_events);
        diagnostics
    }
```

Add `watch` to the `crate::{...}` import list at the top of the file, and `VmEventSink` to it if Task 5 did not already:

```rust
use crate::{
    HcsClient, HcsSystem, KnownVm, MetadataStore, VmComputeSystemMapping, VmConnections,
    VmCreationPipeline, VmDeletionPipeline, VmForceStopPipeline, VmShutdownPipeline,
    VmStartPipeline,
    hcs::{HCS_ACCESS_ALL, HcsSystemState},
    hcs_config::{self, VmTopology},
    layout, list_known_vms,
    reconnect::{ReconnectOutcome, reconnect_known_vms},
    vhd, watch,
    watch::VmEventSink,
};
```

- [ ] **Step 4: Give the repository's connections the same sink**

`HcsVmRepository::new` builds `VmConnections::default()`, whose sink is its own and therefore not the one `take_diagnostics` drains. Replace the field initialisation in `new`:

```rust
        let events = VmEventSink::default();
        Self {
            client: HcsClient::new(),
            store: MetadataStore::new(storage_root.join(MAPPING_FILE_NAME)),
            storage_root,
            connections: VmConnections::with_events(events.clone()),
            creation: VmCreationPipeline::production(),
            start: VmStartPipeline::production(),
            shutdown: VmShutdownPipeline::production(),
            force_stop: VmForceStopPipeline::production(),
            delete: VmDeletionPipeline::production(),
            diagnostics: Mutex::new(Vec::new()),
            events,
            initialized: false,
        }
```

`initialize` replaces `self.connections` with the reconnect report's connections, and `reconnect_known_vms(&self.store, &self.events)` already builds those from the same sink, so both paths share one queue.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu repository`

Expected: PASS.

- [ ] **Step 6: Correct `ARCHITECTURE.md`**

Replace this sentence (in the paragraph about HCS enumeration states):

```
A missing state therefore means `Created`, and that
absence is the only signal separating the two. Whether a running guest has
finished booting stays unobservable until the watch/event work lands.
```

with:

```
A missing state therefore means `Created`, and that
absence is the only signal separating the two.
```

Then add this paragraph after it:

```
`platform::watch` registers an HCS event callback on every compute system
VMLord holds, which is the only source for what the enumeration cannot say: why
a VM stopped, that its guest crashed, and that the Host Compute Service
disconnected. The callback runs on a thread HCS owns, so it only classifies the
event and queues it; the repository drains that queue in `take_diagnostics` on
every refresh, logs each event, surfaces the significant ones as diagnostics,
and releases the handle of a VM that is gone. The enumeration remains the sole
authority on VM state. Whether a running guest has finished booting is still
unobservable -- HCS reports nothing about it -- so `AgentStatus` stays
`Unknown` until the guest agent lands.
```

- [ ] **Step 7: Verify the whole workspace**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu`
Expected: PASS.

Run: `cargo test -p vmlord-core -p vmlord-app`
Expected: PASS, unchanged — this plan does not touch those crates.

Run: `cargo build --target=x86_64-pc-windows-gnu`
Expected: success.

Run: `cargo clippy --target=x86_64-pc-windows-gnu --all-targets`
Expected: no warnings.

- [ ] **Step 8: Commit**

```bash
git add crates/platform/src/repository.rs ARCHITECTURE.md
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
  git commit -m "TASK-33: Report HCS events through the repository's diagnostics"
```

---

## Task 7: Prove HCS actually calls back

Every test so far exercises code that runs *after* the callback. This is the one that proves HCS calls it at all, so it must run against a live Hyper-V host.

**Files:**
- Modify: `crates/platform/tests/hyperv.rs`

**Interfaces:**
- Consumes: `SystemWatch::register`, `VmEventSink`, `HcsEventKind` (Task 4), `open_by_vm_name`, `VmCreationPipeline`, `VmStartPipeline`, `VmForceStopPipeline` (existing).
- Produces: nothing further tasks depend on.

- [ ] **Step 1: Write the test**

Add to `crates/platform/tests/hyperv.rs`:

```rust
/// Exercises TASK-33's watch against the real Host Compute Service: creates a
/// VM, starts it, watches its compute system, terminates it, and waits for the
/// resulting exit event.
///
/// This is the only check that proves HCS calls back into VMLord at all --
/// every other watch test exercises code that runs after the callback. A
/// termination is enough: it needs nothing from the guest, so installer media
/// works here.
///
/// Set `VMLORD_TEST_IMAGE_PATH` to a real bootable ISO.
///
/// Run elevated with:
/// `cargo test -p vmlord-platform --test hyperv -- --ignored --exact a_terminated_vm_reports_its_exit --nocapture`
#[test]
#[ignore = "requires an elevated Windows host with Hyper-V/HCS and VMLORD_TEST_IMAGE_PATH set"]
fn a_terminated_vm_reports_its_exit() {
    let image_path = std::env::var("VMLORD_TEST_IMAGE_PATH")
        .expect("VMLORD_TEST_IMAGE_PATH must point to a real ISO image");
    let root = std::env::temp_dir().join(format!("vmlord-hcs-watch-e2e-{}", std::process::id()));
    fs::create_dir_all(&root).expect("test root should be created");

    let request = VmCreateRequest {
        name: format!("vmlord-e2e-watch-test-{}", std::process::id()),
        image_path,
        ram_mb: 2048,
        disk_gb: 8,
        cpu_cores: 2,
        gpu_mode: GpuMode::None,
        network_mode: NetworkMode::None,
        username: "admin".into(),
        password: "not used by a watch".into(),
        ssh_enabled: false,
        ssh_deploy_key: false,
    };
    let store = MetadataStore::new(root.join("vm-mapping.json"));
    let vm_directory = root.join("vm");

    let mapping = VmCreationPipeline::production()
        .create(&store, &request, &vm_directory)
        .expect("VM creation should succeed on an elevated Hyper-V host");
    VmStartPipeline::production()
        .start(&store, &mapping.vm_name, &vm_directory)
        .expect("the created VM must start before its exit can be watched");

    let system = open_by_vm_name(&store, &mapping.vm_name, HCS_ACCESS_ALL)
        .expect("HCS must report the compute system of a running VM");
    let sink = VmEventSink::default();
    let watch = SystemWatch::register(&system, mapping.vm_id, &mapping.vm_name, &sink)
        .expect("HCS should accept an event callback on a running compute system");

    let terminated = VmForceStopPipeline::production().force_stop(&store, &mapping.vm_name);

    // HCS delivers the exit asynchronously, on a thread of its own, after the
    // termination operation has already completed.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut events = Vec::new();
    while Instant::now() < deadline {
        events.extend(sink.drain().0);
        if events.iter().any(|event| event.kind == HcsEventKind::Exited) {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    // The watch is dropped before the system it is registered on, which is the
    // order `VmConnections` guarantees in production through field order.
    drop(watch);
    drop(system);
    let _ = fs::remove_dir_all(&root);

    terminated.expect("a running VM must accept a forced stop");
    assert!(
        events.iter().any(|event| event.kind == HcsEventKind::Exited),
        "HCS must report the exit of a terminated compute system; got {events:?}"
    );
}
```

Extend the imports at the top of the file:

```rust
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
```

and add `HcsEventKind, SystemWatch, VmEventSink` to the `vmlord_platform::{...}` import list.

- [ ] **Step 2: Verify the test compiles and stays ignored**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu --test hyperv`

Expected: compiles; every test in the file reports as ignored, including the new one. It must not run here — it needs an elevated Hyper-V host.

- [ ] **Step 3: Run the full verification**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu`
Expected: PASS.

Run: `cargo test -p vmlord-core -p vmlord-app`
Expected: PASS.

Run: `cargo build --target=x86_64-pc-windows-gnu`
Expected: success.

Run: `cargo clippy --target=x86_64-pc-windows-gnu --all-targets`
Expected: no warnings.

- [ ] **Step 4: Commit**

```bash
git add crates/platform/tests/hyperv.rs
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
  git commit -m "TASK-33: Cover HCS event delivery with a Hyper-V integration test"
```

- [ ] **Step 5: Report what still needs a human on real hardware**

These cannot be automated and must be verified manually on the elevated Windows host, per the epic's testing decision. Report the outcome of each rather than assuming it:

1. `cargo test -p vmlord-platform --test hyperv -- --ignored --exact a_terminated_vm_reports_its_exit --nocapture` passes.
2. Start a VM with a real guest OS from VMLord, shut it down from inside the guest, and confirm: an `Exited` event appears in the log at `INFO`, a "VM ... stopped" diagnostic appears in the UI panel, and the DEBUG line `closed the compute-system handle held for VM <id>` confirms the handle was released.
3. Confirm `SystemWatch::drop` clears cleanly: stop a watched VM through VMLord and check that the log contains **no** "could not remove the HCS event callback" error. If it does, the leak branch is the normal path on this Windows build — say so, because that changes the `ERROR` to an expected `DEBUG` and is a follow-up change, not a bug in this work.
4. Stop the Host Compute Service (`Stop-Service vmcompute`) while a VM is held and confirm a `ServiceDisconnect` error reaches the log and the diagnostics panel.

---

## Self-Review

**Spec coverage:**

| Spec section | Task |
| --- | --- |
| Goal: why a VM stopped, crash, service disconnect, handle hygiene | 3, 6 |
| Scope: watcher layered on the poll; `VmState`/`AgentStatus`/`BackendStatus` untouched | Enforced by omission — no task modifies `vm_state`, `AgentStatus`, or `BackendStatus`; asserted in Task 3's disconnect test comment and documented in Task 6 Step 6 |
| The Windows API, `HcsEventOptionEnableVmLifecycle` | 4 |
| `HcsVmEvent`, `HcsEventKind`, `classify` | 1 |
| The queue: capacity 256, drop oldest, count, poisoned-lock recovery | 2 |
| The registration guard, `raw_handle` | 4 |
| Callback lifetime: `Arc::into_raw`, borrow-only, clear-then-reclaim, deliberate leak | 4 |
| Callback discipline: no panic across FFI, no logging, fast return | 4 |
| Where registration happens: `hold_started_system`, `reconnect_known_vms` | 5 |
| `WatchedSystem` field order | 5 |
| Errors: registration warns, overflow warns | 3 (overflow), 5 (registration) |
| Draining: the event table, `EventData` unparsed, 200-character excerpt | 3 |
| Service disconnect does not touch `BackendStatus` | 3 |
| App and UI unchanged | Global Constraints |
| Unit tests: classify, queue, report, callback | 1, 2, 3, 4 |
| Repository unit test | 6 |
| Hyper-V integration test | 7 |
| Manual verification list | 7 Step 5 |
| `ARCHITECTURE.md` correction | 6 |

No spec requirement is unassigned.

**Placeholder scan:** none. Every code step carries the code to write; every verification step carries the command and the expected outcome. The two places where the plan admits uncertainty (the `windows` constant names in Task 1 Step 4 and Task 4 Step 6) give an explicit resolution procedure rather than deferring the decision, because those names were transcribed from documentation rather than from a build.

**Type consistency:** `VmEventSink::push`/`drain`, `drain_events(&VmEventSink) -> (Vec<Diagnostic>, Vec<Uuid>)`, `SystemWatch::register(&HcsSystem, Uuid, &str, &VmEventSink)`, `VmConnections::with_events(VmEventSink)`, `VmConnections::insert(&VmComputeSystemMapping, HcsSystem) -> Result<(), RepositoryError>` and `reconnect_known_vms(&MetadataStore, &VmEventSink)` are spelled identically everywhere they appear. `HcsEventKind::releases_handle` is used only inside `watch.rs` and is private there. One deviation from the spec, made deliberately: the spec described the repository unit test as asserting the handle is released, which cannot be done without a live compute system — Task 6's test asserts the diagnostic, and Task 3's tests assert the released-id list that drives the release.
