//! What has been observed about each running VM's GPU, while it runs.
//!
//! Two threads write here -- the one that starts a VM and the one that serves
//! its agent -- and the refresh that lists VMs reads. Nothing is persisted:
//! `VmGpuStatus` describes a moment, and facts recorded by a process that is
//! gone are confirmed by nothing. A VM whose agent reconnects re-observes them
//! within seconds, which is cheaper than being wrong about a GPU that was
//! taken away while VMLord was not running.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard},
    time::SystemTime,
};

use uuid::Uuid;
use vmlord_core::{GpuAssignment, GuestGpuReport, VmGpuFacts};

/// The GPU facts of every VM this process has observed anything about.
///
/// Cloned into the threads that write; a clone shares the map rather than
/// copying it.
#[derive(Clone, Default)]
pub(crate) struct GpuFacts(Arc<Mutex<BTreeMap<Uuid, VmGpuFacts>>>);

impl GpuFacts {
    /// Records what the host side did for a VM.
    pub(crate) fn record_assignment(&self, vm_id: Uuid, assignment: GpuAssignment) {
        let mut facts = self.lock();
        let entry = facts.entry(vm_id).or_default();
        entry.assignment = Some(assignment);
        entry.observed_at = Some(SystemTime::now());
    }

    /// Records what a VM's guest said about the GPU it was given.
    pub(crate) fn record_guest(&self, vm_id: Uuid, report: GuestGpuReport) {
        let mut facts = self.lock();
        let entry = facts.entry(vm_id).or_default();
        entry.guest = Some(report);
        entry.observed_at = Some(SystemTime::now());
    }

    /// Drops everything observed about one VM, for a run that is over.
    ///
    /// Called wherever a run ends. A stopped VM reads as having no GPU either
    /// way, but leaving the facts would show the previous run's report the
    /// moment the VM started again and before anything had been observed of
    /// the new one.
    pub(crate) fn forget(&self, vm_id: Uuid) {
        self.lock().remove(&vm_id);
    }

    /// Drops everything, for a VMLord that is going away.
    pub(crate) fn forget_all(&self) {
        self.lock().clear();
    }

    /// What has been observed about one VM, which may be nothing.
    pub(crate) fn snapshot(&self, vm_id: Uuid) -> VmGpuFacts {
        self.lock().get(&vm_id).cloned().unwrap_or_default()
    }

    /// Recovers a poisoned lock rather than propagating the panic: a thread
    /// that died must not take the list of VMs down with it.
    fn lock(&self) -> MutexGuard<'_, BTreeMap<Uuid, VmGpuFacts>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::GpuFacts;
    use uuid::Uuid;
    use vmlord_core::{
        GpuAssignment, GpuFailure, GpuStatusCode, GuestGpuDetail, GuestGpuReport, NativeGpuDetail,
    };

    #[test]
    fn a_vm_nothing_was_observed_about_has_nothing_to_report() {
        let facts = GpuFacts::default();

        assert_eq!(facts.snapshot(Uuid::from_u128(1)).assignment, None);
        assert_eq!(
            facts.snapshot(Uuid::from_u128(1)).observed_at,
            None,
            "inventing a time would date an observation that was never made"
        );
    }

    #[test]
    fn what_each_side_observed_is_kept_beside_the_other() {
        let facts = GpuFacts::default();
        let vm = Uuid::from_u128(1);

        facts.record_assignment(
            vm,
            GpuAssignment::Complete(NativeGpuDetail {
                adapter: Some("NVIDIA RTX 4070".into()),
                adapters: 1,
            }),
        );
        facts.record_guest(
            vm,
            GuestGpuReport::Ready(GuestGpuDetail {
                driver: Some("dxgkrnl".into()),
                render_node: Some("/dev/dri/renderD128".into()),
            }),
        );

        let snapshot = facts.snapshot(vm);
        assert!(matches!(
            snapshot.assignment,
            Some(GpuAssignment::Complete(_))
        ));
        assert!(matches!(snapshot.guest, Some(GuestGpuReport::Ready(_))));
        assert!(snapshot.observed_at.is_some());
    }

    #[test]
    fn an_observation_is_dated_as_it_is_written() {
        let facts = GpuFacts::default();
        let vm = Uuid::from_u128(1);

        facts.record_assignment(vm, GpuAssignment::Unknown);
        let first = facts.snapshot(vm).observed_at.expect("recorded");
        facts.record_guest(
            vm,
            GuestGpuReport::Failed(GpuFailure::new(GpuStatusCode::GuestFailed, "no dxgkrnl")),
        );
        let second = facts.snapshot(vm).observed_at.expect("recorded");

        assert!(second >= first, "the newest observation dates the facts");
    }

    #[test]
    fn a_vm_whose_run_is_over_leaves_nothing_behind() {
        let facts = GpuFacts::default();
        let vm = Uuid::from_u128(1);
        facts.record_guest(vm, GuestGpuReport::Ready(GuestGpuDetail::default()));

        facts.forget(vm);

        assert_eq!(
            facts.snapshot(vm).guest,
            None,
            "a stopped VM must not show yesterday's report on its next start"
        );
    }

    #[test]
    fn forgetting_one_vm_leaves_the_others_alone() {
        let facts = GpuFacts::default();
        facts.record_assignment(Uuid::from_u128(1), GpuAssignment::Unknown);
        facts.record_assignment(Uuid::from_u128(2), GpuAssignment::Unknown);

        facts.forget(Uuid::from_u128(1));

        assert!(facts.snapshot(Uuid::from_u128(2)).assignment.is_some());
    }

    #[test]
    fn a_vmlord_that_is_going_away_forgets_every_vm() {
        let facts = GpuFacts::default();
        facts.record_assignment(Uuid::from_u128(1), GpuAssignment::Unknown);
        facts.record_assignment(Uuid::from_u128(2), GpuAssignment::Unknown);

        facts.forget_all();

        assert_eq!(facts.snapshot(Uuid::from_u128(1)).assignment, None);
        assert_eq!(facts.snapshot(Uuid::from_u128(2)).assignment, None);
    }
}
