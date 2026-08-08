//! Starting an HCS-backed virtual machine.

use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use uuid::Uuid;
use vmlord_core::{NetworkMode, RepositoryError};

use crate::{
    HcsClient, HcsSystem,
    hcn::HcnNetwork,
    hcn_endpoint::{EndpointAddress, HcnEndpoint},
    cleanup,
    hcs::{HCS_ACCESS_ALL, HcsStartFailure},
    hcs_config, layout,
    metadata::{MetadataStore, VmComputeSystemMapping},
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

type AccessGranter = Box<dyn Fn(&str, &Path) -> Result<(), RepositoryError>>;
type SystemStarter = Box<dyn Fn(&str, &str) -> Result<(), HcsStartFailure>>;
type EndpointProvider =
    Box<dyn Fn(&str, Option<Uuid>, EndpointPolicy) -> Result<VmNetworkAdapter, RepositoryError>>;

/// Starts VMs created by [`crate::VmCreationPipeline`].
pub struct VmStartPipeline {
    access_granter: AccessGranter,
    system_starter: SystemStarter,
    endpoint_provider: EndpointProvider,
}

impl VmStartPipeline {
    /// Creates a pipeline backed by the real HCS and HNS APIs.
    #[must_use]
    pub fn production() -> Self {
        Self {
            access_granter: Box::new(grant_vm_access),
            system_starter: Box::new(start_hcs_system),
            endpoint_provider: Box::new(ensure_endpoint),
        }
    }

    #[cfg(test)]
    fn for_test(
        access_granter: impl Fn(&str, &Path) -> Result<(), RepositoryError> + 'static,
        system_starter: impl Fn(&str, &str) -> Result<(), HcsStartFailure> + 'static,
        endpoint_provider: impl Fn(
            &str,
            Option<Uuid>,
            EndpointPolicy,
        ) -> Result<VmNetworkAdapter, RepositoryError>
        + 'static,
    ) -> Self {
        Self {
            access_granter: Box::new(access_granter),
            system_starter: Box::new(system_starter),
            endpoint_provider: Box::new(endpoint_provider),
        }
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
    pub fn start(
        &self,
        store: &MetadataStore,
        vm_name: &str,
        vm_directory: &Path,
    ) -> Result<(), RepositoryError> {
        let mapping = store.find_by_vm_name(vm_name)?.ok_or_else(|| {
            let error = RepositoryError::new(format!("no HCS mapping found for VM \"{vm_name}\""));
            log::error!("{error}");
            error
        })?;

        log::info!(
            "starting VM \"{}\" ({}) as HCS compute system \"{}\"",
            mapping.vm_name,
            mapping.vm_id,
            mapping.hcs_compute_system_id
        );

        let stored = self.read_configuration(&mapping, vm_directory)?;
        let (configuration, endpoint) = self.attach_network(
            store,
            &mapping,
            vm_directory,
            stored.clone(),
            mapping.endpoint_id,
            EndpointPolicy::Reuse,
        )?;
        self.grant_access_to_attachments(&mapping, &configuration)?;

        let failure = match (self.system_starter)(&mapping.hcs_compute_system_id, &configuration) {
            Ok(()) => {
                log::info!("started VM \"{}\" ({})", mapping.vm_name, mapping.vm_id);
                return Ok(());
            }
            Err(failure) => failure,
        };

        // Only one failure is recoverable, and only for a VM that has an
        // endpoint to replace.
        let busy = match failure {
            HcsStartFailure::EndpointBusy(error) => error,
            HcsStartFailure::Failed(error) => {
                log::error!("failed to start VM \"{}\": {error}", mapping.vm_name);
                return Err(error);
            }
        };
        let Some(endpoint) = endpoint else {
            log::error!("failed to start VM \"{}\": {busy}", mapping.vm_name);
            return Err(busy);
        };

        log::warn!(
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
        self.grant_access_to_attachments(&mapping, &configuration)?;
        (self.system_starter)(&mapping.hcs_compute_system_id, &configuration).map_err(
            |failure| {
                let error = failure.into_error();
                log::error!("failed to start VM \"{}\": {error}", mapping.vm_name);
                error
            },
        )?;

        log::info!("started VM \"{}\" ({})", mapping.vm_name, mapping.vm_id);
        Ok(())
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
            log::debug!(
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
                log::info!(
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

        log::info!(
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
            log::error!("{error}");
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
            log::error!("{error}");
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
            log::warn!(
                "the HCS configuration of VM \"{}\" attaches no files; \
                 starting without granting any VM access",
                mapping.vm_name
            );
        }
        for path in &paths {
            (self.access_granter)(&mapping.hcs_compute_system_id, path)?;
        }

        Ok(())
    }
}

impl Default for VmStartPipeline {
    fn default() -> Self {
        Self::production()
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
        log::error!("{error}");
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
                log::warn!(
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
                Some(address) => log::info!(
                    "replacing the occupied endpoint of VM \"{vm_name}\" with {id} on {}",
                    address.ip_address
                ),
                None => log::info!(
                    "replacing the occupied endpoint of VM \"{vm_name}\" with {id}"
                ),
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
        log::warn!(
            "HNS no longer knows the occupied endpoint of VM \"{vm_name}\"; \
             its replacement is created without an address of its own"
        );
        return Ok(None);
    };

    let address = endpoint.address()?;
    if address.is_none() {
        log::warn!(
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
fn start_hcs_system(id: &str, configuration: &str) -> Result<(), HcsStartFailure> {
    // The system handle must outlive the start operation it issued.
    let existing =
        HcsSystem::open_if_present(id, HCS_ACCESS_ALL).map_err(HcsStartFailure::Failed)?;
    let system = match existing {
        Some(system) => system,
        None => {
            log::info!(
                "HCS no longer knows compute system \"{id}\"; \
                 re-creating it from the stored configuration before starting it"
            );
            HcsClient::new()
                .create_system_and_wait(id, configuration, CREATE_TIMEOUT)
                .map_err(|failure| tear_down_after_a_failed_creation(id, failure))?
        }
    };

    system.start_and_wait(START_TIMEOUT)
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
        log::warn!("cleanup of the ambiguously-created compute system \"{id}\" also failed: {error}");
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
            atomic::{AtomicU64, Ordering},
        },
    };

    use uuid::Uuid;
    use vmlord_core::{NetworkMode, RepositoryError};

    use super::{EndpointPolicy, VmNetworkAdapter, VmStartPipeline, attachment_paths};
    use crate::{
        hcs::HcsStartFailure,
        metadata::{MetadataStore, VmComputeSystemMapping},
    };

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
        start: Arc<Mutex<Vec<(String, String)>>>,
        endpoint: Arc<Mutex<Vec<EndpointRequest>>>,
    }

    /// One call into the endpoint provider: the identifier it was offered and
    /// what it was asked to do with it.
    type EndpointRequest = (Option<Uuid>, EndpointPolicy);

    /// Which collaborators fail; by default none of them do.
    #[derive(Clone, Copy, Default)]
    struct Behavior {
        fail_start: bool,
        fail_endpoint: bool,
        /// How many leading starts fail with an occupied endpoint before the
        /// starter accepts one.
        busy_starts: usize,
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

    fn pipeline(calls: &Calls, behavior: Behavior) -> VmStartPipeline {
        VmStartPipeline::for_test(
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
                    calls.steps.lock().unwrap().push("start");
                    let mut started = calls.start.lock().unwrap();
                    started.push((id.to_owned(), configuration.to_owned()));
                    if started.len() <= behavior.busy_starts {
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
                    })
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
            vec!["endpoint", "grant", "grant", "start"]
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
            vec!["endpoint", "grant", "grant", "start", "endpoint", "grant", "grant", "start"]
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
}
