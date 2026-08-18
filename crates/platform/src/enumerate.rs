//! Enumeration of live HCS compute systems and resolution of known VMs to
//! their compute-system handles.

use uuid::Uuid;
use vmlord_core::RepositoryError;

use crate::{
    hcs::{HcsClient, HcsSystem, HcsSystemState, HcsSystemSummary},
    metadata::{MetadataStore, VmComputeSystemMapping},
};

/// A VMLord VM mapping together with what HCS currently reports about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnownVm {
    pub mapping: VmComputeSystemMapping,
    /// The state HCS reports for the VM's compute system.
    ///
    /// `None` means HCS does not report the compute system at all, which is
    /// the normal state of every stopped VM: HCS destroys a compute system as
    /// it stops.
    pub state: Option<HcsSystemState>,
    /// The partition the VM is running as, when HCS reports one.
    ///
    /// What an HvSocket address names, and the only way to reach the agent
    /// inside the guest. It is not recorded in the mapping because it is not a
    /// property of the VM: Hyper-V hands out a new one on every start.
    pub runtime_id: Option<Uuid>,
}

impl KnownVm {
    /// Reports whether HCS currently knows the VM's compute system.
    #[must_use]
    pub fn is_present(&self) -> bool {
        self.state.is_some()
    }
}

/// Lists every VM known to `store`, reconciled against the compute systems
/// HCS currently reports.
///
/// A mapping whose compute system HCS no longer reports is still returned
/// (with no state) rather than dropped: HCS destroys a compute system as it
/// stops, so that is the normal state of every stopped VM, not a discrepancy.
pub fn list_known_vms(
    client: &HcsClient,
    store: &MetadataStore,
) -> Result<Vec<KnownVm>, RepositoryError> {
    let live = client.enumerate_systems()?;
    let mappings = store.list()?;
    Ok(reconcile(&live, mappings))
}

fn reconcile(live: &[HcsSystemSummary], mappings: Vec<VmComputeSystemMapping>) -> Vec<KnownVm> {
    mappings
        .into_iter()
        .map(|mapping| {
            let Some(system) = live
                .iter()
                .find(|system| system.id == mapping.hcs_compute_system_id)
            else {
                // Every listing of a stopped VM lands here, so this stays at
                // debug: HCS destroying a compute system as it stops is the
                // expected outcome, not something to warn about.
                log::debug!(
                    "HCS does not report compute system \"{}\" of VM \"{}\" ({}); \
                     it is stopped",
                    mapping.hcs_compute_system_id,
                    mapping.vm_name,
                    mapping.vm_id
                );
                return KnownVm {
                    mapping,
                    state: None,
                    runtime_id: None,
                };
            };

            // HCS has always reported a state alongside the id; a system
            // listed without one is taken to be running, because a running VM
            // the user cannot stop is worse than a stopped one shown as
            // running.
            let state = HcsSystemState::from_enumeration(system.state.clone());
            log::debug!(
                "HCS reports VM \"{}\" ({}) as {state:?}",
                mapping.vm_name,
                mapping.vm_id
            );
            KnownVm {
                mapping,
                state: Some(state),
                runtime_id: system.runtime_id,
            }
        })
        .collect()
}

/// Opens the compute system backing the VM identified by `vm_id` in `store`.
pub fn open_by_vm_id(
    store: &MetadataStore,
    vm_id: Uuid,
    requested_access: u32,
) -> Result<HcsSystem, RepositoryError> {
    let mapping = store.find_by_vm_id(vm_id)?.ok_or_else(|| {
        let error = RepositoryError::new(format!("no HCS mapping found for VM {vm_id}"));
        log::error!("{error}");
        error
    })?;
    open_mapping(&mapping, requested_access)
}

/// Opens the compute system backing the VM named `vm_name` in `store`.
pub fn open_by_vm_name(
    store: &MetadataStore,
    vm_name: &str,
    requested_access: u32,
) -> Result<HcsSystem, RepositoryError> {
    let mapping = store.find_by_vm_name(vm_name)?.ok_or_else(|| {
        let error = RepositoryError::new(format!("no HCS mapping found for VM \"{vm_name}\""));
        log::error!("{error}");
        error
    })?;
    open_mapping(&mapping, requested_access)
}

fn open_mapping(
    mapping: &VmComputeSystemMapping,
    requested_access: u32,
) -> Result<HcsSystem, RepositoryError> {
    log::debug!(
        "opening HCS compute system \"{}\" for VM \"{}\" ({})",
        mapping.hcs_compute_system_id,
        mapping.vm_name,
        mapping.vm_id
    );
    HcsSystem::open(&mapping.hcs_compute_system_id, requested_access)
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use uuid::Uuid;
    use vmlord_core::NetworkMode;

    use super::{
        HcsSystemState, HcsSystemSummary, KnownVm, open_by_vm_id, open_by_vm_name, reconcile,
    };
    use crate::metadata::{MetadataStore, VmComputeSystemMapping};

    fn temporary_mapping_file() -> std::path::PathBuf {
        let unique_id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir()
            .join(format!("vmlord-enumerate-test-{unique_id}"))
            .join("vm-mapping.json")
    }

    fn mapping(vm_id: Uuid, vm_name: &str, hcs_id: &str) -> VmComputeSystemMapping {
        VmComputeSystemMapping {
            vm_id,
            vm_name: vm_name.into(),
            hcs_compute_system_id: hcs_id.into(),
            disk_gb: 20,
            endpoint_id: None,
            network_mode: NetworkMode::None,
            ssh: None,
            gpu_mode: vmlord_core::GpuMode::None,
            guest_target: None,
        }
    }

    fn live(id: &str, state: HcsSystemState) -> HcsSystemSummary {
        HcsSystemSummary {
            id: id.into(),
            state: Some(state),
            runtime_id: Some(RUNTIME_ID),
        }
    }

    /// The partition a live compute system is reported to run as.
    const RUNTIME_ID: Uuid = Uuid::from_u128(0x8636_363d_c5f9_49aa_b507_3b83_f98c_0d14);

    #[test]
    fn reconcile_carries_the_state_hcs_reports_for_each_mapping() {
        let running = mapping(Uuid::new_v4(), "dev-linux", "vmlord-1");
        // A created-but-never-started VM is reported by HCS just like a
        // running one; only its state tells them apart.
        let created = mapping(Uuid::new_v4(), "dev-fresh", "vmlord-2");
        let missing = mapping(Uuid::new_v4(), "dev-other", "vmlord-3");
        let live = vec![
            live("vmlord-1", HcsSystemState::Running),
            live("vmlord-2", HcsSystemState::Created),
        ];

        let result = reconcile(
            &live,
            vec![running.clone(), created.clone(), missing.clone()],
        );

        assert_eq!(
            result,
            vec![
                KnownVm {
                    mapping: running,
                    state: Some(HcsSystemState::Running),
                    runtime_id: Some(RUNTIME_ID),
                },
                KnownVm {
                    mapping: created,
                    state: Some(HcsSystemState::Created),
                    runtime_id: Some(RUNTIME_ID),
                },
                KnownVm {
                    mapping: missing,
                    state: None,
                    runtime_id: None,
                },
            ]
        );
        assert!(result[0].is_present());
        assert!(!result[2].is_present());
    }

    #[test]
    fn a_system_listed_without_a_state_has_never_started() {
        // HCS writes `State` only once a compute system has run, so this is
        // how a created-but-never-started VM appears.
        let dev = mapping(Uuid::new_v4(), "dev-linux", "vmlord-1");
        let live = vec![HcsSystemSummary {
            id: "vmlord-1".into(),
            state: None,
            runtime_id: Some(RUNTIME_ID),
        }];

        let result = reconcile(&live, vec![dev]);

        assert_eq!(result[0].state, Some(HcsSystemState::Created));
    }

    #[test]
    fn a_stopped_vm_is_running_as_no_partition_at_all() {
        // HCS destroys a compute system as it stops, so there is nothing left
        // to address -- and nothing to listen for the VM's agent on.
        let dev = mapping(Uuid::new_v4(), "dev-linux", "vmlord-1");

        let result = reconcile(&[], vec![dev]);

        assert_eq!(result[0].runtime_id, None);
    }

    #[test]
    fn reconcile_is_empty_for_no_mappings() {
        assert_eq!(
            reconcile(&[live("vmlord-1", HcsSystemState::Running)], Vec::new()),
            Vec::new()
        );
    }

    #[test]
    fn open_by_vm_id_reports_a_clear_error_when_unmapped() {
        let store = MetadataStore::new(temporary_mapping_file());

        let error = match open_by_vm_id(&store, Uuid::new_v4(), 0) {
            Ok(_) => panic!("an unmapped VM id must not open a compute system"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("no HCS mapping found"));
    }

    #[test]
    fn open_by_vm_name_reports_a_clear_error_when_unmapped() {
        let store = MetadataStore::new(temporary_mapping_file());

        let error = match open_by_vm_name(&store, "missing-vm", 0) {
            Ok(_) => panic!("an unmapped VM name must not open a compute system"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("missing-vm"));
    }
}
