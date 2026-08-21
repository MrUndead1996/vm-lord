//! The [`VmRepository`] implementation backed by the native HCS pipelines.
//!
//! This is what the composition root wires in place of the legacy AppSandbox
//! backend: it owns the process-wide HCS client, the metadata store and the
//! compute-system handles, and maps each repository operation onto the
//! pipeline that already implements it.

use std::{
    fs,
    net::IpAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use uuid::Uuid;
use vmlord_core::{
    AgentStatus, Diagnostic, DiagnosticLevel, GpuAssignment, GpuMode, GuestReadinessTimeouts,
    HostGpuCapabilities, NetworkMode, RepositoryError, SshAvailability, VmCreateRequest,
    VmDeleteRequest, VmRepository, VmState, VmSummary, VmUpdateRequest,
};

use crate::{
    CloudDiskImporter, Com1LogMode, HcsClient, HcsSystem, KnownVm, MetadataStore,
    VmComputeSystemMapping, VmConnections, VmDeletionPipeline, VmForceStopPipeline,
    VmShutdownPipeline, VmStartPipeline,
    agent::{AgentConnection, AgentSessions},
    build::{BuildRegistry, StartedVm},
    cleanup,
    com1_terminal::{Com1Launcher, Com1Sessions},
    cycle::{CycleOutcome, VmBuildCycle},
    gpu_runs::GpuRuns,
    guest_ready::ReadinessTimeouts,
    hcn::HcnNetwork,
    hcn_endpoint::{EndpointAddress, HcnEndpoint},
    hcs::{HCS_ACCESS_ALL, HcsSystemState},
    hcs_config::{self, VmTopology},
    layout, list_known_vms,
    reconnect::{ReconnectOutcome, reconnect_known_vms},
    shutdown_workers::ShutdownWorkers,
    ssh_launches::SshLaunches,
    ssh_terminal::SshLauncher,
    start_registry::StartRegistry,
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
    /// The whole creation cycle -- build, start, wait for the guest -- shared
    /// with every build thread, which is why it is behind an `Arc`.
    cycle: Arc<VmBuildCycle>,
    /// Kept beside the cycle so that a later `with_readiness_timeouts` can be
    /// told apart from the defaults the cycle was built with.
    readiness_timeouts: ReadinessTimeouts,
    /// The VMs being created right now.
    builds: Arc<BuildRegistry>,
    /// Starts VMs. Shared rather than owned because a start now runs on a
    /// thread of its own.
    start: Arc<VmStartPipeline>,
    /// The VMs being started right now.
    starts: Arc<StartRegistry>,
    /// Opens the COM1 console of a VM that is starting or already running.
    com1_launcher: Com1Launcher,
    /// The consoles VMLord currently owns, one per running VM.
    com1_sessions: Com1Sessions,
    /// The agent connections VMLord currently owns, one per running VM.
    ///
    /// Beside the consoles rather than inside the start pipeline: like a
    /// console, an agent connection belongs to a VM's run and has to be given
    /// up when that run ends, and this is the only place that sees every way a
    /// run can end.
    agent_sessions: AgentSessions,
    /// What has been observed about each running VM's GPU, in memory only.
    gpu_runs: GpuRuns,
    /// Opens interactive SSH sessions into running guests. Nothing is kept
    /// beside it: a session belongs to whoever asked for it, not to VMLord.
    ssh_launcher: Arc<SshLauncher>,
    /// The sessions being opened right now, each on a thread of its own.
    ssh_launches: SshLaunches,
    /// Shared with the worker threads that deliver shutdown requests, which is
    /// why it is behind an `Arc`.
    shutdown: Arc<VmShutdownPipeline>,
    /// The shutdown requests being delivered right now.
    shutdowns: ShutdownWorkers,
    force_stop: VmForceStopPipeline,
    delete: VmDeletionPipeline,
    // `list_vms` takes `&self` but still has findings worth surfacing, and a
    // build thread reports its failure the same way, so the diagnostics buffer
    // is both shared and interior-mutable.
    diagnostics: Arc<Mutex<Vec<Diagnostic>>>,
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
    /// Creates a repository storing its VMs under `storage_root`, importing
    /// cloud images through `cloud_disk`.
    #[must_use]
    pub fn new(storage_root: impl Into<PathBuf>, cloud_disk: CloudDiskImporter) -> Self {
        let storage_root = storage_root.into();
        let events = VmEventSink::default();
        let com1_launcher = Com1Launcher::production();
        // Built before the pipelines, because both a build and a start record
        // what they did to a VM's GPU in it, and they have to be the same one.
        let gpu_runs = GpuRuns::default();
        Self {
            client: HcsClient::new(),
            store: MetadataStore::new(storage_root.join(MAPPING_FILE_NAME)),
            storage_root: storage_root.clone(),
            connections: VmConnections::with_events(events.clone()),
            cycle: Arc::new(VmBuildCycle::production(
                cloud_disk,
                com1_launcher.clone(),
                ReadinessTimeouts::default(),
                gpu_runs.clone(),
                &storage_root,
            )),
            readiness_timeouts: ReadinessTimeouts::default(),
            builds: Arc::new(BuildRegistry::default()),
            start: Arc::new(
                VmStartPipeline::production(com1_launcher.clone())
                    .for_vms_under(&storage_root, gpu_runs.clone()),
            ),
            starts: Arc::new(StartRegistry::default()),
            com1_launcher,
            com1_sessions: Com1Sessions::default(),
            agent_sessions: AgentSessions::default(),
            gpu_runs,
            ssh_launcher: Arc::new(SshLauncher::production()),
            ssh_launches: SshLaunches::default(),
            shutdown: Arc::new(VmShutdownPipeline::production()),
            shutdowns: ShutdownWorkers::default(),
            force_stop: VmForceStopPipeline::production(),
            delete: VmDeletionPipeline::production(),
            diagnostics: Arc::new(Mutex::new(Vec::new())),
            events,
            initialized: false,
            service_disconnect_reported: false,
        }
    }

    /// Replaces the readiness timeouts with the user's own.
    ///
    /// A builder rather than an argument of `new`: the timeouts are the only
    /// part of a repository that comes from settings, and every existing
    /// caller -- the tests included -- means the defaults.
    #[must_use]
    pub fn with_readiness_timeouts(mut self, timeouts: GuestReadinessTimeouts) -> Self {
        let timeouts = ReadinessTimeouts::from(timeouts);
        self.readiness_timeouts = timeouts;
        match Arc::get_mut(&mut self.cycle) {
            // The composition root calls this before any build thread exists,
            // so this is the only reference to the cycle.
            Some(cycle) => cycle.set_timeouts(timeouts),
            None => log::error!(
                "the readiness timeouts stay at their defaults: a build is already running \
                 with the cycle they would have changed"
            ),
        }
        self
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
        let Some(description) = self.live_description(mapping)? else {
            return Ok(());
        };

        let error = RepositoryError::new(format!(
            "VM \"{}\" is {description}; stop it before deleting it",
            mapping.vm_name
        ));
        log::error!("{error}");
        Err(error)
    }

    /// How this VM is live, when it is, in words a refusal can be built from.
    ///
    /// `None` is a VM nothing is attached to: stopped, created but never run,
    /// or unknown to HCS. Shared by everything that may only act on a VM that
    /// is not running, so that two refusals cannot disagree about what running
    /// means.
    fn live_description(
        &self,
        mapping: &VmComputeSystemMapping,
    ) -> Result<Option<String>, RepositoryError> {
        let state = list_known_vms(&self.client, &self.store)?
            .into_iter()
            .find(|known| known.mapping.vm_id == mapping.vm_id)
            .and_then(|known| known.state);

        Ok(match &state {
            None | Some(HcsSystemState::Created) | Some(HcsSystemState::Stopped) => None,
            Some(HcsSystemState::Running) => Some("running".to_string()),
            Some(HcsSystemState::Paused) => Some("paused".to_string()),
            Some(HcsSystemState::Other(other)) => Some(format!("in state \"{other}\"")),
        })
    }

    /// What HCS says about this VM right now.
    ///
    /// Asked of HCS rather than of the application layer's cached list, which
    /// can be a refresh out of date by the time the user clicks.
    fn reported_state(
        &self,
        mapping: &VmComputeSystemMapping,
    ) -> Result<Option<HcsSystemState>, RepositoryError> {
        Ok(list_known_vms(&self.client, &self.store)?
            .into_iter()
            .find(|known| known.mapping.vm_id == mapping.vm_id)
            .and_then(|known| known.state))
    }

    /// Opens the console of a VM HCS reports as being in `state`.
    ///
    /// Split from [`VmRepository::open_console`] at the one call that needs
    /// HCS, so that what it decides can be tested without a compute system.
    fn open_console_in_state(
        &mut self,
        mapping: &VmComputeSystemMapping,
        state: Option<HcsSystemState>,
    ) -> Result<(), RepositoryError> {
        refuse_unless_running(
            &mapping.vm_name,
            state,
            "so it has no COM1 port to open; start it first",
        )?;

        // A console whose window has been closed leaves a session behind that
        // is over. Reaping it here is what makes reopening possible at all --
        // and its failure, if it stopped for a reason worth reporting, is
        // reported rather than dropped on the way.
        for diagnostic in console_failure_diagnostics(&mut self.com1_sessions, &self.storage_root) {
            self.push_diagnostic(diagnostic.level, diagnostic.message);
        }
        if self.com1_sessions.contains(mapping.vm_id) {
            let error = RepositoryError::new(format!(
                "VM \"{}\" already has a COM1 console open; close its window before \
                 opening another",
                mapping.vm_name
            ));
            log::error!("{error}");
            return Err(error);
        }

        let vm_directory = layout::vm_directory(&self.storage_root, &mapping.vm_name)?;
        let session = self
            .com1_launcher
            .launch(mapping, &vm_directory, Com1LogMode::Append)?;
        self.com1_sessions.insert(session);
        log::info!(
            "the COM1 console of VM \"{}\" was reopened on request",
            mapping.vm_name
        );
        Ok(())
    }

    /// Puts the guest's own account of a shutdown on screen.
    ///
    /// A graceful shutdown is the one operation whose progress VMLord cannot
    /// report: HCS answers when the request has been delivered, and everything
    /// after that -- services stopping, filesystems unmounting, a unit that
    /// hangs for its ninety seconds -- is written to COM1 and nowhere else. So
    /// the console is opened before the request goes out, unless a window is
    /// already showing it.
    ///
    /// Nothing here can fail a stop: a guest still gets asked to power off when
    /// its log cannot be shown, and the reason is reported instead.
    fn show_shutdown_console(&mut self, mapping: &VmComputeSystemMapping) {
        // A console whose window has been closed leaves a session behind that
        // is over; reaping it here is what makes reopening possible at all.
        for diagnostic in console_failure_diagnostics(&mut self.com1_sessions, &self.storage_root) {
            self.push_diagnostic(diagnostic.level, diagnostic.message);
        }
        if self.com1_sessions.contains(mapping.vm_id) {
            return;
        }

        // Appended to rather than truncated: this is the boot that is ending,
        // and its log is what the messages about to be written belong to.
        let opened =
            layout::vm_directory(&self.storage_root, &mapping.vm_name).and_then(|vm_directory| {
                self.com1_launcher
                    .launch(mapping, &vm_directory, Com1LogMode::Append)
            });
        match opened {
            Ok(session) => {
                self.com1_sessions.insert(session);
                log::info!(
                    "the COM1 console of VM \"{}\" was opened to show its shutdown",
                    mapping.vm_name
                );
            }
            Err(error) => {
                log::warn!(
                    "VM \"{}\" is being shut down without its console: {error}",
                    mapping.vm_name
                );
                self.push_diagnostic(
                    DiagnosticLevel::Warning,
                    format!(
                        "VM \"{}\" is being shut down, but its COM1 console could not be \
                         opened to show it: {error}",
                        mapping.vm_name
                    ),
                );
            }
        }
    }

    /// Collects the shutdown requests that have been answered since the last
    /// refresh.
    ///
    /// A delivered request means the guest is on its way down, so what belongs
    /// to its run is given up here. A failed one means the opposite -- the guest
    /// never heard the request and keeps running -- so its agent connection and
    /// its event watch stay exactly where they are, and only the reason is
    /// reported.
    fn finish_shutdowns(&mut self) {
        for finished in self.shutdowns.take_finished() {
            match finished.result {
                Ok(()) => {
                    log::info!(
                        "the guest of VM \"{}\" was asked to shut down",
                        finished.vm_name
                    );
                    // The agent goes down with the guest that runs it, and the
                    // partition its listener is bound to stops existing as HCS
                    // tears the compute system down.
                    self.agent_sessions.cancel(finished.vm_id);
                    self.gpu_runs.forget(finished.vm_id);
                    // The console is left alone: the guest is still writing the
                    // messages it prints on its way down, and the pipe closing
                    // is what ends the capture. The guest powers off on its own
                    // schedule and HCS destroys the compute system as it goes,
                    // so the handle is released now rather than kept until it
                    // refers to nothing.
                    self.connections.remove(finished.vm_id);
                }
                Err(error) => {
                    log::error!("failed to stop VM \"{}\": {error}", finished.vm_name);
                    self.push_diagnostic(
                        DiagnosticLevel::Error,
                        format!("Failed to stop VM \"{}\": {error}", finished.vm_name),
                    );
                }
            }
        }
    }

    /// Opens a session into a VM HCS reports as being in `state`.
    ///
    /// Split from [`VmRepository::open_ssh`] at the one call that needs HCS, so
    /// that what it decides can be tested without a compute system.
    /// Starts an interactive session into `mapping`'s guest, on a thread of
    /// its own.
    ///
    /// What is answered here is only whether the session is worth attempting:
    /// a VM that is not running has no guest to log into, and saying so is
    /// instant. Everything the attempt itself costs -- the address from HNS,
    /// the port probe and its three-second timeout, the terminal host starting
    /// -- happens on the worker, because this is called on the UI's thread and
    /// a guest that stopped answering used to freeze the window for the whole
    /// probe.
    ///
    /// So `Ok` means "a session is being opened", not "a session opened". The
    /// difference is not hidden: both outcomes reach the diagnostics the UI
    /// already reads, and they are the only account of a session there ever
    /// was -- once the terminal is up, everything else OpenSSH has to say goes
    /// into that window rather than back here.
    fn open_ssh_in_state(
        &self,
        mapping: &VmComputeSystemMapping,
        state: Option<HcsSystemState>,
    ) -> Result<(), RepositoryError> {
        refuse_unless_running(
            &mapping.vm_name,
            state,
            "so there is no guest to log into; start it first",
        )?;

        let vm_directory = layout::vm_directory(&self.storage_root, &mapping.vm_name)?;
        let launcher = Arc::clone(&self.ssh_launcher);
        let diagnostics = Arc::clone(&self.diagnostics);
        let mapping = mapping.clone();

        self.ssh_launches.start(&mapping.vm_name.clone(), move || {
            match launcher.launch(&mapping, &vm_directory) {
                // The command, not just the fact of it. Everything the session
                // goes on to say lands in its own window, so this line is the
                // only account VMLord can give of what it asked for -- which
                // key, which known-hosts file, which port -- and it is the
                // first thing worth reading when a guest refuses a login.
                Ok(invocation) => push_shared_diagnostic(
                    &diagnostics,
                    DiagnosticLevel::Info,
                    format!(
                        "SSH session for VM \"{}\": {}",
                        mapping.vm_name,
                        invocation.command_line()
                    ),
                ),
                Err(failure) => {
                    let message = format!(
                        "cannot open an SSH session to VM \"{}\": {failure}",
                        mapping.vm_name
                    );
                    log::error!("{message}");
                    push_shared_diagnostic(&diagnostics, DiagnosticLevel::Error, message);
                }
            }
        })
    }

    fn push_diagnostic(&self, level: DiagnosticLevel, message: String) {
        self.diagnostics
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(Diagnostic { level, message });
    }

    /// Takes over the VMs that background builds have started.
    ///
    /// The COM1 session and the compute-system handle a build produced belong
    /// here rather than on its thread: both are reachable only behind
    /// `&mut self`, which a build thread does not have.
    fn adopt_started(&mut self, started: Vec<StartedVm>) {
        for StartedVm { mapping, session } in started {
            log::debug!(
                "taking over the console and the compute system of VM \"{}\", built in the \
                 background",
                mapping.vm_name
            );
            self.com1_sessions.insert(session);
            self.hold_started_system(&mapping);
        }
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
        self.listen_for_agent(mapping, self.runtime_id(mapping));
    }

    /// The partition a VM is running as right now.
    ///
    /// Asked of HCS rather than stored: Hyper-V hands out a new runtime id on
    /// every start, so a recorded one would address the previous run.
    fn runtime_id(&self, mapping: &VmComputeSystemMapping) -> Option<Uuid> {
        list_known_vms(&self.client, &self.store)
            .inspect_err(|error| {
                log::warn!(
                    "VMLord cannot tell which partition VM \"{}\" is running as: {error}",
                    mapping.vm_name
                );
            })
            .ok()?
            .into_iter()
            .find(|known| known.mapping.vm_id == mapping.vm_id)?
            .runtime_id
    }

    /// Starts listening for the agent of a VM that is running.
    ///
    /// Nothing here can fail a start: the VM runs whether or not its agent can
    /// be reached, and every way this can go wrong -- a partition HCS will not
    /// name, a VM created before it was given an agent secret, a service its
    /// configuration does not list -- leaves a working VM with no agent rather
    /// than a broken one. Each is logged, and the VM is then listed with an
    /// agent status of unknown, which is what "VMLord is not listening for
    /// this one" honestly reads as.
    fn listen_for_agent(&mut self, mapping: &VmComputeSystemMapping, runtime_id: Option<Uuid>) {
        let Some(runtime_id) = runtime_id else {
            log::warn!(
                "VM \"{}\" ({}) is running, but HCS names no partition for it, so VMLord \
                 cannot listen for its agent",
                mapping.vm_name,
                mapping.vm_id
            );
            return;
        };
        let vm_directory = match layout::vm_directory(&self.storage_root, &mapping.vm_name) {
            Ok(directory) => directory,
            Err(error) => {
                log::warn!("VMLord cannot listen for the agent of this VM: {error}");
                return;
            }
        };

        // A VM this process is only now discovering was started by somebody
        // else, so nothing here saw what was attached to it. Reporting "not
        // yet" would be a different sentence, and a false one.
        if mapping.gpu_mode != GpuMode::None
            && self.gpu_runs.snapshot(mapping.vm_id).assignment.is_none()
        {
            self.gpu_runs
                .record_assignment(mapping.vm_id, GpuAssignment::Unknown);
        }

        match AgentConnection::start(
            mapping,
            runtime_id,
            &layout::agent_secret_path(&vm_directory),
            // What the start of this run prepared, if this process ran it. A
            // VM reclaimed from a previous process was prepared by nobody
            // here, and has nothing to be told to mount.
            self.gpu_runs.shares(mapping.vm_id),
            self.gpu_runs.clone(),
        ) {
            Ok(connection) => self.agent_sessions.insert(connection),
            Err(error) => log::warn!(
                "VM \"{}\" ({}) is running, but VMLord is not listening for its agent: {error}",
                mapping.vm_name,
                mapping.vm_id
            ),
        }
    }

    fn summary(&self, known: KnownVm) -> VmSummary {
        let KnownVm {
            mapping,
            state,
            runtime_id: _,
        } = known;
        // Read before `mapping.vm_name` is moved into the summary below.
        let network_mode = mapping.network_mode;
        let gpu_mode = mapping.gpu_mode;
        let desktop_profile = mapping.desktop_profile;
        let display_provisioning = mapping.display_provisioning.clone();
        let vm_id = mapping.vm_id;
        let ssh = SshAvailability::from(mapping.ssh.clone());
        let topology = self.topology(&mapping).unwrap_or(VmTopology {
            ram_mb: 0,
            cpu_cores: 0,
        });
        let disk_gb = self.disk_gb(&mapping);

        let state = vm_state(
            &mapping,
            state,
            self.agent_sessions.is_online(mapping.vm_id),
            self.starts.contains(&mapping.vm_name),
        );
        let ip_address = self.guest_address(&mapping, state);

        VmSummary {
            name: mapping.vm_name,
            os_type: OS_TYPE.to_string(),
            state,
            ram_mb: topology.ram_mb,
            disk_gb,
            cpu_cores: topology.cpu_cores,
            gpu_mode,
            gpu: self.gpu_runs.snapshot(vm_id),
            desktop_profile,
            display_provisioning,
            // Nothing observes a guest's display yet: the services that would
            // report it are the guest half of this epic (#115), and inventing
            // a report here would be a fact nobody saw.
            display: vmlord_core::VmDisplayFacts::default(),
            network_mode,
            ip_address,
            ssh,
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

    /// Brings up VMLord's shared NAT network and collects the endpoints in it
    /// that no VM owns.
    ///
    /// Neither step can fail the initialization. The network is ensured again by
    /// every start that needs one, and that is where a host whose HNS is broken
    /// has to be told about it -- refusing to initialize would take away the
    /// VMs, the list and the deletions too, over a service only the networked
    /// ones need. Collecting orphans is housekeeping by definition: nothing
    /// waits on it, and an endpoint left behind for another run costs an address
    /// out of the subnet, not a VM.
    ///
    /// Ensuring the network here rather than leaving it to the first start is
    /// what makes it exist -- with its host adapter, its subnet and its NAT --
    /// from the moment VMLord runs, so the first VM to start meets a network
    /// that is already there.
    fn ensure_network(&self) {
        if let Err(error) = HcnNetwork::ensure() {
            log::warn!(
                "the VMLord network is not available; VMs that ask for one cannot start \
                 until it is: {error}"
            );
        }
        if let Err(error) = cleanup::remove_orphan_endpoints(&self.store) {
            log::warn!("the VMLord network was not tidied up: {error}");
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

/// Joins the VMs the store knows with the ones still being built, listing each
/// name once.
///
/// A build reaches the store partway through: creation registers the VM, and
/// the same thread then starts it and waits for its guest, which takes minutes.
/// For that whole stretch both halves know the same VM, and the build's row is
/// the one worth showing -- it says which step is running, while the stored row
/// would say only "Running" for a guest nobody can use yet.
///
/// A build that failed rolled itself back and never reached the store, so its
/// row simply stops being here.
fn merge_with_builds(known: Vec<VmSummary>, builds: &BuildRegistry) -> Vec<VmSummary> {
    let building = builds.summaries();
    let mut summaries: Vec<VmSummary> = known
        .into_iter()
        .filter(|vm| !building.iter().any(|build| build.name == vm.name))
        .collect();
    summaries.extend(building);
    summaries
}

/// Maps what HCS reports about a compute system onto the VM state the
/// application layer works with.
///
/// A compute system existing is not the same as its VM running: creation
/// leaves behind a `Created` system that has never executed anything, and a
/// `Paused` or already-`Stopped` system is not running either. Only `Running`
/// is running.
///
/// The agent status comes from the connection registry rather than from HCS,
/// which reports nothing about what runs inside a guest. `agent_online` is
/// `None` for a VM VMLord is not listening for at all -- one whose partition
/// HCS would not name, or one created before VMs were given an agent -- and
/// that is reported as unknown rather than offline: an agent that was never
/// offered a socket has not failed to connect.
fn vm_state(
    mapping: &VmComputeSystemMapping,
    state: Option<HcsSystemState>,
    agent_online: Option<bool>,
    starting: bool,
) -> VmState {
    match state {
        // A start in flight outranks anything HCS says short of running: the
        // compute system is still being prepared, and reporting the VM as
        // stopped would offer a start it is already in the middle of.
        _ if starting && !matches!(state, Some(HcsSystemState::Running)) => VmState::Starting,
        Some(HcsSystemState::Running) => VmState::Running {
            agent_status: match agent_online {
                Some(true) => AgentStatus::Online,
                Some(false) => AgentStatus::Offline,
                None => AgentStatus::Unknown,
            },
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

fn record_gpu_mode(
    store: &MetadataStore,
    mapping: &VmComputeSystemMapping,
    gpu_mode: GpuMode,
) -> Result<(), RepositoryError> {
    if mapping.gpu_mode == gpu_mode {
        return Ok(());
    }

    store.insert(VmComputeSystemMapping {
        gpu_mode,
        ..mapping.clone()
    })?;
    log::info!(
        "VM \"{}\" ({}) now asks for GPU mode {gpu_mode:?}; the change applies the next \
         time it starts",
        mapping.vm_name,
        mapping.vm_id
    );
    Ok(())
}

/// Refuses a GPU mode change under a VM that is not stopped.
///
/// The mode is applied while the compute system is prepared and started, so a
/// change under a live VM would leave a stored mode that does not describe the
/// GPU the guest actually has. RAM and CPU are different: they are read from
/// the configuration on the next start, and nothing claims they are in effect
/// before then, so an edit of those on a running VM stays allowed.
///
/// `live` is how the VM is live, or `None` for one that is not.
fn refuse_gpu_mode_change(
    vm_name: &str,
    requested: GpuMode,
    stored: GpuMode,
    live: Option<&str>,
) -> Result<(), RepositoryError> {
    if requested == stored {
        return Ok(());
    }
    let Some(description) = live else {
        return Ok(());
    };

    let error = RepositoryError::new(format!(
        "VM \"{vm_name}\" is {description}; stop it before changing its GPU mode"
    ));
    log::error!("{error}");
    Err(error)
}

impl VmRepository for HcsVmRepository {
    /// Brings up the Host Compute Service and the shared network, and reclaims
    /// the VMs a previous VMLord process left running.
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
        // A VM that survived the previous VMLord process is still writing to
        // its serial port, so its console comes back -- appending, because the
        // log of the boot it is in the middle of is the same log.
        let known = list_known_vms(&self.client, &self.store)?;
        for failure in launch_running_consoles(
            &self.com1_launcher,
            &mut self.com1_sessions,
            &known,
            &self.storage_root,
        ) {
            log::warn!("{failure}");
            self.push_diagnostic(DiagnosticLevel::Warning, failure);
        }
        // A VM that survived the previous VMLord process has an agent inside it
        // that is trying to reconnect, so the offer it connects to is put back
        // up here rather than waiting for the VM to be started again.
        for known in &known {
            if known.state == Some(HcsSystemState::Running) {
                self.listen_for_agent(&known.mapping, known.runtime_id);
            }
        }
        // Before `initialized`, so no start of this process can have created an
        // endpoint the cleanup would then collect as one nobody owns.
        self.ensure_network();
        self.initialized = true;
        log::info!(
            "the HCS backend is ready with {} reconnected VM(s) under {}",
            self.connections.len(),
            self.storage_root.display()
        );
        Ok(())
    }

    /// Accepts the creation of a VM and returns; the VM is built on a thread of
    /// its own and appears in the list as `Building` until it is done.
    ///
    /// Everything that can be refused cheaply and certainly is refused here,
    /// before the thread: an obvious mistake belongs in the return value of the
    /// call that made it, not in a diagnostic a second later.
    fn create_vm(&mut self, request: VmCreateRequest) -> Result<(), RepositoryError> {
        self.require_initialized()?;
        request.validate()?;

        if self.store.find_by_vm_name(&request.name)?.is_some()
            || self.builds.contains(&request.name)
        {
            let error = RepositoryError::new(format!("VM \"{}\" already exists", request.name));
            log::error!("{error}");
            return Err(error);
        }
        let vm_directory = layout::vm_directory(&self.storage_root, &request.name)?;
        if vm_directory.exists() {
            let error = RepositoryError::new(format!(
                "VM directory already exists: {}",
                vm_directory.display()
            ));
            log::error!("{error}");
            return Err(error);
        }

        let cycle = Arc::clone(&self.cycle);
        let store = self.store.clone();
        let diagnostics = Arc::clone(&self.diagnostics);
        let name = request.name.clone();
        self.builds.start(request.clone(), move |monitor| {
            let report = cycle.run(&store, &request, &vm_directory, monitor);
            match report.outcome {
                CycleOutcome::Ready => {
                    log::info!("VM \"{name}\" finished building and its guest is ready");
                }
                CycleOutcome::Degraded { detail } => push_shared_diagnostic(
                    &diagnostics,
                    DiagnosticLevel::Warning,
                    format!("VM \"{name}\" is up, but cloud-init finished degraded: {detail}"),
                ),
                CycleOutcome::Unverified { detail } => push_shared_diagnostic(
                    &diagnostics,
                    DiagnosticLevel::Warning,
                    format!("VM \"{name}\" was created and started, but is not confirmed ready: {detail}"),
                ),
                CycleOutcome::NotReady { reason } => push_shared_diagnostic(
                    &diagnostics,
                    DiagnosticLevel::Error,
                    format!(
                        "VM \"{name}\" was created and started, but never became ready: {reason}"
                    ),
                ),
                CycleOutcome::Failed { reason } => push_shared_diagnostic(
                    &diagnostics,
                    DiagnosticLevel::Error,
                    format!("Failed to create VM \"{name}\": {reason}"),
                ),
                CycleOutcome::Cancelled => push_shared_diagnostic(
                    &diagnostics,
                    DiagnosticLevel::Info,
                    format!("Creating VM \"{name}\" was cancelled"),
                ),
            }
            report.started
        })
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
        self.builds.refuse_if_building(&request.name)?;
        self.starts.refuse_if_starting(&request.name)?;

        let mapping = self.mapping(&request.name)?;
        refuse_gpu_mode_change(
            &mapping.vm_name,
            request.gpu_mode,
            mapping.gpu_mode,
            self.live_description(&mapping)?.as_deref(),
        )?;
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
        record_gpu_mode(&self.store, &mapping, request.gpu_mode)?;

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

    /// Starts VM `name`, on a thread of its own.
    ///
    /// Returning `Ok` means the start is under way, not that the VM is
    /// running: a start with a GPU stages a payload first, which unpacks an
    /// archive and hashes the staged tree, and neither can happen on the
    /// thread that draws the window. What the thread produces is taken over on
    /// the next refresh -- see [`HcsVmRepository::take_diagnostics`] -- and
    /// what it could not do arrives there as a diagnostic.
    ///
    /// Everything that can be refused cheaply and certainly is refused here,
    /// before the thread, so an obvious mistake is the return value of the call
    /// that made it rather than a diagnostic a moment later.
    fn start_vm(&mut self, name: &str) -> Result<(), RepositoryError> {
        self.require_initialized()?;
        self.builds.refuse_if_building(name)?;
        self.starts.refuse_if_starting(name)?;

        // Read here rather than on the thread: a VM VMLord does not know, or
        // one whose directory cannot be named, is the return value of the call
        // that asked for it instead of a diagnostic a moment later.
        let mapping = self.mapping(name)?;
        let vm_directory = layout::vm_directory(&self.storage_root, name)?;
        let store = self.store.clone();
        let start = Arc::clone(&self.start);
        let diagnostics = Arc::clone(&self.diagnostics);

        self.starts.start(name, move || {
            match start.start(&store, &mapping.vm_name, &vm_directory) {
                Ok(session) => Some(StartedVm { mapping, session }),
                Err(error) => {
                    let message =
                        format!("VM \"{}\" could not be started: {error}", mapping.vm_name);
                    log::error!("{message}");
                    diagnostics
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push(Diagnostic {
                            level: DiagnosticLevel::Error,
                            message,
                        });
                    None
                }
            }
        })
    }

    /// Asks the guest of `name` to shut down, without waiting for the answer.
    ///
    /// Returning `Ok` means the request is on its way, not that HCS has
    /// delivered it: the delivery waits on the Host Compute Service, and this
    /// is called from the thread that draws the window. What comes back is
    /// collected on the next refresh -- see
    /// [`HcsVmRepository::finish_shutdowns`].
    ///
    /// Everything that can be refused cheaply and certainly is refused here,
    /// before the thread, so an obvious mistake is the return value of the call
    /// that made it rather than a diagnostic a moment later.
    fn stop_vm(&mut self, name: &str) -> Result<(), RepositoryError> {
        self.require_initialized()?;
        self.builds.refuse_if_building(name)?;

        let mapping = self.mapping(name)?;
        // Before the request: what the guest prints on its way down is the only
        // account there is of a shutdown that stalls, and the user watching it
        // is why the wait no longer happens on the UI thread.
        self.show_shutdown_console(&mapping);

        let store = self.store.clone();
        let shutdown = Arc::clone(&self.shutdown);
        let vm_name = mapping.vm_name.clone();
        self.shutdowns
            .start(mapping.vm_id, &mapping.vm_name, move || {
                shutdown.shutdown(&store, &vm_name)
            })
    }

    fn force_stop_vm(&mut self, name: &str) -> Result<(), RepositoryError> {
        self.require_initialized()?;
        self.builds.refuse_if_building(name)?;

        let mapping = self.mapping(name)?;
        self.force_stop.force_stop(&self.store, name)?;
        // Nothing will close the pipe from the other end: the compute system
        // was torn down under its guest.
        self.com1_sessions.cancel(mapping.vm_id);
        self.agent_sessions.cancel(mapping.vm_id);
        self.gpu_runs.forget(mapping.vm_id);
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
        self.builds.refuse_if_building(&request.name)?;
        // A VM in the middle of starting is not a VM to remove: its thread is
        // about to hand over a console and a compute system for a VM that
        // would no longer exist.
        self.starts.refuse_if_starting(&request.name)?;

        let mapping = self.mapping(&request.name)?;
        self.refuse_if_live(&mapping)?;

        // Before the directory goes: the reader has `com1.log` open, and the
        // agent listener holds the secret read out of a file that is about to
        // be removed.
        self.com1_sessions.cancel(mapping.vm_id);
        self.agent_sessions.cancel(mapping.vm_id);
        self.gpu_runs.forget(mapping.vm_id);
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

    /// Where VM `name`'s private key is, or will be once it is created.
    ///
    /// A name that cannot be a directory has no key path rather than an error:
    /// this answers a label in a dialog, and the same name is refused with a
    /// message of its own the moment the VM is actually created.
    fn ssh_key_path(&self, name: &str) -> Option<PathBuf> {
        layout::vm_directory(&self.storage_root, name)
            .ok()
            .map(|directory| layout::ssh_key_path(&directory))
    }

    fn cancel_create(&mut self, name: &str) -> Result<(), RepositoryError> {
        self.require_initialized()?;
        self.builds.cancel(name)
    }

    /// Reopens the COM1 console of a running VM.
    ///
    /// The console opens by itself when a VM starts and when VMLord reconnects
    /// to one that was already running; this is how it comes back after its
    /// window has been closed. The log is appended to rather than truncated:
    /// the boot it is capturing is the same boot, and the messages already
    /// written are the ones the console is usually reopened to read.
    ///
    /// A VM that already has a reader is refused rather than given a second
    /// one: two readers on one pipe split the guest's output between two
    /// windows and neither shows all of it.
    fn open_console(&mut self, name: &str) -> Result<(), RepositoryError> {
        self.require_initialized()?;
        self.builds.refuse_if_building(name)?;

        let mapping = self.mapping(name)?;
        let state = self.reported_state(&mapping)?;
        self.open_console_in_state(&mapping, state)
    }

    /// Opens an interactive SSH session into a running guest.
    ///
    /// Nothing is recorded afterwards. The session runs in a terminal of its
    /// own, outlives whatever VMLord does next, and a VM may have as many of
    /// them as a person opens: this is a shell somebody asked for, not a
    /// resource VMLord owns. That is the whole difference from
    /// [`VmRepository::open_console`], which owns exactly one reader per VM
    /// because two on one pipe would split the guest's output.
    ///
    /// The state is asked of HCS rather than taken from the list the user
    /// clicked in, which can be a refresh out of date. What comes back on
    /// failure is the preflight check that stopped the session -- which Windows
    /// feature is missing, which port did not answer, which key is gone --
    /// because once the terminal is up, everything else OpenSSH has to say goes
    /// into that window and not into this process.
    fn open_ssh(&mut self, name: &str) -> Result<(), RepositoryError> {
        self.require_initialized()?;
        self.builds.refuse_if_building(name)?;

        let mapping = self.mapping(name)?;
        let state = self.reported_state(&mapping)?;
        self.open_ssh_in_state(&mapping, state)
    }

    /// Reads the host afresh on every call: see [`crate::discover_host_gpu`].
    fn host_gpu_capabilities(&self) -> Result<HostGpuCapabilities, RepositoryError> {
        Ok(crate::gpu_discovery::discover_host_gpu())
    }

    fn list_vms(&self) -> Result<Vec<VmSummary>, RepositoryError> {
        self.require_initialized()?;

        let known: Vec<VmSummary> = list_known_vms(&self.client, &self.store)?
            .into_iter()
            .map(|known| self.summary(known))
            .collect();
        Ok(merge_with_builds(known, &self.builds))
    }

    /// Reports everything the repository has to say since the last call,
    /// including the HCS events its watches queued.
    ///
    /// Draining here rather than in `list_vms` is deliberate: this is the
    /// `&mut self` call the application already makes on every refresh, right
    /// after listing, so it is where a released handle can actually be
    /// released.
    fn take_diagnostics(&mut self) -> Vec<Diagnostic> {
        // The `&mut self` call the application already makes on every refresh,
        // right after listing: the place a finished build can be joined, and
        // the place what it started can be taken over.
        let started = self.builds.take_started();
        self.adopt_started(started);
        // The same call for the same reason: a start that has ended has a
        // console session and a compute system to hand over, and both are
        // reachable only here.
        let started = self.starts.take_started();
        self.adopt_started(started);
        // The same call, for the same reason: a shutdown request that has been
        // answered has handles to give up, and they are reachable only here.
        self.finish_shutdowns();
        let drained = watch::drain_events(&self.events, |vm_id, generation| {
            self.connections.is_superseded(vm_id, generation)
        });
        for vm_id in drained.released {
            // The compute system is gone, so its reader has nothing left to
            // read; cancelling closes the window rather than leaving it open on
            // a pipe that will never deliver again.
            self.com1_sessions.cancel(vm_id);
            self.agent_sessions.cancel(vm_id);
            self.gpu_runs.forget(vm_id);
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
        diagnostics.extend(console_failure_diagnostics(
            &mut self.com1_sessions,
            &self.storage_root,
        ));
        diagnostics
    }
}

/// Stops every build, and waits for every shutdown request, before the process
/// leaves.
///
/// Without this, shutting VMLord down either kills a thread in the middle of
/// writing a VHDX -- leaving the directory it was told to remove -- or waits
/// forever on one that was never told to stop.
impl Drop for HcsVmRepository {
    fn drop(&mut self) {
        // First: a terminal window left open after VMLord is gone has nobody
        // to close it.
        self.com1_sessions.cancel_all();
        // Each listener's thread is joined as it goes, so no socket outlives
        // the process that bound it.
        self.agent_sessions.cancel_all();
        self.gpu_runs.forget_all();
        // Joined rather than abandoned: HCS has either been asked to run each
        // system or has not, and a thread left behind would outlive the
        // process that owns what it is about to produce.
        self.starts.join_all();
        self.builds.cancel_all_and_join();
        // A request still being delivered holds a handle to a compute system
        // this repository is about to drop.
        self.shutdowns.join_all();
        // A launch still probing holds nothing of the repository's, but its
        // thread must not outlive the process that started it.
        self.ssh_launches.join_all();
    }
}

/// Opens a COM1 console for every VM that is still running, appending to the
/// log its previous console was writing.
///
/// Returns one message per VM whose console could not be opened. A failure here
/// is not a reason to stop a guest: the VM was running before VMLord started
/// and its owner did not ask for it to be touched.
fn launch_running_consoles(
    launcher: &Com1Launcher,
    sessions: &mut Com1Sessions,
    known: &[KnownVm],
    storage_root: &Path,
) -> Vec<String> {
    let mut failures = Vec::new();
    for vm in known
        .iter()
        .filter(|vm| vm.state == Some(HcsSystemState::Running))
    {
        let vm_directory = match layout::vm_directory(storage_root, &vm.mapping.vm_name) {
            Ok(directory) => directory,
            Err(error) => {
                failures.push(format!(
                    "Could not reopen the COM1 console of VM \"{}\": {error}",
                    vm.mapping.vm_name
                ));
                continue;
            }
        };
        match launcher.launch(&vm.mapping, &vm_directory, Com1LogMode::Append) {
            Ok(session) => sessions.insert(session),
            Err(error) => failures.push(format!(
                "Could not reopen the COM1 console of VM \"{}\": {error}",
                vm.mapping.vm_name
            )),
        }
    }
    failures
}

/// Refuses an operation on a live guest unless HCS reports the VM as running.
///
/// Both things a person can open on a guest -- its console and a shell -- exist
/// only while the compute system does: the named pipe behind COM1 belongs to
/// it, and so does the network endpoint the guest answers SSH on. Only
/// `Running` is accepted -- a VM that is merely `Created` has a configuration
/// and no compute system to talk to, and a paused one has neither a pipe
/// anybody is writing to nor a guest anybody can log into.
///
/// `consequence` says what this particular VM therefore does not have, so that
/// the message names the thing the person actually asked for.
fn refuse_unless_running(
    vm_name: &str,
    state: Option<HcsSystemState>,
    consequence: &str,
) -> Result<(), RepositoryError> {
    let description = match &state {
        Some(HcsSystemState::Running) => return Ok(()),
        None | Some(HcsSystemState::Stopped) => "stopped".to_string(),
        Some(HcsSystemState::Created) => "created but not running".to_string(),
        Some(HcsSystemState::Paused) => "paused".to_string(),
        Some(HcsSystemState::Other(other)) => format!("in state \"{other}\""),
    };

    let error = RepositoryError::new(format!("VM \"{vm_name}\" is {description}, {consequence}"));
    log::error!("{error}");
    Err(error)
}

/// Turns every reader that stopped for the wrong reason into a diagnostic, and
/// forgets every reader that is over.
fn console_failure_diagnostics(
    sessions: &mut Com1Sessions,
    storage_root: &Path,
) -> Vec<Diagnostic> {
    sessions
        .reap()
        .into_iter()
        .map(|failure| {
            let log_path = layout::vm_directory(storage_root, &failure.vm_name)
                .map(|directory| layout::com1_log_path(&directory).display().to_string())
                .unwrap_or_else(|_| layout::COM1_LOG_FILE_NAME.to_owned());
            let message = format!(
                "COM1 diagnostics for VM \"{}\" stopped unexpectedly; see {log_path}",
                failure.vm_name
            );
            log::error!("{message}");
            Diagnostic {
                level: DiagnosticLevel::Error,
                message,
            }
        })
        .collect()
}

/// Records a diagnostic in a buffer shared with the build threads.
///
/// Free rather than a method because a build thread has the buffer and not the
/// repository: the repository is not `Send`, and does not need to be.
fn push_shared_diagnostic(
    diagnostics: &Mutex<Vec<Diagnostic>>,
    level: DiagnosticLevel,
    message: String,
) {
    diagnostics
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(Diagnostic { level, message });
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::{Arc, Mutex},
    };

    use uuid::Uuid;
    use vmlord_core::{
        AgentStatus, DiagnosticLevel, GpuMode, NetworkMode, RepositoryError, SshAuthentication,
        SshAvailability, SshConfig, SshPort, VmDeleteRequest, VmGpuFacts, VmRepository, VmState,
        VmSummary, VmUpdateRequest,
    };

    use super::{
        GpuAssignment, HcsSystemState, HcsVmRepository, OS_TYPE, console_failure_diagnostics,
        guest_ip, launch_running_consoles, merge_with_builds, record_gpu_mode, record_network_mode,
        refuse_gpu_mode_change,
    };
    use std::sync::atomic::AtomicBool;

    use vmlord_core::{GuestGpuDetail, GuestGpuReport};

    use crate::{
        Com1Launcher, Com1LogMode, KnownVm, MetadataStore, VmComputeSystemMapping,
        agent::AgentConnection,
        build::BuildRegistry,
        com1_terminal::{Com1Sessions, TerminalCommand},
        hcn_endpoint::EndpointAddress,
        ssh_terminal::SshLauncher,
        watch::{HcsEventKind, HcsVmEvent},
    };

    fn repository() -> HcsVmRepository {
        HcsVmRepository::new(
            std::env::temp_dir().join("vmlord-repository-test"),
            Box::new(|_, _, _, _| {
                Err(RepositoryError::new(
                    "this test creates no VM from a cloud image",
                ))
            }),
        )
    }

    #[test]
    fn a_vm_being_built_is_listed_once_even_after_it_reaches_the_store() {
        // A build now outlives its own registration: creation writes the VM to
        // the store, and the thread carries on starting it and waiting for its
        // guest for minutes afterwards. Both halves of `list_vms` therefore
        // know the same VM, and the build's row is the one that tells the user
        // what is still happening to it.
        let (root, store) = temp_store("one-row");
        let mapping = mapping(NetworkMode::Nat);
        let name = mapping.vm_name.clone();
        store.insert(mapping).expect("the mapping should be stored");
        let builds = BuildRegistry::default();
        let release = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let held = std::sync::Arc::clone(&release);
        builds
            .start(create_request(&name), move |_| {
                while !held.load(std::sync::atomic::Ordering::Relaxed) {
                    std::thread::yield_now();
                }
                None
            })
            .expect("the build should start");

        let known = vec![VmSummary {
            name: name.clone(),
            os_type: OS_TYPE.to_string(),
            state: VmState::Running {
                agent_status: AgentStatus::Unknown,
            },
            ram_mb: 2048,
            disk_gb: 20,
            cpu_cores: 2,
            gpu_mode: GpuMode::None,
            gpu: VmGpuFacts::default(),
            desktop_profile: vmlord_core::DesktopProfile::Headless,
            display_provisioning: vmlord_core::DisplayProvisioning::NotRequested,
            display: vmlord_core::VmDisplayFacts::default(),
            network_mode: NetworkMode::Nat,
            ip_address: None,
            ssh: SshAvailability::Disabled,
        }];
        let listed = merge_with_builds(known, &builds);

        release.store(true, std::sync::atomic::Ordering::Relaxed);
        builds.cancel_all_and_join();
        let _ = fs::remove_dir_all(&root);

        assert_eq!(listed.len(), 1, "a VM appears once: {listed:?}");
        assert!(
            matches!(listed[0].state, VmState::Building { .. }),
            "while it is still being built, the build's row is the one that shows: {:?}",
            listed[0].state
        );
    }

    #[test]
    fn the_readiness_timeouts_come_from_the_settings() {
        let repository =
            repository().with_readiness_timeouts(vmlord_core::GuestReadinessTimeouts {
                address_secs: 1,
                ssh_port_secs: 2,
                cloud_init_secs: 3,
                connect_timeout_secs: 4,
            });

        assert_eq!(
            repository.readiness_timeouts,
            crate::guest_ready::ReadinessTimeouts {
                address: std::time::Duration::from_secs(1),
                ssh_port: std::time::Duration::from_secs(2),
                cloud_init: std::time::Duration::from_secs(3),
                connect: std::time::Duration::from_secs(4),
            }
        );
    }

    #[test]
    fn a_repository_left_alone_keeps_the_default_timeouts() {
        assert_eq!(
            repository().readiness_timeouts,
            crate::guest_ready::ReadinessTimeouts::default()
        );
    }

    #[test]
    fn a_started_vm_handed_over_by_a_build_is_held_and_its_console_kept() {
        // A build thread can touch neither `com1_sessions` nor a compute-system
        // handle: both live behind `&mut self`. Taking them over on refresh is
        // what makes a VM created in the background indistinguishable from one
        // started by hand.
        let mut repository = repository();
        let mapping = mapping(NetworkMode::Nat);
        let vm_id = mapping.vm_id;
        let session = crate::com1_terminal::Com1Launcher::session_for_test(&mapping);

        repository.adopt_started(vec![crate::build::StartedVm { mapping, session }]);

        assert!(repository.com1_sessions.contains(vm_id));
    }

    fn create_request(name: &str) -> vmlord_core::VmCreateRequest {
        vmlord_core::VmCreateRequest {
            name: name.into(),
            source: vmlord_core::VmSource::LocalMedia {
                path: "C:\\images\\ubuntu.iso".into(),
            },
            ram_mb: 2048,
            disk_gb: 20,
            cpu_cores: 2,
            gpu_mode: GpuMode::None,
            network_mode: NetworkMode::None,
        }
    }

    /// The create form shows this path beside the toggle that asks for a key
    /// pair, so it has to be the path the VM will actually get -- named by
    /// `layout`, and answered before the VM exists.
    #[test]
    fn the_key_path_of_a_vm_is_answered_before_the_vm_exists() {
        let repository = repository();

        let path = repository
            .ssh_key_path("dev")
            .expect("a plain name has a key path");

        assert_eq!(
            path,
            crate::layout::ssh_key_path(
                &crate::layout::vm_directory(
                    &std::env::temp_dir().join("vmlord-repository-test"),
                    "dev",
                )
                .unwrap(),
            )
        );
        assert!(!path.exists(), "no VM has been created");
        assert_eq!(
            repository.ssh_key_path("../escape"),
            None,
            "a name that is not a directory names no file either"
        );
    }

    /// A build in flight is not in the metadata store yet, so without this the
    /// duplicate-name check would let a second creation through and the two
    /// would fight over one directory.
    #[test]
    fn a_name_being_built_counts_as_taken() {
        let repository = repository();
        let release = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let held = std::sync::Arc::clone(&release);
        repository
            .builds
            .start(create_request("dev"), move |_| {
                while !held.load(std::sync::atomic::Ordering::Relaxed) {
                    std::thread::yield_now();
                }
                None
            })
            .expect("the build should start");

        let error = repository
            .builds
            .refuse_if_building("dev")
            .expect_err("a VM that is still being created cannot be started or deleted");
        assert!(error.to_string().contains("still being created"));
        assert!(repository.builds.contains("dev"));

        release.store(true, std::sync::atomic::Ordering::Relaxed);
        repository.builds.cancel_all_and_join();
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
        let root =
            std::env::temp_dir().join(format!("vmlord-repository-{label}-{}", std::process::id()));
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
            ssh: None,
            gpu_mode: GpuMode::None,
            desktop_profile: vmlord_core::DesktopProfile::Headless,
            display_provisioning: vmlord_core::DisplayProvisioning::NotRequested,
            guest_target: None,
        }
    }

    /// A VM as HCS and the store together report it.
    fn known(name: &str, state: Option<HcsSystemState>) -> KnownVm {
        KnownVm {
            mapping: VmComputeSystemMapping {
                vm_id: Uuid::new_v4(),
                vm_name: name.to_owned(),
                hcs_compute_system_id: format!("vmlord-{name}"),
                disk_gb: 20,
                endpoint_id: None,
                network_mode: NetworkMode::None,
                ssh: None,
                gpu_mode: GpuMode::None,
                desktop_profile: vmlord_core::DesktopProfile::Headless,
                display_provisioning: vmlord_core::DisplayProvisioning::NotRequested,
                guest_target: None,
            },
            state,
            runtime_id: None,
        }
    }

    /// A launcher that records the console it would have opened.
    fn console_launcher(recorded: Arc<Mutex<Vec<String>>>) -> Com1Launcher {
        Com1Launcher::for_test(
            std::path::PathBuf::from(r"C:\VMLord\vmlord-com1.exe"),
            move |command: &TerminalCommand| {
                recorded.lock().unwrap().push(
                    command
                        .args
                        .iter()
                        .map(|argument| argument.to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                        .join(" "),
                );
                Ok(())
            },
        )
    }

    #[test]
    fn reconnect_launches_append_only_for_running_vms() {
        // A VM that is not running writes nothing to its serial port, and a
        // truncating console would throw away the log of the boot that is still
        // the last thing that happened to it.
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let launcher = console_launcher(recorded.clone());
        let mut sessions = Com1Sessions::default();
        let known = [
            known("running", Some(HcsSystemState::Running)),
            known("created", Some(HcsSystemState::Created)),
            known("stopped", None),
        ];

        let failures = launch_running_consoles(
            &launcher,
            &mut sessions,
            &known,
            std::path::Path::new(r"C:\vms"),
        );

        let commands = recorded.lock().unwrap().clone();
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(commands.len(), 1);
        assert!(commands[0].contains("--mode append"), "{}", commands[0]);
        assert!(commands[0].contains("--vm-name running"), "{}", commands[0]);
        assert!(sessions.contains(known[0].mapping.vm_id));
        assert!(!sessions.contains(known[1].mapping.vm_id));
    }

    #[test]
    fn a_reconnect_that_cannot_open_a_console_leaves_the_guest_running() {
        // The VM is already up: refusing to keep it because its window could
        // not be restored would take a running guest down for a diagnostic.
        let launcher = Com1Launcher::for_test(
            std::path::PathBuf::from(r"C:\VMLord\vmlord-com1.exe"),
            |_command: &TerminalCommand| Err(std::io::Error::other("no terminal here")),
        );
        let mut sessions = Com1Sessions::default();
        let known = [known("running", Some(HcsSystemState::Running))];

        let failures = launch_running_consoles(
            &launcher,
            &mut sessions,
            &known,
            std::path::Path::new(r"C:\vms"),
        );

        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("running"), "{}", failures[0]);
        assert!(!sessions.contains(known[0].mapping.vm_id));
    }

    /// A repository whose consoles are recorded instead of opened.
    fn repository_with_console(recorded: Arc<Mutex<Vec<String>>>) -> HcsVmRepository {
        let mut repository = repository();
        repository.com1_launcher = console_launcher(recorded);
        repository
    }

    #[test]
    fn the_console_of_a_stopped_vm_is_refused_without_opening_a_window() {
        // There is no pipe to read: HCS destroys the compute system as the VM
        // stops, and a window opened on a name nothing serves would sit there
        // empty.
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let mut repository = repository_with_console(recorded.clone());
        let mapping = mapping(NetworkMode::None);

        let error = repository
            .open_console_in_state(&mapping, Some(HcsSystemState::Stopped))
            .expect_err("a stopped VM has no COM1 port");

        assert!(error.to_string().contains("stopped"), "{error}");
        assert!(error.to_string().contains("start it first"), "{error}");
        assert!(recorded.lock().unwrap().is_empty(), "nothing was launched");
        assert!(!repository.com1_sessions.contains(mapping.vm_id));
    }

    #[test]
    fn a_console_is_opened_appending_to_the_log_of_the_boot_it_joins() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let mut repository = repository_with_console(recorded.clone());
        let mapping = mapping(NetworkMode::None);

        repository
            .open_console_in_state(&mapping, Some(HcsSystemState::Running))
            .expect("a running VM has a pipe to read");

        let commands = recorded.lock().unwrap().clone();
        assert_eq!(commands.len(), 1);
        assert!(
            commands[0].contains("--mode append"),
            "the boost that is already running keeps its log: {}",
            commands[0]
        );
        assert!(commands[0].contains("--vm-name dev"), "{}", commands[0]);
        assert!(repository.com1_sessions.contains(mapping.vm_id));
    }

    #[test]
    fn a_second_console_for_the_same_vm_is_refused_rather_than_opened() {
        // Two readers on one pipe split the guest's output between two windows
        // and neither shows all of it.
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let mut repository = repository_with_console(recorded.clone());
        let mapping = mapping(NetworkMode::None);
        repository
            .open_console_in_state(&mapping, Some(HcsSystemState::Running))
            .expect("the first console opens");

        let error = repository
            .open_console_in_state(&mapping, Some(HcsSystemState::Running))
            .expect_err("the second is refused");

        assert!(error.to_string().contains("already has a COM1 console"));
        assert_eq!(
            recorded.lock().unwrap().len(),
            1,
            "the refusal opened no second window"
        );
        assert!(repository.com1_sessions.contains(mapping.vm_id));
    }

    #[test]
    fn a_console_whose_window_was_closed_can_be_opened_again() {
        // This is what the action exists for: the reader that is over is
        // forgotten, and the VM gets its console back.
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let mut repository = repository_with_console(recorded.clone());
        let mapping = mapping(NetworkMode::None);
        repository
            .open_console_in_state(&mapping, Some(HcsSystemState::Running))
            .expect("the first console opens");
        repository.com1_sessions.finish_for_test(mapping.vm_id);

        repository
            .open_console_in_state(&mapping, Some(HcsSystemState::Running))
            .expect("a closed console reopens");

        assert_eq!(recorded.lock().unwrap().len(), 2);
        assert!(repository.com1_sessions.contains(mapping.vm_id));
    }

    #[test]
    fn a_stop_opens_the_console_so_the_guest_is_watched_as_it_powers_off() {
        // Everything a shutdown does after HCS has delivered the request is
        // written to COM1 and nowhere else, so a stop with no window shows the
        // user nothing at all.
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let mut repository = repository_with_console(recorded.clone());
        let mapping = mapping(NetworkMode::None);

        repository.show_shutdown_console(&mapping);

        let commands = recorded.lock().unwrap().clone();
        assert_eq!(commands.len(), 1);
        assert!(
            commands[0].contains("--mode append"),
            "the boot that is ending keeps its log: {}",
            commands[0]
        );
        assert!(commands[0].contains("--vm-name dev"), "{}", commands[0]);
        assert!(repository.com1_sessions.contains(mapping.vm_id));
    }

    #[test]
    fn a_stop_leaves_a_console_that_is_already_open_alone() {
        // Two readers on one pipe split the guest's output between two windows,
        // which is exactly what a shutdown must not do to its own log.
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let mut repository = repository_with_console(recorded.clone());
        let mapping = mapping(NetworkMode::None);
        repository
            .open_console_in_state(&mapping, Some(HcsSystemState::Running))
            .expect("the console opens");

        repository.show_shutdown_console(&mapping);

        assert_eq!(
            recorded.lock().unwrap().len(),
            1,
            "the window already showing the guest is the one to watch it in"
        );
        assert!(repository.com1_sessions.contains(mapping.vm_id));
    }

    #[test]
    fn a_console_that_cannot_be_opened_does_not_keep_a_guest_running() {
        // The user asked for the VM to stop. A window is how they watch it, not
        // what they asked for.
        let mut repository = repository();
        repository.com1_launcher = Com1Launcher::for_test(
            std::path::PathBuf::from(r"C:\VMLord\vmlord-com1.exe"),
            |_command: &TerminalCommand| Err(std::io::Error::other("no terminal here")),
        );
        let mapping = mapping(NetworkMode::None);

        repository.show_shutdown_console(&mapping);

        assert!(!repository.com1_sessions.contains(mapping.vm_id));
        let diagnostics = repository.diagnostics.lock().unwrap().clone();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].level, DiagnosticLevel::Warning);
        assert!(
            diagnostics[0].message.contains("could not be opened"),
            "{}",
            diagnostics[0].message
        );
    }

    #[test]
    fn a_delivered_shutdown_request_gives_up_what_the_run_leaves_behind() {
        let mut repository = repository();
        let mapping = mapping(NetworkMode::None);
        repository
            .shutdowns
            .start(mapping.vm_id, &mapping.vm_name, || Ok(()))
            .expect("the request should be dispatched");

        repository.shutdowns.wait_until_answered();
        repository.finish_shutdowns();

        assert!(
            repository.diagnostics.lock().unwrap().is_empty(),
            "a request that went through is not news"
        );
        // The same VM can be asked again once its request is over: a guest that
        // ignored the first one is stopped by a second.
        repository
            .shutdowns
            .start(mapping.vm_id, &mapping.vm_name, || Ok(()))
            .expect("the VM is no longer being shut down");
        repository.shutdowns.join_all();
    }

    #[test]
    fn a_shutdown_request_that_failed_is_reported_rather_than_lost() {
        // The wait happens on a thread of its own, so the failure cannot come
        // back as the return value of the click that caused it.
        let mut repository = repository();
        let mapping = mapping(NetworkMode::None);
        repository
            .shutdowns
            .start(mapping.vm_id, &mapping.vm_name, || {
                Err(RepositoryError::new("injected shutdown failure"))
            })
            .expect("the request should be dispatched");

        repository.shutdowns.wait_until_answered();
        repository.finish_shutdowns();

        let diagnostics = repository.diagnostics.lock().unwrap().clone();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].level, DiagnosticLevel::Error);
        assert!(
            diagnostics[0].message.contains("injected shutdown failure")
                && diagnostics[0].message.contains("dev"),
            "{}",
            diagnostics[0].message
        );
    }

    /// A VM with SSH access, an address and a key: everything a session needs,
    /// so that a test can take one thing away at a time.
    fn ssh_mapping() -> VmComputeSystemMapping {
        VmComputeSystemMapping {
            ssh: Some(SshConfig {
                username: "machi".to_owned(),
                port: SshPort::DEFAULT,
                authentication: SshAuthentication::VmlordKey,
            }),
            ..mapping(NetworkMode::Nat)
        }
    }

    /// A repository whose SSH sessions are recorded instead of opened.
    fn repository_with_ssh(recorded: Arc<Mutex<Vec<String>>>) -> HcsVmRepository {
        let mut repository = repository();
        repository.ssh_launcher = Arc::new(SshLauncher::for_test(
            |_| Ok(Some("172.22.42.7".parse().unwrap())),
            |_, _, _| Ok(()),
            |_| true,
            move |command: &TerminalCommand| {
                recorded
                    .lock()
                    .unwrap()
                    .push(command.program.display().to_string());
                Ok(())
            },
        ));
        repository
    }

    #[test]
    fn an_ssh_session_into_a_stopped_vm_is_refused_without_opening_a_window() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let repository = repository_with_ssh(recorded.clone());

        let error = repository
            .open_ssh_in_state(&ssh_mapping(), Some(HcsSystemState::Stopped))
            .expect_err("a stopped VM has no guest to log into");

        assert!(error.to_string().contains("stopped"), "{error}");
        assert!(error.to_string().contains("start it first"), "{error}");
        assert!(recorded.lock().unwrap().is_empty(), "nothing was launched");
    }

    #[test]
    fn an_ssh_session_into_a_running_vm_opens_a_terminal() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let repository = repository_with_ssh(recorded.clone());

        repository
            .open_ssh_in_state(&ssh_mapping(), Some(HcsSystemState::Running))
            .expect("a running guest can be logged into");
        repository.ssh_launches.wait_until_opened();

        assert_eq!(recorded.lock().unwrap().clone(), ["wt.exe"]);
    }

    /// The call the UI makes has to come back before the guest has been
    /// reached: probing a VM that stopped answering costs seconds, and they
    /// used to be seconds the window did not repaint in.
    #[test]
    fn opening_a_session_returns_before_the_guest_is_reached() {
        let reached = Arc::new(Mutex::new(false));
        let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut repository = repository();
        repository.ssh_launcher = Arc::new(SshLauncher::for_test(
            |_| Ok(Some("172.22.42.7".parse().unwrap())),
            {
                let reached = Arc::clone(&reached);
                let held = Arc::clone(&release);
                move |_, _, _| {
                    while !held.load(std::sync::atomic::Ordering::Relaxed) {
                        std::thread::yield_now();
                    }
                    *reached.lock().unwrap() = true;
                    Ok(())
                }
            },
            |_| true,
            |_: &TerminalCommand| Ok(()),
        ));

        repository
            .open_ssh_in_state(&ssh_mapping(), Some(HcsSystemState::Running))
            .expect("a running guest can be logged into");

        assert!(
            !*reached.lock().unwrap(),
            "the caller waited for the probe it was supposed to hand off"
        );
        release.store(true, std::sync::atomic::Ordering::Relaxed);
        repository.ssh_launches.wait_until_opened();
        assert!(*reached.lock().unwrap());
    }

    /// A session that opened tells VMLord nothing afterwards, so the command it
    /// was opened with is what the log has to carry: the key, the known-hosts
    /// file and the port that were actually asked for.
    #[test]
    fn an_opened_session_leaves_its_command_in_the_diagnostics() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let mut repository = repository_with_ssh(recorded);

        repository
            .open_ssh_in_state(&ssh_mapping(), Some(HcsSystemState::Running))
            .expect("a running guest can be logged into");
        repository.ssh_launches.wait_until_opened();

        let diagnostics = repository.take_diagnostics();
        let logged = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.message.contains("ssh.exe"))
            .expect("the command has to be there to be read");

        assert_eq!(logged.level, DiagnosticLevel::Info);
        assert!(logged.message.contains("dev"), "{}", logged.message);
        assert!(logged.message.contains("-l machi"), "{}", logged.message);
        assert!(logged.message.contains("-p 22"), "{}", logged.message);
        assert!(logged.message.contains("172.22.42.7"), "{}", logged.message);
        assert!(logged.message.contains("known_hosts"), "{}", logged.message);
    }

    /// Two shells into one guest is an ordinary thing to want, and the
    /// repository keeps nothing that would make the second one a collision.
    #[test]
    fn a_running_vm_may_be_logged_into_more_than_once() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let repository = repository_with_ssh(recorded.clone());
        let mapping = ssh_mapping();

        for _ in 0..2 {
            repository
                .open_ssh_in_state(&mapping, Some(HcsSystemState::Running))
                .expect("a second shell is not a second reader on one pipe");
        }
        repository.ssh_launches.wait_until_opened();

        assert_eq!(recorded.lock().unwrap().len(), 2);
    }

    /// The preflight checks exist to be reported: what the person sees has to
    /// name the thing that stopped the session, not that one did not open.
    ///
    /// It reaches them as a diagnostic rather than as this call's answer,
    /// because the check runs after the call has already returned -- which is
    /// what keeps the window painting. The UI reads the same buffer either way.
    #[test]
    fn a_preflight_failure_reaches_the_diagnostics_with_its_own_reason() {
        let mut repository = repository();
        repository.ssh_launcher = Arc::new(SshLauncher::for_test(
            |_| Ok(Some("172.22.42.7".parse().unwrap())),
            |_, _, _| Err("connection refused".to_owned()),
            |_| panic!("a guest that does not answer is not asked for its key"),
            |_| panic!("nor given a terminal"),
        ));

        repository
            .open_ssh_in_state(&ssh_mapping(), Some(HcsSystemState::Running))
            .expect("the attempt is made in the background, so it is accepted");
        repository.ssh_launches.wait_until_opened();

        let diagnostics = repository.take_diagnostics();
        let reported = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.level == DiagnosticLevel::Error)
            .expect("a refused session has to be reported");

        assert!(reported.message.contains("dev"), "{reported:?}");
        assert!(reported.message.contains("port 22"), "{reported:?}");
        assert!(
            reported.message.contains("connection refused"),
            "{reported:?}"
        );
    }

    #[test]
    fn a_failed_finished_reader_becomes_a_repository_diagnostic() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let launcher = console_launcher(recorded);
        let mut sessions = Com1Sessions::default();
        let mapping = known("dev", Some(HcsSystemState::Running)).mapping;
        let session = launcher
            .launch(
                &mapping,
                std::path::Path::new(r"C:\vms\dev"),
                Com1LogMode::Append,
            )
            .unwrap();
        session.fail_for_test();
        sessions.insert(session);

        let diagnostics =
            console_failure_diagnostics(&mut sessions, std::path::Path::new(r"C:\vms"));

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].level, DiagnosticLevel::Error);
        assert!(diagnostics[0].message.contains("COM1"), "{diagnostics:?}");
        assert!(diagnostics[0].message.contains("dev"), "{diagnostics:?}");
        assert!(
            diagnostics[0].message.contains("com1.log"),
            "a person asked about a reader that stopped needs the file to look in: {diagnostics:?}"
        );
        assert!(!sessions.contains(mapping.vm_id));
    }

    #[test]
    fn a_reader_that_finished_with_its_pipe_is_no_diagnostic() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let launcher = console_launcher(recorded);
        let mut sessions = Com1Sessions::default();
        let mapping = known("dev", Some(HcsSystemState::Running)).mapping;
        let session = launcher
            .launch(
                &mapping,
                std::path::Path::new(r"C:\vms\dev"),
                Com1LogMode::Append,
            )
            .unwrap();
        session.finish_for_test();
        sessions.insert(session);

        let diagnostics =
            console_failure_diagnostics(&mut sessions, std::path::Path::new(r"C:\vms"));

        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(!sessions.contains(mapping.vm_id));
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

    /// A repository with one stopped VM and a start held on a flag.
    ///
    /// The start is a substitute rather than the real pipeline: what these
    /// tests are about is what the rest of VMLord does while a start is in
    /// flight, and a real one would need a compute system.
    ///
    /// The flag is released by [`Held::release`] before anything is asserted,
    /// because dropping the repository joins the start thread -- a failed
    /// assertion with the flag still set would hang the suite instead of
    /// failing it.
    struct Held {
        root: std::path::PathBuf,
        repository: HcsVmRepository,
        release: Arc<AtomicBool>,
    }

    impl Held {
        fn new(label: &str) -> Self {
            let (root, store) = temp_store(label);
            store
                .insert(mapping(NetworkMode::None))
                .expect("the mapping should be stored");
            let mut repository = repository();
            repository.store = store;
            repository.initialized = true;
            let release = Arc::new(AtomicBool::new(false));
            let held = Arc::clone(&release);
            repository
                .starts
                .start("dev", move || {
                    while !held.load(std::sync::atomic::Ordering::Relaxed) {
                        std::thread::yield_now();
                    }
                    None
                })
                .expect("the start should be accepted");
            Self {
                root,
                repository,
                release,
            }
        }

        fn release(&self) {
            self.release
                .store(true, std::sync::atomic::Ordering::Relaxed);
            self.repository.starts.join_all();
        }
    }

    impl Drop for Held {
        fn drop(&mut self) {
            self.release
                .store(true, std::sync::atomic::Ordering::Relaxed);
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn a_vm_whose_start_is_in_flight_is_listed_as_starting() {
        let held = Held::new("start-listed");

        let summary = held.repository.summary(KnownVm {
            mapping: mapping(NetworkMode::None),
            state: None,
            runtime_id: None,
        });
        held.release();

        assert_eq!(
            summary.state,
            VmState::Starting,
            "a VM being prepared is not a stopped VM offering another start"
        );
    }

    #[test]
    fn a_vm_that_is_already_running_is_not_relisted_as_starting() {
        let held = Held::new("start-running");

        let summary = held.repository.summary(KnownVm {
            mapping: mapping(NetworkMode::None),
            state: Some(HcsSystemState::Running),
            runtime_id: None,
        });
        held.release();

        assert!(
            matches!(summary.state, VmState::Running { .. }),
            "HCS reports it running, which is the later fact: {:?}",
            summary.state
        );
    }

    #[test]
    fn a_vm_being_started_may_not_be_deleted() {
        let mut held = Held::new("start-delete");

        let outcome = held.repository.delete_vm(VmDeleteRequest {
            name: "dev".into(),
            delete_disks: true,
        });
        held.release();

        let error = outcome.expect_err("a VM in the middle of starting is not one to remove");
        assert!(error.to_string().contains("starting"), "{error}");
    }

    #[test]
    fn a_vm_being_started_may_not_be_edited() {
        let mut held = Held::new("start-update");

        let outcome = held.repository.update_vm(update_request());
        held.release();

        let error = outcome.expect_err("the configuration a start is reading must not move");
        assert!(error.to_string().contains("starting"), "{error}");
    }

    #[test]
    fn a_vm_being_started_may_not_be_started_again() {
        let mut held = Held::new("start-twice");

        let outcome = held.repository.start_vm("dev");
        held.release();

        let error =
            outcome.expect_err("two threads must not prepare the same configuration at once");
        assert!(error.to_string().contains("starting"), "{error}");
    }

    #[test]
    fn a_start_of_a_vm_vmlord_does_not_know_is_refused_before_any_thread() {
        let (root, store) = temp_store("start-unknown");
        let mut repository = repository();
        repository.store = store;
        repository.initialized = true;

        let error = repository
            .start_vm("nothing-of-this-name")
            .expect_err("an unknown VM is the return value of the call that asked for it");

        assert!(
            error.to_string().contains("nothing-of-this-name"),
            "{error}"
        );
        assert!(
            !repository.starts.contains("nothing-of-this-name"),
            "a refusal starts no thread"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_changed_gpu_mode_is_recorded_in_the_mapping() {
        let (root, store) = temp_store("gpu-mode-changed");
        let mapping = mapping(NetworkMode::None);
        store.insert(mapping.clone()).unwrap();

        record_gpu_mode(&store, &mapping, GpuMode::Mirror).unwrap();

        let stored = store.find_by_vm_name("dev").unwrap().unwrap();
        assert_eq!(stored.gpu_mode, GpuMode::Mirror);
        // Nothing else about the VM may move with its GPU mode.
        assert_eq!(stored.vm_id, mapping.vm_id);
        assert_eq!(stored.network_mode, mapping.network_mode);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn an_unchanged_gpu_mode_leaves_the_mapping_alone() {
        let (root, store) = temp_store("gpu-mode-unchanged");
        let mapping = mapping(NetworkMode::Nat);
        store.insert(mapping.clone()).unwrap();

        record_gpu_mode(&store, &mapping, GpuMode::None).unwrap();

        assert_eq!(store.find_by_vm_name("dev").unwrap().unwrap(), mapping);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_stopped_vm_may_change_its_gpu_mode() {
        refuse_gpu_mode_change("dev", GpuMode::Mirror, GpuMode::None, None)
            .expect("a stopped VM gets its new mode on its next start");
    }

    #[test]
    fn a_running_vm_may_not_change_its_gpu_mode() {
        let error = refuse_gpu_mode_change("dev", GpuMode::Mirror, GpuMode::None, Some("running"))
            .expect_err("the mode is applied at start, so it may not change under a running VM");

        assert!(
            error.to_string().contains("stop it"),
            "the refusal has to say what to do about it: {error}"
        );
    }

    #[test]
    fn a_running_vm_may_still_be_edited_as_long_as_its_gpu_mode_stands() {
        refuse_gpu_mode_change("dev", GpuMode::Mirror, GpuMode::Mirror, Some("running"))
            .expect("only the GPU mode is frozen while a VM runs, not the whole form");
    }

    #[test]
    fn a_summary_carries_what_was_observed_about_the_vms_gpu() {
        let repository = repository();
        let mapping = VmComputeSystemMapping {
            gpu_mode: GpuMode::Default,
            ..mapping(NetworkMode::None)
        };
        repository.gpu_runs.record_guest(
            mapping.vm_id,
            GuestGpuReport::Ready(GuestGpuDetail {
                driver: Some("dxgkrnl".into()),
                render_node: Some("/dev/dri/renderD128".into()),
            }),
        );

        let summary = repository.summary(KnownVm {
            mapping,
            state: None,
            runtime_id: None,
        });

        assert!(
            matches!(summary.gpu.guest, Some(GuestGpuReport::Ready(_))),
            "the facts a session recorded have to reach the list that reads them"
        );
    }

    #[test]
    fn a_summary_of_a_vm_nothing_was_observed_about_carries_nothing() {
        let repository = repository();

        let summary = repository.summary(KnownVm {
            mapping: mapping(NetworkMode::None),
            state: None,
            runtime_id: None,
        });

        assert_eq!(summary.gpu, vmlord_core::VmGpuFacts::default());
    }

    #[test]
    fn a_run_that_is_over_leaves_no_gpu_facts_behind() {
        let repository = repository();
        let mapping = mapping(NetworkMode::None);
        repository
            .gpu_runs
            .record_assignment(mapping.vm_id, GpuAssignment::Unknown);

        repository.gpu_runs.forget(mapping.vm_id);

        let summary = repository.summary(KnownVm {
            mapping,
            state: None,
            runtime_id: None,
        });
        assert_eq!(summary.gpu.assignment, None);
    }

    #[test]
    fn a_summary_reports_the_gpu_mode_the_mapping_records() {
        // The edit form is filled from `VmSummary`, so a summary that always
        // said `None` would make an unrelated edit switch the GPU off.
        let repository = repository();

        let summary = repository.summary(KnownVm {
            mapping: VmComputeSystemMapping {
                gpu_mode: GpuMode::Mirror,
                ..mapping(NetworkMode::None)
            },
            state: None,
            runtime_id: None,
        });

        assert_eq!(summary.gpu_mode, GpuMode::Mirror);
    }

    #[test]
    fn a_summary_reports_the_network_mode_the_mapping_records() {
        // The edit form is filled from `VmSummary`, so a summary that always
        // said `None` would make an unrelated edit switch NAT off.
        let repository = repository();

        let summary = repository.summary(KnownVm {
            mapping: mapping(NetworkMode::Nat),
            state: None,
            runtime_id: None,
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
            runtime_id: None,
        });

        assert_eq!(summary.state, VmState::Stopped);
        assert_eq!(summary.ip_address, None);
    }

    #[test]
    fn a_running_vm_nobody_listens_for_has_an_agent_of_unknown_status() {
        // Not offline: an agent that was never offered a socket -- a VM whose
        // partition HCS would not name, one created before VMs had a secret --
        // has not failed to connect.
        let repository = repository();

        let summary = repository.summary(KnownVm {
            mapping: mapping(NetworkMode::None),
            state: Some(HcsSystemState::Running),
            runtime_id: Some(Uuid::new_v4()),
        });

        assert_eq!(
            summary.state,
            VmState::Running {
                agent_status: AgentStatus::Unknown
            }
        );
    }

    #[test]
    fn a_listener_reports_its_agent_as_offline_until_a_session_opens() {
        let mut repository = repository();
        let mapping = mapping(NetworkMode::None);
        repository
            .agent_sessions
            .insert(AgentConnection::for_test(mapping.vm_id, false));

        let waiting = repository.summary(KnownVm {
            mapping: mapping.clone(),
            state: Some(HcsSystemState::Running),
            runtime_id: Some(Uuid::new_v4()),
        });
        repository
            .agent_sessions
            .insert(AgentConnection::for_test(mapping.vm_id, true));
        let connected = repository.summary(KnownVm {
            mapping,
            state: Some(HcsSystemState::Running),
            runtime_id: Some(Uuid::new_v4()),
        });

        assert_eq!(
            waiting.state,
            VmState::Running {
                agent_status: AgentStatus::Offline
            }
        );
        assert_eq!(
            connected.state,
            VmState::Running {
                agent_status: AgentStatus::Online
            }
        );
    }

    #[test]
    fn a_running_vm_without_an_endpoint_is_listed_without_an_address() {
        // A VM that has never started on the shared network has no endpoint to
        // read an address from, and one that asked for no network never will.
        let repository = repository();

        let summary = repository.summary(KnownVm {
            mapping: mapping(NetworkMode::None),
            state: Some(HcsSystemState::Running),
            runtime_id: Some(Uuid::new_v4()),
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
        assert_not_initialized(repository.open_console("dev"));
        assert_not_initialized(repository.open_ssh("dev"));
    }

    #[test]
    fn a_display_connection_reports_that_the_native_backend_lacks_it() {
        let mut repository = repository();

        assert!(
            repository
                .open_display("dev")
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
