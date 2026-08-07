//! Reopening the compute systems of known VMs after a VMLord restart.
//!
//! VMLord's handles do not survive its process, so a restart leaves every VM it
//! created running without an open handle. Reconnecting reopens one handle per
//! known VM and keeps it for as long as VMLord runs, which is what makes a
//! restarted VMLord the owner of its VMs again rather than an observer of them.

use std::{collections::HashMap, sync::Arc};

use uuid::Uuid;
use vmlord_core::RepositoryError;

use crate::{
    HcsSystem,
    hcs::HCS_ACCESS_ALL,
    metadata::{MetadataStore, VmComputeSystemMapping},
    watch::{SystemWatch, VmEventSink},
};

/// What reconnecting to a single known VM produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReconnectOutcome {
    /// HCS still knows the VM's compute system and VMLord now holds a handle
    /// to it.
    Reconnected,
    /// HCS does not know the VM's compute system.
    ///
    /// This is the normal state of a stopped VM -- HCS destroys a compute
    /// system as it exits -- and it is also how a VM deleted outside VMLord
    /// looks, because nothing distinguishes the two through HCS alone.
    Absent,
    /// HCS knows the compute system but refused to open it.
    Failed(String),
}

/// One known VM's reconnect result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconnectedVm {
    pub mapping: VmComputeSystemMapping,
    pub outcome: ReconnectOutcome,
}

/// The compute-system handles VMLord holds for the VMs it knows, each watched
/// for HCS events.
///
/// Dropping this closes every handle it holds, so it is meant to live for as
/// long as the VMLord process does.
#[derive(Default)]
pub struct VmConnections {
    systems: HashMap<Uuid, WatchedSystem>,
    events: VmEventSink,
    /// The generation the next [`VmConnections::insert`] stamps on its watch.
    ///
    /// Only ever increases, and never on removal: a generation must not be
    /// reused, because reuse is exactly what would let a stale event pass the
    /// staleness check. This is the only thing that registers a watch, so a
    /// counter per `VmConnections` is enough to keep the values unique among
    /// every event that can reach a drain.
    next_generation: u64,
}

/// A held compute system together with its event registration.
struct WatchedSystem {
    /// `None` when registration failed: the VM stays held and usable, just
    /// unwatched.
    watch: Option<SystemWatch>,
    system: Arc<HcsSystem>,
    /// Which registration this is, matched against the generation an event
    /// carries.
    generation: u64,
}

impl VmConnections {
    /// Creates connections that report their HCS events into `events`.
    #[must_use]
    pub fn with_events(events: VmEventSink) -> Self {
        Self {
            systems: HashMap::new(),
            events,
            next_generation: 0,
        }
    }

    /// Returns the open compute system of `vm_id`, if one is held.
    #[must_use]
    pub fn handle(&self, vm_id: Uuid) -> Option<&HcsSystem> {
        self.systems.get(&vm_id).map(|held| &*held.system)
    }

    /// Reports whether HCS events are actively watched for `vm_id`.
    ///
    /// `false` both for a VM that is not held and for one held but whose
    /// registration failed -- [`VmConnections::insert`]'s `Err` case.
    #[must_use]
    pub fn is_watched(&self, vm_id: Uuid) -> bool {
        self.systems
            .get(&vm_id)
            .is_some_and(|held| held.watch.is_some())
    }

    /// Whether an event queued by watch `generation` for `vm_id` describes a
    /// compute system VMLord has already replaced.
    ///
    /// `false` when no handle is held for `vm_id` at all: nothing superseded
    /// such an event, and what it reports still stands -- the VM really did
    /// exit -- there is simply no handle left to release. Only a *different*
    /// generation means another `insert` has happened since, and that the
    /// `vm_id` now names a compute system the event knows nothing about.
    #[must_use]
    pub fn is_superseded(&self, vm_id: Uuid, generation: u64) -> bool {
        self.systems
            .get(&vm_id)
            .is_some_and(|held| held.generation != generation)
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
        // Drop any watch already held for this VM before registering the new
        // one: HCS has a single callback slot per compute system, so a guard
        // dropped afterwards would clear the registration made here.
        self.systems.remove(&mapping.vm_id);

        // `HcsSystem` is neither `Send` nor `Sync`, so clippy's default lint
        // suspects this `Arc` of crossing threads unsafely. It never does: the
        // `Arc` only lets `VmConnections` and its `SystemWatch` share ownership
        // of the same handle on this thread, per the amendment in Task 5's
        // brief -- the HCS callback thread only touches `WatchContext`, never
        // this `Arc` or the `HcsSystem` it wraps.
        #[allow(clippy::arc_with_non_send_sync)]
        let system = Arc::new(system);
        let generation = self.next_generation;
        self.next_generation += 1;
        let registration = SystemWatch::register(
            &system,
            mapping.vm_id,
            &mapping.vm_name,
            generation,
            &self.events,
        );
        let failure = registration.as_ref().err().cloned();
        self.systems.insert(
            mapping.vm_id,
            WatchedSystem {
                watch: registration.ok(),
                system,
                generation,
            },
        );
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Closes and forgets the handle held for `vm_id`, if any.
    ///
    /// HCS destroys a compute system as it stops, so a handle kept past a stop
    /// would refer to a system that no longer exists.
    pub fn remove(&mut self, vm_id: Uuid) {
        if self.systems.remove(&vm_id).is_some() {
            log::debug!("closed the compute-system handle held for VM {vm_id}");
        }
    }

    /// Reports whether a handle is held for `vm_id`.
    #[must_use]
    pub fn is_connected(&self, vm_id: Uuid) -> bool {
        self.systems.contains_key(&vm_id)
    }

    /// Returns how many compute systems are currently held open.
    #[must_use]
    pub fn len(&self) -> usize {
        self.systems.len()
    }

    /// Reports whether no compute system is held open.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.systems.is_empty()
    }
}

/// The outcome of a startup reconnect: the handles that were reopened, and
/// what happened to every known VM.
pub struct ReconnectReport {
    pub connections: VmConnections,
    pub outcomes: Vec<ReconnectedVm>,
}

/// Reopens a compute-system handle for every VM in `store`.
///
/// A VM that cannot be reconnected does not abort the others: reconnect runs
/// at startup, where losing every VM because one of them is in a bad state
/// would be worse than reporting that one. Only a failure to read the store
/// itself is fatal, because then nothing is known at all.
///
/// Mappings whose compute system HCS does not report are kept: everything a VM
/// is made of -- its disks, its stored `config.json` and this mapping --
/// survives a stop, and [`crate::VmStartPipeline`] rebuilds the compute system
/// from them. Dropping the mapping here would turn every stopped VM into a
/// deleted one.
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
    let mappings = store.list().inspect_err(|error| {
        log::error!("cannot reconnect to any VM: {error}");
    })?;
    log::info!("reconnecting to {} known VM(s)", mappings.len());

    let mut connections = VmConnections::with_events(events.clone());
    let mut outcomes = Vec::with_capacity(mappings.len());

    for mapping in mappings {
        log::debug!(
            "reconnecting to VM \"{}\" ({}) through HCS compute system \"{}\"",
            mapping.vm_name,
            mapping.vm_id,
            mapping.hcs_compute_system_id
        );
        let outcome = match open(&mapping) {
            Ok(Some(system)) => {
                log::info!(
                    "reconnected to VM \"{}\" ({})",
                    mapping.vm_name,
                    mapping.vm_id
                );
                if let Err(error) = connections.insert(&mapping, system) {
                    log::warn!(
                        "reconnected to VM \"{}\" ({}) but cannot watch its HCS events: {error}",
                        mapping.vm_name,
                        mapping.vm_id
                    );
                }
                ReconnectOutcome::Reconnected
            }
            Ok(None) => {
                log::warn!(
                    "HCS does not report a compute system for VM \"{}\" ({}); \
                     it is stopped or was deleted outside VMLord",
                    mapping.vm_name,
                    mapping.vm_id
                );
                ReconnectOutcome::Absent
            }
            Err(error) => {
                log::error!(
                    "failed to reconnect to VM \"{}\" ({}): {error}",
                    mapping.vm_name,
                    mapping.vm_id
                );
                ReconnectOutcome::Failed(error.to_string())
            }
        };
        outcomes.push(ReconnectedVm { mapping, outcome });
    }

    log::info!(
        "reconnected to {} of {} known VM(s); {} absent, {} failed",
        connections.len(),
        outcomes.len(),
        count(&outcomes, &ReconnectOutcome::Absent),
        outcomes
            .iter()
            .filter(|vm| matches!(vm.outcome, ReconnectOutcome::Failed(_)))
            .count()
    );

    Ok(ReconnectReport {
        connections,
        outcomes,
    })
}

fn count(outcomes: &[ReconnectedVm], outcome: &ReconnectOutcome) -> usize {
    outcomes.iter().filter(|vm| &vm.outcome == outcome).count()
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
        },
    };

    use uuid::Uuid;
    use vmlord_core::RepositoryError;

    use super::{ReconnectOutcome, VmConnections, reconnect_known_vms, reconnect_with};
    use crate::metadata::{MetadataStore, VmComputeSystemMapping};
    use crate::watch::{HcsEventKind, HcsVmEvent, VmEventSink};

    struct TempRoot(PathBuf);

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temp_root(label: &str) -> TempRoot {
        static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "vmlord-reconnect-test-{label}-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("test root should be created");
        TempRoot(path)
    }

    fn mapping(vm_name: &str) -> VmComputeSystemMapping {
        let vm_id = Uuid::new_v4();
        VmComputeSystemMapping {
            vm_id,
            vm_name: vm_name.into(),
            hcs_compute_system_id: format!("vmlord-{}", vm_id.as_simple()),
            disk_gb: 20,
        }
    }

    struct Fixture {
        _root: TempRoot,
        store: MetadataStore,
        opened: Arc<Mutex<Vec<String>>>,
    }

    fn fixture(label: &str, mappings: &[VmComputeSystemMapping]) -> Fixture {
        let root = temp_root(label);
        let store = MetadataStore::new(root.0.join("vm-mapping.json"));
        for mapping in mappings {
            store
                .insert(mapping.clone())
                .expect("mapping should be persisted");
        }
        Fixture {
            store,
            opened: Arc::new(Mutex::new(Vec::new())),
            _root: root,
        }
    }

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
            generation: 0,
            kind: HcsEventKind::Exited,
            details: None,
        });

        assert_eq!(sink.drain().0.len(), 1);
    }

    /// An event about a VM no handle is held for is not superseded by anything:
    /// its facts still stand, there is simply nothing left to release. Only a
    /// *different* generation means a watch replaced the one that queued it.
    #[test]
    fn an_event_for_a_vm_no_longer_held_is_not_superseded() {
        let connections = VmConnections::with_events(VmEventSink::default());

        assert!(!connections.is_superseded(Uuid::new_v4(), 3));
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

    #[test]
    fn an_empty_store_reconnects_to_nothing() {
        let fixture = fixture("empty", &[]);

        let report = reconnect_with(&fixture.store, &VmEventSink::default(), |_| Ok(None))
            .expect("an empty store should reconnect successfully");

        assert!(report.outcomes.is_empty());
        assert!(report.connections.is_empty());
    }

    #[test]
    fn a_vm_hcs_no_longer_knows_is_reported_absent_and_stays_mapped() {
        let dev = mapping("dev");
        let fixture = fixture("absent", std::slice::from_ref(&dev));
        let opened = Arc::clone(&fixture.opened);

        let report = reconnect_with(&fixture.store, &VmEventSink::default(), move |mapping| {
            opened
                .lock()
                .unwrap()
                .push(mapping.hcs_compute_system_id.clone());
            Ok(None)
        })
        .expect("an absent compute system must not fail the reconnect");

        assert_eq!(report.outcomes.len(), 1);
        assert_eq!(report.outcomes[0].mapping, dev);
        assert_eq!(report.outcomes[0].outcome, ReconnectOutcome::Absent);
        assert!(!report.connections.is_connected(dev.vm_id));
        assert_eq!(
            fixture.opened.lock().unwrap().as_slice(),
            std::slice::from_ref(&dev.hcs_compute_system_id)
        );
        assert_eq!(
            fixture.store.find_by_vm_id(dev.vm_id).unwrap(),
            Some(dev),
            "a stopped VM is absent from HCS too, so its mapping must survive"
        );
    }

    #[test]
    fn a_failure_to_open_one_vm_does_not_abort_the_others() {
        let broken = mapping("broken");
        let healthy = mapping("healthy");
        let fixture = fixture("failure", &[broken.clone(), healthy.clone()]);
        let opened = Arc::clone(&fixture.opened);
        let broken_id = broken.hcs_compute_system_id.clone();

        let report = reconnect_with(&fixture.store, &VmEventSink::default(), move |mapping| {
            opened
                .lock()
                .unwrap()
                .push(mapping.hcs_compute_system_id.clone());
            if mapping.hcs_compute_system_id == broken_id {
                return Err(RepositoryError::new("injected open failure"));
            }
            Ok(None)
        })
        .expect("a single failing VM must not fail the reconnect");

        let outcomes: Vec<_> = report
            .outcomes
            .iter()
            .map(|vm| (vm.mapping.vm_name.clone(), vm.outcome.clone()))
            .collect();
        assert_eq!(
            outcomes,
            vec![
                (
                    broken.vm_name,
                    ReconnectOutcome::Failed("injected open failure".into())
                ),
                (healthy.vm_name, ReconnectOutcome::Absent),
            ]
        );
        assert_eq!(fixture.opened.lock().unwrap().len(), 2);
    }

    #[test]
    fn an_unreadable_store_fails_the_reconnect() {
        let root = temp_root("corrupt");
        let path = root.0.join("vm-mapping.json");
        fs::write(&path, b"not json").unwrap();

        let error = reconnect_known_vms(&MetadataStore::new(&path), &VmEventSink::default())
            .err()
            .expect("a corrupt store must fail the reconnect");

        assert!(error.to_string().contains("parse metadata mapping"));
    }
}
