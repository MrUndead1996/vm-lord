//! What this process knows about each running VM's display, while it runs.
//!
//! The display twin of `gpu_runs`, and a second map rather than two more
//! fields in that one: the two stacks are observed by different requests, fail
//! separately, and a VM can perfectly well have a working display and no GPU.
//!
//! Nothing is persisted, for the reason nothing about the GPU is: a version
//! read before a stop says nothing about what is installed after one, and a
//! guest that reconnects re-reports within seconds.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard},
    time::SystemTime,
};

use uuid::Uuid;
use vmlord_core::{DisplayFailure, DisplayPayloadFacts, DisplayShare, VmDisplayFacts};

/// One VM's display, for the run it is in the middle of.
#[derive(Clone, Default)]
struct DisplayRun {
    facts: VmDisplayFacts,
    /// What the guest is offered on every session of this run.
    ///
    /// `None` is a VM nothing was staged for -- a headless one, one this
    /// process did not start, or one this release carries no payload for.
    share: Option<DisplayShare>,
}

/// The display of every VM this process knows anything about right now.
#[derive(Clone, Default)]
pub(crate) struct DisplayRuns(Arc<Mutex<BTreeMap<Uuid, DisplayRun>>>);

impl DisplayRuns {
    /// Records what the host side made of a VM's display payload.
    ///
    /// `available` is the version the release could offer; `failure` is why
    /// there is nothing to offer, when there is nothing.
    pub(crate) fn record_host(
        &self,
        vm_id: Uuid,
        available: Option<String>,
        failure: Option<DisplayFailure>,
    ) {
        let mut runs = self.lock();
        let entry = runs.entry(vm_id).or_default();
        entry.facts.payload.available = available;
        entry.facts.failure = failure;
        entry.facts.observed_at = Some(SystemTime::now());
    }

    /// Records what a VM's guest said about the payload it has.
    pub(crate) fn record_guest_payload(
        &self,
        vm_id: Uuid,
        installed: Option<String>,
        previous: Option<String>,
        loaded: Option<String>,
        failure: Option<DisplayFailure>,
    ) {
        let mut runs = self.lock();
        let entry = runs.entry(vm_id).or_default();
        entry.facts.payload = DisplayPayloadFacts {
            installed,
            previous,
            loaded,
            // Kept: what the release offers was decided by the host before the
            // guest said anything, and the guest has nothing to say about it.
            available: entry.facts.payload.available.clone(),
        };
        entry.facts.failure = failure;
        entry.facts.observed_at = Some(SystemTime::now());
    }

    /// Records the share a start prepared for this VM's guest to mount.
    pub(crate) fn record_share(&self, vm_id: Uuid, share: DisplayShare) {
        self.lock().entry(vm_id).or_default().share = Some(share);
    }

    /// What this VM's guest is to be offered, if anything.
    pub(crate) fn share(&self, vm_id: Uuid) -> Option<DisplayShare> {
        self.lock().get(&vm_id)?.share.clone()
    }

    /// What has been observed about one VM, which may be nothing.
    pub(crate) fn snapshot(&self, vm_id: Uuid) -> VmDisplayFacts {
        self.lock()
            .get(&vm_id)
            .map(|run| run.facts.clone())
            .unwrap_or_default()
    }

    /// Drops everything observed about one VM, for a run that is over.
    pub(crate) fn forget(&self, vm_id: Uuid) {
        self.lock().remove(&vm_id);
    }

    /// Drops everything, for a VMLord that is going away.
    pub(crate) fn forget_all(&self) {
        self.lock().clear();
    }

    /// Recovers a poisoned lock rather than propagating the panic: a thread
    /// that died must not take the list of VMs down with it.
    fn lock(&self) -> MutexGuard<'_, BTreeMap<Uuid, DisplayRun>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;
    use vmlord_core::{DisplayFailure, DisplayStage, DisplayStatusCode};

    use super::DisplayRuns;

    #[test]
    fn a_vm_nothing_was_observed_about_has_nothing_to_report() {
        let runs = DisplayRuns::default();

        let facts = runs.snapshot(Uuid::from_u128(1));

        assert_eq!(facts.payload.installed, None);
        assert_eq!(
            facts.observed_at, None,
            "inventing a time would date an observation that was never made"
        );
    }

    #[test]
    fn what_the_release_offers_survives_what_the_guest_reports() {
        let runs = DisplayRuns::default();
        let vm = Uuid::from_u128(1);

        runs.record_host(vm, Some("0.2.0".into()), None);
        runs.record_guest_payload(vm, Some("0.1.0".into()), None, Some("0.1.0".into()), None);

        let facts = runs.snapshot(vm);
        assert_eq!(facts.payload.available.as_deref(), Some("0.2.0"));
        assert_eq!(facts.payload.installed.as_deref(), Some("0.1.0"));
        assert!(
            facts.payload.update_available(),
            "a guest one version behind the release is a guest with an update waiting"
        );
    }

    #[test]
    fn a_run_that_ended_leaves_nothing_behind() {
        let runs = DisplayRuns::default();
        let vm = Uuid::from_u128(1);
        runs.record_host(
            vm,
            None,
            Some(DisplayFailure::new(
                DisplayStage::Payload,
                DisplayStatusCode::PayloadMissing,
                "no payload for this guest",
            )),
        );

        runs.forget(vm);

        assert_eq!(runs.snapshot(vm).failure, None);
    }
}
