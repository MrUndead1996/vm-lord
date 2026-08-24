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
use windows::Win32::System::HostComputeSystem::{
    HcsEventServiceDisconnect, HcsEventSystemCrashInitiated, HcsEventSystemCrashReport,
    HcsEventSystemExited,
};

/// One thing HCS reported about a compute system, already translated out of FFI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HcsVmEvent {
    pub vm_id: Uuid,
    pub vm_name: String,
    /// Which registration queued this, as counted by the sink it was queued in.
    ///
    /// HCS delivers asynchronously, so an event can outlive the compute system
    /// it describes: the enumeration can already report a VM stopped, the user
    /// can start it again, and only then does the old `Exited` arrive. Acting on
    /// it would release the handle of the VM that is now running and tell the
    /// user it stopped. The generation is what makes the two incarnations
    /// distinguishable, since they share a `vm_id`.
    pub generation: u64,
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

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
};

/// How many events are kept before the oldest are discarded.
const EVENT_CAPACITY: usize = 256;

/// The queue a watched compute system's callback writes into and the repository
/// drains.
///
/// Cloning shares the queue: the callback holds one clone on HCS's thread while
/// the repository drains through another on the main thread.
#[derive(Clone, Default)]
pub struct VmEventSink(Arc<SharedQueue>);

#[derive(Default)]
struct SharedQueue {
    queue: Mutex<EventQueue>,
    /// The generation the next [`VmEventSink::next_generation`] hands out.
    ///
    /// The counter belongs to the sink rather than to the connections that
    /// stamp watches with it, because the sink is what mixes the events: every
    /// event a single drain sees was queued into this one queue, so a counter
    /// per queue is what makes the generations unique among exactly the events
    /// that are ever compared. Two `VmConnections` sharing a sink -- which is
    /// what a second `reconnect_known_vms` against the same sink builds --
    /// therefore cannot hand out the same generation twice.
    next_generation: AtomicU64,
}

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

    /// Hands out a generation no other watch reporting into this sink has had.
    ///
    /// Only ever increases, and never on removal: a generation must not be
    /// reused, because reuse is exactly what would let a stale event pass the
    /// staleness check. `Relaxed` is enough -- the value is only ever compared
    /// for equality, and nothing is published through it.
    pub(crate) fn next_generation(&self) -> u64 {
        self.0.next_generation.fetch_add(1, Ordering::Relaxed)
    }

    /// Recovers a poisoned lock rather than propagating the panic.
    ///
    /// The queue holds plain owned data that a panic elsewhere cannot leave
    /// half-written, and losing all event reporting because an unrelated thread
    /// panicked would be worse than reading it.
    fn lock(&self) -> MutexGuard<'_, EventQueue> {
        self.0
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

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

/// What a drain leaves the caller to act on.
pub(crate) struct DrainedEvents {
    /// The diagnostics to surface, in arrival order.
    pub diagnostics: Vec<Diagnostic>,
    /// The VMs whose held handle the events proved dead.
    pub released: Vec<Uuid>,
    /// The VMs whose compute system stopped executing.
    ///
    /// A subset of [`DrainedEvents::released`], and a separate list because the
    /// two mean different things: a released handle is also what a disconnected
    /// service leaves behind, while this is a VM that is no longer running.
    /// A guest that reboots keeps its compute system, so it is not here.
    pub stopped: Vec<Uuid>,
    /// Whether this drain saw a `ServiceDisconnect` it did not drop as stale.
    ///
    /// One flag rather than one entry per VM: the service disconnecting is a
    /// single event that HCS happens to deliver once per compute system, and it
    /// costs VMLord the same thing however many VMs it names.
    pub service_disconnected: bool,
}

/// Drains `sink`, logging every event, and reports what the caller must act on.
///
/// `superseded` answers whether a `(vm_id, generation)` pair belongs to a watch
/// the caller has already replaced. Such an event describes a compute system
/// that no longer exists, while the `vm_id` it carries now names a different
/// one, so it is logged at DEBUG and otherwise dropped: neither reported nor
/// acted on. That is expected rather than an error -- HCS delivers
/// asynchronously and a user may restart a VM in the meantime.
///
/// Releasing handles is left to the caller because `VmConnections` lives in the
/// repository and is not `Sync`; an event only records a fact.
pub(crate) fn drain_events(
    sink: &VmEventSink,
    mut superseded: impl FnMut(Uuid, u64) -> bool,
) -> DrainedEvents {
    let (events, dropped) = sink.drain();
    if dropped > 0 {
        log::warn!("{dropped} HCS event(s) were discarded because VMLord's event queue was full");
    }

    let mut drained = DrainedEvents {
        diagnostics: Vec::new(),
        released: Vec::new(),
        stopped: Vec::new(),
        service_disconnected: false,
    };
    for event in events {
        if superseded(event.vm_id, event.generation) {
            log::debug!(
                "dropping a stale HCS event for VM \"{}\" ({}): it was queued by \
                 watch generation {}, which VMLord has since replaced",
                event.vm_name,
                event.vm_id,
                event.generation
            );
            continue;
        }
        if event.kind.releases_handle() {
            drained.released.push(event.vm_id);
        }
        if matches!(event.kind, HcsEventKind::Exited) {
            drained.stopped.push(event.vm_id);
        }
        // Outside the branch above on purpose: a disconnect happens to release
        // the handle too, but what makes it worth reporting is the service being
        // gone, which is true whether or not the handle went with it.
        drained.service_disconnected |= matches!(event.kind, HcsEventKind::ServiceDisconnect);
        if let Some(diagnostic) = report(&event) {
            drained.diagnostics.push(diagnostic);
        }
    }
    drained
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

use std::{ffi::c_void, mem, panic};

use vmlord_core::RepositoryError;
use windows::Win32::System::HostComputeSystem::{
    HCS_EVENT, HcsEventOptionEnableVmLifecycle, HcsEventOptionNone, HcsSetComputeSystemCallback,
};

use crate::{error::windows_error, hcs::HcsSystem};

/// What the callback needs in order to turn an `HCS_EVENT` into an
/// [`HcsVmEvent`].
struct WatchContext {
    vm_id: Uuid,
    vm_name: String,
    /// Which registration this is, so a drain can tell an event queued by a
    /// replaced watch from one queued by the watch VMLord still holds. A plain
    /// `Copy` value read and never written after registration, which is what
    /// keeps the callback's shared borrow of this context sound.
    generation: u64,
    sink: VmEventSink,
}

/// [`on_hcs_event`] reaches a `WatchContext` through a raw pointer, on a thread
/// HCS owns. That dereference erases the `Sync` check the compiler would apply
/// to a shared reference, so a future `Cell`, `Rc` or `RefCell` field here would
/// be a cross-thread data race with nothing to catch it. This makes it a build
/// error instead.
const _: () = {
    fn assert_sync<T: Sync>() {}
    let _ = assert_sync::<WatchContext>;
};

/// An active `HcsSetComputeSystemCallback` registration, removed on drop.
pub struct SystemWatch {
    /// The system the callback is registered on.
    ///
    /// Shared ownership rather than a bare `HCS_SYSTEM` copy, so the handle
    /// cannot be closed while this watch is alive. That matters beyond leaking:
    /// [`HcsSystem::drop`] closes the handle unconditionally, and HCS may then
    /// reuse that value for a different compute system, so clearing a
    /// registration on a stale handle could silently unregister another VM's
    /// callback. Holding the `Arc` makes the drop order of the two irrelevant.
    system: Arc<HcsSystem>,
    /// The `Arc<WatchContext>` handed to HCS, as the raw pointer HCS holds.
    context: *const WatchContext,
}

impl SystemWatch {
    /// Asks HCS to report `system`'s lifecycle events into `sink`.
    pub fn register(
        system: &Arc<HcsSystem>,
        vm_id: Uuid,
        vm_name: &str,
        generation: u64,
        sink: &VmEventSink,
    ) -> Result<Self, RepositoryError> {
        let system = Arc::clone(system);
        let context = Arc::into_raw(Arc::new(WatchContext {
            vm_id,
            vm_name: vm_name.to_owned(),
            generation,
            sink: sink.clone(),
        }));

        // SAFETY: the handle is kept open by the `Arc<HcsSystem>` this watch
        // holds, so it stays valid for as long as the registration does;
        // `context` is a live allocation this watch owns until it clears the
        // registration in `Drop`. `HcsEventOptionEnableVmLifecycle` is what
        // makes HCS deliver VM lifecycle events rather than only operation
        // callbacks.
        let registered = unsafe {
            HcsSetComputeSystemCallback(
                system.raw_handle(),
                HcsEventOptionEnableVmLifecycle,
                Some(context.cast()),
                Some(on_hcs_event),
            )
        };

        if let Err(error) = registered {
            // SAFETY: HCS rejected the registration, so it holds no pointer to
            // the context and no callback can be running. The `Arc` was created
            // by `Arc::into_raw` just above and has not been reclaimed since.
            drop(unsafe { Arc::from_raw(context) });
            return Err(windows_error(
                "set compute system callback",
                Some(vm_name),
                error,
            ));
        }

        log::debug!(
            "watching the HCS events of VM \"{vm_name}\" ({vm_id}) as generation {generation}"
        );
        Ok(Self { system, context })
    }
}

impl Drop for SystemWatch {
    fn drop(&mut self) {
        // SAFETY: the handle is still open, and still refers to the same
        // compute system it did at registration, because this watch holds an
        // `Arc<HcsSystem>` keeping it from being closed and its value recycled.
        let cleared = unsafe {
            HcsSetComputeSystemCallback(self.system.raw_handle(), HcsEventOptionNone, None, None)
        };

        match cleared {
            Ok(()) => {
                // SAFETY: the pointer came from `Arc::into_raw` in `register`
                // and is reclaimed exactly once, here: `register` reclaims it
                // only on the path where it returns an error and no watch
                // exists. Clearing succeeded, so HCS has dropped the pointer
                // and will not start another callback with it -- and freeing
                // after a successful clear is the strongest guarantee this API
                // offers, since it exposes no way to await a callback already
                // running.
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
            generation: context.generation,
            kind: classify(event.Type.0),
            details,
        });
    }));

    // A panic here cannot be reported: logging is not allowed on this thread and
    // the queue is what panicked. Swallowing it is the only sound option, and
    // the loss shows up as an event that never arrives.
    //
    // The payload is forgotten rather than dropped, because dropping it happens
    // outside the barrier: a payload whose own `Drop` panicked would unwind
    // across `extern "system"`, which is the undefined behaviour the barrier
    // exists to prevent. Leaking one payload on a path that has already lost the
    // event costs nothing by comparison.
    if queued.is_err() {
        mem::forget(queued);
    }
}

#[cfg(test)]
mod tests {
    use super::{EVENT_CAPACITY, HcsEventKind, HcsVmEvent, VmEventSink, classify};
    use uuid::Uuid;

    fn event(vm_name: &str) -> HcsVmEvent {
        HcsVmEvent {
            vm_id: Uuid::new_v4(),
            vm_name: vm_name.into(),
            generation: 0,
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

    /// The generations belong to the sink, so every clone of it counts from the
    /// same place: that is what keeps two `VmConnections` around one queue from
    /// stamping two watches with the same generation.
    #[test]
    fn a_sink_never_hands_out_the_same_generation_twice() {
        let sink = VmEventSink::default();
        let clone = sink.clone();

        let generations = [
            sink.next_generation(),
            clone.next_generation(),
            sink.next_generation(),
        ];

        assert_eq!(generations, [0, 1, 2]);
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

    use super::{DETAILS_LIMIT, DrainedEvents, drain_events};
    use vmlord_core::DiagnosticLevel;

    fn event_of(kind: HcsEventKind, details: Option<&str>) -> HcsVmEvent {
        HcsVmEvent {
            vm_id: Uuid::new_v4(),
            vm_name: "dev".into(),
            generation: 0,
            kind,
            details: details.map(str::to_owned),
        }
    }

    /// Nothing supersedes an event: what every test that does not care about
    /// generations wants.
    fn nothing_superseded(_vm_id: Uuid, _generation: u64) -> bool {
        false
    }

    #[test]
    fn an_exit_is_reported_and_releases_the_handle() {
        let sink = VmEventSink::default();
        let exited = event_of(HcsEventKind::Exited, Some("{\"ExitCode\":0}"));
        let vm_id = exited.vm_id;
        sink.push(exited);

        let DrainedEvents {
            diagnostics,
            released,
            stopped,
            ..
        } = drain_events(&sink, nothing_superseded);

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].level, DiagnosticLevel::Info);
        assert!(diagnostics[0].message.contains("dev"));
        assert_eq!(released, vec![vm_id]);
        assert_eq!(
            stopped,
            vec![vm_id],
            "an exit is what a VM that stopped looks like, and what its \
             display window is closed on"
        );
    }

    #[test]
    fn a_starting_crash_warns_and_keeps_the_handle() {
        let sink = VmEventSink::default();
        sink.push(event_of(HcsEventKind::CrashInitiated, None));

        let DrainedEvents {
            diagnostics,
            released,
            ..
        } = drain_events(&sink, nothing_superseded);

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

        let DrainedEvents {
            diagnostics,
            released,
            ..
        } = drain_events(&sink, nothing_superseded);

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

        let DrainedEvents {
            diagnostics,
            released,
            stopped,
            ..
        } = drain_events(&sink, nothing_superseded);

        assert_eq!(diagnostics[0].level, DiagnosticLevel::Error);
        assert_eq!(released, vec![vm_id]);
        assert!(
            stopped.is_empty(),
            "the service disconnecting says nothing about whether the VM is \
             still running, so its display window is left alone"
        );
    }

    #[test]
    fn an_ignored_event_produces_no_diagnostic() {
        let sink = VmEventSink::default();
        sink.push(event_of(HcsEventKind::Ignored(4), Some("noise")));

        let DrainedEvents {
            diagnostics,
            released,
            ..
        } = drain_events(&sink, nothing_superseded);

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

        let DrainedEvents { diagnostics, .. } = drain_events(&sink, nothing_superseded);

        assert!(diagnostics[0].message.ends_with("..."));
        assert!(diagnostics[0].message.chars().count() < DETAILS_LIMIT * 2);
    }

    #[test]
    fn draining_reports_every_event_in_order() {
        let sink = VmEventSink::default();
        sink.push(event_of(HcsEventKind::CrashInitiated, None));
        sink.push(event_of(HcsEventKind::Exited, None));

        let DrainedEvents {
            diagnostics,
            released,
            ..
        } = drain_events(&sink, nothing_superseded);

        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.level)
                .collect::<Vec<_>>(),
            vec![DiagnosticLevel::Warning, DiagnosticLevel::Info]
        );
        assert_eq!(released.len(), 1);
    }

    use super::{WatchContext, on_hcs_event};
    use std::{mem, sync::Arc};
    use windows::{
        Win32::System::HostComputeSystem::{HCS_EVENT, HCS_EVENT_TYPE, HcsEventSystemExited},
        core::{HSTRING, PCWSTR},
    };

    fn context(sink: &VmEventSink, vm_id: Uuid) -> Arc<WatchContext> {
        Arc::new(WatchContext {
            vm_id,
            vm_name: "dev".into(),
            generation: 7,
            sink: sink.clone(),
        })
    }

    /// `HCS_EVENT` is zeroed rather than built field by field: only `Type` and
    /// `EventData` are read, and zeroing avoids depending on how the `windows`
    /// crate spells an idle `HCS_OPERATION`.
    fn hcs_event(event_type: HCS_EVENT_TYPE, event_data: PCWSTR) -> HCS_EVENT {
        // SAFETY: `HCS_EVENT` is a plain `#[repr(C)]` struct of a wrapped i32
        // and two pointers, for which an all-zero value is valid.
        let mut event: HCS_EVENT = unsafe { mem::zeroed() };
        event.Type = event_type;
        event.EventData = event_data;
        event
    }

    #[test]
    fn the_callback_queues_the_event_it_is_handed() {
        let sink = VmEventSink::default();
        let vm_id = Uuid::new_v4();
        let context = context(&sink, vm_id);
        let data = HSTRING::from("{\"ExitCode\":0}");
        let event = hcs_event(HcsEventSystemExited, PCWSTR(data.as_ptr()));

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
        let event = hcs_event(HcsEventSystemExited, PCWSTR::null());

        // SAFETY: as above; a null `EventData` is what HCS sends for an event
        // that carries no document.
        unsafe { on_hcs_event(&event, Arc::as_ptr(&context).cast()) };

        let (events, _dropped) = sink.drain();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].details, None);
    }

    /// HCS delivers more event types than VMLord acts on, and the callback must
    /// queue those too rather than filter them out: dropping them on this thread
    /// would lose the DEBUG line the drain logs for an unrecognized type.
    #[test]
    fn the_callback_queues_an_event_type_vmlord_does_not_act_on() {
        let sink = VmEventSink::default();
        let context = context(&sink, Uuid::new_v4());
        let event = hcs_event(HCS_EVENT_TYPE(9_999), PCWSTR::null());

        // SAFETY: as above; only the event type differs.
        unsafe { on_hcs_event(&event, Arc::as_ptr(&context).cast()) };

        let (events, _dropped) = sink.drain();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, HcsEventKind::Ignored(9_999));
    }

    /// Nothing documents that HCS never passes a null, and dereferencing one
    /// would take the whole process down from a thread VMLord does not own.
    #[test]
    fn the_callback_ignores_null_arguments() {
        let sink = VmEventSink::default();
        let context = context(&sink, Uuid::new_v4());
        let event = hcs_event(HcsEventSystemExited, PCWSTR::null());

        // SAFETY: passing null is the case under test; the callback must return
        // without dereferencing either pointer.
        unsafe { on_hcs_event(std::ptr::null(), Arc::as_ptr(&context).cast()) };
        // SAFETY: as above, with the context pointer null instead.
        unsafe { on_hcs_event(&event, std::ptr::null()) };

        assert!(sink.drain().0.is_empty());
    }

    /// The scenario Finding 1 describes: a guest powers itself off, the poll
    /// beats HCS's callback to it, the user restarts the VM, and only then does
    /// the stale `Exited` arrive. The watch that queued it is gone, so the event
    /// says nothing true about the compute system VMLord now holds: releasing
    /// that handle would switch the feature off for a running VM, and reporting
    /// it would tell the user a VM they just started has stopped.
    #[test]
    fn an_event_superseded_by_a_newer_watch_is_neither_reported_nor_released() {
        let sink = VmEventSink::default();
        let mut stale = event_of(HcsEventKind::Exited, None);
        stale.generation = 4;
        let vm_id = stale.vm_id;
        sink.push(stale);

        let drained = drain_events(&sink, |asked, generation| {
            assert_eq!(asked, vm_id);
            generation != 5
        });

        assert!(
            drained.diagnostics.is_empty(),
            "a superseded event must not tell the user a running VM stopped: {:?}",
            drained.diagnostics
        );
        assert!(
            drained.released.is_empty(),
            "a superseded event must not release the handle of the watch that replaced it"
        );
        assert!(!drained.service_disconnected);
    }

    /// The check is per generation, not per VM: the event of the watch VMLord
    /// still holds must go through even though an older one for the same VM
    /// would not.
    #[test]
    fn an_event_from_the_watch_still_held_is_reported_and_released() {
        let sink = VmEventSink::default();
        let mut current = event_of(HcsEventKind::Exited, None);
        current.generation = 5;
        let vm_id = current.vm_id;
        sink.push(current);

        let drained = drain_events(&sink, |_vm_id, generation| generation != 5);

        assert_eq!(drained.diagnostics.len(), 1);
        assert_eq!(drained.released, vec![vm_id]);
    }

    /// Every kind is filtered, not just the ones that release a handle: a
    /// superseded crash report describes a compute system VMLord has already
    /// replaced.
    #[test]
    fn a_superseded_crash_report_is_not_reported_either() {
        let sink = VmEventSink::default();
        sink.push(event_of(HcsEventKind::CrashReport, Some("old dump")));

        let drained = drain_events(&sink, |_vm_id, _generation| true);

        assert!(drained.diagnostics.is_empty());
    }

    /// Finding 2: the disconnect releases every handle and nothing
    /// re-registers a watch, so the drain has to report that event reporting is
    /// over for this run. One flag rather than one per VM, because the service
    /// disconnecting is a single event that happens to be delivered per system.
    #[test]
    fn a_service_disconnect_reports_that_event_reporting_stopped_once() {
        let sink = VmEventSink::default();
        sink.push(event_of(HcsEventKind::ServiceDisconnect, None));
        sink.push(event_of(HcsEventKind::ServiceDisconnect, None));

        let drained = drain_events(&sink, nothing_superseded);

        assert!(drained.service_disconnected);
        assert_eq!(drained.released.len(), 2);
    }

    #[test]
    fn an_exit_alone_does_not_report_a_service_disconnect() {
        let sink = VmEventSink::default();
        sink.push(event_of(HcsEventKind::Exited, None));
        sink.push(event_of(HcsEventKind::Ignored(5), None));

        assert!(!drain_events(&sink, nothing_superseded).service_disconnected);
    }

    /// A superseded disconnect releases nothing, so it must not claim event
    /// reporting stopped either.
    #[test]
    fn a_superseded_service_disconnect_reports_nothing() {
        let sink = VmEventSink::default();
        sink.push(event_of(HcsEventKind::ServiceDisconnect, None));

        let drained = drain_events(&sink, |_vm_id, _generation| true);

        assert!(!drained.service_disconnected);
        assert!(drained.released.is_empty());
        assert!(drained.diagnostics.is_empty());
    }

    /// The generation is what makes the staleness check possible at all, so the
    /// callback has to carry it out of the context it was registered with.
    #[test]
    fn the_callback_carries_the_generation_of_its_watch() {
        let sink = VmEventSink::default();
        let context = context(&sink, Uuid::new_v4());
        let event = hcs_event(HcsEventSystemExited, PCWSTR::null());

        // SAFETY: both pointers are to live values owned by this test for the
        // duration of the call, which is exactly HCS's own contract.
        unsafe { on_hcs_event(&event, Arc::as_ptr(&context).cast()) };

        assert_eq!(sink.drain().0[0].generation, 7);
    }
}
