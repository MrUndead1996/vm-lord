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
}
