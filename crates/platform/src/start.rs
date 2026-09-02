//! Starting an HCS-backed virtual machine.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::Duration,
};

use uuid::Uuid;
use vmlord_core::{GpuAssignment, GpuFailure, GpuMode, NetworkMode, RepositoryError};

use crate::{
    Com1LogMode, HcsClient, HcsSystem, cleanup,
    com1_terminal::{Com1Launcher, Com1Session},
    dhcp::{self, DhcpRegistrar},
    display_exports::DisplayExport,
    display_prepare::{self, PreparedDisplay},
    display_runs::DisplayRuns,
    gpu_assignment,
    gpu_exports::GpuExports,
    gpu_prepare::{self, PreparedGpu},
    gpu_runs::GpuRuns,
    hcn::HcnNetwork,
    hcn_endpoint::{EndpointAddress, HcnEndpoint},
    hcs::{HCS_ACCESS_ALL, HcsStartFailure, HcsSystemState},
    hcs_config::{self, Plan9Export},
    layout,
    metadata::{MetadataStore, VmComputeSystemMapping},
    tools_volume,
};

/// A start operation completes once HCS has handed the VM to its worker
/// process, well before the guest OS has booted; the generous bound only
/// guards against a wedged Host Compute Service.
const START_TIMEOUT: Duration = Duration::from_secs(60);

/// Bounds the re-creation of a compute system HCS no longer knows; it is the
/// same operation `VmCreationPipeline` waits on.
const CREATE_TIMEOUT: Duration = Duration::from_secs(60);

/// What a VM needs written into its configuration to reach the network.
pub(crate) struct VmNetworkAdapter {
    pub(crate) endpoint_id: Uuid,
    pub(crate) mac_address: String,
    /// The address HNS assigned to the endpoint, which the DHCP server is what
    /// actually delivers to the guest.
    pub(crate) address: Option<EndpointAddress>,
}

/// Whether a start may reuse the endpoint the VM already has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EndpointPolicy {
    /// Reuse the recorded endpoint, creating one only when the VM has none.
    Reuse,
    /// Replace the recorded endpoint: HNS still has it attached to a compute
    /// system that no longer exists, so nothing can attach to it again.
    Replace,
}

type AccessGranter = Box<dyn Fn(&str, &Path) -> Result<(), RepositoryError> + Send + Sync>;
/// Brings the compute system into the shape `configuration` describes, without
/// running it.
type SystemPreparer = Box<dyn Fn(&str, &str) -> Result<(), HcsStartFailure> + Send + Sync>;
/// Starts a compute system a preparer has already brought into shape.
type SystemStarter = Box<dyn Fn(&str) -> Result<(), HcsStartFailure> + Send + Sync>;
type EndpointProvider = Box<
    dyn Fn(&str, Option<Uuid>, EndpointPolicy) -> Result<VmNetworkAdapter, RepositoryError>
        + Send
        + Sync,
>;

/// Prepares a VM's display payload before its compute system is built.
type DisplayPreparer =
    Box<dyn Fn(&VmComputeSystemMapping, &Path) -> Option<PreparedDisplay> + Send + Sync>;
/// Prepares a VM's GPU before its compute system is built.
type GpuPreparer = Box<dyn Fn(&VmComputeSystemMapping, &Path) -> Option<PreparedGpu> + Send + Sync>;
/// Attaches the adapters a mode asks for to a compute system that is running.
type GpuAssigner = Box<dyn Fn(&str, GpuMode) -> Result<(), GpuFailure> + Send + Sync>;

/// Starts VMs created by [`crate::VmCreationPipeline`].
pub struct VmStartPipeline {
    com1: Com1Launcher,
    access_granter: AccessGranter,
    system_preparer: SystemPreparer,
    system_starter: SystemStarter,
    endpoint_provider: EndpointProvider,
    dhcp_registrar: DhcpRegistrar,
    gpu_preparer: GpuPreparer,
    display_preparer: DisplayPreparer,
    gpu_assigner: GpuAssigner,
    /// Where what a start observes about a VM's GPU is recorded, and where the
    /// manifest its agent will be offered is left.
    gpu_runs: GpuRuns,
    /// The same, for the display payload: what was staged, and what could not
    /// be.
    display_runs: DisplayRuns,
}

impl VmStartPipeline {
    /// Creates a pipeline backed by the real HCS and HNS APIs.
    ///
    /// The launcher is passed in rather than made here: the repository owns the
    /// sessions this pipeline opens, and both halves have to be the same one.
    #[must_use]
    pub fn production(com1: Com1Launcher) -> Self {
        Self {
            com1,
            access_granter: Box::new(grant_vm_access),
            system_preparer: Box::new(prepare_hcs_system),
            system_starter: Box::new(start_hcs_system),
            endpoint_provider: Box::new(ensure_endpoint),
            dhcp_registrar: dhcp::registrar(),
            // A pipeline nobody has given a place to record GPU in starts VMs
            // without one. `for_vms_under` is what the application builds.
            gpu_preparer: Box::new(|_mapping, _vm_directory| None),
            // Likewise: a pipeline with nowhere to stage a display payload
            // starts VMs whose guests find none.
            display_preparer: Box::new(|_mapping, _vm_directory| None),
            gpu_assigner: Box::new(gpu_assignment::assign_to_system),
            gpu_runs: GpuRuns::default(),
            display_runs: DisplayRuns::default(),
        }
    }

    /// The same pipeline, applying GPU-PV to the VMs under `storage_root`.
    ///
    /// Separate from [`Self::production`] because what a start observes about
    /// a GPU has to be recorded where the list of VMs reads it, and only the
    /// repository owns that. A pipeline built without this one still starts
    /// VMs; it simply attaches no GPU to them.
    #[must_use]
    pub(crate) fn for_vms_under(
        mut self,
        storage_root: &Path,
        gpu_runs: GpuRuns,
        display_runs: DisplayRuns,
    ) -> Self {
        // Read once, here rather than per start: the executable does not move
        // while VMLord runs, and a start that could not name its own directory
        // is a start with no payload rather than one that fails.
        let executable_directory = std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(Path::to_path_buf))
            .unwrap_or_default();
        let cache_root = layout::gpu_payload_cache_root(storage_root);

        self.gpu_preparer = Box::new(move |mapping, vm_directory| {
            gpu_prepare::prepare(
                mapping,
                vm_directory,
                &executable_directory,
                &cache_root,
                // Nothing cancels a start: HCS has either been asked to run the
                // system or has not, and a half-staged payload is a share that
                // is simply not offered.
                &AtomicBool::new(false),
            )
        });
        let display_executable = executable_directory_for_display();
        let display_cache_root = layout::payload_cache_root(storage_root);
        self.display_preparer = Box::new(move |mapping, vm_directory| {
            display_prepare::prepare(
                mapping,
                vm_directory,
                &display_executable,
                &display_cache_root,
                &canonicalize_for_export,
            )
        });
        self.gpu_runs = gpu_runs;
        self.display_runs = display_runs;
        self
    }

    #[cfg(test)]
    fn for_test(
        com1: Com1Launcher,
        access_granter: impl Fn(&str, &Path) -> Result<(), RepositoryError> + Send + Sync + 'static,
        system_preparer: impl Fn(&str, &str) -> Result<(), HcsStartFailure> + Send + Sync + 'static,
        system_starter: impl Fn(&str) -> Result<(), HcsStartFailure> + Send + Sync + 'static,
        endpoint_provider: impl Fn(
            &str,
            Option<Uuid>,
            EndpointPolicy,
        ) -> Result<VmNetworkAdapter, RepositoryError>
        + Send
        + Sync
        + 'static,
        dhcp_registrar: impl Fn(&MetadataStore, &str, &EndpointAddress) -> Result<(), RepositoryError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            com1,
            access_granter: Box::new(access_granter),
            system_preparer: Box::new(system_preparer),
            system_starter: Box::new(system_starter),
            endpoint_provider: Box::new(endpoint_provider),
            dhcp_registrar: Box::new(dhcp_registrar),
            // No display payload either: a test of the start itself is a test
            // of the start itself.
            display_preparer: Box::new(|_mapping, _vm_directory| None),
            // A VM with no GPU is what every test of the start itself is
            // about; the GPU steps have tests of their own below.
            gpu_preparer: Box::new(|_mapping, _vm_directory| None),
            gpu_assigner: Box::new(|_hcs_id, _mode| Ok(())),
            gpu_runs: GpuRuns::default(),
            display_runs: DisplayRuns::default(),
        }
    }

    /// The same pipeline with its GPU steps substituted.
    /// The same pipeline, preparing a display payload the caller supplies.
    ///
    /// Its own builder rather than an argument to `with_gpu`: a test of the
    /// display half must be able to say nothing about the GPU, which is what
    /// every VM in this crate's tests does.
    #[cfg(test)]
    fn with_display(
        mut self,
        display_preparer: impl Fn(&VmComputeSystemMapping, &Path) -> Option<PreparedDisplay>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        self.display_preparer = Box::new(display_preparer);
        self
    }

    #[cfg(test)]
    fn with_gpu(
        mut self,
        gpu_preparer: impl Fn(&VmComputeSystemMapping, &Path) -> Option<PreparedGpu>
        + Send
        + Sync
        + 'static,
        gpu_assigner: impl Fn(&str, GpuMode) -> Result<(), GpuFailure> + Send + Sync + 'static,
        gpu_runs: GpuRuns,
    ) -> Self {
        self.gpu_preparer = Box::new(gpu_preparer);
        self.gpu_assigner = Box::new(gpu_assigner);
        self.gpu_runs = gpu_runs;
        self
    }

    /// Starts the VM named `vm_name`, whose configuration lives under
    /// `vm_directory`.
    ///
    /// Every file the configuration attaches is re-granted to the VM's
    /// security principal first: Hyper-V opens those files as the VM itself,
    /// so a start without the grant fails with `ERROR_ACCESS_DENIED` even
    /// when the calling (elevated) process can read them.
    ///
    /// The stored `config.json` is also what a VM whose compute system HCS no
    /// longer knows is rebuilt from, so a start after a stop needs no other
    /// state than what creation persisted.
    ///
    /// A VM asking for [`NetworkMode::Nat`] is given its endpoint here, before
    /// anything is started: the network and the endpoint have to exist and be
    /// written into the configuration for the VM to come up with an adapter at
    /// all. Failing to provide either fails the start rather than quietly
    /// bringing the VM up without a network.
    ///
    /// The VM's COM1 console is opened before any of that, and the session that
    /// owns it is what a successful start returns: a VM whose diagnostics could
    /// not be captured is not started at all, because the output that explains
    /// a failed boot is written in the first seconds of one. Every failure
    /// after the launch drops the session, which tells the reader to stop.
    pub fn start(
        &self,
        store: &MetadataStore,
        vm_name: &str,
        vm_directory: &Path,
    ) -> Result<Com1Session, RepositoryError> {
        let mapping = store.find_by_vm_name(vm_name)?.ok_or_else(|| {
            let error = RepositoryError::new(format!("no HCS mapping found for VM \"{vm_name}\""));
            tracing::error!("{error}");
            error
        })?;

        tracing::info!(
            "starting VM \"{}\" ({}) as HCS compute system \"{}\"",
            mapping.vm_name,
            mapping.vm_id,
            mapping.hcs_compute_system_id
        );

        // After reading the configuration, so that a VM whose stored state is
        // unusable never opens a window for a start that cannot happen.
        let stored = self.read_configuration(&mapping, vm_directory)?;

        // While the VM is still down, which is the only time a volume attached
        // to it can be written. What the guest does with it happens on the
        // boot this start is about to begin.
        tools_volume::refresh(&mapping.vm_name, vm_directory);

        // Before anything is granted or built: the shares below become part of
        // the compute system, and a system's Plan9 section is fixed for the
        // lifetime of a boot. Prepared once even though the start below may be
        // retried -- staging, enumeration and assignment are never repeated.
        let prepared = self.prepare_gpu(&mapping, vm_directory);
        let display = self.prepare_display(&mapping, vm_directory);

        let (configuration, endpoint) = self.attach_network(
            store,
            &mapping,
            vm_directory,
            stored.clone(),
            mapping.endpoint_id,
            EndpointPolicy::Reuse,
        )?;
        let configuration = with_plan9_shares(
            &mapping,
            configuration,
            prepared.as_ref(),
            display.as_ref().and_then(|display| display.export.as_ref()),
        );
        self.grant_access_to_attachments(&mapping, &configuration)?;

        let failure = match self.open_console_and_start(&mapping, vm_directory, &configuration) {
            Ok(session) => {
                tracing::info!("started VM \"{}\" ({})", mapping.vm_name, mapping.vm_id);
                self.attach_gpu(&mapping, prepared.as_ref());
                return Ok(session);
            }
            Err(failure) => failure,
        };

        // Only one failure is recoverable, and only for a VM that has an
        // endpoint to replace.
        let busy = match failure {
            HcsStartFailure::EndpointBusy(error) => error,
            HcsStartFailure::Failed(error) => {
                tracing::error!("failed to start VM \"{}\": {error}", mapping.vm_name);
                return Err(error);
            }
        };
        let Some(endpoint) = endpoint else {
            tracing::error!("failed to start VM \"{}\": {busy}", mapping.vm_name);
            return Err(busy);
        };

        tracing::warn!(
            "VM \"{}\" could not start because HNS still has endpoint {endpoint} attached to a \
             compute system that no longer exists: {busy}; replacing the endpoint and retrying \
             the start once",
            mapping.vm_name
        );

        let (configuration, _) = self.attach_network(
            store,
            &mapping,
            vm_directory,
            stored,
            Some(endpoint),
            EndpointPolicy::Replace,
        )?;
        let configuration = with_plan9_shares(
            &mapping,
            configuration,
            prepared.as_ref(),
            display.as_ref().and_then(|display| display.export.as_ref()),
        );
        self.grant_access_to_attachments(&mapping, &configuration)?;
        let session = self
            .open_console_and_start(&mapping, vm_directory, &configuration)
            .map_err(|failure| {
                let error = failure.into_error();
                tracing::error!("failed to start VM \"{}\": {error}", mapping.vm_name);
                error
            })?;

        tracing::info!("started VM \"{}\" ({})", mapping.vm_name, mapping.vm_id);
        self.attach_gpu(&mapping, prepared.as_ref());
        Ok(session)
    }

    /// Works out what this VM's GPU can be, and records what was found.
    ///
    /// The fact is recorded before the VM starts, so that a start which then
    /// fails still leaves an honest answer behind rather than the silence of a
    /// GPU nobody looked at.
    fn prepare_gpu(
        &self,
        mapping: &VmComputeSystemMapping,
        vm_directory: &Path,
    ) -> Option<PreparedGpu> {
        let prepared = (self.gpu_preparer)(mapping, vm_directory)?;
        self.gpu_runs
            .record_assignment(mapping.vm_id, prepared.assignment.clone());
        self.gpu_runs
            .record_shares(mapping.vm_id, prepared.manifest.clone());
        Some(prepared)
    }

    /// Stages this VM's display payload and records what that came to.
    ///
    /// Recorded here rather than by the preparer, for the reason the GPU's
    /// assignment is: the registry is what the list of VMs reads, and a fact
    /// that stayed on this thread would be a fact only the log ever saw.
    fn prepare_display(
        &self,
        mapping: &VmComputeSystemMapping,
        vm_directory: &Path,
    ) -> Option<PreparedDisplay> {
        let prepared = (self.display_preparer)(mapping, vm_directory)?;
        self.display_runs.record_host(
            mapping.vm_id,
            prepared.available_version.clone(),
            prepared.failure.clone(),
        );
        if let Some(export) = prepared.export.as_ref() {
            self.display_runs
                .record_share(mapping.vm_id, export.share().clone());
        }
        Some(prepared)
    }

    /// Attaches the adapters the VM's mode asks for, once, best effort.
    ///
    /// Called only after the system is running, because assignment modifies a
    /// live compute system. A failure replaces the fact recorded before the
    /// start and changes nothing else: the VM is running, and GPU never
    /// decides that.
    ///
    /// Not retried, here or anywhere: a second attempt at a modify HCS refused
    /// is a second refusal, and a loop around it is how a VM spends its life
    /// asking for a GPU it will not get.
    fn attach_gpu(&self, mapping: &VmComputeSystemMapping, prepared: Option<&PreparedGpu>) {
        let Some(prepared) = prepared else {
            return;
        };
        // Nothing could be handed over at all, and the reason is already
        // recorded. Attaching adapters whose drivers the guest cannot reach
        // would not make that less true.
        if matches!(prepared.assignment, GpuAssignment::Failed(_)) {
            tracing::info!(
                "VM \"{}\" is not asked to attach any GPU adapter, because none could be \
                 handed to it",
                mapping.vm_name
            );
            return;
        }

        match (self.gpu_assigner)(&mapping.hcs_compute_system_id, mapping.gpu_mode) {
            Ok(()) => tracing::info!(
                "VM \"{}\" has its GPU attached in mode {:?}",
                mapping.vm_name,
                mapping.gpu_mode
            ),
            Err(failure) => {
                tracing::warn!(
                    "VM \"{}\" is running without the GPU it asked for: {}",
                    mapping.vm_name,
                    failure.message
                );
                self.gpu_runs
                    .record_assignment(mapping.vm_id, GpuAssignment::Failed(failure));
            }
        }
    }

    /// Brings the compute system into shape, opens the VM's console, and only
    /// then starts it.
    ///
    /// The order is the whole point. Preparing may destroy and re-create the
    /// compute system, which destroys the named pipe its COM1 is served
    /// through -- a console opened before that would be reading a pipe that
    /// stops existing a moment later, which is exactly what it looks like: an
    /// empty `com1.log` and a terminal window that closes itself. Opening the
    /// console after the system is in its final shape and before it executes
    /// anything keeps the guarantee that matters: no VM runs a single
    /// instruction unobserved.
    ///
    /// The session is returned rather than stored, and dropping it is what
    /// tells the reader to stop, so a failed start takes its console with it --
    /// including the retry, which prepares the system again.
    fn open_console_and_start(
        &self,
        mapping: &VmComputeSystemMapping,
        vm_directory: &Path,
        configuration: &str,
    ) -> Result<Com1Session, HcsStartFailure> {
        (self.system_preparer)(&mapping.hcs_compute_system_id, configuration)?;
        let session = self
            .com1
            .launch(mapping, vm_directory, Com1LogMode::Truncate)
            .map_err(HcsStartFailure::Failed)?;
        (self.system_starter)(&mapping.hcs_compute_system_id)?;
        Ok(session)
    }

    /// Gives the VM its endpoint and writes the adapter into `configuration`,
    /// returning the updated document and the endpoint the VM will start on.
    ///
    /// A VM that asked for no network is left off HNS entirely and reports no
    /// endpoint; the adapter an earlier start may have written is removed from
    /// its configuration.
    ///
    /// `recorded` is the endpoint to reuse or replace, which is not always the
    /// one the mapping was read with: a retry replaces the endpoint the failed
    /// attempt actually used.
    ///
    /// Neither the endpoint nor the recorded `endpoint_id` is undone when a
    /// later step fails: the endpoint outlives stops and lives until the VM is
    /// deleted, and dropping it after a failed start would hand the guest a new
    /// address on the next attempt.
    fn attach_network(
        &self,
        store: &MetadataStore,
        mapping: &VmComputeSystemMapping,
        vm_directory: &Path,
        configuration: String,
        recorded: Option<Uuid>,
        policy: EndpointPolicy,
    ) -> Result<(String, Option<Uuid>), RepositoryError> {
        if mapping.network_mode != NetworkMode::Nat {
            tracing::debug!(
                "VM \"{}\" asks for {:?} networking; starting it without an endpoint",
                mapping.vm_name,
                mapping.network_mode
            );
            // A VM edited off the network still has the adapter an earlier
            // start wrote; without this it would come up on the network it was
            // just taken off. The endpoint itself stays in HNS until the VM is
            // deleted, so switching back to NAT keeps the guest's address.
            let updated = hcs_config::remove_network_adapter(&configuration)?;
            if updated != configuration {
                self.write_configuration(mapping, vm_directory, &updated)?;
                tracing::info!(
                    "VM \"{}\" ({}) no longer asks for a network; its adapter was removed \
                     from the stored configuration",
                    mapping.vm_name,
                    mapping.vm_id
                );
            }
            return Ok((updated, None));
        }

        let adapter = (self.endpoint_provider)(&mapping.vm_name, recorded, policy)?;
        if recorded != Some(adapter.endpoint_id) {
            store.insert(VmComputeSystemMapping {
                endpoint_id: Some(adapter.endpoint_id),
                ..mapping.clone()
            })?;
        }

        let updated = hcs_config::apply_network_adapter(
            &configuration,
            adapter.endpoint_id,
            &adapter.mac_address,
        )?;
        if updated != configuration {
            self.write_configuration(mapping, vm_directory, &updated)?;
        }

        // HNS NAT does not answer the guest's DHCP, so an endpoint whose
        // address nothing serves leaves the guest with an adapter and no
        // configuration at all -- which is exactly what asking for a network
        // was meant to avoid.
        let Some(address) = adapter.address.as_ref() else {
            let error = RepositoryError::new(format!(
                "HNS reports no address for endpoint {} of VM \"{}\", so the guest cannot be \
                 told one over DHCP",
                adapter.endpoint_id, mapping.vm_name
            ));
            tracing::error!("{error}");
            return Err(error);
        };
        (self.dhcp_registrar)(store, &adapter.mac_address, address)?;

        tracing::info!(
            "VM \"{}\" ({}) starts on endpoint {}",
            mapping.vm_name,
            mapping.vm_id,
            adapter.endpoint_id
        );
        Ok((updated, Some(adapter.endpoint_id)))
    }

    fn read_configuration(
        &self,
        mapping: &VmComputeSystemMapping,
        vm_directory: &Path,
    ) -> Result<String, RepositoryError> {
        let configuration_path = layout::configuration_path(vm_directory);
        fs::read_to_string(&configuration_path).map_err(|error| {
            let error = RepositoryError::new(format!(
                "failed to read the HCS configuration of VM \"{}\" from {}: {error}",
                mapping.vm_name,
                configuration_path.display()
            ));
            tracing::error!("{error}");
            error
        })
    }

    /// Persists a configuration the start had to change.
    ///
    /// The document on disk is what a compute system HCS has forgotten is
    /// rebuilt from, so an adapter that lived only in memory would be lost the
    /// first time that happened.
    fn write_configuration(
        &self,
        mapping: &VmComputeSystemMapping,
        vm_directory: &Path,
        configuration: &str,
    ) -> Result<(), RepositoryError> {
        let configuration_path = layout::configuration_path(vm_directory);
        fs::write(&configuration_path, configuration).map_err(|error| {
            let error = RepositoryError::new(format!(
                "failed to write the HCS configuration of VM \"{}\" to {}: {error}",
                mapping.vm_name,
                configuration_path.display()
            ));
            tracing::error!("{error}");
            error
        })
    }

    fn grant_access_to_attachments(
        &self,
        mapping: &VmComputeSystemMapping,
        document: &str,
    ) -> Result<(), RepositoryError> {
        let paths = attachment_paths(document)?;
        if paths.is_empty() {
            tracing::warn!(
                "the HCS configuration of VM \"{}\" attaches no files; \
                 starting without granting any VM access",
                mapping.vm_name
            );
        }
        for path in &paths {
            // Fatal here, unlike the GPU shares: Hyper-V opens an attachment as
            // the VM itself, so a file it was not granted is a start that fails
            // with `ERROR_ACCESS_DENIED` deep inside HCS instead of here.
            (self.access_granter)(&mapping.hcs_compute_system_id, path).inspect_err(|error| {
                tracing::error!(
                    "VM \"{}\" cannot be started: it could not be granted access to \"{}\": \
                     {error}",
                    mapping.vm_name,
                    path.display()
                );
            })?;
        }

        Ok(())
    }
}

/// Collects the path of every file attached by an HCS configuration document.
///
/// Attachment entries without a `Path` are skipped rather than treated as a
/// parse failure: HCS attachment kinds that carry no host file are valid, and
/// only the ones that do need an access grant.
fn attachment_paths(document: &str) -> Result<Vec<PathBuf>, RepositoryError> {
    let configuration: serde_json::Value = serde_json::from_str(document).map_err(|error| {
        let error = RepositoryError::new(format!(
            "the stored HCS configuration is not valid JSON: {error}"
        ));
        tracing::error!("{error}");
        error
    })?;

    let Some(attachments) = configuration
        .pointer("/VirtualMachine/Devices/Scsi/Primary/Attachments")
        .and_then(serde_json::Value::as_object)
    else {
        return Ok(Vec::new());
    };

    Ok(attachments
        .values()
        .filter_map(|attachment| {
            attachment
                .get("Path")
                .and_then(serde_json::Value::as_str)
                .map(PathBuf::from)
        })
        .collect())
}

/// Writes this run's shares into the configuration the system is built from,
/// or leaves it alone when there are none.
///
/// The GPU's shares and the display's meet here and nowhere earlier: they are
/// decided separately and mean different things when they are missing, but HCS
/// has one Plan9 device and it takes one list.
///
/// A configuration that will not take them is logged and used as it stands: a
/// VM that starts without its shares is a VM whose guest finds no GPU
/// userspace and no display payload, which is worse than a VM with both and
/// better than no VM.
fn with_plan9_shares(
    mapping: &VmComputeSystemMapping,
    configuration: String,
    prepared: Option<&PreparedGpu>,
    display_payload: Option<&DisplayExport>,
) -> String {
    let mut exports: Vec<Plan9Export<'_>> = prepared
        .and_then(|prepared| prepared.exports.as_ref())
        .into_iter()
        .flat_map(GpuExports::iter)
        .map(|export| Plan9Export {
            name: export.name(),
            host_path: export.host_path(),
        })
        .collect();
    if let Some(display) = display_payload {
        exports.push(Plan9Export {
            name: display.name(),
            host_path: display.host_path(),
        });
    }
    if exports.is_empty() {
        return configuration;
    }

    match hcs_config::apply_plan9_shares(&configuration, &exports) {
        Ok(updated) => updated,
        Err(error) => {
            tracing::warn!(
                "VM \"{}\" starts without its Plan9 shares: {error}",
                mapping.vm_name
            );
            configuration
        }
    }
}

/// The directory the running executable lives in, for the display payload.
///
/// Read once per pipeline for the same reason the GPU's is: the executable
/// does not move while VMLord runs, and a start that could not name its own
/// directory is a start with no payload rather than one that fails.
fn executable_directory_for_display() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_default()
}

/// Canonicalizes a path the way an export must have it.
pub(crate) fn canonicalize_for_export(path: &Path) -> Result<PathBuf, RepositoryError> {
    std::fs::canonicalize(path)
        .map_err(|error| RepositoryError::new(format!("{}: {error}", path.display())))
}

fn grant_vm_access(id: &str, path: &Path) -> Result<(), RepositoryError> {
    HcsClient::new().grant_vm_access(id, path)
}

/// Resolves the endpoint VM `vm_name` starts on.
///
/// `recorded` is the identifier the VM's mapping remembers, if any.
///
/// Under [`EndpointPolicy::Reuse`] the recorded endpoint is opened rather than
/// trusted: one deleted outside VMLord, or lost to an HNS reset, is replaced by
/// a new one instead of failing the start. That hands the guest a different
/// address, but the alternative is a VM that can no longer start at all.
///
/// Under [`EndpointPolicy::Replace`] the recorded endpoint is deleted and a new
/// one created in its place, because HNS still has it attached to a compute
/// system that no longer exists. The address of the old endpoint is read first
/// and asked for again, so the guest keeps the address it had.
fn ensure_endpoint(
    vm_name: &str,
    recorded: Option<Uuid>,
    policy: EndpointPolicy,
) -> Result<VmNetworkAdapter, RepositoryError> {
    // The network first: an endpoint cannot be created outside one, and an
    // installation that has never had a VM on the network has none yet.
    let network = HcnNetwork::ensure()?;

    let existing = match recorded {
        Some(id) => HcnEndpoint::open_if_present(id)?.map(|endpoint| (id, endpoint)),
        None => None,
    };

    let (endpoint_id, endpoint) = match (policy, existing) {
        (EndpointPolicy::Reuse, Some(existing)) => existing,
        (EndpointPolicy::Reuse, None) => {
            if let Some(id) = recorded {
                tracing::warn!(
                    "HNS no longer knows endpoint {id} of VM \"{vm_name}\"; \
                     creating a new one, which changes the address the guest is offered"
                );
            }
            let id = Uuid::new_v4();
            (id, HcnEndpoint::create(&network, id, vm_name)?)
        }
        (EndpointPolicy::Replace, existing) => {
            let address = replaced_address(vm_name, existing.as_ref())?;
            if let Some(id) = recorded {
                // The handle has to close before HNS will delete what it points
                // at.
                drop(existing);
                HcnEndpoint::delete(id)?;
            }
            let id = Uuid::new_v4();
            match &address {
                Some(address) => tracing::info!(
                    "replacing the occupied endpoint of VM \"{vm_name}\" with {id} on {}",
                    address.ip_address
                ),
                None => {
                    tracing::info!("replacing the occupied endpoint of VM \"{vm_name}\" with {id}")
                }
            }
            (
                id,
                HcnEndpoint::create_with_address(&network, id, vm_name, address.as_ref())?,
            )
        }
    };

    Ok(VmNetworkAdapter {
        endpoint_id,
        mac_address: endpoint.mac_address()?,
        address: endpoint.address()?,
    })
}

/// The address a replacement endpoint should ask for.
///
/// `None` when HNS no longer has the old endpoint or reports no address for it:
/// the guest then gets whatever the network's IPAM assigns, which is worse than
/// keeping its address but far better than not starting.
fn replaced_address(
    vm_name: &str,
    existing: Option<&(Uuid, HcnEndpoint)>,
) -> Result<Option<EndpointAddress>, RepositoryError> {
    let Some((id, endpoint)) = existing else {
        tracing::warn!(
            "HNS no longer knows the occupied endpoint of VM \"{vm_name}\"; \
             its replacement is created without an address of its own"
        );
        return Ok(None);
    };

    let address = endpoint.address()?;
    if address.is_none() {
        tracing::warn!(
            "HNS reports no address for endpoint {id} of VM \"{vm_name}\"; \
             its replacement cannot ask for the old one, so the guest is offered a new address"
        );
    }
    Ok(address)
}

/// Starts the compute system `id`, re-creating it from `configuration` first
/// if HCS no longer knows it.
///
/// HCS destroys a compute system when it exits, so every VM that has been
/// stopped -- by its guest or by a forced stop -- has to be rebuilt before it
/// can run again. Re-creating from the stored configuration keeps the VM's id,
/// disks and metadata mapping unchanged, so a stop stays a stop rather than
/// becoming an implicit delete.
fn prepare_hcs_system(id: &str, configuration: &str) -> Result<(), HcsStartFailure> {
    let existing =
        HcsSystem::open_if_present(id, HCS_ACCESS_ALL).map_err(HcsStartFailure::Failed)?;
    let reusable = match existing {
        Some(system) => {
            let state = reported_state(id).map_err(HcsStartFailure::Failed)?;
            match plan_for_existing(&state) {
                ExistingSystemPlan::StartAsIs => Some(system),
                ExistingSystemPlan::Rebuild => {
                    tracing::info!(
                        "compute system \"{id}\" has never run, so it is rebuilt from the \
                         configuration this start prepared -- the one that carries the VM's \
                         network adapter"
                    );
                    // Before the teardown: HCS refuses to destroy a system this
                    // process still holds open.
                    drop(system);
                    if let Err(error) = cleanup::teardown_compute_system(id) {
                        tracing::error!(
                            "the compute system \"{id}\" could not be rebuilt: {error}"
                        );
                        return Err(HcsStartFailure::Failed(error));
                    }
                    None
                }
            }
        }
        None => {
            tracing::info!(
                "HCS no longer knows compute system \"{id}\"; \
                 re-creating it from the stored configuration before starting it"
            );
            None
        }
    };

    if reusable.is_none() {
        HcsClient::new()
            .create_system_and_wait(id, configuration, CREATE_TIMEOUT)
            .map_err(|failure| tear_down_after_a_failed_creation(id, failure))?;
    }
    Ok(())
}

/// Starts the compute system [`prepare_hcs_system`] has already brought into
/// shape.
fn start_hcs_system(id: &str) -> Result<(), HcsStartFailure> {
    // The handle must outlive the start operation it issued.
    let system = HcsSystem::open(id, HCS_ACCESS_ALL).map_err(HcsStartFailure::Failed)?;
    system.start_and_wait(START_TIMEOUT)
}

/// What a start does with a compute system HCS already knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExistingSystemPlan {
    /// Start it as it stands.
    StartAsIs,
    /// Destroy it and create it again from the configuration this start
    /// prepared.
    Rebuild,
}

/// Decides what to do with a compute system that already exists.
///
/// Creation makes the compute system before the VM has an endpoint -- one is
/// created on the first start, so that a VM nobody ever starts takes no address
/// -- so the document creation used carries no `NetworkAdapters` section. The
/// start is what creates the endpoint and writes the adapter in, and until this
/// existed the freshly created system was simply started as it stood: the guest
/// came up with no network card, while HNS held an endpoint with an address and
/// nothing attached to it. Every later start already rebuilt the system, because
/// HCS destroys one as it stops, so only the first start after a creation was
/// ever wrong.
///
/// A system in `Created` has executed nothing, so rebuilding it destroys no
/// state. Every other state has a guest behind it and is started as it stands.
fn plan_for_existing(state: &HcsSystemState) -> ExistingSystemPlan {
    match state {
        HcsSystemState::Created => ExistingSystemPlan::Rebuild,
        _ => ExistingSystemPlan::StartAsIs,
    }
}

/// The state HCS reports for compute system `id`.
///
/// Read from the enumeration rather than from the system's own properties: a
/// system that has been created and never started refuses a property query
/// outright, and that is exactly the state this has to recognise. A system the
/// enumeration does not mention is treated as `Created` for the same reason
/// [`HcsSystemState::from_enumeration`] does -- it has certainly never run.
fn reported_state(id: &str) -> Result<HcsSystemState, RepositoryError> {
    let systems = HcsClient::new().enumerate_systems()?;
    let reported = systems
        .into_iter()
        .find(|system| system.id == id)
        .and_then(|system| system.state);
    Ok(HcsSystemState::from_enumeration(reported))
}

/// Removes a compute system HCS may have created before the creation failed.
///
/// `HcsCreateComputeSystem` can succeed while its operation fails, leaving a
/// system that holds the very configuration -- and therefore the very endpoint
/// -- the failed attempt named. A retry with a replaced endpoint would find
/// that system through `open_if_present` and start it with the stale adapter,
/// so it has to go first. The teardown is best-effort: it explains a start that
/// failed, it does not decide it.
fn tear_down_after_a_failed_creation(id: &str, failure: HcsStartFailure) -> HcsStartFailure {
    if let Err(error) = cleanup::teardown_compute_system(id) {
        tracing::warn!(
            "cleanup of the ambiguously-created compute system \"{id}\" also failed: {error}"
        );
    }
    failure
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, AtomicUsize, Ordering},
        },
    };

    use uuid::Uuid;
    use vmlord_core::{GpuAssignment, GpuFailure, NetworkMode, RepositoryError};

    use super::{
        EndpointPolicy, ExistingSystemPlan, GpuRuns, HcsSystemState, PreparedDisplay, PreparedGpu,
        VmNetworkAdapter, VmStartPipeline, attachment_paths, canonicalize_for_export,
        plan_for_existing,
    };
    use crate::{
        Com1Launcher,
        hcn_endpoint::EndpointAddress,
        hcs::HcsStartFailure,
        metadata::{MetadataStore, VmComputeSystemMapping},
    };

    #[test]
    fn a_system_that_has_never_run_is_rebuilt_from_the_configuration_the_start_prepared() {
        // The compute system creation makes carries no network adapter: the
        // endpoint does not exist yet, and the start is what creates it and
        // writes it into the configuration. Starting the system as created
        // therefore boots a guest with no network card at all, while the
        // endpoint sits in HNS with an address nothing is attached to. A
        // system in `Created` has executed nothing, so rebuilding it costs
        // nothing and makes the start run the configuration it just prepared.
        assert_eq!(
            plan_for_existing(&HcsSystemState::Created),
            ExistingSystemPlan::Rebuild
        );
    }

    #[test]
    fn a_system_that_has_already_run_is_started_as_it_stands() {
        // Anything but `Created` has state a rebuild would destroy, and HCS
        // destroys a compute system as it stops -- so a VM being started after
        // a stop is not found at all and takes the re-creation path anyway.
        for state in [
            HcsSystemState::Running,
            HcsSystemState::Paused,
            HcsSystemState::Stopped,
            HcsSystemState::Other("Zombie".to_owned()),
        ] {
            assert_eq!(
                plan_for_existing(&state),
                ExistingSystemPlan::StartAsIs,
                "{state:?}"
            );
        }
    }

    #[test]
    fn the_pipelines_a_build_thread_needs_can_be_moved_to_it() {
        // Creating a VM now starts it and waits for its guest, all on the build
        // thread, so everything that cycle owns has to be able to go there.
        const fn assert_send_sync<T: Send + Sync>() {}
        const fn assert_send<T: Send>() {}

        assert_send_sync::<VmStartPipeline>();
        assert_send_sync::<crate::force_stop::VmForceStopPipeline>();
        assert_send_sync::<crate::delete::VmDeletionPipeline>();
        assert_send::<crate::com1_terminal::Com1Session>();
    }

    struct TempRoot(PathBuf);

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temp_root(label: &str) -> TempRoot {
        static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "vmlord-start-test-{label}-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("test root should be created");
        TempRoot(path)
    }

    fn configuration(disk: &str, iso: &str) -> String {
        serde_json::json!({
            "VirtualMachine": {
                "Devices": {
                    "Scsi": { "Primary": { "Attachments": {
                        "0": { "Type": "VirtualDisk", "Path": disk },
                        "1": { "Type": "Iso", "Path": iso }
                    }}}
                }
            }
        })
        .to_string()
    }

    /// The endpoint the test provider hands out when it creates a new one.
    const NEW_ENDPOINT_ID: Uuid = Uuid::from_u128(0x3f2b_0c11_5c78_4c1b_9e2f_3a8b_7d4c_6e50);
    const NEW_ENDPOINT_GUID: &str = "3F2B0C11-5C78-4C1B-9E2F-3A8B7D4C6E50";
    const MAC_ADDRESS: &str = "00-15-5D-01-02-03";

    /// The address the test endpoint provider reports for its endpoint.
    fn endpoint_address() -> EndpointAddress {
        EndpointAddress {
            ip_address: "172.22.42.5".to_owned(),
            prefix_length: 24,
        }
    }

    /// The endpoint the test provider hands out when it replaces an occupied one.
    const REPLACEMENT_ENDPOINT_ID: Uuid =
        Uuid::from_u128(0x7a1c_44e0_5c78_4c1b_9e2f_3a8b_7d4c_6e50);
    const REPLACEMENT_ENDPOINT_GUID: &str = "7A1C44E0-5C78-4C1B-9E2F-3A8B7D4C6E50";

    #[derive(Clone, Default)]
    struct Calls {
        /// Every step in the order it ran, so a test can assert on ordering
        /// rather than only on what each collaborator saw.
        steps: Arc<Mutex<Vec<&'static str>>>,
        grant: Arc<Mutex<Vec<(String, PathBuf)>>>,
        /// Every compute system a preparer was asked to bring into shape, with
        /// the configuration it was given. A start runs on whatever the
        /// preparer left behind, so this is where the document reaches HCS.
        start: Arc<Mutex<Vec<(String, String)>>>,
        /// How many times the system was actually started.
        starts: Arc<Mutex<usize>>,
        endpoint: Arc<Mutex<Vec<EndpointRequest>>>,
        dhcp: Arc<Mutex<Vec<(String, EndpointAddress)>>>,
        /// The command line each COM1 console was opened with.
        console: Arc<Mutex<Vec<String>>>,
        /// How many COM1 sessions were told to stop.
        console_cancellations: Arc<AtomicUsize>,
    }

    /// One call into the endpoint provider: the identifier it was offered and
    /// what it was asked to do with it.
    type EndpointRequest = (Option<Uuid>, EndpointPolicy);

    /// Which collaborators fail; by default none of them do.
    #[derive(Clone, Copy, Default)]
    struct Behavior {
        fail_start: bool,
        fail_endpoint: bool,
        fail_dhcp: bool,
        /// How many leading starts fail with an occupied endpoint before the
        /// starter accepts one.
        busy_starts: usize,
    }

    impl Behavior {
        fn start_fails() -> Self {
            Self {
                fail_start: true,
                ..Self::default()
            }
        }
    }

    struct Fixture {
        _root: TempRoot,
        store: MetadataStore,
        vm_directory: PathBuf,
        mapping: VmComputeSystemMapping,
        calls: Calls,
    }

    impl Fixture {
        fn configuration(&self) -> serde_json::Value {
            serde_json::from_str(
                &fs::read_to_string(self.vm_directory.join("config.json")).unwrap(),
            )
            .unwrap()
        }

        fn recorded_endpoint(&self) -> Option<Uuid> {
            self.store
                .find_by_vm_name(&self.mapping.vm_name)
                .unwrap()
                .unwrap()
                .endpoint_id
        }
    }

    fn fixture(label: &str) -> Fixture {
        fixture_with(label, NetworkMode::None, None)
    }

    fn fixture_with(label: &str, network_mode: NetworkMode, endpoint_id: Option<Uuid>) -> Fixture {
        let root = temp_root(label);
        let vm_directory = root.0.join("vm");
        fs::create_dir_all(&vm_directory).expect("VM directory should be created");
        fs::write(
            vm_directory.join("config.json"),
            configuration(
                "C:\\vms\\dev\\disks\\system.vhdx",
                "C:\\images\\installer.iso",
            ),
        )
        .expect("HCS configuration should be written");

        let mapping = VmComputeSystemMapping {
            vm_id: Uuid::new_v4(),
            vm_name: "dev".into(),
            hcs_compute_system_id: "vmlord-dev".into(),
            disk_gb: 20,
            endpoint_id,
            network_mode,
            ssh: None,
            ssh_daemon: None,
            gpu_mode: vmlord_core::GpuMode::None,
            desktop_profile: vmlord_core::DesktopProfile::Headless,
            display_provisioning: vmlord_core::DisplayProvisioning::NotRequested,
            display_mode: None,
            guest_target: None,
        };
        let store = MetadataStore::new(root.0.join("vm-mapping.json"));
        store
            .insert(mapping.clone())
            .expect("mapping should be persisted");

        Fixture {
            store,
            vm_directory,
            mapping,
            calls: Calls::default(),
            _root: root,
        }
    }

    /// A launcher that records the console it would have opened instead of
    /// opening one, and lets a test see the sessions that were told to stop.
    fn console_launcher(calls: &Calls) -> Com1Launcher {
        let recorded = calls.console.clone();
        let steps = calls.steps.clone();
        let mut launcher = Com1Launcher::for_test(
            PathBuf::from(r"C:\VMLord\vmlord-com1.exe"),
            move |command: &crate::com1_terminal::TerminalCommand| {
                steps.lock().unwrap().push("console");
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
        );
        launcher.observe_cancellations(calls.console_cancellations.clone());
        launcher
    }

    #[test]
    fn opens_the_console_after_the_system_is_prepared_and_before_it_runs() {
        // Not before preparing: preparing may destroy and re-create the compute
        // system, and with it the named pipe COM1 is served through, leaving a
        // console reading a pipe that no longer exists. Not after starting
        // either: the output that explains a failed boot is written in the
        // seconds right after HCS hands the VM to its worker.
        let fixture = fixture_with("console-order", NetworkMode::Nat, None);
        let calls = fixture.calls.clone();

        let _session = pipeline(&calls, Behavior::default())
            .start(&fixture.store, "dev", &fixture.vm_directory)
            .unwrap();

        assert_eq!(
            calls.steps.lock().unwrap().as_slice(),
            [
                "endpoint", "dhcp", "grant", "grant", "prepare", "console", "start"
            ]
        );
    }

    #[test]
    fn an_explicit_start_truncates_the_log_of_the_vm_it_starts() {
        let fixture = fixture_with("console-arguments", NetworkMode::Nat, None);
        let calls = fixture.calls.clone();

        let _session = pipeline(&calls, Behavior::default())
            .start(&fixture.store, "dev", &fixture.vm_directory)
            .unwrap();

        let console = calls.console.lock().unwrap().clone();
        assert_eq!(console.len(), 1);
        assert!(console[0].contains("--mode truncate"), "{}", console[0]);
        assert!(
            console[0].contains(&fixture.mapping.vm_id.as_simple().to_string()),
            "the console must read the pipe of this VM: {}",
            console[0]
        );
        assert!(
            console[0].contains(&fixture.vm_directory.display().to_string()),
            "the log belongs beside the VM: {}",
            console[0]
        );
    }

    #[test]
    fn a_failure_after_console_launch_cancels_the_session() {
        let fixture = fixture_with("console-cancel", NetworkMode::Nat, None);
        let calls = fixture.calls.clone();

        let error = pipeline(
            &calls,
            Behavior {
                fail_start: true,
                ..Behavior::default()
            },
        )
        .start(&fixture.store, "dev", &fixture.vm_directory)
        .unwrap_err();

        assert!(
            error.to_string().contains("injected start failure"),
            "{error}"
        );
        assert_eq!(calls.console_cancellations.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_failure_before_the_console_launch_opens_no_console_at_all() {
        // The endpoint is now settled before the console is opened, so a VM
        // that cannot be given one never gets a terminal window it would only
        // have to close again.
        let fixture = fixture_with("console-none", NetworkMode::Nat, None);
        let calls = fixture.calls.clone();

        let error = pipeline(
            &calls,
            Behavior {
                fail_endpoint: true,
                ..Behavior::default()
            },
        )
        .start(&fixture.store, "dev", &fixture.vm_directory)
        .unwrap_err();

        assert!(error.to_string().contains("endpoint"), "{error}");
        assert!(calls.console.lock().unwrap().is_empty());
        assert_eq!(calls.console_cancellations.load(Ordering::Relaxed), 0);
    }

    fn pipeline(calls: &Calls, behavior: Behavior) -> VmStartPipeline {
        VmStartPipeline::for_test(
            console_launcher(calls),
            {
                let calls = calls.clone();
                move |id: &str, path: &Path| {
                    calls.steps.lock().unwrap().push("grant");
                    calls
                        .grant
                        .lock()
                        .unwrap()
                        .push((id.to_owned(), path.to_path_buf()));
                    Ok(())
                }
            },
            {
                let calls = calls.clone();
                move |id: &str, configuration: &str| {
                    calls.steps.lock().unwrap().push("prepare");
                    calls
                        .start
                        .lock()
                        .unwrap()
                        .push((id.to_owned(), configuration.to_owned()));
                    Ok(())
                }
            },
            {
                let calls = calls.clone();
                move |_id: &str| {
                    calls.steps.lock().unwrap().push("start");
                    let mut starts = calls.starts.lock().unwrap();
                    *starts += 1;
                    if *starts <= behavior.busy_starts {
                        return Err(HcsStartFailure::EndpointBusy(RepositoryError::new(
                            "injected endpoint-busy failure",
                        )));
                    }
                    if behavior.fail_start {
                        return Err(HcsStartFailure::Failed(RepositoryError::new(
                            "injected start failure",
                        )));
                    }
                    Ok(())
                }
            },
            {
                let calls = calls.clone();
                move |_vm_name: &str, recorded: Option<Uuid>, policy: EndpointPolicy| {
                    calls.steps.lock().unwrap().push("endpoint");
                    calls.endpoint.lock().unwrap().push((recorded, policy));
                    if behavior.fail_endpoint {
                        return Err(RepositoryError::new("injected endpoint failure"));
                    }
                    // A recorded endpoint is the one that gets reused; a VM
                    // without one, and a replacement, are handed a fresh id.
                    let endpoint_id = match policy {
                        EndpointPolicy::Reuse => recorded.unwrap_or(NEW_ENDPOINT_ID),
                        EndpointPolicy::Replace => REPLACEMENT_ENDPOINT_ID,
                    };
                    Ok(VmNetworkAdapter {
                        endpoint_id,
                        mac_address: MAC_ADDRESS.to_owned(),
                        address: Some(endpoint_address()),
                    })
                }
            },
            {
                let calls = calls.clone();
                move |_store: &MetadataStore, mac: &str, address: &EndpointAddress| {
                    calls.steps.lock().unwrap().push("dhcp");
                    calls
                        .dhcp
                        .lock()
                        .unwrap()
                        .push((mac.to_owned(), address.clone()));
                    if behavior.fail_dhcp {
                        return Err(RepositoryError::new("injected DHCP failure"));
                    }
                    Ok(())
                }
            },
        )
    }

    #[test]
    fn grants_access_to_every_attachment_before_starting() {
        let fixture = fixture("happy");
        let calls = fixture.calls.clone();

        pipeline(&calls, Behavior::default())
            .start(&fixture.store, "dev", &fixture.vm_directory)
            .expect("start should succeed");

        let mut granted = calls.grant.lock().unwrap().clone();
        granted.sort();
        assert_eq!(
            granted,
            vec![
                (
                    fixture.mapping.hcs_compute_system_id.clone(),
                    PathBuf::from("C:\\images\\installer.iso")
                ),
                (
                    fixture.mapping.hcs_compute_system_id.clone(),
                    PathBuf::from("C:\\vms\\dev\\disks\\system.vhdx")
                ),
            ]
        );
        let started = calls.start.lock().unwrap().clone();
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].0, fixture.mapping.hcs_compute_system_id);
    }

    #[test]
    fn hands_the_stored_configuration_to_the_starter() {
        // The starter re-creates a compute system HCS no longer knows, so it
        // needs the very document creation persisted.
        let fixture = fixture("configuration");
        let calls = fixture.calls.clone();

        pipeline(&calls, Behavior::default())
            .start(&fixture.store, "dev", &fixture.vm_directory)
            .expect("start should succeed");

        let started = calls.start.lock().unwrap().clone();
        assert_eq!(
            started[0].1,
            fs::read_to_string(fixture.vm_directory.join("config.json")).unwrap()
        );
    }

    #[test]
    fn rejects_an_unmapped_vm_without_touching_hcs() {
        let fixture = fixture("unmapped");
        let calls = fixture.calls.clone();

        let error = pipeline(&calls, Behavior::default())
            .start(&fixture.store, "missing-vm", &fixture.vm_directory)
            .expect_err("an unmapped VM must not be started");

        assert!(error.to_string().contains("missing-vm"));
        assert!(calls.grant.lock().unwrap().is_empty());
        assert!(calls.start.lock().unwrap().is_empty());
    }

    #[test]
    fn a_missing_configuration_aborts_before_starting() {
        let fixture = fixture("no-config");
        let calls = fixture.calls.clone();
        fs::remove_file(fixture.vm_directory.join("config.json")).unwrap();

        let error = pipeline(&calls, Behavior::default())
            .start(&fixture.store, "dev", &fixture.vm_directory)
            .expect_err("a missing configuration must abort the start");

        assert!(error.to_string().contains("HCS configuration"));
        assert!(calls.start.lock().unwrap().is_empty());
    }

    #[test]
    fn a_malformed_configuration_aborts_before_starting() {
        let fixture = fixture("bad-config");
        let calls = fixture.calls.clone();
        fs::write(fixture.vm_directory.join("config.json"), b"not json").unwrap();

        let error = pipeline(&calls, Behavior::default())
            .start(&fixture.store, "dev", &fixture.vm_directory)
            .expect_err("a malformed configuration must abort the start");

        assert!(error.to_string().contains("not valid JSON"));
        assert!(calls.grant.lock().unwrap().is_empty());
        assert!(calls.start.lock().unwrap().is_empty());
    }

    #[test]
    fn propagates_a_start_failure() {
        let fixture = fixture("start-failure");
        let calls = fixture.calls.clone();

        let error = pipeline(
            &calls,
            Behavior {
                fail_start: true,
                ..Behavior::default()
            },
        )
        .start(&fixture.store, "dev", &fixture.vm_directory)
        .expect_err("a failed start must be reported");

        assert!(error.to_string().contains("injected start failure"));
        assert_eq!(calls.grant.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_vm_without_networking_never_asks_for_an_endpoint() {
        let fixture = fixture("no-network");
        let calls = fixture.calls.clone();

        pipeline(&calls, Behavior::default())
            .start(&fixture.store, "dev", &fixture.vm_directory)
            .expect("start should succeed");

        assert!(calls.endpoint.lock().unwrap().is_empty());
        assert!(
            fixture
                .configuration()
                .pointer("/VirtualMachine/Devices/NetworkAdapters")
                .is_none()
        );
        assert_eq!(fixture.recorded_endpoint(), None);
    }

    #[test]
    fn a_vm_switched_off_the_network_loses_the_adapter_it_used_to_have() {
        // The VM ran with NAT, then its mode was edited back to `None`: the
        // section a previous start wrote must not survive into this one.
        let fixture = fixture("network-removed");
        let calls = fixture.calls.clone();
        let stale = crate::hcs_config::apply_network_adapter(
            &fs::read_to_string(fixture.vm_directory.join("config.json")).unwrap(),
            NEW_ENDPOINT_ID,
            MAC_ADDRESS,
        )
        .unwrap();
        fs::write(fixture.vm_directory.join("config.json"), &stale).unwrap();

        pipeline(&calls, Behavior::default())
            .start(&fixture.store, "dev", &fixture.vm_directory)
            .expect("start should succeed");

        assert!(calls.endpoint.lock().unwrap().is_empty());
        assert!(
            fixture
                .configuration()
                .pointer("/VirtualMachine/Devices/NetworkAdapters")
                .is_none(),
            "the stale adapter must be gone from the stored configuration"
        );
        let started = calls.start.lock().unwrap().clone();
        assert!(
            !started[0].1.contains("NetworkAdapters"),
            "the starter must be handed the document without the adapter"
        );
    }

    #[test]
    fn a_nat_vm_gets_its_endpoint_before_anything_is_granted_or_started() {
        // The adapter has to be in the document the starter is handed, so the
        // endpoint cannot be an afterthought once the VM is already running.
        let fixture = fixture_with("nat-order", NetworkMode::Nat, None);
        let calls = fixture.calls.clone();

        pipeline(&calls, Behavior::default())
            .start(&fixture.store, "dev", &fixture.vm_directory)
            .expect("start should succeed");

        assert_eq!(
            calls.steps.lock().unwrap().clone(),
            vec![
                "endpoint", "dhcp", "grant", "grant", "prepare", "console", "start"
            ]
        );
    }

    #[test]
    fn a_new_endpoint_is_recorded_in_the_mapping() {
        let fixture = fixture_with("nat-record", NetworkMode::Nat, None);
        let calls = fixture.calls.clone();

        pipeline(&calls, Behavior::default())
            .start(&fixture.store, "dev", &fixture.vm_directory)
            .expect("start should succeed");

        assert_eq!(
            calls.endpoint.lock().unwrap().clone(),
            vec![(None, EndpointPolicy::Reuse)]
        );
        assert_eq!(fixture.recorded_endpoint(), Some(NEW_ENDPOINT_ID));
    }

    #[test]
    fn a_recorded_endpoint_is_offered_for_reuse_rather_than_replaced() {
        // Re-creating the endpoint per start would hand the guest a new address
        // every time and break everything that remembered the old one.
        let recorded = Uuid::new_v4();
        let fixture = fixture_with("nat-reuse", NetworkMode::Nat, Some(recorded));
        let calls = fixture.calls.clone();

        pipeline(&calls, Behavior::default())
            .start(&fixture.store, "dev", &fixture.vm_directory)
            .expect("start should succeed");

        assert_eq!(
            calls.endpoint.lock().unwrap().clone(),
            vec![(Some(recorded), EndpointPolicy::Reuse)]
        );
        assert_eq!(fixture.recorded_endpoint(), Some(recorded));
    }

    #[test]
    fn the_adapter_reaches_both_the_stored_configuration_and_the_starter() {
        let fixture = fixture_with("nat-config", NetworkMode::Nat, None);
        let calls = fixture.calls.clone();

        pipeline(&calls, Behavior::default())
            .start(&fixture.store, "dev", &fixture.vm_directory)
            .expect("start should succeed");

        let expected = serde_json::json!({
            NEW_ENDPOINT_GUID: {
                "EndpointId": NEW_ENDPOINT_GUID,
                "MacAddress": MAC_ADDRESS
            }
        });
        assert_eq!(
            fixture
                .configuration()
                .pointer("/VirtualMachine/Devices/NetworkAdapters"),
            Some(&expected)
        );

        let started = calls.start.lock().unwrap().clone();
        assert_eq!(
            started[0].1,
            fs::read_to_string(fixture.vm_directory.join("config.json")).unwrap()
        );
    }

    #[test]
    fn a_failing_endpoint_provider_aborts_the_start() {
        // A VM that asked for a network and did not get one must not come up
        // silently without it.
        let fixture = fixture_with("nat-failure", NetworkMode::Nat, None);
        let calls = fixture.calls.clone();

        let error = pipeline(
            &calls,
            Behavior {
                fail_endpoint: true,
                ..Behavior::default()
            },
        )
        .start(&fixture.store, "dev", &fixture.vm_directory)
        .expect_err("a VM without its endpoint must not be started");

        assert!(error.to_string().contains("injected endpoint failure"));
        assert!(calls.grant.lock().unwrap().is_empty());
        assert!(calls.start.lock().unwrap().is_empty());
        assert_eq!(fixture.recorded_endpoint(), None);
    }

    #[test]
    fn a_failed_start_keeps_the_endpoint_it_created() {
        // The endpoint lives until the VM is deleted. Dropping it here would
        // give the guest a different address on the next attempt.
        let fixture = fixture_with("nat-start-failure", NetworkMode::Nat, None);
        let calls = fixture.calls.clone();

        pipeline(
            &calls,
            Behavior {
                fail_start: true,
                ..Behavior::default()
            },
        )
        .start(&fixture.store, "dev", &fixture.vm_directory)
        .expect_err("a failed start must be reported");

        assert_eq!(fixture.recorded_endpoint(), Some(NEW_ENDPOINT_ID));
        assert!(
            fixture
                .configuration()
                .pointer("/VirtualMachine/Devices/NetworkAdapters")
                .is_some()
        );
    }

    #[test]
    fn an_occupied_endpoint_is_replaced_and_the_start_retried_once() {
        // A VM stopped without a detach leaves HNS holding its endpoint against
        // a compute system that no longer exists. Nothing can attach to it
        // again, so the only way back is a different endpoint.
        let recorded = Uuid::new_v4();
        let fixture = fixture_with("busy-retry", NetworkMode::Nat, Some(recorded));
        let calls = fixture.calls.clone();

        pipeline(
            &calls,
            Behavior {
                busy_starts: 1,
                ..Behavior::default()
            },
        )
        .start(&fixture.store, "dev", &fixture.vm_directory)
        .expect("a start blocked by an occupied endpoint must recover");

        assert_eq!(
            calls.endpoint.lock().unwrap().clone(),
            vec![
                (Some(recorded), EndpointPolicy::Reuse),
                (Some(recorded), EndpointPolicy::Replace),
            ]
        );
        assert_eq!(
            calls.steps.lock().unwrap().clone(),
            vec![
                "endpoint", "dhcp", "grant", "grant", "prepare", "console", "start", "endpoint",
                "dhcp", "grant", "grant", "prepare", "console", "start"
            ]
        );
    }

    #[test]
    fn the_replacement_endpoint_reaches_the_mapping_and_the_configuration() {
        let recorded = Uuid::new_v4();
        let fixture = fixture_with("busy-recorded", NetworkMode::Nat, Some(recorded));
        let calls = fixture.calls.clone();

        pipeline(
            &calls,
            Behavior {
                busy_starts: 1,
                ..Behavior::default()
            },
        )
        .start(&fixture.store, "dev", &fixture.vm_directory)
        .expect("a start blocked by an occupied endpoint must recover");

        assert_eq!(fixture.recorded_endpoint(), Some(REPLACEMENT_ENDPOINT_ID));
        assert_eq!(
            fixture
                .configuration()
                .pointer("/VirtualMachine/Devices/NetworkAdapters"),
            Some(&serde_json::json!({
                REPLACEMENT_ENDPOINT_GUID: {
                    "EndpointId": REPLACEMENT_ENDPOINT_GUID,
                    "MacAddress": MAC_ADDRESS
                }
            }))
        );
        let started = calls.start.lock().unwrap().clone();
        assert_eq!(
            started[1].1,
            fs::read_to_string(fixture.vm_directory.join("config.json")).unwrap()
        );
    }

    #[test]
    fn a_second_occupied_endpoint_is_not_retried_again() {
        // One replacement is a recovery; a second means something other than a
        // stale attachment is wrong, and retrying forever would create an
        // endpoint per attempt.
        let fixture = fixture_with("busy-twice", NetworkMode::Nat, Some(Uuid::new_v4()));
        let calls = fixture.calls.clone();

        let error = pipeline(
            &calls,
            Behavior {
                busy_starts: 2,
                ..Behavior::default()
            },
        )
        .start(&fixture.store, "dev", &fixture.vm_directory)
        .expect_err("a second occupied endpoint must fail the start");

        assert!(error.to_string().contains("injected endpoint-busy failure"));
        assert_eq!(calls.start.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_vm_without_networking_never_replaces_an_endpoint() {
        // Without NAT there is no endpoint to blame, so the failure is reported
        // as it came rather than retried.
        let fixture = fixture("busy-no-network");
        let calls = fixture.calls.clone();

        let error = pipeline(
            &calls,
            Behavior {
                busy_starts: 1,
                ..Behavior::default()
            },
        )
        .start(&fixture.store, "dev", &fixture.vm_directory)
        .expect_err("a VM without an endpoint has no recovery");

        assert!(error.to_string().contains("injected endpoint-busy failure"));
        assert!(calls.endpoint.lock().unwrap().is_empty());
        assert_eq!(calls.start.lock().unwrap().len(), 1);
    }

    #[test]
    fn an_ordinary_start_failure_is_not_retried() {
        let fixture = fixture_with("plain-failure", NetworkMode::Nat, Some(Uuid::new_v4()));
        let calls = fixture.calls.clone();

        let error = pipeline(
            &calls,
            Behavior {
                fail_start: true,
                ..Behavior::default()
            },
        )
        .start(&fixture.store, "dev", &fixture.vm_directory)
        .expect_err("a failed start must be reported");

        assert!(error.to_string().contains("injected start failure"));
        assert_eq!(calls.start.lock().unwrap().len(), 1);
    }

    /// A prepared GPU with one share and the given assignment.
    fn prepared(assignment: GpuAssignment) -> PreparedGpu {
        let exports = crate::gpu_exports::GpuExports::for_test(vec![(
            vmlord_core::GpuShare::wsl_lib(),
            PathBuf::from("C:\\Windows\\System32\\lxss\\lib"),
        )]);
        PreparedGpu {
            manifest: exports.manifest(),
            exports: Some(exports),
            assignment,
        }
    }

    fn complete() -> GpuAssignment {
        GpuAssignment::Complete(vmlord_core::NativeGpuDetail {
            adapter: Some("nvidia".into()),
            adapters: 1,
        })
    }

    #[test]
    fn a_prepared_gpu_reaches_the_configuration_the_system_is_built_from() {
        let fixture = fixture("gpu-shares");
        let calls = fixture.calls.clone();
        let runs = GpuRuns::default();
        let pipeline = pipeline(&calls, Behavior::default()).with_gpu(
            move |_mapping, _directory| Some(prepared(complete())),
            |_id, _mode| Ok(()),
            runs.clone(),
        );

        pipeline
            .start(&fixture.store, "dev", &fixture.vm_directory)
            .expect("the start must succeed");

        let (_id, configuration) = calls.start.lock().unwrap()[0].clone();
        assert!(
            configuration.contains("vmlord.gpu.wsl-lib"),
            "the shares have to be in the system that is built, not beside it: {configuration}"
        );
    }

    #[test]
    fn a_prepared_display_payload_reaches_the_configuration_beside_the_gpus_shares() {
        let fixture = fixture("display-share");
        let calls = fixture.calls.clone();
        let active = fixture.vm_directory.join("display-payload").join("active");
        fs::create_dir_all(&active).expect("a published payload");
        let vm_directory = fixture.vm_directory.clone();
        let pipeline = pipeline(&calls, Behavior::default())
            .with_gpu(
                move |_mapping, _directory| Some(prepared(complete())),
                |_id, _mode| Ok(()),
                GpuRuns::default(),
            )
            .with_display(move |_mapping, _directory| {
                Some(PreparedDisplay {
                    export: crate::display_exports::build(
                        &vm_directory,
                        Some(&active),
                        &canonicalize_for_export,
                    ),
                    failure: None,
                    available_version: Some("0.1.0".to_owned()),
                })
            });

        pipeline
            .start(&fixture.store, "dev", &fixture.vm_directory)
            .expect("the start must succeed");

        let (_id, configuration) = calls.start.lock().unwrap()[0].clone();
        assert!(
            configuration.contains("vmlord.display.payload"),
            "the display share travels in the same Plan9 device: {configuration}"
        );
        assert!(
            configuration.contains("vmlord.gpu.wsl-lib"),
            "and it does not displace the GPU's: {configuration}"
        );
    }

    #[test]
    fn a_vm_with_no_display_payload_still_starts() {
        let fixture = fixture("display-missing");
        let calls = fixture.calls.clone();
        let pipeline =
            pipeline(&calls, Behavior::default()).with_display(|_mapping, _directory| {
                Some(PreparedDisplay {
                    export: None,
                    failure: Some(vmlord_core::DisplayFailure::new(
                        vmlord_core::DisplayStage::Payload,
                        vmlord_core::DisplayStatusCode::PayloadMissing,
                        "no display payload for this guest",
                    )),
                    available_version: None,
                })
            });

        pipeline
            .start(&fixture.store, "dev", &fixture.vm_directory)
            .expect("a display that could not be staged is not a start that failed");
    }

    #[test]
    fn a_prepared_gpu_is_never_written_back_to_the_stored_configuration() {
        // The shares name this host's paths and this run's staging directory.
        // Persisting them would describe a boot that is over.
        let fixture = fixture("gpu-not-stored");
        let calls = fixture.calls.clone();
        let pipeline = pipeline(&calls, Behavior::default()).with_gpu(
            move |_mapping, _directory| Some(prepared(complete())),
            |_id, _mode| Ok(()),
            GpuRuns::default(),
        );

        pipeline
            .start(&fixture.store, "dev", &fixture.vm_directory)
            .expect("the start must succeed");

        let stored = fs::read_to_string(fixture.vm_directory.join("config.json")).unwrap();
        assert!(
            !stored.contains("vmlord.gpu.wsl-lib"),
            "a per-boot section has no place in the stored configuration: {stored}"
        );
    }

    #[test]
    fn a_start_records_what_it_prepared_before_the_system_runs() {
        let fixture = fixture("gpu-recorded");
        let calls = fixture.calls.clone();
        let runs = GpuRuns::default();
        let pipeline = pipeline(&calls, Behavior::default()).with_gpu(
            move |_mapping, _directory| Some(prepared(complete())),
            |_id, _mode| Ok(()),
            runs.clone(),
        );

        pipeline
            .start(&fixture.store, "dev", &fixture.vm_directory)
            .expect("the start must succeed");

        assert!(matches!(
            runs.snapshot(fixture.mapping.vm_id).assignment,
            Some(GpuAssignment::Complete(_))
        ));
        assert!(
            runs.shares(fixture.mapping.vm_id).is_some(),
            "the manifest the agent will be offered is left where the listener reads it"
        );
    }

    #[test]
    fn a_vm_without_a_gpu_has_nothing_recorded_about_one() {
        let fixture = fixture("gpu-none");
        let calls = fixture.calls.clone();
        let runs = GpuRuns::default();
        let pipeline = pipeline(&calls, Behavior::default()).with_gpu(
            |_mapping, _directory| None,
            |_id, _mode| Ok(()),
            runs.clone(),
        );

        pipeline
            .start(&fixture.store, "dev", &fixture.vm_directory)
            .expect("the start must succeed");

        assert_eq!(
            runs.snapshot(fixture.mapping.vm_id).assignment,
            None,
            "a VM that asks for no GPU is not a VM whose GPU failed"
        );
        assert_eq!(runs.shares(fixture.mapping.vm_id), None);
    }

    #[test]
    fn a_gpu_that_could_not_be_attached_does_not_fail_the_start() {
        let fixture = fixture("gpu-assign-failed");
        let calls = fixture.calls.clone();
        let runs = GpuRuns::default();
        let pipeline = pipeline(&calls, Behavior::default()).with_gpu(
            move |_mapping, _directory| Some(prepared(complete())),
            |_id, _mode| {
                Err(GpuFailure::new(
                    vmlord_core::GpuStatusCode::AssignmentFailed,
                    "HCS refused the update",
                ))
            },
            runs.clone(),
        );

        pipeline
            .start(&fixture.store, "dev", &fixture.vm_directory)
            .expect("GPU is best effort and never fails a start");

        let GpuAssignment::Failed(failure) = runs
            .snapshot(fixture.mapping.vm_id)
            .assignment
            .expect("the start recorded something")
        else {
            panic!("the assigner refused, so the fact has to say so");
        };
        assert!(failure.message.contains("HCS refused"), "{failure:?}");
    }

    #[test]
    fn a_gpu_is_attached_exactly_once_and_never_retried() {
        let fixture = fixture("gpu-once");
        let calls = fixture.calls.clone();
        let attempts = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&attempts);
        let pipeline = pipeline(&calls, Behavior::default()).with_gpu(
            move |_mapping, _directory| Some(prepared(complete())),
            move |_id, _mode| {
                counted.fetch_add(1, Ordering::Relaxed);
                Err(GpuFailure::new(
                    vmlord_core::GpuStatusCode::AssignmentFailed,
                    "HCS refused the update",
                ))
            },
            GpuRuns::default(),
        );

        pipeline
            .start(&fixture.store, "dev", &fixture.vm_directory)
            .expect("the start must succeed");

        assert_eq!(
            attempts.load(Ordering::Relaxed),
            1,
            "a GPU that would not attach is not attached again"
        );
    }

    #[test]
    fn a_host_that_could_hand_over_nothing_is_not_asked_to_attach_anything() {
        let fixture = fixture("gpu-nothing");
        let calls = fixture.calls.clone();
        let attempts = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&attempts);
        let pipeline = pipeline(&calls, Behavior::default()).with_gpu(
            move |_mapping, _directory| {
                Some(PreparedGpu {
                    exports: None,
                    manifest: vmlord_core::GpuShareManifest::default(),
                    assignment: GpuAssignment::Failed(GpuFailure::new(
                        vmlord_core::GpuStatusCode::HostNoAdapter,
                        "this host presents no GPU partition adapter",
                    )),
                })
            },
            move |_id, _mode| {
                counted.fetch_add(1, Ordering::Relaxed);
                Ok(())
            },
            GpuRuns::default(),
        );

        pipeline
            .start(&fixture.store, "dev", &fixture.vm_directory)
            .expect("the start must succeed");

        assert_eq!(
            attempts.load(Ordering::Relaxed),
            0,
            "there is nothing on this host to attach"
        );
    }

    #[test]
    fn a_start_that_failed_still_leaves_what_it_found_out_about_the_gpu() {
        let fixture = fixture("gpu-start-failed");
        let calls = fixture.calls.clone();
        let runs = GpuRuns::default();
        let pipeline = pipeline(&calls, Behavior::start_fails()).with_gpu(
            move |_mapping, _directory| Some(prepared(complete())),
            |_id, _mode| Ok(()),
            runs.clone(),
        );

        pipeline
            .start(&fixture.store, "dev", &fixture.vm_directory)
            .expect_err("this start fails");

        assert!(
            runs.snapshot(fixture.mapping.vm_id).assignment.is_some(),
            "what was found out before the start is worth more than the silence after it"
        );
    }

    #[test]
    fn attachment_paths_are_empty_for_a_configuration_without_attachments() {
        assert_eq!(
            attachment_paths(r#"{"VirtualMachine":{"Devices":{}}}"#).unwrap(),
            Vec::<PathBuf>::new()
        );
    }

    #[test]
    fn attachment_paths_skip_entries_without_a_path() {
        let document = serde_json::json!({
            "VirtualMachine": { "Devices": { "Scsi": { "Primary": { "Attachments": {
                "0": { "Type": "PassThru" },
                "1": { "Type": "Iso", "Path": "C:\\images\\installer.iso" }
            }}}}}
        })
        .to_string();

        assert_eq!(
            attachment_paths(&document).unwrap(),
            vec![PathBuf::from("C:\\images\\installer.iso")]
        );
    }

    #[test]
    fn attachment_paths_reject_malformed_json() {
        assert!(attachment_paths("not json").is_err());
    }

    #[test]
    fn a_nat_vm_is_registered_with_dhcp_before_it_starts() {
        let fixture = fixture_with("dhcp-registers", NetworkMode::Nat, None);
        let calls = fixture.calls.clone();

        pipeline(&calls, Behavior::default())
            .start(&fixture.store, "dev", &fixture.vm_directory)
            .expect("start should succeed");

        assert_eq!(
            *calls.dhcp.lock().unwrap(),
            vec![(MAC_ADDRESS.to_owned(), endpoint_address())]
        );
        let steps = calls.steps.lock().unwrap().clone();
        let dhcp = steps.iter().position(|step| *step == "dhcp").unwrap();
        let start = steps.iter().position(|step| *step == "start").unwrap();
        assert!(
            dhcp < start,
            "the guest must be able to get its address the moment it boots: {steps:?}"
        );
    }

    #[test]
    fn a_vm_without_a_network_is_not_registered_with_dhcp() {
        let fixture = fixture("dhcp-none");
        let calls = fixture.calls.clone();

        pipeline(&calls, Behavior::default())
            .start(&fixture.store, "dev", &fixture.vm_directory)
            .expect("start should succeed");

        assert!(calls.dhcp.lock().unwrap().is_empty());
    }

    #[test]
    fn a_dhcp_failure_fails_the_start() {
        // A VM that asked for a network and cannot be told its address would
        // come up with an adapter and no configuration at all.
        let fixture = fixture_with("dhcp-fails", NetworkMode::Nat, None);
        let calls = fixture.calls.clone();

        let error = pipeline(
            &calls,
            Behavior {
                fail_dhcp: true,
                ..Behavior::default()
            },
        )
        .start(&fixture.store, "dev", &fixture.vm_directory)
        .expect_err("a start that cannot serve the guest its address must fail");

        assert!(
            error.to_string().contains("injected DHCP failure"),
            "{error}"
        );
        assert!(calls.start.lock().unwrap().is_empty());
    }

    #[test]
    fn an_endpoint_without_an_address_fails_the_start() {
        let fixture = fixture_with("dhcp-no-address", NetworkMode::Nat, None);
        let calls = fixture.calls.clone();

        let pipeline = VmStartPipeline::for_test(
            console_launcher(&calls),
            |_id: &str, _path: &Path| Ok(()),
            |_id: &str, _configuration: &str| Ok(()),
            |_id: &str| Ok(()),
            |_vm_name: &str, _recorded: Option<Uuid>, _policy: EndpointPolicy| {
                Ok(VmNetworkAdapter {
                    endpoint_id: NEW_ENDPOINT_ID,
                    mac_address: MAC_ADDRESS.to_owned(),
                    address: None,
                })
            },
            {
                let calls = calls.clone();
                move |_store: &MetadataStore, mac: &str, address: &EndpointAddress| {
                    calls
                        .dhcp
                        .lock()
                        .unwrap()
                        .push((mac.to_owned(), address.clone()));
                    Ok(())
                }
            },
        );

        let error = pipeline
            .start(&fixture.store, "dev", &fixture.vm_directory)
            .expect_err("an endpoint HNS reports no address for must fail the start");

        assert!(
            error.to_string().contains(&NEW_ENDPOINT_ID.to_string()),
            "{error}"
        );
        assert!(calls.dhcp.lock().unwrap().is_empty());
    }
}
