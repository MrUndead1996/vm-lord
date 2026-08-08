//! The [`VmRepository`] implementation backed by the native HCS pipelines.
//!
//! This is what the composition root wires in place of the legacy AppSandbox
//! backend: it owns the process-wide HCS client, the metadata store and the
//! compute-system handles, and maps each repository operation onto the
//! pipeline that already implements it.

use std::{fs, net::IpAddr, path::PathBuf, sync::Mutex};

use vmlord_core::{
    AgentStatus, Diagnostic, DiagnosticLevel, GpuMode, NetworkMode, RepositoryError,
    VmCreateRequest, VmDeleteRequest, VmRepository, VmState, VmSummary, VmUpdateRequest,
};

use crate::{
    HcsClient, HcsSystem, KnownVm, MetadataStore, VmComputeSystemMapping, VmConnections,
    VmCreationPipeline, VmDeletionPipeline, VmForceStopPipeline, VmShutdownPipeline,
    VmStartPipeline,
    hcn_endpoint::{EndpointAddress, HcnEndpoint},
    hcs::{HCS_ACCESS_ALL, HcsSystemState},
    hcs_config::{self, VmTopology},
    layout, list_known_vms,
    reconnect::{ReconnectOutcome, reconnect_known_vms},
    vhd, watch,
    watch::VmEventSink,
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
    events: VmEventSink,
    creation: VmCreationPipeline,
    start: VmStartPipeline,
    shutdown: VmShutdownPipeline,
    force_stop: VmForceStopPipeline,
    delete: VmDeletionPipeline,
    // `list_vms` takes `&self` but still has findings worth surfacing, so the
    // diagnostics buffer needs interior mutability.
    diagnostics: Mutex<Vec<Diagnostic>>,
    initialized: bool,
    /// Whether the user has already been told that HCS event reporting stopped.
    ///
    /// HCS delivers `ServiceDisconnect` once per compute system, and those
    /// deliveries can straddle a refresh boundary, so the flag has to outlive a
    /// single drain: the service disconnecting is one event to report, however
    /// many drains its per-VM deliveries are spread over.
    service_disconnect_reported: bool,
}

impl HcsVmRepository {
    /// Creates a repository storing its VMs under `storage_root`.
    #[must_use]
    pub fn new(storage_root: impl Into<PathBuf>) -> Self {
        let storage_root = storage_root.into();
        let events = VmEventSink::default();
        Self {
            client: HcsClient::new(),
            store: MetadataStore::new(storage_root.join(MAPPING_FILE_NAME)),
            storage_root,
            connections: VmConnections::with_events(events.clone()),
            creation: VmCreationPipeline::production(),
            start: VmStartPipeline::production(),
            shutdown: VmShutdownPipeline::production(),
            force_stop: VmForceStopPipeline::production(),
            delete: VmDeletionPipeline::production(),
            diagnostics: Mutex::new(Vec::new()),
            events,
            initialized: false,
            service_disconnect_reported: false,
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

    /// Refuses deletion unless HCS reports the VM as definitely not live.
    ///
    /// The application layer's cached list can be stale by the time the user
    /// acts on it, so a destructive operation asks HCS itself. This is an
    /// allow-list rather than a deny-list: only a compute system HCS does not
    /// report at all, or reports as `Created` or `Stopped`, is safe to tear
    /// down. `Running`, `Paused`, and any state VMLord does not recognise are
    /// refused, because a state this check gets wrong cannot be undone once
    /// deletion runs.
    fn refuse_if_live(&self, mapping: &VmComputeSystemMapping) -> Result<(), RepositoryError> {
        let state = list_known_vms(&self.client, &self.store)?
            .into_iter()
            .find(|known| known.mapping.vm_id == mapping.vm_id)
            .and_then(|known| known.state);

        let description = match &state {
            None | Some(HcsSystemState::Created) | Some(HcsSystemState::Stopped) => {
                return Ok(());
            }
            Some(HcsSystemState::Running) => "running".to_string(),
            Some(HcsSystemState::Paused) => "paused".to_string(),
            Some(HcsSystemState::Other(other)) => format!("in state \"{other}\""),
        };

        let error = RepositoryError::new(format!(
            "VM \"{}\" is {description}; stop it before deleting it",
            mapping.vm_name
        ));
        log::error!("{error}");
        Err(error)
    }

    fn push_diagnostic(&self, level: DiagnosticLevel, message: String) {
        self.diagnostics
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(Diagnostic { level, message });
    }

    /// Reopens and holds the compute system of a VM that has just started, and
    /// starts watching its HCS events.
    ///
    /// A start that HCS accepted is not undone by a failure to hold or watch
    /// its handle, so this only warns: the VM runs either way.
    ///
    /// Failing here leaves the previous watch and its generation in place, so a
    /// stale event queued before the restart still passes the staleness check
    /// and can be reported once. That residue is deliberate for now: the entry
    /// it comes from is the same one a working reopen would have replaced, and
    /// clearing it belongs with re-registering watches rather than here.
    fn hold_started_system(&mut self, mapping: &VmComputeSystemMapping) {
        match HcsSystem::open_if_present(&mapping.hcs_compute_system_id, HCS_ACCESS_ALL) {
            Ok(Some(system)) => {
                if let Err(error) = self.connections.insert(mapping, system) {
                    log::warn!(
                        "VM \"{}\" ({}) started and is held, but VMLord cannot watch \
                         its HCS events: {error}",
                        mapping.vm_name,
                        mapping.vm_id
                    );
                    self.push_diagnostic(
                        DiagnosticLevel::Warning,
                        format!(
                            "VM \"{}\" started, but VMLord cannot report its HCS events",
                            mapping.vm_name
                        ),
                    );
                }
            }
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
        // Read before `mapping.vm_name` is moved into the summary below.
        let network_mode = mapping.network_mode;
        let topology = self.topology(&mapping).unwrap_or(VmTopology {
            ram_mb: 0,
            cpu_cores: 0,
        });
        let disk_gb = self.disk_gb(&mapping);

        let state = vm_state(&mapping, state);
        let ip_address = self.guest_address(&mapping, state);

        VmSummary {
            name: mapping.vm_name,
            os_type: OS_TYPE.to_string(),
            state,
            ram_mb: topology.ram_mb,
            disk_gb,
            cpu_cores: topology.cpu_cores,
            // GPU and SSH are not wired to the native backend yet and are
            // reported as absent rather than guessed at.
            gpu_mode: GpuMode::None,
            network_mode,
            ip_address,
            ssh_port: None,
        }
    }

    /// The address the guest of `mapping` is expected to answer at.
    ///
    /// Read from the VM's HNS endpoint, not from the guest. HNS assigns the
    /// address on the host side and VMLord's DHCP server offers the guest that
    /// one and no other, so the endpoint is where it is known; nothing here
    /// observes whether the guest took the lease, and until it has, this is the
    /// address the guest is *going* to have rather than one it answers at.
    ///
    /// Only a running VM reports one. The endpoint keeps its address across
    /// stops -- that is the point of keeping the endpoint -- but a stopped
    /// guest answers nowhere, and an address shown beside a stopped VM would
    /// read as somewhere to connect.
    ///
    /// No absence here is an error: a VM with no endpoint yet, an endpoint HNS
    /// no longer has or reports no address for, and an address that does not
    /// parse all report `None`. Losing a VM from the list over its address
    /// would be far worse than listing it without one.
    fn guest_address(&self, mapping: &VmComputeSystemMapping, state: VmState) -> Option<IpAddr> {
        if !matches!(state, VmState::Running { .. }) {
            return None;
        }
        let endpoint_id = mapping.endpoint_id?;

        // Every log here stays at debug: this runs for every running VM on
        // every refresh, a second apart, so anything louder would repeat one
        // unreadable endpoint into the log forever.
        let endpoint = match HcnEndpoint::open_if_present(endpoint_id) {
            Ok(Some(endpoint)) => endpoint,
            Ok(None) => {
                log::debug!(
                    "HNS no longer knows endpoint {endpoint_id} of running VM \"{}\", \
                     so it is listed without an address",
                    mapping.vm_name
                );
                return None;
            }
            Err(error) => {
                log::debug!(
                    "cannot open endpoint {endpoint_id} of VM \"{}\": {error}",
                    mapping.vm_name
                );
                return None;
            }
        };

        let address = match endpoint.address() {
            Ok(Some(address)) => address,
            Ok(None) => {
                log::debug!(
                    "HNS reports no address for endpoint {endpoint_id} of VM \"{}\"",
                    mapping.vm_name
                );
                return None;
            }
            Err(error) => {
                log::debug!(
                    "cannot read the address of endpoint {endpoint_id} of VM \"{}\": {error}",
                    mapping.vm_name
                );
                return None;
            }
        };

        guest_ip(&mapping.vm_name, &address)
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
/// Whether a running guest has finished booting is still not observable --
/// HCS reports nothing about it, watch included -- so the agent status stays
/// unknown until the guest agent lands.
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

/// Turns the address HNS reported for an endpoint into one a summary carries.
///
/// HNS spells its addresses in a JSON document, so the text is only known to be
/// an address once it parses. One that does not is reported as no address at
/// all rather than passed on: `VmSummary::ip_address` is what the rest of
/// VMLord connects to, and a string that is not an address is not somewhere to
/// connect.
fn guest_ip(vm_name: &str, address: &EndpointAddress) -> Option<IpAddr> {
    address
        .ip_address
        .parse()
        .inspect_err(|error| {
            log::debug!(
                "HNS reported \"{}\" as the address of VM \"{vm_name}\", \
                 which is not an IP address: {error}",
                address.ip_address
            );
        })
        .ok()
}

/// Records a VM's network mode in its mapping, if it changed.
///
/// The mapping, not `config.json`, is where the mode lives: the document
/// describes the adapter a VM already has, while the mode is what the next
/// start reads to decide whether it should have one at all.
fn record_network_mode(
    store: &MetadataStore,
    mapping: &VmComputeSystemMapping,
    network_mode: NetworkMode,
) -> Result<(), RepositoryError> {
    if mapping.network_mode == network_mode {
        return Ok(());
    }

    store.insert(VmComputeSystemMapping {
        network_mode,
        ..mapping.clone()
    })?;
    log::info!(
        "VM \"{}\" ({}) now asks for {:?} networking; the change applies the next time it starts",
        mapping.vm_name,
        mapping.vm_id,
        network_mode
    );
    Ok(())
}

impl VmRepository for HcsVmRepository {
    /// Brings up the Host Compute Service and reclaims the VMs a previous
    /// VMLord process left running.
    fn initialize(&mut self) -> Result<(), RepositoryError> {
        if self.initialized {
            return Ok(());
        }

        self.client.initialize()?;
        let report = reconnect_known_vms(&self.store, &self.events)?;
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
        hcs_config::ensure_supported_network_mode(request.network_mode)?;

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

        record_network_mode(&self.store, &mapping, request.network_mode)?;

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
        self.refuse_if_live(&mapping)?;

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

    /// Reports everything the repository has to say since the last call,
    /// including the HCS events its watches queued.
    ///
    /// Draining here rather than in `list_vms` is deliberate: this is the
    /// `&mut self` call the application already makes on every refresh, right
    /// after listing, so it is where a released handle can actually be
    /// released.
    fn take_diagnostics(&mut self) -> Vec<Diagnostic> {
        let drained = watch::drain_events(&self.events, |vm_id, generation| {
            self.connections.is_superseded(vm_id, generation)
        });
        for vm_id in drained.released {
            self.connections.remove(vm_id);
        }
        if drained.service_disconnected && !self.service_disconnect_reported {
            self.service_disconnect_reported = true;
            // Nothing reopens a handle or re-registers a callback outside
            // `initialize`, so this run is over as far as HCS events go. Saying
            // so is the honest minimum: silently losing the feature would leave
            // the user believing a crash would still be reported.
            log::warn!(
                "the Host Compute Service disconnected, so every HCS event watch \
                 was released; VMLord reports no further HCS events until it is \
                 restarted"
            );
            self.push_diagnostic(
                DiagnosticLevel::Warning,
                "The Host Compute Service disconnected, so VMLord has stopped \
                 reporting HCS events; restart VMLord to resume them."
                    .to_string(),
            );
        }

        let mut diagnostics: Vec<Diagnostic> = self
            .diagnostics
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain(..)
            .collect();
        diagnostics.extend(drained.diagnostics);
        diagnostics
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use uuid::Uuid;
    use vmlord_core::{
        DiagnosticLevel, GpuMode, NetworkMode, RepositoryError, VmDeleteRequest, VmRepository,
        VmState, VmUpdateRequest,
    };

    use super::{HcsSystemState, HcsVmRepository, guest_ip, record_network_mode};
    use crate::{
        KnownVm, MetadataStore, VmComputeSystemMapping,
        hcn_endpoint::EndpointAddress,
        watch::{HcsEventKind, HcsVmEvent},
    };

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

    /// A store under a directory of this test's own, removed by the test that
    /// created it. The repository tests never share one.
    fn temp_store(label: &str) -> (std::path::PathBuf, MetadataStore) {
        let root = std::env::temp_dir().join(format!(
            "vmlord-repository-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("test root should be created");
        let store = MetadataStore::new(root.join("vm-mapping.json"));
        (root, store)
    }

    fn mapping(network_mode: NetworkMode) -> VmComputeSystemMapping {
        VmComputeSystemMapping {
            vm_id: Uuid::new_v4(),
            vm_name: "dev".into(),
            hcs_compute_system_id: "vmlord-dev".into(),
            disk_gb: 20,
            endpoint_id: None,
            network_mode,
        }
    }

    #[test]
    fn a_changed_network_mode_is_recorded_in_the_mapping() {
        let (root, store) = temp_store("mode-changed");
        let mapping = mapping(NetworkMode::None);
        store.insert(mapping.clone()).unwrap();

        record_network_mode(&store, &mapping, NetworkMode::Nat).unwrap();

        let stored = store.find_by_vm_name("dev").unwrap().unwrap();
        assert_eq!(stored.network_mode, NetworkMode::Nat);
        // Nothing else about the VM may move with its network mode.
        assert_eq!(stored.vm_id, mapping.vm_id);
        assert_eq!(stored.disk_gb, mapping.disk_gb);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn an_unchanged_network_mode_leaves_the_mapping_alone() {
        let (root, store) = temp_store("mode-unchanged");
        let mapping = mapping(NetworkMode::Nat);
        store.insert(mapping.clone()).unwrap();

        record_network_mode(&store, &mapping, NetworkMode::Nat).unwrap();

        assert_eq!(store.find_by_vm_name("dev").unwrap().unwrap(), mapping);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_summary_reports_the_network_mode_the_mapping_records() {
        // The edit form is filled from `VmSummary`, so a summary that always
        // said `None` would make an unrelated edit switch NAT off.
        let repository = repository();

        let summary = repository.summary(KnownVm {
            mapping: mapping(NetworkMode::Nat),
            state: None,
        });

        assert_eq!(summary.network_mode, NetworkMode::Nat);
        assert_eq!(summary.state, VmState::Stopped);
    }

    #[test]
    fn the_address_hns_assigned_becomes_the_summarys_address() {
        let address = EndpointAddress {
            ip_address: "172.22.42.7".into(),
            prefix_length: 24,
        };

        assert_eq!(
            guest_ip("dev", &address),
            Some("172.22.42.7".parse::<std::net::IpAddr>().unwrap())
        );
    }

    #[test]
    fn an_address_that_is_not_an_address_is_reported_as_none() {
        // HNS answers with a JSON document, so nothing but a successful parse
        // says the text really is an address. A VM has to stay listed either
        // way, so this is an absent address rather than a failed listing.
        for text in ["", "dhcp", "172.22.42.7/24", "172.22.42.256"] {
            let address = EndpointAddress {
                ip_address: text.into(),
                prefix_length: 24,
            };

            assert_eq!(guest_ip("dev", &address), None, "{text}");
        }
    }

    #[test]
    fn a_stopped_vm_is_listed_without_an_address() {
        // The endpoint keeps its address while the VM is stopped, but the guest
        // is not there to answer at it, and a listed address reads as somewhere
        // to connect. This also keeps the listing of a stopped VM from asking
        // HNS anything at all.
        let repository = repository();
        let mapping = VmComputeSystemMapping {
            endpoint_id: Some(Uuid::new_v4()),
            ..mapping(NetworkMode::Nat)
        };

        let summary = repository.summary(KnownVm {
            mapping,
            state: None,
        });

        assert_eq!(summary.state, VmState::Stopped);
        assert_eq!(summary.ip_address, None);
    }

    #[test]
    fn a_running_vm_without_an_endpoint_is_listed_without_an_address() {
        // A VM that has never started on the shared network has no endpoint to
        // read an address from, and one that asked for no network never will.
        let repository = repository();

        let summary = repository.summary(KnownVm {
            mapping: mapping(NetworkMode::None),
            state: Some(HcsSystemState::Running),
        });

        assert!(matches!(summary.state, VmState::Running { .. }));
        assert_eq!(summary.ip_address, None);
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

    /// The drain runs inside `take_diagnostics` because that is already called
    /// on every refresh, right after `list_vms`, so an event reaches the user
    /// within one refresh interval without any new machinery.
    ///
    /// Releasing the handle is asserted by `watch::drain_events`' own tests and
    /// by the ignored Hyper-V test; it cannot be asserted here, because holding
    /// a handle requires a live compute system.
    #[test]
    fn a_queued_exit_event_becomes_a_diagnostic() {
        let mut repository = repository();
        repository.events.push(HcsVmEvent {
            vm_id: Uuid::new_v4(),
            vm_name: "dev".into(),
            generation: 0,
            kind: HcsEventKind::Exited,
            details: None,
        });

        let diagnostics = repository.take_diagnostics();

        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic.level == DiagnosticLevel::Info && diagnostic.message.contains("dev")
            }),
            "a VM that stopped on its own must be reported: {diagnostics:?}"
        );
    }

    /// The disconnect releases every handle VMLord holds and nothing
    /// re-registers a watch afterwards, so the user has to be told that HCS
    /// event reporting is over until VMLord restarts. The per-VM `Error` lines
    /// say what happened; this says what it means.
    #[test]
    fn a_service_disconnect_warns_that_event_reporting_stopped_until_a_restart() {
        let mut repository = repository();
        for vm_name in ["dev", "build"] {
            repository.events.push(HcsVmEvent {
                vm_id: Uuid::new_v4(),
                vm_name: vm_name.into(),
                generation: 0,
                kind: HcsEventKind::ServiceDisconnect,
                details: None,
            });
        }

        let diagnostics = repository.take_diagnostics();

        let warnings: Vec<_> = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.level == DiagnosticLevel::Warning)
            .collect();
        assert_eq!(
            warnings.len(),
            1,
            "the service disconnecting is one event, however many VMs it is \
             delivered for: {diagnostics:?}"
        );
        assert!(
            warnings[0].message.contains("restart"),
            "the warning has to say how to get event reporting back: {:?}",
            warnings[0].message
        );
    }

    /// HCS delivers the disconnect once per compute system, and those deliveries
    /// need not land in the same drain: a refresh can fall between them. The
    /// warning still belongs to the service, not to the drain that saw it.
    #[test]
    fn a_service_disconnect_split_across_two_drains_still_warns_only_once() {
        let mut repository = repository();

        let mut warnings = 0;
        for vm_name in ["dev", "build"] {
            repository.events.push(HcsVmEvent {
                vm_id: Uuid::new_v4(),
                vm_name: vm_name.into(),
                generation: 0,
                kind: HcsEventKind::ServiceDisconnect,
                details: None,
            });
            warnings += repository
                .take_diagnostics()
                .iter()
                .filter(|diagnostic| diagnostic.level == DiagnosticLevel::Warning)
                .count();
        }

        assert_eq!(
            warnings, 1,
            "the second drain must not repeat a warning the user has already seen"
        );
    }

    #[test]
    fn a_queued_ignored_event_produces_no_diagnostic() {
        let mut repository = repository();
        repository.events.push(HcsVmEvent {
            vm_id: Uuid::new_v4(),
            vm_name: "dev".into(),
            generation: 0,
            kind: HcsEventKind::Ignored(5),
            details: Some("silo job created".into()),
        });

        assert!(repository.take_diagnostics().is_empty());
    }
}
