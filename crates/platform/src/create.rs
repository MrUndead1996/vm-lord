//! Transactional creation of an HCS-backed virtual machine.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

use uuid::Uuid;
use vmlord_agent_protocol::auth;
use vmlord_core::{
    BuildMonitor, BuildStep, CloudImage, Provisioning, RepositoryError, SshAccess, VmCreateRequest,
    VmSource,
};

use crate::{
    HcsClient,
    cleanup::{self, SystemTeardown},
    hcs_config::{self, HcsVmConfigBuilder},
    layout,
    metadata::{MetadataStore, VmComputeSystemMapping, guest_target_key},
    password_hash,
    vhd::create_dynamic_vhdx,
    vm_key,
};

const CREATE_TIMEOUT: Duration = Duration::from_secs(30);
const BYTES_PER_GIB: u64 = 1024 * 1024 * 1024;

type VhdCreator = Box<dyn Fn(&Path, u64) -> Result<(), RepositoryError> + Send + Sync>;
type AccessGranter = Box<dyn Fn(&str, &Path) -> Result<(), RepositoryError> + Send + Sync>;
type SystemCreator = Box<dyn Fn(&str, &str) -> Result<(), RepositoryError> + Send + Sync>;
type StateFileCreator = Box<dyn Fn(&Path, &Path) -> Result<(), RepositoryError> + Send + Sync>;
type AgentReader = Box<dyn Fn() -> Option<Vec<u8>> + Send + Sync>;

const AGENT_FILE_NAME: &str = "vmlord-agent";

/// Makes the VM's system disk out of a cloud image: fetch the image the release
/// means, then write it into a VHDX at the given path, sized for the VM rather
/// than for the image.
///
/// Injected rather than called directly because the fetching half is not
/// Windows's business: it lives in `vmlord-image`, which knows no Windows API,
/// and the composition root joins the two. The pipeline keeps the half that is
/// Windows -- writing into a VHDX through the disk it is attached as.
///
/// The monitor comes with it because both halves are long: the importer
/// reports `Downloading` and `WritingDisk` itself, and passes the cancellation
/// flag down to the download. Whoever runs a step is who reports it -- from
/// outside this closure the two are one call.
///
/// `Send + Sync` because creation runs on its own thread, and every seam of the
/// pipeline goes with it.
pub type CloudDiskImporter = Box<
    dyn Fn(&CloudImage, u64, &Path, &BuildMonitor) -> Result<(), RepositoryError> + Send + Sync,
>;

/// Removes what a creation had built if it leaves without disarming this.
///
/// The `Err` path disarms the guard and rolls back explicitly, because the
/// error the caller sees has to be able to say what the rollback itself could
/// not do. What is left for the guard is the path with no `Err` to carry a
/// message: a panic, which would otherwise leave a VM directory -- and
/// possibly a compute system -- that nothing else knows about.
///
/// `catch_unwind` would be the other way to do this, and cannot be: the
/// pipeline's seams are boxed closures, which are not `UnwindSafe`, and
/// `AssertUnwindSafe` would assert exactly what needs proving.
struct CreationGuard<'a> {
    vm_directory: &'a Path,
    teardown: &'a SystemTeardown,
    hcs_compute_system_id: &'a str,
    system_created: bool,
    armed: bool,
}

impl Drop for CreationGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        tracing::error!(
            "creating the VM at {} was interrupted; removing what it had created",
            self.vm_directory.display()
        );
        if self.system_created
            && let Err(error) = (self.teardown)(self.hcs_compute_system_id)
        {
            tracing::error!(
                "the compute system \"{}\" of the interrupted creation could not be \
                 torn down: {error}",
                self.hcs_compute_system_id
            );
        }
        if let Err(error) = cleanup::remove_vm_directory(self.vm_directory) {
            tracing::error!(
                "the directory of the interrupted creation at {} could not be removed: {error}",
                self.vm_directory.display()
            );
        }
    }
}

/// Orchestrates the multi-step, transactional creation of an HCS-backed VM.
///
/// Every step after the VM directory is created is wrapped so that any
/// failure rolls back: the compute system (if created) is torn down and the
/// VM directory is removed.
pub struct VmCreationPipeline {
    vhd_creator: VhdCreator,
    cloud_disk: CloudDiskImporter,
    access_granter: AccessGranter,
    state_file_creator: StateFileCreator,
    system_creator: SystemCreator,
    system_teardown: SystemTeardown,
    agent_reader: AgentReader,
}

impl VmCreationPipeline {
    /// Creates a pipeline backed by the real VHDX and HCS APIs, importing cloud
    /// images through `cloud_disk`.
    ///
    /// The importer is required rather than optional: a pipeline that silently
    /// cannot build a VM from a cloud image is a state better left unspellable.
    #[must_use]
    pub fn production(cloud_disk: CloudDiskImporter) -> Self {
        Self {
            vhd_creator: Box::new(create_dynamic_vhdx),
            cloud_disk,
            access_granter: Box::new(grant_vm_access),
            state_file_creator: Box::new(create_state_files),
            system_creator: Box::new(create_hcs_system),
            system_teardown: Box::new(cleanup::teardown_compute_system),
            agent_reader: Box::new(read_agent_beside_executable),
        }
    }

    #[cfg(test)]
    fn for_test(
        vhd_creator: impl Fn(&Path, u64) -> Result<(), RepositoryError> + Send + Sync + 'static,
        cloud_disk: impl Fn(&CloudImage, u64, &Path, &BuildMonitor) -> Result<(), RepositoryError>
        + Send
        + Sync
        + 'static,
        access_granter: impl Fn(&str, &Path) -> Result<(), RepositoryError> + Send + Sync + 'static,
        state_file_creator: impl Fn(&Path, &Path) -> Result<(), RepositoryError> + Send + Sync + 'static,
        system_creator: impl Fn(&str, &str) -> Result<(), RepositoryError> + Send + Sync + 'static,
        system_teardown: impl Fn(&str) -> Result<(), RepositoryError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            vhd_creator: Box::new(vhd_creator),
            cloud_disk: Box::new(cloud_disk),
            access_granter: Box::new(access_granter),
            state_file_creator: Box::new(state_file_creator),
            system_creator: Box::new(system_creator),
            system_teardown: Box::new(system_teardown),
            agent_reader: Box::new(|| Some(b"test agent".to_vec())),
        }
    }

    /// Creates a new VM under `vm_directory`, registering it in `store` once
    /// every step has succeeded. Rolls back all partial state on failure.
    pub fn create(
        &self,
        store: &MetadataStore,
        request: &VmCreateRequest,
        vm_directory: &Path,
        monitor: &BuildMonitor,
    ) -> Result<VmComputeSystemMapping, RepositoryError> {
        request.validate()?;

        if store.find_by_vm_name(&request.name)?.is_some() {
            return Err(RepositoryError::new(format!(
                "VM \"{}\" already exists",
                request.name
            )));
        }
        if vm_directory.exists() {
            return Err(RepositoryError::new(format!(
                "VM directory already exists: {}",
                vm_directory.display()
            )));
        }

        let vm_id = Uuid::new_v4();
        let hcs_compute_system_id = format!("vmlord-{}", vm_id.as_simple());
        let system_disk_path = layout::system_disk_path(vm_directory);
        let seed_path = layout::seed_path(vm_directory);
        let agent = match &request.source {
            VmSource::CloudImage { .. } => (self.agent_reader)(),
            VmSource::LocalMedia { .. } => None,
        };
        let tools_path = agent.as_ref().map(|_| layout::tools_path(vm_directory));
        let guest_state_path = layout::guest_state_path(vm_directory);
        let runtime_state_path = layout::runtime_state_path(vm_directory);
        // Rejects an unsupported request (name, GPU/network mode, ...) before
        // any filesystem or HCS side effect.
        let configuration = HcsVmConfigBuilder::build(
            request,
            &system_disk_path,
            &seed_path,
            tools_path.as_deref(),
            &hcs_config::StateFilePaths {
                guest_state: &guest_state_path,
                runtime_state: &runtime_state_path,
            },
            vm_id,
        )?;
        let media_path = hcs_config::media_path(request, &seed_path).to_path_buf();

        tracing::info!(
            "creating VM \"{}\" ({vm_id}) as HCS compute system \"{hcs_compute_system_id}\"",
            request.name
        );

        let disk_directory = system_disk_path
            .parent()
            .expect("system disk path always has a parent under vm_directory");
        fs::create_dir_all(disk_directory).map_err(|error| {
            let error = RepositoryError::new(format!(
                "failed to create VM directory {}: {error}",
                vm_directory.display()
            ));
            tracing::error!("{error}");
            error
        })?;

        let mapping = VmComputeSystemMapping {
            vm_id,
            vm_name: request.name.clone(),
            hcs_compute_system_id: hcs_compute_system_id.clone(),
            disk_gb: request.disk_gb,
            // No endpoint yet: it is created on the first start, so that a VM
            // that is never started never takes an address.
            endpoint_id: None,
            network_mode: request.network_mode,
            // What a person asked cloud-init to set up, recorded as what a
            // later connection has to be told -- including the port, which is
            // the one the seed below configures the daemon with. Locally
            // installed media gets none: VMLord promises nothing about the
            // system inside it.
            ssh: match &request.source {
                VmSource::LocalMedia { .. } => None,
                VmSource::CloudImage { provisioning, .. } => provisioning.ssh_config(),
            },
            // How that daemon is carried, taken from the same profile the seed
            // is printed from. Moving the port later writes the drop-ins named
            // here, so recording them is what keeps a reconfiguration from
            // guessing which distribution answered.
            ssh_daemon: match &request.source {
                VmSource::LocalMedia { .. } => None,
                VmSource::CloudImage { image, .. } => Some(image.profile.ssh.clone()),
            },
            gpu_mode: request.gpu_mode,
            // The desktop the seed below is asked to install, and the fact
            // that nothing has installed it yet. A build that never reaches
            // the guest leaves `Pending` behind, which is what later offers a
            // retry instead of reporting a desktop that is not there.
            desktop_profile: request.desktop_profile(),
            display_provisioning: vmlord_core::DisplayProvisioning::requested(
                request.desktop_profile(),
            ),
            display_mode: None,
            // The same three facts for the same reason: a payload is chosen
            // before the guest that will use it has booted, and only the
            // creation knows what system was installed.
            guest_target: guest_target_key(&request.source),
        };

        let mut guard = CreationGuard {
            vm_directory,
            teardown: &self.system_teardown,
            hcs_compute_system_id: &hcs_compute_system_id,
            system_created: false,
            armed: true,
        };
        // The disk is made the size the VM asked for, whichever way it is
        // filled: an empty VHDX and an imported image agree on this much.
        let disk_size_bytes = u64::from(request.disk_gb) * BYTES_PER_GIB;
        let result = (|guard: &mut CreationGuard| {
            monitor.check_cancelled()?;
            match &request.source {
                VmSource::LocalMedia { .. } => {
                    monitor.report(BuildStep::WritingDisk);
                    (self.vhd_creator)(&system_disk_path, disk_size_bytes)?;
                    if !media_path.is_file() {
                        return Err(RepositoryError::new(format!(
                            "VM image no longer exists: {}",
                            media_path.display()
                        )));
                    }
                }
                VmSource::CloudImage {
                    image,
                    provisioning,
                } => {
                    tracing::debug!(
                        "importing {} {} into {}",
                        image.profile.name,
                        image.release,
                        system_disk_path.display()
                    );
                    // The importer reports `Downloading` and `WritingDisk`
                    // itself: both happen inside this one call.
                    (self.cloud_disk)(image, disk_size_bytes, &system_disk_path, monitor)?;
                    monitor.check_cancelled()?;
                    monitor.report(BuildStep::Provisioning);
                    write_provisioning(
                        vm_directory,
                        &seed_path,
                        &request.name,
                        &hcs_compute_system_id,
                        image,
                        provisioning,
                        agent.as_deref(),
                    )?;
                }
            }
            monitor.check_cancelled()?;
            // Local media reaches provisioning here: it writes no seed and no
            // keys, but the configuration and the grants are still files
            // written for the VM.
            monitor.report(BuildStep::Provisioning);

            fs::write(layout::configuration_path(vm_directory), &configuration).map_err(
                |error| RepositoryError::new(format!("failed to write HCS configuration: {error}")),
            )?;

            // The firmware and runtime state stores the configuration names.
            // HCS makes them, rather than this pipeline writing empty files:
            // both have a format only Hyper-V knows, and a compute system is
            // refused outright if what it is pointed at is not one.
            (self.state_file_creator)(&guest_state_path, &runtime_state_path)?;

            // Hyper-V opens VM-owned files under the VM's own security
            // principal, not the creating user's token: without this, start
            // fails with access denied even though both files exist and are
            // readable by this (elevated) process.
            (self.access_granter)(&hcs_compute_system_id, &system_disk_path)?;
            (self.access_granter)(&hcs_compute_system_id, &media_path)?;
            if let Some(tools_path) = &tools_path {
                (self.access_granter)(&hcs_compute_system_id, tools_path)?;
            }
            // The worker writes to both of these, so the grant is what lets it
            // start at all -- and, on a reset, put the machine back together.
            (self.access_granter)(&hcs_compute_system_id, &guest_state_path)?;
            (self.access_granter)(&hcs_compute_system_id, &runtime_state_path)?;

            monitor.check_cancelled()?;
            monitor.report(BuildStep::Registering);
            (self.system_creator)(&hcs_compute_system_id, &configuration)?;
            guard.system_created = true;

            store.insert(mapping.clone())?;
            Ok(())
        })(&mut guard);

        guard.armed = false;
        match result {
            Ok(()) => {
                tracing::info!("created VM \"{}\" ({vm_id})", request.name);
                Ok(mapping)
            }
            Err(error) => Err(self.rollback(vm_directory, &mapping, guard.system_created, error)),
        }
    }

    fn rollback(
        &self,
        vm_directory: &Path,
        mapping: &VmComputeSystemMapping,
        system_created: bool,
        error: RepositoryError,
    ) -> RepositoryError {
        let mut failures = vec![error.to_string()];

        if system_created
            && let Err(teardown_error) = (self.system_teardown)(&mapping.hcs_compute_system_id)
        {
            failures.push(format!("rollback teardown also failed: {teardown_error}"));
        }
        if let Err(remove_error) = cleanup::remove_vm_directory(vm_directory) {
            failures.push(format!("rollback also failed: {remove_error}"));
        }

        cleanup::combine_failures(
            &format!("creation of VM \"{}\" failed", mapping.vm_name),
            failures,
        )
    }
}

fn grant_vm_access(id: &str, path: &Path) -> Result<(), RepositoryError> {
    HcsClient::new().grant_vm_access(id, path)
}

fn create_state_files(guest_state: &Path, runtime_state: &Path) -> Result<(), RepositoryError> {
    HcsClient::new().create_state_files(guest_state, runtime_state)
}

fn create_hcs_system(id: &str, configuration: &str) -> Result<(), RepositoryError> {
    let (system, operation) = HcsClient::new().create_system(id, configuration)?;
    let result = operation.wait_for_completion(CREATE_TIMEOUT);
    // Persisting past this handle close relies on the configuration setting
    // `ShouldTerminateOnLastHandleClosed: false` (see `hcs_config`); with
    // `true`, HCS discards even a never-started system as soon as the
    // creating handle closes.
    drop(system);
    match result {
        Ok(_document) => Ok(()),
        Err(error) => {
            // The outcome is ambiguous: `HcsCreateComputeSystem` may already
            // have succeeded, so the system could persist in HCS without a
            // metadata entry. It belongs to this operation; tear it down
            // best-effort before reporting the failure.
            let mut message = error.to_string();
            if let Err(teardown_error) = cleanup::teardown_compute_system(id) {
                message.push_str(&format!(
                    "; cleanup of the ambiguously-created compute system also failed: {teardown_error}"
                ));
            }
            Err(RepositoryError::new(message))
        }
    }
}

/// Reads the guest agent bundled beside the running VMLord executable.
///
/// A missing binary is a packaging problem but not a reason to reject a cloud
/// VM: its normal cloud-init provisioning can still complete without the
/// optional agent service.
fn read_agent_beside_executable() -> Option<Vec<u8>> {
    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(error) => {
            tracing::warn!(
                "cannot locate the VMLord executable to find {AGENT_FILE_NAME}: {error}"
            );
            return None;
        }
    };
    let agent_path: PathBuf = executable.with_file_name(AGENT_FILE_NAME);
    match fs::read(&agent_path) {
        Ok(agent) => Some(agent),
        Err(error) => {
            tracing::warn!(
                "cannot read the guest agent at {}: {error}; cloud VMs will not include a tools volume",
                agent_path.display()
            );
            None
        }
    }
}

/// Writes everything the guest's first boot reads: the VM's key pair, its
/// agent secret, and the seed volume carrying the cloud-config documents.
///
/// The password is hashed here rather than carried further: what reaches
/// `vmlord-seed` -- and through it the volume that stays attached to a running
/// VM -- is a `$6$` entry, never the plaintext.
fn write_provisioning(
    vm_directory: &Path,
    seed_path: &Path,
    vm_name: &str,
    instance_id: &str,
    image: &CloudImage,
    provisioning: &Provisioning,
    agent: Option<&[u8]>,
) -> Result<(), RepositoryError> {
    let authorized_key = match provisioning.ssh {
        SshAccess::Enabled {
            deploy_key: true, ..
        } => {
            let pair = vmlord_keys::generate(vm_name)?;
            vm_key::write_key_pair(vm_directory, &pair)?;
            Some(pair.public_openssh().to_owned())
        }
        _ => None,
    };

    let password_hash = provisioning
        .password
        .as_ref()
        .map(password_hash::hash_password)
        .transpose()?;

    let agent_secret = agent.map(|_| auth::Secret::generate().to_base64());
    // Minted here and written twice: once for the host, which is what verifies
    // a session, and once into the seed, which is the only way it reaches the
    // guest. It is never rotated -- a VM's secret lives as long as the VM --
    // and never travels on the agent protocol itself.
    if let Some(agent_secret) = &agent_secret {
        let agent_secret_path = layout::agent_secret_path(vm_directory);
        write_restricted(
            &agent_secret_path,
            format!("{}\n", agent_secret.as_str()).as_bytes(),
            "the agent secret",
        )?;
        tracing::debug!("wrote the agent secret at {}", agent_secret_path.display());
    }

    let seed = vmlord_seed::build(&vmlord_seed::SeedRequest {
        vm_name,
        instance_id,
        username: &provisioning.username,
        password_hash: password_hash.as_deref(),
        authorized_key: authorized_key.as_deref(),
        ssh: provisioning.ssh,
        locale: &provisioning.locale,
        keyboard: &provisioning.keyboard,
        timezone: &provisioning.timezone,
        admin_group: &image.profile.admin_group,
        ssh_daemon: &image.profile.ssh,
        agent_secret: agent_secret.as_ref().map(|secret| secret.as_str()),
        // What this distribution says installing the chosen desktop takes,
        // and nothing when it was not asked for one -- or when the profile
        // describes no desktop at all.
        desktop_packages: image
            .profile
            .desktop_for(provisioning.desktop)
            .map_or(&[][..], |desktop| desktop.packages.as_slice()),
    });

    write_restricted(seed_path, &vmlord_seed::image(&seed), "the cloud-init seed")?;
    tracing::debug!("wrote the cloud-init seed at {}", seed_path.display());
    if let Some(agent) = agent {
        let tools_path = layout::tools_path(vm_directory);
        fs::write(&tools_path, vmlord_seed::tools_image(agent))
            .map_err(|error| write_failure(&tools_path, "the cloud-init tools volume", &error))?;
        tracing::debug!(
            "wrote the cloud-init tools volume at {}",
            tools_path.display()
        );
    }
    Ok(())
}

/// Writes a file of the VM's under the same DACL the private key gets.
///
/// Both callers hold a secret: the seed carries the password's SHA-512-crypt
/// entry and the guest's copy of the agent secret, and `agent.secret` is the
/// host's copy of the same. The storage root is a path the owner chooses --
/// point it somewhere with an inherited `Users:(R)` and either file would be
/// readable by every local account. The ordering is `vm_key`'s: create the
/// file empty, narrow it, and only then write, so that the bytes never exist
/// under permissions wider than the ones they end up with.
///
/// The DACL is protected, which severs what the parent hands down but not what
/// is added explicitly afterwards -- `HcsGrantVmAccess` still puts the VM's own
/// SID on the seed, which is why the guest can go on reading it. Nothing
/// grants the VM its way onto `agent.secret`: the guest has its own copy.
fn write_restricted(path: &Path, bytes: &[u8], description: &str) -> Result<(), RepositoryError> {
    let mut file =
        fs::File::create(path).map_err(|error| write_failure(path, description, &error))?;
    vm_key::restrict_to_owner(path)?;
    file.write_all(bytes)
        .and_then(|()| file.flush())
        .map_err(|error| write_failure(path, description, &error))
}

fn write_failure(path: &Path, description: &str, error: &std::io::Error) -> RepositoryError {
    let error = RepositoryError::new(format!(
        "failed to write {description} at {}: {error}",
        path.display()
    ));
    tracing::error!("{error}");
    error
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

    use vmlord_core::{
        CloudImage, GpuMode, NetworkMode, Password, Provisioning, SshAccess, SshPort,
        VmCreateRequest, VmSource,
    };

    use vmlord_core::{BuildMonitor, BuildStep};

    use super::VmCreationPipeline;
    use crate::MetadataStore;

    fn monitor() -> BuildMonitor {
        BuildMonitor::new(BuildStep::WritingDisk)
    }

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn path(&self) -> &PathBuf {
            &self.0
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temp_root(label: &str) -> TempRoot {
        static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "vmlord-create-test-{label}-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("test root should be created");
        TempRoot(path)
    }

    #[derive(Clone, Default)]
    struct Calls {
        vhd: Arc<Mutex<Vec<(PathBuf, u64)>>>,
        cloud: Arc<Mutex<Vec<(String, u64, PathBuf)>>>,
        grant: Arc<Mutex<Vec<(String, PathBuf)>>>,
        state: Arc<Mutex<Vec<(PathBuf, PathBuf)>>>,
        create: Arc<Mutex<Vec<(String, String)>>>,
        teardown: Arc<Mutex<Vec<String>>>,
    }

    struct Fixture {
        _root: TempRoot,
        store: MetadataStore,
        vm_directory: PathBuf,
        request: VmCreateRequest,
        image_path: PathBuf,
        calls: Calls,
    }

    fn fixture(label: &str) -> Fixture {
        let root = temp_root(label);
        let image_path = root.path().join("installer.iso");
        fs::write(&image_path, b"iso").expect("test image should be written");
        let request = VmCreateRequest {
            name: "test-vm".into(),
            source: VmSource::LocalMedia {
                path: image_path.to_string_lossy().into_owned(),
            },
            ram_mb: 512,
            disk_gb: 1,
            cpu_cores: 1,
            gpu_mode: GpuMode::None,
            network_mode: NetworkMode::None,
        };
        Fixture {
            store: MetadataStore::new(root.path().join("vm-mapping.json")),
            vm_directory: root.path().join("vm"),
            request,
            image_path,
            calls: Calls::default(),
            _root: root,
        }
    }

    fn pipeline(
        calls: &Calls,
        fail_vhd: bool,
        fail_cloud: bool,
        fail_create: bool,
    ) -> VmCreationPipeline {
        VmCreationPipeline::for_test(
            {
                let calls = calls.clone();
                move |path, size| {
                    calls.vhd.lock().unwrap().push((path.to_path_buf(), size));
                    if fail_vhd {
                        return Err(vmlord_core::RepositoryError::new("injected disk failure"));
                    }
                    fs::write(path, b"vhdx")
                        .map_err(|error| vmlord_core::RepositoryError::new(format!("vhd: {error}")))
                }
            },
            {
                let calls = calls.clone();
                move |image: &vmlord_core::CloudImage,
                      size,
                      path: &std::path::Path,
                      _: &BuildMonitor| {
                    calls.cloud.lock().unwrap().push((
                        image.release.clone(),
                        size,
                        path.to_path_buf(),
                    ));
                    if fail_cloud {
                        return Err(vmlord_core::RepositoryError::new(
                            "injected cloud image failure",
                        ));
                    }
                    fs::write(path, b"imported vhdx").map_err(|error| {
                        vmlord_core::RepositoryError::new(format!("import: {error}"))
                    })
                }
            },
            {
                let calls = calls.clone();
                move |id, path| {
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
                move |guest_state: &std::path::Path, runtime_state: &std::path::Path| {
                    calls
                        .state
                        .lock()
                        .unwrap()
                        .push((guest_state.to_path_buf(), runtime_state.to_path_buf()));
                    // Empty stand-ins: the production creator is an HCS call,
                    // and what the tests check is that the paths reached it
                    // before the compute system was created.
                    fs::write(guest_state, b"vmgs").unwrap();
                    fs::write(runtime_state, b"vmrs").unwrap();
                    Ok(())
                }
            },
            {
                let calls = calls.clone();
                move |id, config| {
                    calls
                        .create
                        .lock()
                        .unwrap()
                        .push((id.to_owned(), config.to_owned()));
                    if fail_create {
                        // Simulate the production contract: an ambiguous
                        // create failure tears the system down inside the
                        // creator itself before returning the error.
                        calls.teardown.lock().unwrap().push(id.to_owned());
                        return Err(vmlord_core::RepositoryError::new(
                            "create compute system timed out after 30s",
                        ));
                    }
                    Ok(())
                }
            },
            {
                let calls = calls.clone();
                move |id| {
                    calls.teardown.lock().unwrap().push(id.to_owned());
                    Ok(())
                }
            },
        )
    }

    fn pipeline_without_agent(
        calls: &Calls,
        fail_vhd: bool,
        fail_cloud: bool,
        fail_create: bool,
    ) -> VmCreationPipeline {
        let mut pipeline = pipeline(calls, fail_vhd, fail_cloud, fail_create);
        pipeline.agent_reader = Box::new(|| None);
        pipeline
    }

    /// A pipeline whose seams record the step the monitor was reporting when
    /// each of them ran.
    fn observing_pipeline(
        calls: &Calls,
        monitor: &BuildMonitor,
        seen: &Arc<Mutex<Vec<BuildStep>>>,
    ) -> VmCreationPipeline {
        // Boxed so the same recorder can be handed to three seams; a plain
        // closure would be moved into the first of them.
        let record: Arc<dyn Fn() + Send + Sync> = Arc::new({
            let monitor = monitor.clone();
            let seen = Arc::clone(seen);
            move || seen.lock().unwrap().push(monitor.snapshot().step)
        });
        VmCreationPipeline::for_test(
            {
                let calls = calls.clone();
                let record = Arc::clone(&record);
                move |path: &std::path::Path, size| {
                    record();
                    calls.vhd.lock().unwrap().push((path.to_path_buf(), size));
                    fs::write(path, b"vhdx")
                        .map_err(|error| vmlord_core::RepositoryError::new(format!("vhd: {error}")))
                }
            },
            |_: &CloudImage, _, _: &std::path::Path, _: &BuildMonitor| Ok(()),
            {
                let record = Arc::clone(&record);
                move |_: &str, _: &std::path::Path| {
                    record();
                    Ok(())
                }
            },
            {
                let record = Arc::clone(&record);
                move |_: &std::path::Path, _: &std::path::Path| {
                    record();
                    Ok(())
                }
            },
            {
                let record = Arc::clone(&record);
                move |_: &str, _: &str| {
                    record();
                    Ok(())
                }
            },
            |_| Ok(()),
        )
    }

    /// A build runs on its own thread, and a panic there would otherwise leave
    /// the VM's directory behind for good: nothing else knows it was ever
    /// being created.
    #[test]
    fn a_panicking_step_leaves_no_vm_directory_behind() {
        let fixture = fixture("panicking");
        let calls = fixture.calls.clone();
        let pipeline = VmCreationPipeline::for_test(
            {
                let calls = calls.clone();
                move |path: &std::path::Path, size| {
                    calls.vhd.lock().unwrap().push((path.to_path_buf(), size));
                    fs::write(path, b"vhdx").unwrap();
                    Ok(())
                }
            },
            |_: &CloudImage, _, _: &std::path::Path, _: &BuildMonitor| Ok(()),
            |_: &str, _: &std::path::Path| Ok(()),
            |_: &std::path::Path, _: &std::path::Path| Ok(()),
            |_: &str, _: &str| panic!("the HCS client panicked"),
            {
                let calls = calls.clone();
                move |id: &str| {
                    calls.teardown.lock().unwrap().push(id.to_owned());
                    Ok(())
                }
            },
        );

        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = pipeline.create(
                &fixture.store,
                &fixture.request,
                &fixture.vm_directory,
                &monitor(),
            );
        }));

        assert!(panicked.is_err(), "the panic must reach the caller");
        assert!(
            !fixture.vm_directory.exists(),
            "the guard must remove what the interrupted build had created"
        );
        assert!(fixture.store.list().unwrap().is_empty());
    }

    #[test]
    fn a_local_media_build_reports_its_steps_in_order() {
        let fixture = fixture("steps-local");
        let calls = fixture.calls.clone();
        let monitor = monitor();
        // Each injected seam records the step the pipeline had reported by the
        // time it was called, which is what "in order" can be checked against.
        let seen: Arc<Mutex<Vec<BuildStep>>> = Arc::new(Mutex::new(Vec::new()));
        let pipeline = observing_pipeline(&calls, &monitor, &seen);

        pipeline
            .create(
                &fixture.store,
                &fixture.request,
                &fixture.vm_directory,
                &monitor,
            )
            .expect("creation should succeed");

        assert_eq!(
            seen.lock().unwrap().as_slice(),
            &[
                BuildStep::WritingDisk,
                BuildStep::Provisioning,
                BuildStep::Provisioning,
                BuildStep::Provisioning,
                BuildStep::Provisioning,
                BuildStep::Provisioning,
                BuildStep::Registering,
            ],
            "the disk, then the files written for the VM -- its state files \
             included -- and their grants, then the compute system"
        );
        assert_eq!(monitor.snapshot().step, BuildStep::Registering);
    }

    #[test]
    fn a_cancelled_build_stops_before_touching_the_disk() {
        let fixture = fixture("cancelled-early");
        let calls = fixture.calls.clone();
        let monitor = monitor();
        monitor.cancel();
        let pipeline = pipeline(&calls, false, false, false);

        let error = pipeline
            .create(
                &fixture.store,
                &fixture.request,
                &fixture.vm_directory,
                &monitor,
            )
            .expect_err("a cancelled build must not create a VM");

        assert!(error.to_string().contains("cancelled"), "got {error}");
        assert!(calls.vhd.lock().unwrap().is_empty());
        assert!(calls.create.lock().unwrap().is_empty());
        assert!(!fixture.vm_directory.exists());
        assert!(fixture.store.list().unwrap().is_empty());
    }

    #[test]
    fn a_build_cancelled_while_writing_the_disk_is_rolled_back() {
        let fixture = fixture("cancelled-midway");
        let calls = fixture.calls.clone();
        let monitor = monitor();
        let pipeline = VmCreationPipeline::for_test(
            {
                let calls = calls.clone();
                let monitor = monitor.clone();
                move |path: &std::path::Path, size| {
                    calls.vhd.lock().unwrap().push((path.to_path_buf(), size));
                    fs::write(path, b"vhdx").unwrap();
                    // The user pressed Cancel while the disk was being written.
                    monitor.cancel();
                    Ok(())
                }
            },
            |_: &CloudImage, _, _: &std::path::Path, _: &BuildMonitor| Ok(()),
            |_: &str, _: &std::path::Path| Ok(()),
            |_: &std::path::Path, _: &std::path::Path| Ok(()),
            {
                let calls = calls.clone();
                move |id: &str, config: &str| {
                    calls
                        .create
                        .lock()
                        .unwrap()
                        .push((id.to_owned(), config.to_owned()));
                    Ok(())
                }
            },
            |_| Ok(()),
        );

        let error = pipeline
            .create(
                &fixture.store,
                &fixture.request,
                &fixture.vm_directory,
                &monitor,
            )
            .expect_err("a cancelled build must not create a VM");

        assert!(error.to_string().contains("cancelled"), "got {error}");
        assert!(
            calls.create.lock().unwrap().is_empty(),
            "cancellation must be noticed before the compute system is created"
        );
        assert!(!fixture.vm_directory.exists());
        assert!(fixture.store.list().unwrap().is_empty());
    }

    #[test]
    fn a_local_media_vm_never_reaches_the_cloud_image_importer() {
        let fixture = fixture("no-cloud");
        let calls = fixture.calls.clone();
        let pipeline = pipeline(&calls, false, false, false);

        pipeline
            .create(
                &fixture.store,
                &fixture.request,
                &fixture.vm_directory,
                &monitor(),
            )
            .expect("creation should succeed");

        assert!(calls.cloud.lock().unwrap().is_empty());
        assert_eq!(calls.vhd.lock().unwrap().len(), 1);
    }

    #[test]
    fn creates_and_registers_a_vm_through_all_stages() {
        let fixture = fixture("happy");
        let calls = fixture.calls.clone();
        let pipeline = pipeline(&calls, false, false, false);

        let mapping = pipeline
            .create(
                &fixture.store,
                &fixture.request,
                &fixture.vm_directory,
                &monitor(),
            )
            .expect("creation should succeed");

        assert_eq!(mapping.vm_name, "test-vm");
        assert_eq!(
            mapping.hcs_compute_system_id,
            format!("vmlord-{}", mapping.vm_id.as_simple())
        );
        assert!(fixture.vm_directory.join("config.json").is_file());
        assert!(
            fixture
                .vm_directory
                .join("disks")
                .join("system.vhdx")
                .is_file()
        );
        assert_eq!(fixture.store.list().unwrap(), vec![mapping.clone()]);

        let create_calls = calls.create.lock().unwrap();
        assert_eq!(create_calls.len(), 1);
        assert_eq!(create_calls[0].0, mapping.hcs_compute_system_id);
        let config: serde_json::Value =
            serde_json::from_str(&create_calls[0].1).expect("HCS config should be valid JSON");
        assert_eq!(
            config.pointer("/VirtualMachine/Devices/Scsi/Primary/Attachments/0/Path"),
            Some(&serde_json::json!(
                fixture.vm_directory.join("disks").join("system.vhdx")
            ))
        );
        // Neither the plaintext password nor the `$6$` hash it becomes may
        // reach HCS or the `config.json` the pipeline leaves behind: the hash
        // travels in the seed volume alone.
        let stored = fs::read_to_string(fixture.vm_directory.join("config.json")).unwrap();
        for document in [&create_calls[0].1, &stored] {
            assert!(!document.contains("hunter2"), "got {document}");
            assert!(!document.contains("$6$"), "got {document}");
        }
        assert!(calls.teardown.lock().unwrap().is_empty());

        let grant_calls = calls.grant.lock().unwrap();
        assert_eq!(
            grant_calls.as_slice(),
            &[
                (
                    mapping.hcs_compute_system_id.clone(),
                    fixture.vm_directory.join("disks").join("system.vhdx")
                ),
                (
                    mapping.hcs_compute_system_id.clone(),
                    fixture.image_path.clone()
                ),
                (
                    mapping.hcs_compute_system_id.clone(),
                    fixture.vm_directory.join("vm.vmgs")
                ),
                (
                    mapping.hcs_compute_system_id.clone(),
                    fixture.vm_directory.join("vm.vmrs")
                ),
            ],
            "the VM must be granted access to its disk, its installer image and \
             its state files before HCS create"
        );
    }

    /// The two state files are HCS's to make, and they have to exist -- and be
    /// the VM's to write -- before the compute system that names them does.
    /// A VM created without them boots and refuses to reboot: bug #110.
    #[test]
    fn a_created_vm_gets_the_state_files_its_configuration_names() {
        let fixture = fixture("state-files");
        let calls = fixture.calls.clone();
        let pipeline = pipeline(&calls, false, false, false);

        let mapping = pipeline
            .create(
                &fixture.store,
                &fixture.request,
                &fixture.vm_directory,
                &monitor(),
            )
            .expect("creation should succeed");

        assert_eq!(
            calls.state.lock().unwrap().as_slice(),
            &[(
                fixture.vm_directory.join("vm.vmgs"),
                fixture.vm_directory.join("vm.vmrs")
            )]
        );

        let create_calls = calls.create.lock().unwrap();
        let configuration: serde_json::Value = serde_json::from_str(&create_calls[0].1).unwrap();
        assert_eq!(
            configuration.pointer("/VirtualMachine/GuestState"),
            Some(&serde_json::json!({
                "GuestStateFilePath": fixture.vm_directory.join("vm.vmgs"),
                "RuntimeStateFilePath": fixture.vm_directory.join("vm.vmrs")
            })),
            "the compute system must be told where its own state lives"
        );
        drop(create_calls);

        let grants = calls.grant.lock().unwrap();
        assert!(
            grants.contains(&(
                mapping.hcs_compute_system_id.clone(),
                fixture.vm_directory.join("vm.vmgs")
            )) && grants.contains(&(
                mapping.hcs_compute_system_id.clone(),
                fixture.vm_directory.join("vm.vmrs")
            )),
            "the worker writes to both files, so both need the VM's own grant: {grants:?}"
        );
    }

    #[test]
    fn rejects_a_duplicate_vm_name_before_any_side_effect() {
        let fixture = fixture("duplicate");
        let calls = fixture.calls.clone();
        let pipeline = pipeline(&calls, false, false, false);
        pipeline
            .create(
                &fixture.store,
                &fixture.request,
                &fixture.vm_directory,
                &monitor(),
            )
            .expect("the first creation should succeed");

        let other_directory = fixture.vm_directory.with_file_name("vm-2");
        let error = pipeline
            .create(
                &fixture.store,
                &fixture.request,
                &other_directory,
                &monitor(),
            )
            .expect_err("a duplicate VM name must be rejected");

        assert!(error.to_string().contains("test-vm"));
        assert_eq!(calls.vhd.lock().unwrap().len(), 1);
        assert_eq!(calls.create.lock().unwrap().len(), 1);
        assert_eq!(fixture.store.list().unwrap().len(), 1);
    }

    #[test]
    fn rejects_when_the_vm_directory_already_exists() {
        let fixture = fixture("existing-dir");
        let calls = fixture.calls.clone();
        fs::create_dir_all(&fixture.vm_directory).unwrap();
        let pipeline = pipeline(&calls, false, false, false);

        let error = pipeline
            .create(
                &fixture.store,
                &fixture.request,
                &fixture.vm_directory,
                &monitor(),
            )
            .expect_err("an existing VM directory must be rejected");

        assert!(error.to_string().contains("already exists"));
        assert!(calls.vhd.lock().unwrap().is_empty());
    }

    #[test]
    fn a_disk_creation_failure_removes_the_vm_directory_without_hcs() {
        let fixture = fixture("disk-failure");
        let calls = fixture.calls.clone();
        let pipeline = pipeline(&calls, true, false, false);

        let error = pipeline
            .create(
                &fixture.store,
                &fixture.request,
                &fixture.vm_directory,
                &monitor(),
            )
            .expect_err("disk failure must abort creation");

        assert!(error.to_string().contains("injected disk failure"));
        assert!(calls.create.lock().unwrap().is_empty());
        assert!(calls.teardown.lock().unwrap().is_empty());
        assert!(!fixture.vm_directory.exists());
        assert!(fixture.store.list().unwrap().is_empty());
    }

    #[test]
    fn a_missing_image_at_create_time_aborts_before_hcs() {
        let fixture = fixture("image-gone");
        let calls = fixture.calls.clone();
        let pipeline = pipeline(&calls, false, false, false);
        fs::remove_file(&fixture.image_path).unwrap();

        let error = pipeline
            .create(
                &fixture.store,
                &fixture.request,
                &fixture.vm_directory,
                &monitor(),
            )
            .expect_err("a vanished image must abort creation");

        assert!(error.to_string().contains("image"));
        assert!(calls.create.lock().unwrap().is_empty());
        assert!(!fixture.vm_directory.exists());
    }

    #[test]
    fn an_hcs_create_failure_tears_down_the_ambiguous_system() {
        let fixture = fixture("create-failure");
        let calls = fixture.calls.clone();
        let pipeline = pipeline(&calls, false, false, true);

        let error = pipeline
            .create(
                &fixture.store,
                &fixture.request,
                &fixture.vm_directory,
                &monitor(),
            )
            .expect_err("an HCS create failure must abort creation");

        assert!(error.to_string().contains("timed out"));
        let create_calls = calls.create.lock().unwrap();
        assert_eq!(create_calls.len(), 1);
        // The ambiguous system is torn down by the creator itself (recorded
        // by the injected closure); the pipeline's own rollback must not
        // tear it down a second time because `system_created` stays false.
        assert_eq!(
            calls.teardown.lock().unwrap().as_slice(),
            &[create_calls[0].0.clone()]
        );
        drop(create_calls);
        assert!(!fixture.vm_directory.exists());
        assert!(fixture.store.list().unwrap().is_empty());
    }

    #[test]
    fn a_metadata_registration_failure_tears_down_the_created_system() {
        let fixture = fixture("metadata-failure");
        let calls = fixture.calls.clone();
        // Pointing the mapping file at a path whose parent cannot be created
        // (it collides with an existing file) makes the final `store.insert`
        // step fail with a real filesystem error, without adding a metadata
        // injection seam just for this test.
        let blocked_parent = fixture.vm_directory.with_file_name("blocked");
        fs::write(&blocked_parent, b"file").unwrap();
        let blocked_store = MetadataStore::new(blocked_parent.join("vm-mapping.json"));
        let pipeline = pipeline(&calls, false, false, false);

        let error = pipeline
            .create(
                &blocked_store,
                &fixture.request,
                &fixture.vm_directory,
                &monitor(),
            )
            .expect_err("a metadata registration failure must abort creation");

        assert!(error.to_string().contains("creation of VM"));
        let create_calls = calls.create.lock().unwrap();
        let teardown_calls = calls.teardown.lock().unwrap();
        assert_eq!(create_calls.len(), 1);
        assert_eq!(teardown_calls.as_slice(), &[create_calls[0].0.clone()]);
        drop(teardown_calls);
        drop(create_calls);
        assert!(!fixture.vm_directory.exists());
    }

    #[test]
    fn rollback_never_touches_the_source_image() {
        let fixture = fixture("image-safe");
        let calls = fixture.calls.clone();
        let pipeline = pipeline(&calls, true, false, false);

        let _ = pipeline.create(
            &fixture.store,
            &fixture.request,
            &fixture.vm_directory,
            &monitor(),
        );

        assert_eq!(fs::read(&fixture.image_path).unwrap(), b"iso");
    }

    fn cloud_request(name: &str) -> VmCreateRequest {
        VmCreateRequest {
            name: name.into(),
            source: VmSource::CloudImage {
                image: CloudImage {
                    profile: vmlord_core::ubuntu(),
                    release: "24.04".into(),
                },
                provisioning: Provisioning {
                    username: "dev".into(),
                    password: Some(Password::new("hunter2")),
                    ssh: SshAccess::Enabled {
                        deploy_key: true,
                        port: SshPort::DEFAULT,
                    },
                    locale: "en_US.UTF-8".into(),
                    keyboard: "us".into(),
                    timezone: "Europe/Moscow".into(),
                    desktop: vmlord_core::DesktopProfile::Headless,
                },
            },
            ram_mb: 512,
            disk_gb: 1,
            cpu_cores: 1,
            gpu_mode: GpuMode::None,
            network_mode: NetworkMode::None,
        }
    }

    #[test]
    fn a_created_vm_records_the_gpu_mode_and_the_guest_it_was_built_from() {
        let fixture = fixture("cloud-gpu");
        let calls = fixture.calls.clone();
        let pipeline = pipeline(&calls, false, false, false);
        let request = VmCreateRequest {
            gpu_mode: GpuMode::Mirror,
            ..cloud_request("cloud-gpu-vm")
        };

        let mapping = pipeline
            .create(&fixture.store, &request, &fixture.vm_directory, &monitor())
            .expect("creation must succeed");

        assert_eq!(mapping.gpu_mode, GpuMode::Mirror);
        let target = mapping
            .guest_target
            .expect("a cloud image names the guest it provisions");
        assert_eq!(target.distribution, "ubuntu");
        assert_eq!(target.release, "24.04");
        assert_eq!(target.architecture, "amd64");
    }

    /// The seed's drop-ins are written from the image's profile, and moving
    /// the port later has to write the same files -- by which time the profile
    /// is gone. So it is recorded with the VM.
    #[test]
    fn a_created_vm_records_how_its_ssh_daemon_is_carried() {
        let fixture = fixture("cloud-daemon");
        let calls = fixture.calls.clone();
        let pipeline = pipeline(&calls, false, false, false);

        let mapping = pipeline
            .create(
                &fixture.store,
                &cloud_request("cloud-daemon-vm"),
                &fixture.vm_directory,
                &monitor(),
            )
            .expect("creation must succeed");

        assert_eq!(
            mapping.ssh_daemon,
            Some(vmlord_core::ubuntu().ssh),
            "the drop-ins a later reconfiguration writes are this profile's"
        );
    }

    /// A guest VMLord did not provision has an SSH daemon of somebody else's,
    /// and a profile recorded for it would be a guess.
    #[test]
    fn a_vm_built_from_installation_media_records_no_ssh_daemon() {
        let fixture = fixture("media-daemon");
        let calls = fixture.calls.clone();
        let pipeline = pipeline(&calls, false, false, false);

        let mapping = pipeline
            .create(
                &fixture.store,
                &fixture.request.clone(),
                &fixture.vm_directory,
                &monitor(),
            )
            .expect("creation must succeed");

        assert_eq!(mapping.ssh_daemon, None);
    }

    #[test]
    fn a_vm_built_from_installation_media_names_no_guest_to_stage_a_payload_for() {
        let fixture = fixture("media-gpu");
        let calls = fixture.calls.clone();
        let pipeline = pipeline(&calls, false, false, false);
        let request = VmCreateRequest {
            gpu_mode: GpuMode::Default,
            ..fixture.request.clone()
        };

        let mapping = pipeline
            .create(&fixture.store, &request, &fixture.vm_directory, &monitor())
            .expect("creation must succeed");

        assert_eq!(mapping.gpu_mode, GpuMode::Default);
        assert_eq!(
            mapping.guest_target, None,
            "VMLord promises nothing about the system inside installation media"
        );
    }

    /// The bytes of the seed volume the pipeline wrote.
    fn seed_bytes(vm_directory: &std::path::Path) -> String {
        String::from_utf8_lossy(&fs::read(vm_directory.join("seed.iso")).unwrap()).into_owned()
    }

    #[test]
    fn a_cloud_vm_gets_an_imported_disk_a_key_pair_and_a_seed() {
        let fixture = fixture("cloud-happy");
        let calls = fixture.calls.clone();
        let pipeline = pipeline(&calls, false, false, false);
        let request = cloud_request("cloud-vm");

        let mapping = pipeline
            .create(&fixture.store, &request, &fixture.vm_directory, &monitor())
            .expect("creation should succeed");

        // The disk comes from the importer, not from an empty VHDX.
        assert!(calls.vhd.lock().unwrap().is_empty());
        assert_eq!(
            calls.cloud.lock().unwrap().as_slice(),
            &[(
                "24.04".to_owned(),
                1024 * 1024 * 1024,
                fixture.vm_directory.join("disks").join("system.vhdx")
            )]
        );

        assert!(fixture.vm_directory.join("seed.iso").is_file());
        assert!(
            fixture.vm_directory.join("tools.iso").is_file(),
            "an available agent is packed into the VM's tools volume"
        );
        assert!(
            fixture
                .vm_directory
                .join("keys")
                .join("id_ed25519")
                .is_file()
        );
        let public_key =
            fs::read_to_string(fixture.vm_directory.join("keys").join("id_ed25519.pub")).unwrap();

        // What the guest is told is in the seed, and only there.
        let seed = seed_bytes(&fixture.vm_directory);
        assert!(seed.contains("$6$"), "the seed carries the password hash");
        assert!(
            !seed.contains("hunter2"),
            "and the hash instead of the password, not beside it"
        );
        assert!(seed.contains(public_key.trim_end()), "and the public key");
        assert!(
            seed.contains("vmlord-agent.service"),
            "the seed installs and enables the service for the attached agent"
        );
        // The whole case for leaving the seed attached rests on this id being
        // the compute system's own, so that it never changes across boots.
        assert!(seed.contains(&format!("instance-id: '{}'", mapping.hcs_compute_system_id)));

        let stored = fs::read_to_string(fixture.vm_directory.join("config.json")).unwrap();
        let create_calls = calls.create.lock().unwrap();
        for document in [&create_calls[0].1, &stored] {
            assert!(!document.contains("hunter2"), "got {document}");
            assert!(!document.contains("$6$"), "got {document}");
        }
        drop(create_calls);

        // The VM must be able to open the seed, exactly as it opens its disk.
        assert_eq!(
            calls.grant.lock().unwrap().as_slice(),
            &[
                (
                    mapping.hcs_compute_system_id.clone(),
                    fixture.vm_directory.join("disks").join("system.vhdx")
                ),
                (
                    mapping.hcs_compute_system_id.clone(),
                    fixture.vm_directory.join("seed.iso")
                ),
                (
                    mapping.hcs_compute_system_id.clone(),
                    fixture.vm_directory.join("tools.iso")
                ),
                (
                    mapping.hcs_compute_system_id.clone(),
                    fixture.vm_directory.join("vm.vmgs")
                ),
                (
                    mapping.hcs_compute_system_id.clone(),
                    fixture.vm_directory.join("vm.vmrs")
                ),
            ]
        );

        let configuration = fs::read_to_string(fixture.vm_directory.join("config.json")).unwrap();
        assert!(configuration.contains("tools.iso"));
    }

    /// The secret exists in exactly two places: a host file the VM cannot
    /// read, and the seed the guest boots from. It never reaches the HCS
    /// document, which is not a secret at all.
    #[test]
    fn a_cloud_vm_gets_an_agent_secret_the_host_and_the_seed_agree_on() {
        let fixture = fixture("cloud-agent-secret");
        let calls = fixture.calls.clone();
        let pipeline = pipeline(&calls, false, false, false);

        pipeline
            .create(
                &fixture.store,
                &cloud_request("cloud-agent-vm"),
                &fixture.vm_directory,
                &monitor(),
            )
            .expect("creation should succeed");

        let stored =
            fs::read_to_string(crate::layout::agent_secret_path(&fixture.vm_directory)).unwrap();
        // Readable as a secret at all, which a truncated or mangled file
        // would not be.
        vmlord_agent_protocol::auth::Secret::from_base64(&stored)
            .expect("the host's copy is a secret");

        let seed = seed_bytes(&fixture.vm_directory);
        assert!(
            seed.contains(stored.trim_end()),
            "the guest gets the same secret through its seed"
        );
        assert!(seed.contains("/etc/vmlord/agent.secret"));

        let configuration = fs::read_to_string(fixture.vm_directory.join("config.json")).unwrap();
        assert!(!configuration.contains(stored.trim_end()));
    }

    #[test]
    fn a_cloud_vm_without_the_agent_binary_skips_agent_provisioning() {
        // This catches the wrong branch: packaging the service or secret when
        // the executable was not installed leaves a guest with a unit that can
        // never start.
        let fixture = fixture("cloud-without-agent");
        let calls = fixture.calls.clone();
        let pipeline = pipeline_without_agent(&calls, false, false, false);

        pipeline
            .create(
                &fixture.store,
                &cloud_request("cloud-without-agent-vm"),
                &fixture.vm_directory,
                &monitor(),
            )
            .expect("a missing optional agent must not prevent cloud VM creation");

        assert!(!crate::layout::tools_path(&fixture.vm_directory).exists());
        assert!(!crate::layout::agent_secret_path(&fixture.vm_directory).exists());
        let seed = seed_bytes(&fixture.vm_directory);
        assert!(!seed.contains("vmlord-agent.service"), "got {seed}");

        let configuration = fs::read_to_string(fixture.vm_directory.join("config.json")).unwrap();
        let configuration_json = serde_json::from_str::<serde_json::Value>(&configuration).unwrap();
        let attachments = configuration_json
            .pointer("/VirtualMachine/Devices/Scsi/Primary/Attachments")
            .and_then(serde_json::Value::as_object)
            .unwrap();
        assert_eq!(
            attachments.keys().collect::<Vec<_>>(),
            vec!["0", "1"],
            "a cloud VM without an agent keeps only its disk and seed"
        );
        assert_eq!(
            calls.grant.lock().unwrap().len(),
            4,
            "the disk and the seed, plus the two state files"
        );
    }

    #[test]
    fn the_agent_secret_is_written_with_the_same_restricted_dacl_as_the_key() {
        let fixture = fixture("cloud-agent-secret-dacl");
        let calls = fixture.calls.clone();
        let pipeline = pipeline(&calls, false, false, false);

        pipeline
            .create(
                &fixture.store,
                &cloud_request("cloud-agent-dacl-vm"),
                &fixture.vm_directory,
                &monitor(),
            )
            .expect("creation should succeed");

        let descriptor = crate::vm_key::security_descriptor(&crate::layout::agent_secret_path(
            &fixture.vm_directory,
        ))
        .expect("the DACL should be read back");
        assert!(descriptor.contains("D:P"), "{descriptor}");
        assert!(
            !descriptor.contains(";ID;"),
            "nothing may be inherited from the storage root: {descriptor}"
        );
        assert_eq!(
            descriptor.matches("(A;;FA;;;").count(),
            3,
            "SYSTEM, Administrators and the owner, and nobody else: {descriptor}"
        );
    }

    /// The VM opens its seed and nothing else of VMLord's: the host's copy of
    /// the secret is not something the guest has any business reading.
    #[test]
    fn the_vm_is_never_granted_access_to_the_hosts_copy_of_the_secret() {
        let fixture = fixture("cloud-agent-secret-grant");
        let calls = fixture.calls.clone();
        let pipeline = pipeline(&calls, false, false, false);

        pipeline
            .create(
                &fixture.store,
                &cloud_request("cloud-agent-grant-vm"),
                &fixture.vm_directory,
                &monitor(),
            )
            .expect("creation should succeed");

        let secret = crate::layout::agent_secret_path(&fixture.vm_directory);
        assert!(
            calls
                .grant
                .lock()
                .unwrap()
                .iter()
                .all(|(_, path)| path != &secret)
        );
    }

    /// A VM installed from local media runs no agent: VMLord promises nothing
    /// about the system inside it, so there is nothing to authenticate.
    #[test]
    fn a_local_media_vm_gets_no_agent_secret() {
        let fixture = fixture("local-media-agent-secret");
        let calls = fixture.calls.clone();
        let pipeline = pipeline(&calls, false, false, false);

        pipeline
            .create(
                &fixture.store,
                &fixture.request,
                &fixture.vm_directory,
                &monitor(),
            )
            .expect("creation should succeed");

        assert!(!crate::layout::agent_secret_path(&fixture.vm_directory).exists());
    }

    #[test]
    fn a_key_only_cloud_vm_leaves_no_password_hash_anywhere() {
        let fixture = fixture("cloud-key-only");
        let calls = fixture.calls.clone();
        let pipeline = pipeline(&calls, false, false, false);
        let request = VmCreateRequest {
            source: VmSource::CloudImage {
                image: CloudImage {
                    profile: vmlord_core::ubuntu(),
                    release: "24.04".into(),
                },
                provisioning: Provisioning {
                    username: "dev".into(),
                    password: None,
                    ssh: SshAccess::Enabled {
                        deploy_key: true,
                        port: SshPort::DEFAULT,
                    },
                    locale: "en_US.UTF-8".into(),
                    keyboard: "us".into(),
                    timezone: "Europe/Moscow".into(),
                    desktop: vmlord_core::DesktopProfile::Headless,
                },
            },
            ..cloud_request("cloud-key-only-vm")
        };

        pipeline
            .create(&fixture.store, &request, &fixture.vm_directory, &monitor())
            .expect("a key-only login is a valid VM");

        assert!(!seed_bytes(&fixture.vm_directory).contains("$6$"));
    }

    #[test]
    fn a_cloud_vm_without_ssh_gets_no_key_pair() {
        let fixture = fixture("cloud-no-ssh");
        let calls = fixture.calls.clone();
        let pipeline = pipeline(&calls, false, false, false);
        let request = VmCreateRequest {
            source: VmSource::CloudImage {
                image: CloudImage {
                    profile: vmlord_core::ubuntu(),
                    release: "24.04".into(),
                },
                provisioning: Provisioning {
                    username: "dev".into(),
                    password: Some(Password::new("hunter2")),
                    ssh: SshAccess::Disabled,
                    locale: "en_US.UTF-8".into(),
                    keyboard: "us".into(),
                    timezone: "Europe/Moscow".into(),
                    desktop: vmlord_core::DesktopProfile::Headless,
                },
            },
            ..cloud_request("cloud-no-ssh-vm")
        };

        pipeline
            .create(&fixture.store, &request, &fixture.vm_directory, &monitor())
            .expect("a password-only VM is a valid VM");

        assert!(!fixture.vm_directory.join("keys").exists());
        assert!(fixture.vm_directory.join("seed.iso").is_file());
    }

    /// The seed carries the password's hash, so it is a secret on disk in the
    /// same sense the private key is, and it is written under the same DACL.
    /// The storage root is the owner's to choose; one with an inherited
    /// `Users:(R)` would otherwise hand the hash to every local account.
    #[test]
    fn the_seed_is_written_with_the_same_restricted_dacl_as_the_key() {
        let fixture = fixture("cloud-seed-dacl");
        let calls = fixture.calls.clone();
        let pipeline = pipeline(&calls, false, false, false);

        pipeline
            .create(
                &fixture.store,
                &cloud_request("cloud-dacl-vm"),
                &fixture.vm_directory,
                &monitor(),
            )
            .expect("creation should succeed");

        let descriptor = crate::vm_key::security_descriptor(&fixture.vm_directory.join("seed.iso"))
            .expect("the DACL should be read back");
        assert!(descriptor.contains("D:P"), "{descriptor}");
        assert!(
            !descriptor.contains(";ID;"),
            "nothing may be inherited from the storage root: {descriptor}"
        );
        assert_eq!(
            descriptor.matches("(A;;FA;;;").count(),
            3,
            "SYSTEM, Administrators and the owner, and nobody else: {descriptor}"
        );
    }

    /// SSH being on is not the same question as whether we deploy a key for
    /// it: a VM the owner reaches with their own key asks for one and not the
    /// other, and generating a key pair nobody asked for would put a private
    /// key on disk and an unrequested login into the guest.
    #[test]
    fn a_cloud_vm_that_did_not_ask_for_a_key_gets_none() {
        let fixture = fixture("cloud-no-deploy-key");
        let calls = fixture.calls.clone();
        let pipeline = pipeline(&calls, false, false, false);
        let request = VmCreateRequest {
            source: VmSource::CloudImage {
                image: CloudImage {
                    profile: vmlord_core::ubuntu(),
                    release: "24.04".into(),
                },
                provisioning: Provisioning {
                    username: "dev".into(),
                    password: Some(Password::new("hunter2")),
                    ssh: SshAccess::Enabled {
                        deploy_key: false,
                        port: SshPort::DEFAULT,
                    },
                    locale: "en_US.UTF-8".into(),
                    keyboard: "us".into(),
                    timezone: "Europe/Moscow".into(),
                    desktop: vmlord_core::DesktopProfile::Headless,
                },
            },
            ..cloud_request("cloud-no-deploy-key-vm")
        };

        pipeline
            .create(&fixture.store, &request, &fixture.vm_directory, &monitor())
            .expect("SSH without a deployed key is a valid VM");

        assert!(!fixture.vm_directory.join("keys").exists());
        let seed = seed_bytes(&fixture.vm_directory);
        assert!(!seed.contains("ssh_authorized_keys"), "got {seed}");
    }

    #[test]
    fn a_failed_import_takes_the_vm_directory_with_it() {
        let fixture = fixture("cloud-import-failure");
        let calls = fixture.calls.clone();
        let pipeline = pipeline(&calls, false, true, false);

        let error = pipeline
            .create(
                &fixture.store,
                &cloud_request("cloud-doomed"),
                &fixture.vm_directory,
                &monitor(),
            )
            .expect_err("an import failure must abort creation");

        assert!(error.to_string().contains("injected cloud image failure"));
        assert!(!fixture.vm_directory.exists());
        assert!(calls.create.lock().unwrap().is_empty());
        assert!(fixture.store.list().unwrap().is_empty());
    }

    #[test]
    fn a_failed_hcs_create_takes_the_seed_and_the_keys_with_it() {
        let fixture = fixture("cloud-create-failure");
        let calls = fixture.calls.clone();
        let pipeline = pipeline(&calls, false, false, true);

        let error = pipeline
            .create(
                &fixture.store,
                &cloud_request("cloud-rollback"),
                &fixture.vm_directory,
                &monitor(),
            )
            .expect_err("an HCS create failure must abort creation");

        assert!(error.to_string().contains("timed out"));
        // The whole directory goes, seed and private key included: nothing is
        // left of a VM that does not exist.
        assert!(!fixture.vm_directory.exists());
        assert!(fixture.store.list().unwrap().is_empty());
    }
}
