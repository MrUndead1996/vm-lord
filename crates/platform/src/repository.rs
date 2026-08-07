//! The [`VmRepository`] implementation backed by the native HCS pipelines.
//!
//! This is what the composition root wires in place of the legacy AppSandbox
//! backend: it owns the process-wide HCS client, the metadata store and the
//! compute-system handles, and maps each repository operation onto the
//! pipeline that already implements it.

use std::{fs, path::PathBuf, sync::Mutex, time::Duration};

use vmlord_core::{
    AgentStatus, Diagnostic, DiagnosticLevel, GpuMode, NetworkMode, RepositoryError,
    VmCreateRequest, VmRepository, VmState, VmSummary, VmUpdateRequest,
};

use crate::{
    HcsClient, HcsSystem, KnownVm, MetadataStore, VmComputeSystemMapping, VmConnections,
    VmCreationPipeline, VmForceStopPipeline, VmShutdownPipeline, VmStartPipeline,
    hcs::{HCS_ACCESS_ALL, HcsSystemState},
    hcs_config::{self, VmTopology},
    layout, list_known_vms,
    reconnect::{ReconnectOutcome, reconnect_known_vms},
    vhd,
};

/// The metadata document, kept next to the VM directories it describes.
const MAPPING_FILE_NAME: &str = "vm-mapping.json";

const BYTES_PER_GIB: u64 = 1024 * 1024 * 1024;

/// A property query is answered from HCS's own bookkeeping, so it returns
/// promptly; the bound only guards against a wedged Host Compute Service.
const STATE_QUERY_TIMEOUT: Duration = Duration::from_secs(10);

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
        let KnownVm { mapping, present } = known;
        let topology = self.topology(&mapping).unwrap_or(VmTopology {
            ram_mb: 0,
            cpu_cores: 0,
        });
        let disk_gb = self.disk_gb(&mapping.vm_name);

        let state = self.state(&mapping, present);

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

    /// Reports the state of a VM whose compute system HCS does or does not
    /// currently know.
    ///
    /// A compute system existing is not the same as its VM running: creation
    /// leaves behind a `Created` system that has never executed anything, so a
    /// present system is asked what state it is in. An absent one needs no
    /// question -- HCS destroys a compute system as it stops.
    ///
    /// Whether a running guest has finished booting is not observable until the
    /// watch/event work lands, so the agent status stays unknown.
    fn state(&self, mapping: &VmComputeSystemMapping, present: bool) -> VmState {
        if !present {
            return VmState::Stopped;
        }

        let state = HcsSystem::open_if_present(&mapping.hcs_compute_system_id, HCS_ACCESS_ALL)
            .and_then(|system| {
                system.map_or(Ok(None), |system| {
                    system.state(STATE_QUERY_TIMEOUT).map(Some)
                })
            });
        match state {
            Ok(Some(HcsSystemState::Running)) => VmState::Running {
                agent_status: AgentStatus::Unknown,
            },
            // A created-but-never-started system, a paused one, and one that
            // has stopped without being destroyed yet are all "not running" as
            // far as the VM list is concerned.
            Ok(Some(state)) => {
                log::debug!(
                    "VM \"{}\" ({}) is listed as stopped because HCS reports {state:?}",
                    mapping.vm_name,
                    mapping.vm_id
                );
                VmState::Stopped
            }
            Ok(None) => VmState::Stopped,
            Err(error) => {
                self.push_diagnostic(
                    DiagnosticLevel::Warning,
                    format!(
                        "Cannot read the state of VM \"{}\": {error}",
                        mapping.vm_name
                    ),
                );
                VmState::Stopped
            }
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

    fn disk_gb(&self, vm_name: &str) -> u32 {
        let size = layout::vm_directory(&self.storage_root, vm_name)
            .map(|directory| layout::system_disk_path(&directory))
            .and_then(|path| vhd::virtual_size_bytes(&path));
        match size {
            Ok(bytes) => u32::try_from(bytes / BYTES_PER_GIB).unwrap_or(u32::MAX),
            Err(error) => {
                self.push_diagnostic(
                    DiagnosticLevel::Warning,
                    format!("Cannot read the system disk of VM \"{vm_name}\": {error}"),
                );
                0
            }
        }
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
    use vmlord_core::{GpuMode, NetworkMode, RepositoryError, VmRepository, VmUpdateRequest};

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
