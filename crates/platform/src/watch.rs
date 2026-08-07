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

#[cfg(test)]
mod tests {
    use super::{EVENT_CAPACITY, HcsEventKind, HcsVmEvent, VmEventSink, classify};
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
}
