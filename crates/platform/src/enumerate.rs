//! Enumeration of live HCS compute systems and resolution of known VMs to
//! their compute-system handles.

use uuid::Uuid;
use vmlord_core::RepositoryError;

use crate::{
    hcs::{HcsClient, HcsSystem},
    metadata::{MetadataStore, VmComputeSystemMapping},
};

/// A VMLord VM mapping together with whether HCS currently reports a live
/// compute system for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnownVm {
    pub mapping: VmComputeSystemMapping,
    pub present: bool,
}

/// Lists every VM known to `store`, reconciled against the compute systems
/// HCS currently reports.
///
/// A mapping whose compute system HCS no longer reports is still returned
/// (with `present: false`) rather than dropped, so callers can surface the
/// discrepancy instead of silently losing track of the VM.
pub fn list_known_vms(
    client: &HcsClient,
    store: &MetadataStore,
) -> Result<Vec<KnownVm>, RepositoryError> {
    let live_ids = client.enumerate_system_ids()?;
    let mappings = store.list()?;
    Ok(reconcile(&live_ids, mappings))
}

fn reconcile(live_ids: &[String], mappings: Vec<VmComputeSystemMapping>) -> Vec<KnownVm> {
    mappings
        .into_iter()
        .map(|mapping| {
            let present = live_ids
                .iter()
                .any(|id| id == &mapping.hcs_compute_system_id);
            if !present {
                log::warn!(
                    "VM \"{}\" ({}) is mapped to HCS compute system \"{}\", \
                     but HCS does not currently report it",
                    mapping.vm_name,
                    mapping.vm_id,
                    mapping.hcs_compute_system_id
                );
            }
            KnownVm { mapping, present }
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

    use super::{KnownVm, open_by_vm_id, open_by_vm_name, reconcile};
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
        }
    }

    #[test]
    fn reconcile_marks_mappings_present_when_hcs_reports_them() {
        let present = mapping(Uuid::new_v4(), "dev-linux", "vmlord-1");
        let missing = mapping(Uuid::new_v4(), "dev-other", "vmlord-2");
        let live_ids = vec!["vmlord-1".to_string()];

        let result = reconcile(&live_ids, vec![present.clone(), missing.clone()]);

        assert_eq!(
            result,
            vec![
                KnownVm {
                    mapping: present,
                    present: true
                },
                KnownVm {
                    mapping: missing,
                    present: false
                },
            ]
        );
    }

    #[test]
    fn reconcile_is_empty_for_no_mappings() {
        assert_eq!(reconcile(&["vmlord-1".to_string()], Vec::new()), Vec::new());
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
