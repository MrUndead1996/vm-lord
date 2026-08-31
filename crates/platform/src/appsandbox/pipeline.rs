//! Assembling one import's real side effects out of the production pipelines.
//!
//! [`super::worker::ImportWorkerActions`] is a table of seams so that the
//! rollback/retain decisions can be tested without HCS, HNS, SSH or a large
//! VHDX. This module is the other half: it fills that table with the things
//! those seams stand for, and it is the only place that knows both the copy and
//! the conversion, which is why it -- rather than the repository -- owns the
//! assembly. `copy_vhdx` is private to this module tree on purpose: nothing
//! outside it may name an AppSandbox path.

use std::{
    fs,
    io::Read,
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use uuid::Uuid;
use vmlord_agent_protocol::auth;
use vmlord_core::{GpuMode, RepositoryError, SshAuthentication, SshConfig, SshEndpoint, SshPort};

use super::{
    BootstrapRequest, BootstrapSshFacts, BootstrapVm, ConversionCommand, ConversionRequest,
    ConversionRunner, ConversionStep, GUEST_SSH_PORT, ImportBootstrapPipeline, ImportJournal,
    ImportResources, SecretText, ValidatedSource, Verification, VerificationRequest,
    copy::{CopyRequest, copy_vhdx},
    source_agent::{AddressOutcome, SourceAgent},
    worker::ImportWorkerActions,
};
use crate::{
    cleanup,
    com1_terminal::Com1Session,
    create,
    guest_ready::ReadinessTimeouts,
    hcs::{HCS_ACCESS_ALL, HcsClient},
    layout,
    metadata::{MetadataStore, VmComputeSystemMapping},
    ssh::{self, SshInvocation},
    start::VmStartPipeline,
    subnet::Ipv4Subnet,
};

/// How often the waits below ask again.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How long one conversion command may take.
///
/// Generous because the longest of them installs packages inside the guest,
/// and short enough that a guest which stopped answering does not hold an
/// import open for the rest of the session.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// How long the converted guest is given to power itself off before the second
/// boot takes its compute system apart.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2 * 60);

/// How long one second-boot check may take. Each of them asks the guest one
/// question and expects an immediate answer.
const CHECK_TIMEOUT: Duration = Duration::from_secs(60);

/// How long to wait before asking a second-boot check again.
///
/// Longer than [`POLL_INTERVAL`] because every attempt is a whole SSH session,
/// process and handshake included, and because what is being waited for moves
/// in seconds: the guest answers SSH as soon as sshd is up, but the agent has
/// still to connect to the host and mount the two payload shares.
const CHECK_INTERVAL: Duration = Duration::from_secs(3);

/// The file name the guest agent is shipped beside VMLord under.
const AGENT_FILE_NAME: &str = "vmlord-agent";

/// What the second boot has to prove before ordinary metadata may claim it.
///
/// Every one of these is a fact about the guest that a *created* VM gets the
/// same way -- the key VMLord deployed, the unit VMLord installed, and the two
/// payload shares the host offers and the agent mounts. None of them reads a
/// payload out of the guest's own disk, because nothing puts one there.
const SSH_CHECK: &str = "true";
const AGENT_CHECK: &str = "systemctl is-active vmlord-agent.service";
const DISPLAY_CHECK: &str = "mountpoint -q /opt/vmlord/display-payload";
const GPU_CHECK: &str = "mountpoint -q /usr/lib/wsl/lib";

/// One import, as the pipeline needs to know it.
///
/// Owned rather than borrowed: every field crosses onto the import's thread.
pub(crate) struct ImportSubject {
    pub(crate) vm_name: String,
    pub(crate) destination: PathBuf,
    pub(crate) import_id: Uuid,
    /// The discovered source, when the latest discovery still resolves it.
    ///
    /// `None` on a retry taken after a restart: an import whose copy is already
    /// promoted has nothing left to read from AppSandbox, and refusing it for
    /// want of a rediscovery would strand recoverable work.
    pub(crate) source: Option<ValidatedSource>,
    pub(crate) resources: ImportResources,
    pub(crate) ssh: BootstrapSshFacts,
    pub(crate) desired_gpu: GpuMode,
}

/// Builds the side effects of one import out of the production pipelines.
pub(crate) struct ImportPipeline {
    storage_root: PathBuf,
    store: MetadataStore,
    bootstrap: Arc<ImportBootstrapPipeline>,
    start: Arc<VmStartPipeline>,
    timeouts: ReadinessTimeouts,
}

impl ImportPipeline {
    pub(crate) fn new(
        storage_root: impl Into<PathBuf>,
        store: MetadataStore,
        start: Arc<VmStartPipeline>,
        timeouts: ReadinessTimeouts,
    ) -> Self {
        Self {
            storage_root: storage_root.into(),
            store,
            bootstrap: Arc::new(ImportBootstrapPipeline::production()),
            start,
            timeouts,
        }
    }

    /// Fills the worker's table for `subject`.
    ///
    /// Everything that can be refused without touching a disk is refused here,
    /// before the thread: an import that has no `ssh.exe` to run or no agent to
    /// install is not going to acquire one halfway through copying eighty
    /// gigabytes.
    pub(crate) fn actions(
        &self,
        subject: ImportSubject,
    ) -> Result<ImportWorkerActions, RepositoryError> {
        let ssh_client = ssh::client_path().ok_or_else(|| {
            RepositoryError::new(
                "importing an AppSandbox VM needs Windows OpenSSH, and ssh.exe could not be found",
            )
        })?;
        let scp_client = ssh::copy_client_path().ok_or_else(|| {
            RepositoryError::new(
                "importing an AppSandbox VM needs Windows OpenSSH, and scp.exe could not be found",
            )
        })?;
        let agent_binary = agent_binary_path()?;

        let storage_root = self.storage_root.clone();
        let store = self.store.clone();
        let bootstrap_pipeline = Arc::clone(&self.bootstrap);
        let start = Arc::clone(&self.start);
        let timeouts = self.timeouts;

        let ImportSubject {
            vm_name,
            destination,
            import_id,
            source,
            resources,
            ssh: bootstrap_ssh,
            desired_gpu,
        } = subject;
        let source_disk_path = source
            .as_ref()
            .map_or_else(PathBuf::new, |source| source.source_disk.clone());

        let staging = layout::import_staging_directory(&storage_root, import_id);
        let staged_disk = staging.join("system.vhdx");
        let transcript = layout::import_transcript_path(&staging);
        let verify_transcript = transcript.clone();
        let bundle_directory = staging.join("bundle");
        let final_disk = layout::system_disk_path(&destination);

        // The first boot's console. Held here rather than on `BootstrapVm`,
        // which is what the worker's seams pass around: dropping a session is
        // what cancels its reader, and the reader has to live until the guest
        // it is reading from is shut down for the second boot.
        let bootstrap_console: Arc<Mutex<Option<Com1Session>>> = Arc::new(Mutex::new(None));

        Ok(ImportWorkerActions {
            copy: {
                let source = source.clone();
                let disk_path = source_disk_path.clone();
                let staged_disk = staged_disk.clone();
                let final_disk = final_disk.clone();
                Box::new(move |cancel, publish| {
                    if final_disk.is_file() {
                        // A resumed import: the copy this run would make is
                        // already promoted, and remaking it would overwrite the
                        // guest the conversion has been changing.
                        tracing::info!(
                            "the copied disk of this import is already in place at {}",
                            final_disk.display()
                        );
                        return Ok(());
                    }
                    fs::create_dir_all(&staging).map_err(|error| {
                        RepositoryError::new(format!(
                            "failed to create the import staging directory {}: {error}",
                            staging.display()
                        ))
                    })?;
                    if staged_disk.exists() {
                        // A previous attempt was interrupted mid-copy. Its
                        // bytes are unverifiable, and the copy refuses to write
                        // over an existing staging file.
                        fs::remove_file(&staged_disk).map_err(|error| {
                            RepositoryError::new(format!(
                                "failed to remove the partial import staging disk {}: {error}",
                                staged_disk.display()
                            ))
                        })?;
                    }
                    let Some(source) = source.as_ref() else {
                        return Err(RepositoryError::new(format!(
                            "the AppSandbox VM at {} is no longer among the discovered sources, \
                             so its disk cannot be copied; discover AppSandbox VMs again and \
                             retry",
                            disk_path.display()
                        )));
                    };
                    copy_vhdx(CopyRequest {
                        source,
                        target: &staged_disk,
                        cancel,
                        publish,
                    })
                    .map(drop)
                })
            },
            promote: {
                let staged_disk = staged_disk.clone();
                let final_disk = final_disk.clone();
                Box::new(move || {
                    if final_disk.is_file() {
                        return Ok(());
                    }
                    let disks = final_disk.parent().ok_or_else(|| {
                        RepositoryError::new("the imported system disk has no parent directory")
                    })?;
                    fs::create_dir_all(disks).map_err(|error| {
                        RepositoryError::new(format!(
                            "failed to create the VM disk directory {}: {error}",
                            disks.display()
                        ))
                    })?;
                    fs::rename(&staged_disk, &final_disk).map_err(|error| {
                        RepositoryError::new(format!(
                            "failed to move the copied disk into {}: {error}",
                            final_disk.display()
                        ))
                    })
                })
            },
            bootstrap: {
                let store = store.clone();
                let vm_name = vm_name.clone();
                let destination = destination.clone();
                let resources = resources.clone();
                let bootstrap_ssh = bootstrap_ssh.clone();
                Box::new(move || {
                    // A resumed import finds its own previous compute system
                    // registered. The disk is the expensive half of an import
                    // and the system is the cheap one, so the system is
                    // rebuilt rather than adopted: nothing here has to reason
                    // about what state a half-configured system was left in.
                    discard_previous_bootstrap(&store, &vm_name)?;
                    bootstrap_pipeline.create(
                        &store,
                        &BootstrapRequest {
                            vm_name: &vm_name,
                            vm_directory: &destination,
                            resources: &resources,
                            ssh: &bootstrap_ssh,
                        },
                    )
                })
            },
            start_bootstrap: {
                let store = store.clone();
                let start = Arc::clone(&start);
                let destination = destination.clone();
                let console = Arc::clone(&bootstrap_console);
                Box::new(move |bootstrap| {
                    let started = start.start_mapping(&store, &bootstrap.mapping, &destination)?;
                    // The endpoint the start took is how the address is found
                    // below, so it travels back with the bootstrap rather than
                    // being read out of the store again.
                    bootstrap.mapping = started.mapping;
                    *console
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(started.session);
                    Ok(())
                })
            },
            convert: {
                let storage_root = storage_root.clone();
                let destination = destination.clone();
                let source_key = source.as_ref().map(|source| source.private_key.clone());
                let ssh_client = ssh_client.clone();
                let scp_client = scp_client.clone();
                let bootstrap_ssh = bootstrap_ssh.clone();
                Box::new(move |bootstrap| {
                    // Reloaded rather than shared: the runner advances the
                    // confirmed conversion step through the same file the
                    // worker writes its stages into, and the worker reloads it
                    // afterwards. It is read before the guest is reached
                    // because what it confirms decides how to reach it.
                    let mut journal = ImportJournal::load(&storage_root, import_id)?;
                    let endpoint = wait_for_guest(
                        bootstrap,
                        &bootstrap_ssh,
                        journal.last_confirmed_conversion_step(),
                        &timeouts,
                    )?;
                    let secret = agent_secret(&destination)?;
                    let public_key = fs::read_to_string(layout::ssh_public_key_path(&destination))
                        .map_err(|error| {
                            RepositoryError::new(format!(
                                "failed to read the imported VM's own public key: {error}"
                            ))
                        })?;
                    let Some(source_key) = source_key.as_ref() else {
                        return Err(RepositoryError::new(
                            "converting the copied guest needs the AppSandbox key of its source, \
                             which the latest discovery no longer resolves; discover AppSandbox \
                             VMs again and retry",
                        ));
                    };
                    let transcript = transcript.clone();
                    let report = ConversionRunner::new(
                        ConversionRequest {
                            endpoint: &endpoint,
                            vm_directory: &destination,
                            ssh_client: &ssh_client,
                            scp_client: &scp_client,
                            bootstrap_key: source_key,
                            staging_directory: &bundle_directory,
                            agent_binary: &agent_binary,
                            vmlord_public_key: public_key.trim(),
                            agent_secret: &secret,
                        },
                        &mut journal,
                        Box::new(move |command: &ConversionCommand| {
                            run_remote(&command.invocation, &transcript, COMMAND_TIMEOUT)
                        }),
                    )
                    .run()?;
                    Ok(report.identity)
                })
            },
            restart: {
                let store = store.clone();
                let start = Arc::clone(&start);
                let destination = destination.clone();
                let console = Arc::clone(&bootstrap_console);
                Box::new(move |mapping| {
                    // The conversion's last command asked the guest to power
                    // off. Its console is released and its compute system taken
                    // apart before the second one is built, because HCS holds
                    // one system per identifier and the second boot's Plan9
                    // sections are fixed for the lifetime of a boot.
                    console
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .take();
                    wait_for_shutdown(&mapping.hcs_compute_system_id)?;
                    start.start_mapping(&store, &mapping, &destination)
                })
            },
            verify: {
                let destination = destination.clone();
                let ssh_client = ssh_client.clone();
                let transcript = verify_transcript.clone();
                let desktop_profile = resources.desktop_profile;
                Box::new(move |started| {
                    let checks = GuestChecks {
                        mapping: started.mapping.clone(),
                        vm_directory: destination.clone(),
                        client: ssh_client.clone(),
                        transcript: transcript.clone(),
                        timeouts,
                    };
                    Verification::new(
                        checks.check("SSH", SSH_CHECK),
                        checks.check("the VMLord agent", AGENT_CHECK),
                        checks.check("the display payload", DISPLAY_CHECK),
                        checks.check("the GPU payload", GPU_CHECK),
                    )
                    .run(VerificationRequest {
                        desktop_profile,
                        gpu_mode: desired_gpu,
                    })
                })
            },
            finalize: {
                let store = store.clone();
                Box::new(move |mapping| store.insert(mapping.clone()))
            },
            rollback: Box::new(move |destination, hcs_id| {
                let mut failures = Vec::new();
                if let Some(hcs_id) = hcs_id
                    && let Err(error) = cleanup::teardown_compute_system(hcs_id)
                {
                    failures.push(error.to_string());
                }
                if let Some(hcs_id) = hcs_id
                    && let Err(error) = forget_mapping(&store, hcs_id)
                {
                    failures.push(error.to_string());
                }
                if let Err(error) = cleanup::remove_vm_directory(destination) {
                    failures.push(error.to_string());
                }
                if failures.is_empty() {
                    Ok(())
                } else {
                    Err(cleanup::combine_failures(
                        "the AppSandbox import could not be rolled back completely",
                        failures,
                    ))
                }
            }),
        })
    }
}

/// The four second-boot checks, which differ only in what they ask.
struct GuestChecks {
    mapping: VmComputeSystemMapping,
    vm_directory: PathBuf,
    client: PathBuf,
    /// Where a check's answer is left, which is the import's own transcript and
    /// not a VM log another operation writes.
    transcript: PathBuf,
    timeouts: ReadinessTimeouts,
}

impl GuestChecks {
    fn check(
        &self,
        what: &'static str,
        command: &'static str,
    ) -> impl Fn() -> Result<(), RepositoryError> + Send + Sync + use<> {
        let mapping = self.mapping.clone();
        let vm_directory = self.vm_directory.clone();
        let client = self.client.clone();
        let transcript = self.transcript.clone();
        let timeouts = self.timeouts;
        move || {
            let Some(config) = mapping.ssh.clone() else {
                return Err(RepositoryError::new(
                    "the imported VM has no SSH configuration to verify it with",
                ));
            };
            let (address, _) = wait_for_address(&mapping, &timeouts)?;
            let address = IpAddr::V4(address);
            wait_for_port(address, config.port.get(), &timeouts)?;
            let endpoint = SshEndpoint::new(mapping.vm_id, &config, address)?;
            let invocation = ssh::invocation(
                &client,
                &endpoint,
                &vm_directory,
                Some(timeouts.connect),
                Some(command),
            );
            // Asked until it is true rather than once. A guest answers SSH the
            // moment sshd is up, which is before its agent has connected to the
            // host and mounted the payload shares -- so a single ask reports a
            // guest that is merely early as a guest that is wrong.
            let deadline = Instant::now() + timeouts.ssh_port;
            keep_asking(deadline, CHECK_INTERVAL, || {
                run_remote(&invocation, &transcript, CHECK_TIMEOUT).map(drop)
            })
            .map_err(|error| {
                RepositoryError::new(format!(
                    "the imported VM \"{}\" did not answer for {what}: {error}",
                    mapping.vm_name
                ))
            })
        }
    }
}

/// Runs `attempt` until it succeeds or `deadline` passes.
///
/// The failure carried out is the last one, not the first: what a check was
/// still refusing when the time ran out is what says why the guest is wrong,
/// and an early refusal from a guest that was simply not ready yet says
/// nothing.
fn keep_asking(
    deadline: Instant,
    interval: Duration,
    mut attempt: impl FnMut() -> Result<(), RepositoryError>,
) -> Result<(), RepositoryError> {
    loop {
        match attempt() {
            Ok(()) => return Ok(()),
            Err(error) => {
                if Instant::now() >= deadline {
                    return Err(error);
                }
                tracing::debug!("a second-boot check is not true yet: {error}");
            }
        }
        std::thread::sleep(interval);
    }
}

/// Where the guest agent is shipped, beside VMLord itself.
fn agent_binary_path() -> Result<PathBuf, RepositoryError> {
    let executable = std::env::current_exe().map_err(|error| {
        RepositoryError::new(format!(
            "cannot locate the VMLord executable to find {AGENT_FILE_NAME}: {error}"
        ))
    })?;
    let agent = executable.with_file_name(AGENT_FILE_NAME);
    if !agent.is_file() {
        return Err(RepositoryError::new(format!(
            "the guest agent is not installed beside VMLord at {}, so an imported VM could not \
             be given one",
            agent.display()
        )));
    }
    Ok(agent)
}

/// The agent secret for this VM: the one already minted, or a new one.
///
/// Read back rather than reminted on a resumed import, because the guest may
/// already be holding the first one and a VM's secret lives as long as the VM.
fn agent_secret(vm_directory: &Path) -> Result<SecretText, RepositoryError> {
    let path = layout::agent_secret_path(vm_directory);
    if let Ok(existing) = fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Ok(SecretText::new(trimmed));
        }
    }
    let secret = auth::Secret::generate().to_base64();
    create::write_restricted(
        &path,
        format!("{}\n", secret.as_str()).as_bytes(),
        "the agent secret",
    )?;
    Ok(SecretText::new(secret.as_str()))
}

/// Removes whatever a previous attempt at this import registered.
fn discard_previous_bootstrap(store: &MetadataStore, vm_name: &str) -> Result<(), RepositoryError> {
    let Some(previous) = store.find_by_vm_name(vm_name)? else {
        return Ok(());
    };
    tracing::info!(
        "taking apart the previous bootstrap compute system \"{}\" of import \"{vm_name}\"",
        previous.hcs_compute_system_id
    );
    cleanup::teardown_compute_system(&previous.hcs_compute_system_id)?;
    store.remove(previous.vm_id)
}

/// Removes the mapping that names `hcs_id`, if the store still has one.
fn forget_mapping(store: &MetadataStore, hcs_id: &str) -> Result<(), RepositoryError> {
    match store.find_by_hcs_id(hcs_id)? {
        Some(mapping) => store.remove(mapping.vm_id),
        None => Ok(()),
    }
}

/// The bootstrap session's endpoint, once the copied guest answers on it.
///
/// The copied guest does not arrive on VMLord's network by itself. Its source
/// application gave it a static address, deleted every other netplan file and
/// turned cloud-init's network module off, so it never asks for one -- it comes
/// up on a subnet that no longer exists and waiting for an address would only
/// ever time out. What it does still run is the agent that put it there, on an
/// hv_socket service that needs no network, and that agent is asked to move the
/// guest onto the address HNS has already reserved for it.
///
/// A guest that is already up rarely takes the new address there and then: the
/// agent finishes by restarting NetworkManager, and a NetworkManager restarted
/// over an interface that already carries an address assumes that address
/// instead of applying the profile it was just handed. The configuration is on
/// disk either way, so the answer is to restart the guest and let it read the
/// file from cold -- which is the same cold start the address took the first
/// time, when the source application wrote it.
///
/// This is the first thing an import does that changes the copy. It cannot
/// touch the source: an hv_socket address names one partition, and the only
/// partition reachable here is the one the copy is running as.
///
/// All of that is for a guest the conversion has not reached yet. Once the
/// conversion has handed the network over, the guest asks for its address like
/// every other VMLord guest -- and the agent that used to answer for it has
/// been disabled by that same conversion, so a resumed import must not go
/// looking for it.
fn wait_for_guest(
    bootstrap: &BootstrapVm,
    ssh: &BootstrapSshFacts,
    confirmed: Option<ConversionStep>,
    timeouts: &ReadinessTimeouts,
) -> Result<SshEndpoint, RepositoryError> {
    let mapping = &bootstrap.mapping;
    let port = SshPort::new(GUEST_SSH_PORT)?;
    let (address, prefix_length) = wait_for_address(mapping, timeouts)?;

    if confirmed.is_some_and(|step| step >= ConversionStep::GuestNetworkHandedOver) {
        tracing::info!(
            "the copied guest of VM \"{}\" has already been handed its network, so it asks for \
             its own address",
            mapping.vm_name
        );
    } else {
        let gateway = Ipv4Subnet::new(address, prefix_length).gateway();
        let runtime_id = partition_of(&bootstrap.hcs_compute_system_id, &mapping.vm_name)?;

        // One deadline for the whole conversation: the guest is booting through
        // the first half and has to answer within the second, and what the
        // caller is waiting for is a guest it can talk to.
        let deadline = Instant::now() + timeouts.ssh_port;
        let mut agent = SourceAgent::connect(&mapping.vm_name, runtime_id, deadline)?;
        if agent.move_onto(address, prefix_length, gateway, deadline)?
            == AddressOutcome::NeedsRestart
        {
            agent.restart(deadline)?;
        }
        // The connection is finished either way, and a guest that was asked to
        // restart is about to take it down.
        drop(agent);
    }

    // The port wait is what proves the address took, whichever way the guest
    // got it: nothing answers at an address the guest does not hold.
    let address = IpAddr::V4(address);
    wait_for_port(address, port.get(), timeouts)?;
    SshEndpoint::new(
        mapping.vm_id,
        &SshConfig {
            username: ssh.username.clone(),
            port,
            // The AppSandbox key is handed to each invocation explicitly, so
            // this field never decides which credential is offered.
            authentication: SshAuthentication::VmlordKey,
        },
        address,
    )
}

/// The partition a bootstrap VM is running as.
///
/// Asked of HCS rather than remembered: Hyper-V hands out a new runtime id on
/// every start, and it is the only name an hv_socket address knows a VM by.
fn partition_of(hcs_compute_system_id: &str, vm_name: &str) -> Result<Uuid, RepositoryError> {
    let mut client = HcsClient::new();
    client.initialize()?;
    client
        .enumerate_systems()?
        .into_iter()
        .find(|system| system.id == hcs_compute_system_id)
        .and_then(|system| system.runtime_id)
        .ok_or_else(|| {
            let error = RepositoryError::new(format!(
                "HCS does not say which partition the copied guest of VM \"{vm_name}\" is \
                 running as, so its agent cannot be reached"
            ));
            tracing::error!("{error}");
            error
        })
}

/// The address and prefix HNS reserved for the guest, once it has one.
///
/// The prefix comes back with the address because the guest is about to be told
/// what network it is on, not merely where to answer.
fn wait_for_address(
    mapping: &VmComputeSystemMapping,
    timeouts: &ReadinessTimeouts,
) -> Result<(Ipv4Addr, u8), RepositoryError> {
    let deadline = Instant::now() + timeouts.address;
    loop {
        match ssh::guest_endpoint_address(mapping) {
            Ok(Some(address)) => match address.ip_address.parse() {
                Ok(ip) => return Ok((ip, address.prefix_length)),
                Err(error) => tracing::debug!(
                    "HNS reported \"{}\" as the address of VM \"{}\", which is not an IPv4 \
                     address: {error}",
                    address.ip_address,
                    mapping.vm_name
                ),
            },
            Ok(None) => {}
            Err(error) => tracing::debug!(
                "the address of VM \"{}\" is not readable yet: {error}",
                mapping.vm_name
            ),
        }
        if Instant::now() >= deadline {
            return Err(RepositoryError::new(format!(
                "VM \"{}\" was never given an address",
                mapping.vm_name
            )));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn wait_for_port(
    address: IpAddr,
    port: u16,
    timeouts: &ReadinessTimeouts,
) -> Result<(), RepositoryError> {
    let deadline = Instant::now() + timeouts.ssh_port;
    // Deliberately uninitialised: the only way out of this loop that reads it
    // is one that has probed at least once and been refused.
    let mut last_error;
    loop {
        match TcpStream::connect_timeout(&SocketAddr::new(address, port), timeouts.connect) {
            Ok(_) => return Ok(()),
            Err(error) => last_error = error.to_string(),
        }
        if Instant::now() >= deadline {
            return Err(RepositoryError::new(format!(
                "nothing answered at {address}:{port}: {last_error}"
            )));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Waits for a converted guest to finish powering itself off, then makes sure
/// its compute system is gone.
fn wait_for_shutdown(hcs_compute_system_id: &str) -> Result<(), RepositoryError> {
    let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
    loop {
        match crate::HcsSystem::open_if_present(hcs_compute_system_id, HCS_ACCESS_ALL) {
            Ok(None) => return Ok(()),
            Ok(Some(_)) => {}
            Err(error) => tracing::debug!(
                "the state of compute system \"{hcs_compute_system_id}\" is not readable: {error}"
            ),
        }
        if Instant::now() >= deadline {
            // The guest was asked to shut down and did not. Taking the system
            // apart is what the second boot needs, and the disk it leaves is
            // the one the conversion already finished writing.
            tracing::warn!(
                "the converted guest of compute system \"{hcs_compute_system_id}\" did not power \
                 itself off; taking its compute system apart"
            );
            return cleanup::teardown_compute_system(hcs_compute_system_id);
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Runs one remote command and answers with what it printed.
///
/// The output goes through a file rather than a pipe, like every other remote
/// command in VMLord: it is what a person reads when a guest refuses something
/// nobody was watching it refuse, and a file survives the process that wrote
/// it.
fn run_remote(
    invocation: &SshInvocation,
    transcript: &Path,
    timeout: Duration,
) -> Result<String, RepositoryError> {
    if let Some(parent) = transcript.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            RepositoryError::new(format!(
                "failed to create the import transcript directory {}: {error}",
                parent.display()
            ))
        })?;
    }
    let output = fs::File::create(transcript).map_err(|error| {
        RepositoryError::new(format!(
            "failed to open the import transcript {}: {error}",
            transcript.display()
        ))
    })?;
    let errors = output.try_clone().map_err(|error| {
        RepositoryError::new(format!(
            "failed to capture the errors of an import command: {error}"
        ))
    })?;

    tracing::debug!("running {}", invocation.command_line());
    let mut child = spawn(invocation, output, errors)?;

    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let printed = read_transcript(transcript);
                return if status.success() {
                    Ok(printed)
                } else {
                    Err(RepositoryError::new(format!(
                        "the guest command failed with status {}: {}",
                        status
                            .code()
                            .map_or_else(|| "unknown".to_owned(), |code| code.to_string()),
                        printed.trim()
                    )))
                };
            }
            Ok(None) => {}
            Err(error) => {
                return Err(RepositoryError::new(format!(
                    "failed to wait for an import command: {error}"
                )));
            }
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(RepositoryError::new(format!(
                "the guest did not answer within {} seconds: {}",
                timeout.as_secs(),
                read_transcript(transcript).trim()
            )));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(windows)]
fn spawn(
    invocation: &SshInvocation,
    output: fs::File,
    errors: fs::File,
) -> Result<std::process::Child, RepositoryError> {
    use std::os::windows::process::CommandExt;

    /// No console window for a command nobody asked to watch.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    Command::new(&invocation.program)
        .args(&invocation.args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(output))
        .stderr(Stdio::from(errors))
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|error| {
            RepositoryError::new(format!(
                "failed to run {}: {error}",
                invocation.program.display()
            ))
        })
}

#[cfg(not(windows))]
fn spawn(
    invocation: &SshInvocation,
    output: fs::File,
    errors: fs::File,
) -> Result<std::process::Child, RepositoryError> {
    Command::new(&invocation.program)
        .args(&invocation.args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(output))
        .stderr(Stdio::from(errors))
        .spawn()
        .map_err(|error| {
            RepositoryError::new(format!(
                "failed to run {}: {error}",
                invocation.program.display()
            ))
        })
}

/// What a command printed, or an empty answer when the file cannot be read.
fn read_transcript(transcript: &Path) -> String {
    let mut text = String::new();
    if let Ok(mut file) = fs::File::open(transcript) {
        let _ = file.read_to_string(&mut text);
    }
    text
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        time::{Duration, Instant},
    };

    use vmlord_core::RepositoryError;

    use super::keep_asking;

    #[test]
    fn a_check_that_comes_true_stops_being_asked() {
        // The guest answers SSH before its agent has mounted the payload
        // shares, so the first refusals are a guest that is early rather than
        // one that is wrong.
        let attempts = Cell::new(0);

        keep_asking(
            Instant::now() + Duration::from_secs(60),
            Duration::ZERO,
            || {
                attempts.set(attempts.get() + 1);
                if attempts.get() < 3 {
                    return Err(RepositoryError::new("not mounted yet"));
                }
                Ok(())
            },
        )
        .expect("a check that comes true is a check that passed");

        assert_eq!(attempts.get(), 3);
    }

    #[test]
    fn a_check_that_never_comes_true_reports_its_last_refusal() {
        // Not its first: a guest that was still refusing when the time ran out
        // is what says why the import failed.
        let attempts = Cell::new(0);

        let error = keep_asking(Instant::now(), Duration::ZERO, || {
            attempts.set(attempts.get() + 1);
            Err(RepositoryError::new(format!("refusal {}", attempts.get())))
        })
        .expect_err("a check that never comes true is a failure");

        assert_eq!(attempts.get(), 1);
        assert!(error.to_string().contains("refusal 1"), "{error}");
    }
}
