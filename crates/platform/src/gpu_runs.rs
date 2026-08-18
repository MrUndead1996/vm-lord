//! What this process knows about each running VM's GPU, while it runs.
//!
//! Two things per VM, kept together because they belong to the same run and
//! end with it: what has been observed, and what the guest is to be offered.
//! Three threads meet here -- the one that starts a VM, the one that serves
//! its agent, and the refresh that lists VMs -- so one entry with one lifetime
//! is what keeps them from disagreeing.
//!
//! Nothing is persisted. `VmGpuStatus` describes a moment, and facts recorded
//! by a process that is gone are confirmed by nothing; a VM whose agent
//! reconnects re-observes them within seconds, which is cheaper than being
//! wrong about a GPU that was taken away while VMLord was not running. The
//! manifest is per-boot for a harder reason: a compute system's Plan9 section
//! is written when the system is built and cannot change while it runs.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard},
    time::SystemTime,
};

use uuid::Uuid;
use vmlord_core::{GpuAssignment, GpuShareManifest, GuestGpuReport, VmGpuFacts};

/// One VM's GPU, for the run it is in the middle of.
#[derive(Clone, Default)]
struct GpuRun {
    facts: VmGpuFacts,
    /// What the guest is offered on every session of this run.
    ///
    /// `None` is a VM nothing has been prepared for -- one with no GPU, or one
    /// this process did not start. It is not an empty manifest: a session with
    /// nothing to say about GPU sends no manifest at all.
    shares: Option<GpuShareManifest>,
}

/// The GPU of every VM this process knows anything about right now.
///
/// Cloned into the threads that write; a clone shares the map rather than
/// copying it.
#[derive(Clone, Default)]
pub(crate) struct GpuRuns(Arc<Mutex<BTreeMap<Uuid, GpuRun>>>);

impl GpuRuns {
    /// Records what the host side did for a VM.
    pub(crate) fn record_assignment(&self, vm_id: Uuid, assignment: GpuAssignment) {
        let mut runs = self.lock();
        let entry = runs.entry(vm_id).or_default();
        entry.facts.assignment = Some(assignment);
        entry.facts.observed_at = Some(SystemTime::now());
    }

    /// Records what a VM's guest said about the GPU it was given.
    pub(crate) fn record_guest(&self, vm_id: Uuid, report: GuestGpuReport) {
        let mut runs = self.lock();
        let entry = runs.entry(vm_id).or_default();
        entry.facts.guest = Some(report);
        entry.facts.observed_at = Some(SystemTime::now());
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

    /// Records what a start prepared for a VM's guest to mount.
    ///
    /// Written once per run, by the start that built it. Every session of that
    /// run offers the same manifest, because the shares were written into the
    /// compute system before it was started and cannot change while it runs.
    pub(crate) fn record_shares(&self, vm_id: Uuid, shares: GpuShareManifest) {
        self.lock().entry(vm_id).or_default().shares = Some(shares);
    }

    /// What this VM's guest is to be offered, if anything.
    pub(crate) fn shares(&self, vm_id: Uuid) -> Option<GpuShareManifest> {
        self.lock().get(&vm_id)?.shares.clone()
    }

    /// What has been observed about one VM, which may be nothing.
    pub(crate) fn snapshot(&self, vm_id: Uuid) -> VmGpuFacts {
        self.lock()
            .get(&vm_id)
            .map(|run| run.facts.clone())
            .unwrap_or_default()
    }

    /// Recovers a poisoned lock rather than propagating the panic: a thread
    /// that died must not take the list of VMs down with it.
    fn lock(&self) -> MutexGuard<'_, BTreeMap<Uuid, GpuRun>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::GpuRuns;
    use uuid::Uuid;
    use vmlord_core::{
        GpuAssignment, GpuFailure, GpuStatusCode, GuestGpuDetail, GuestGpuReport, NativeGpuDetail,
    };

    #[test]
    fn a_vm_nothing_was_observed_about_has_nothing_to_report() {
        let facts = GpuRuns::default();

        assert_eq!(facts.snapshot(Uuid::from_u128(1)).assignment, None);
        assert_eq!(
            facts.snapshot(Uuid::from_u128(1)).observed_at,
            None,
            "inventing a time would date an observation that was never made"
        );
    }

    #[test]
    fn what_each_side_observed_is_kept_beside_the_other() {
        let facts = GpuRuns::default();
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
        let facts = GpuRuns::default();
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
        let facts = GpuRuns::default();
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
        let facts = GpuRuns::default();
        facts.record_assignment(Uuid::from_u128(1), GpuAssignment::Unknown);
        facts.record_assignment(Uuid::from_u128(2), GpuAssignment::Unknown);

        facts.forget(Uuid::from_u128(1));

        assert!(facts.snapshot(Uuid::from_u128(2)).assignment.is_some());
    }

    #[test]
    fn a_vmlord_that_is_going_away_forgets_every_vm() {
        let facts = GpuRuns::default();
        facts.record_assignment(Uuid::from_u128(1), GpuAssignment::Unknown);
        facts.record_assignment(Uuid::from_u128(2), GpuAssignment::Unknown);

        facts.forget_all();

        assert_eq!(facts.snapshot(Uuid::from_u128(1)).assignment, None);
        assert_eq!(facts.snapshot(Uuid::from_u128(2)).assignment, None);
    }
}
