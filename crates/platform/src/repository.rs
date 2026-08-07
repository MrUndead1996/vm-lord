//! The [`VmRepository`] implementation backed by the native HCS pipelines.
//!
//! This is what the composition root wires in place of the legacy AppSandbox
//! backend: it owns the process-wide HCS client, the metadata store and the
//! compute-system handles, and maps each repository operation onto the
//! pipeline that already implements it.

use std::{fs, path::PathBuf, sync::Mutex};

use vmlord_core::{
    AgentStatus, Diagnostic, DiagnosticLevel, GpuMode, NetworkMode, RepositoryError,
    VmCreateRequest, VmDeleteRequest, VmRepository, VmState, VmSummary, VmUpdateRequest,
};

use crate::{
    HcsClient, HcsSystem, KnownVm, MetadataStore, VmComputeSystemMapping, VmConnections,
    VmCreationPipeline, VmDeletionPipeline, VmForceStopPipeline, VmShutdownPipeline,
    VmStartPipeline,
    hcs::{HCS_ACCESS_ALL, HcsSystemState},
    hcs_config::{self, VmTopology},
    layout, list_known_vms,
    reconnect::{ReconnectOutcome, reconnect_known_vms},
    vhd,
};

/// The metadata document, kept next to the VM directories it describes.
const MAPPING_FILE_NAME: &str = "vm-mapping.json";

const BYTES_PER_GIB: u64 = 1024 * 1024 * 1024;

/// Every VM VMLord creates today is a Linux guest; the native backend has no
/// other guest kind to report yet.
const OS_TYPE: &str = "Linux";

/// A [`VmRepository`] served by the Windows Host Compute System.
pub struct HcsVmRepository {
    client: HcsClient,
    store: MetadataStore,
    storage_root: PathBuf,
    connections: VmConnections,
    creation: VmCreationPipeline,
    start: VmStartPipeline,
    shutdown: VmShutdownPipeline,
    force_stop: VmForceStopPipeline,
    delete: VmDeletionPipeline,
    // `list_vms` takes `&self` but still has findings worth surfacing, so the
    // diagnostics buffer needs interior mutability.
    diagnostics: Mutex<Vec<Diagnostic>>,
    initialized: bool,
}

impl HcsVmRepository {
    /// Creates a repository storing its VMs under `storage_root`.
    #[must_use]
    pub fn new(storage_root: impl Into<PathBuf>) -> Self {
        let storage_root = storage_root.into();
        Self {
            client: HcsClient::new(),
            store: MetadataStore::new(storage_root.join(MAPPING_FILE_NAME)),
            storage_root,
            connections: VmConnections::default(),
            creation: VmCreationPipeline::production(),
            start: VmStartPipeline::production(),
            shutdown: VmShutdownPipeline::production(),
            force_stop: VmForceStopPipeline::production(),
            delete: VmDeletionPipeline::production(),
            diagnostics: Mutex::new(Vec::new()),
            initialized: false,
        }
    }

    fn require_initialized(&self) -> Result<(), RepositoryError> {
        if self.initialized {
            return Ok(());
        }
        Err(RepositoryError::new("the HCS backend is not initialized"))
    }

    fn mapping(&self, vm_name: &str) -> Result<VmComputeSystemMapping, RepositoryError> {
        self.store.find_by_vm_name(vm_name)?.ok_or_else(|| {
            let error = RepositoryError::new(format!("VM \"{vm_name}\" was not found"));
            log::error!("{error}");
            error
        })
    }

    /// Reports whether HCS currently runs the VM behind `mapping`.
    ///
    /// The application layer's cached list can be stale by the time the user
    /// acts on it, so a destructive operation asks HCS itself.
    fn is_running(&self, mapping: &VmComputeSystemMapping) -> Result<bool, RepositoryError> {
        Ok(list_known_vms(&self.client, &self.store)?
            .into_iter()
            .find(|known| known.mapping.vm_id == mapping.vm_id)
            .is_some_and(|known| matches!(known.state, Some(HcsSystemState::Running))))
    }

    fn push_diagnostic(&self, level: DiagnosticLevel, message: String) {
        self.diagnostics
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(Diagnostic { level, message });
    }

    /// Reopens and holds the compute system of a VM that has just started.
    ///
    /// A start that HCS accepted is not undone by a failure to hold its
    /// handle, so this only warns: the VM runs either way.
    fn hold_started_system(&mut self, mapping: &VmComputeSystemMapping) {
        match HcsSystem::open_if_present(&mapping.hcs_compute_system_id, HCS_ACCESS_ALL) {
            Ok(Some(system)) => self.connections.insert(mapping.vm_id, system),
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

    fn summary(&self, known: KnownVm) -> VmSummary {
        let KnownVm { mapping, state } = known;
        let topology = self.topology(&mapping).unwrap_or(VmTopology {
            ram_mb: 0,
            cpu_cores: 0,
        });
        let disk_gb = self.disk_gb(&mapping);

        let state = vm_state(&mapping, state);

        VmSummary {
            name: mapping.vm_name,
            os_type: OS_TYPE.to_string(),
            state,
            ram_mb: topology.ram_mb,
            disk_gb,
            cpu_cores: topology.cpu_cores,
            // GPU, networking and SSH are not wired to the native backend yet
            // and are reported as absent rather than guessed at.
            gpu_mode: GpuMode::None,
            network_mode: NetworkMode::None,
            ip_address: None,
            ssh_port: None,
        }
    }

    /// Reads a VM's memory and processor counts from its stored configuration.
    ///
    /// A VM whose configuration cannot be read is still listed: losing it from
    /// the list would hide a VM that exists, which is worse than reporting it
    /// with unknown sizes.
    fn topology(&self, mapping: &VmComputeSystemMapping) -> Option<VmTopology> {
        self.read_configuration(&mapping.vm_name)
            .and_then(|document| hcs_config::read_topology(&document))
            .inspect_err(|error| {
                self.push_diagnostic(
                    DiagnosticLevel::Warning,
                    format!(
                        "Cannot read the configuration of VM \"{}\": {error}",
                        mapping.vm_name
                    ),
                );
            })
            .ok()
    }

    /// Returns the size of a VM's system disk, in GiB.
    ///
    /// Creation records it, so the disk itself only has to be read for a
    /// mapping written before it did. That read fails while the VM runs --
    /// Hyper-V holds the VHDX open exclusively -- so a failure is logged at
    /// debug level and reported as an unknown size rather than raised as a
    /// diagnostic the user would see on every refresh.
    fn disk_gb(&self, mapping: &VmComputeSystemMapping) -> u32 {
        if mapping.disk_gb > 0 {
            return mapping.disk_gb;
        }

        let size = layout::vm_directory(&self.storage_root, &mapping.vm_name)
            .map(|directory| layout::system_disk_path(&directory))
            .and_then(|path| vhd::virtual_size_bytes(&path));
        let Ok(bytes) = size.inspect_err(|error| {
            log::debug!(
                "cannot read the system disk of VM \"{}\": {error}",
                mapping.vm_name
            );
        }) else {
            return 0;
        };

        let disk_gb = u32::try_from(bytes / BYTES_PER_GIB).unwrap_or(u32::MAX);
        // Record it so the next refresh needs no disk access at all, and so
        // the size survives the VM being started.
        if let Err(error) = self.store.insert(VmComputeSystemMapping {
            disk_gb,
            ..mapping.clone()
        }) {
            log::warn!(
                "could not record the disk size of VM \"{}\": {error}",
                mapping.vm_name
            );
        }
        disk_gb
    }

    fn read_configuration(&self, vm_name: &str) -> Result<String, RepositoryError> {
        let path = layout::configuration_path(&layout::vm_directory(&self.storage_root, vm_name)?);
        fs::read_to_string(&path).map_err(|error| {
            let error = RepositoryError::new(format!(
                "failed to read the HCS configuration of VM \"{vm_name}\" from {}: {error}",
                path.display()
            ));
            log::error!("{error}");
            error
        })
    }
}

/// Maps what HCS reports about a compute system onto the VM state the
/// application layer works with.
///
/// A compute system existing is not the same as its VM running: creation
/// leaves behind a `Created` system that has never executed anything, and a
/// `Paused` or already-`Stopped` system is not running either. Only `Running`
/// is running.
///
/// Whether a running guest has finished booting is not observable until the
/// watch/event work lands, so the agent status stays unknown.
fn vm_state(mapping: &VmComputeSystemMapping, state: Option<HcsSystemState>) -> VmState {
    match state {
        Some(HcsSystemState::Running) => VmState::Running {
            agent_status: AgentStatus::Unknown,
        },
        Some(other) => {
            log::debug!(
                "VM \"{}\" ({}) is listed as stopped because HCS reports {other:?}",
                mapping.vm_name,
                mapping.vm_id
            );
            VmState::Stopped
        }
        None => VmState::Stopped,
    }
}

impl VmRepository for HcsVmRepository {
    /// Brings up the Host Compute Service and reclaims the VMs a previous
    /// VMLord process left running.
    fn initialize(&mut self) -> Result<(), RepositoryError> {
        if self.initialized {
            return Ok(());
        }

        self.client.initialize()?;
        let report = reconnect_known_vms(&self.store)?;
        for reconnected in &report.outcomes {
            if let ReconnectOutcome::Failed(error) = &reconnected.outcome {
                self.push_diagnostic(
                    DiagnosticLevel::Warning,
                    format!(
                        "Could not reconnect to VM \"{}\": {error}",
                        reconnected.mapping.vm_name
                    ),
                );
            }
        }
        self.connections = report.connections;
        self.initialized = true;
        log::info!(
            "the HCS backend is ready with {} reconnected VM(s) under {}",
            self.connections.len(),
            self.storage_root.display()
        );
        Ok(())
    }

    fn create_vm(&mut self, request: VmCreateRequest) -> Result<(), RepositoryError> {
        self.require_initialized()?;

        let vm_directory = layout::vm_directory(&self.storage_root, &request.name)?;
        self.creation
            .create(&self.store, &request, &vm_directory)
            .map(|_mapping| ())
    }

    /// Rewrites the memory and processor counts in the VM's stored
    /// configuration.
    ///
    /// The change reaches the VM the next time it starts: HCS destroys a
    /// compute system as it stops and [`VmStartPipeline`] rebuilds it from
    /// this document, so editing the document is what editing a VM means. A
    /// running VM keeps its current topology until it is restarted.
    fn update_vm(&mut self, request: VmUpdateRequest) -> Result<(), RepositoryError> {
        self.require_initialized()?;

        let mapping = self.mapping(&request.name)?;
        if request.gpu_mode != GpuMode::None {
            return Err(RepositoryError::new(format!(
                "the HCS backend does not support GPU mode {:?} yet",
                request.gpu_mode
            )));
        }
        if request.network_mode != NetworkMode::None {
            return Err(RepositoryError::new(format!(
                "the HCS backend does not support network mode {:?} yet",
                request.network_mode
            )));
        }

        let document = self.read_configuration(&mapping.vm_name)?;
        let updated = hcs_config::apply_topology(
            &document,
            VmTopology {
                ram_mb: request.ram_mb,
                cpu_cores: request.cpu_cores,
            },
        )?;
        let path =
            layout::configuration_path(&layout::vm_directory(&self.storage_root, &request.name)?);
        fs::write(&path, updated).map_err(|error| {
            let error = RepositoryError::new(format!(
                "failed to write the HCS configuration of VM \"{}\" to {}: {error}",
                request.name,
                path.display()
            ));
            log::error!("{error}");
            error
        })?;

        log::info!(
            "VM \"{}\" ({}) now requests {} MiB and {} CPU core(s); \
             the change applies the next time it starts",
            mapping.vm_name,
            mapping.vm_id,
            request.ram_mb,
            request.cpu_cores
        );
        Ok(())
    }

    fn start_vm(&mut self, name: &str) -> Result<(), RepositoryError> {
        self.require_initialized()?;

        let vm_directory = layout::vm_directory(&self.storage_root, name)?;
        self.start.start(&self.store, name, &vm_directory)?;
        let mapping = self.mapping(name)?;
        self.hold_started_system(&mapping);
        Ok(())
    }

    fn stop_vm(&mut self, name: &str) -> Result<(), RepositoryError> {
        self.require_initialized()?;

        let mapping = self.mapping(name)?;
        self.shutdown.shutdown(&self.store, name)?;
        // The guest powers off on its own schedule and HCS destroys the
        // compute system as it goes, so the handle is released now rather than
        // kept until it refers to nothing.
        self.connections.remove(mapping.vm_id);
        Ok(())
    }

    fn force_stop_vm(&mut self, name: &str) -> Result<(), RepositoryError> {
        self.require_initialized()?;

        let mapping = self.mapping(name)?;
        self.force_stop.force_stop(&self.store, name)?;
        self.connections.remove(mapping.vm_id);
        Ok(())
    }

    /// Deletes the VM and everything VMLord created for it.
    ///
    /// A running VM is refused rather than torn down under its guest: deletion
    /// is irreversible, and stopping is the user's decision to make
    /// deliberately.
    fn delete_vm(&mut self, request: VmDeleteRequest) -> Result<(), RepositoryError> {
        self.require_initialized()?;

        let mapping = self.mapping(&request.name)?;
        if self.is_running(&mapping)? {
            let error = RepositoryError::new(format!(
                "VM \"{}\" is running; stop it before deleting it",
                request.name
            ));
            log::error!("{error}");
            return Err(error);
        }

        let vm_directory = layout::vm_directory(&self.storage_root, &request.name)?;
        self.delete.delete(
            &self.store,
            &request.name,
            &vm_directory,
            request.delete_disks,
        )?;
        // The VM is gone, so any handle still held for it refers to nothing.
        self.connections.remove(mapping.vm_id);
        Ok(())
    }

    fn list_vms(&self) -> Result<Vec<VmSummary>, RepositoryError> {
        self.require_initialized()?;

        Ok(list_known_vms(&self.client, &self.store)?
            .into_iter()
            .map(|known| self.summary(known))
            .collect())
    }

    fn take_diagnostics(&mut self) -> Vec<Diagnostic> {
        self.diagnostics
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain(..)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use vmlord_core::{
        GpuMode, NetworkMode, RepositoryError, VmDeleteRequest, VmRepository, VmUpdateRequest,
    };

    use super::HcsVmRepository;

    fn repository() -> HcsVmRepository {
        HcsVmRepository::new(std::env::temp_dir().join("vmlord-repository-test"))
    }

    fn update_request() -> VmUpdateRequest {
        VmUpdateRequest {
            name: "dev".into(),
            ram_mb: 2048,
            cpu_cores: 2,
            gpu_mode: GpuMode::None,
            network_mode: NetworkMode::None,
        }
    }

    fn delete_request() -> VmDeleteRequest {
        VmDeleteRequest {
            name: "dev".into(),
            delete_disks: true,
        }
    }

    fn assert_not_initialized(result: Result<(), RepositoryError>) {
        assert_eq!(
            result.unwrap_err().to_string(),
            "the HCS backend is not initialized"
        );
    }

    #[test]
    fn every_operation_refuses_to_run_before_initialization() {
        let mut repository = repository();

        assert_not_initialized(repository.start_vm("dev"));
        assert_not_initialized(repository.stop_vm("dev"));
        assert_not_initialized(repository.force_stop_vm("dev"));
        assert_not_initialized(repository.update_vm(update_request()));
        assert_not_initialized(repository.delete_vm(delete_request()));
        assert_not_initialized(repository.list_vms().map(|_| ()));
    }

    #[test]
    fn display_and_ssh_report_that_the_native_backend_lacks_them() {
        let mut repository = repository();

        assert!(
            repository
                .open_display("dev")
                .unwrap_err()
                .to_string()
                .contains("not supported")
        );
        assert!(
            repository
                .open_ssh("dev")
                .unwrap_err()
                .to_string()
                .contains("not supported")
        );
    }
}
