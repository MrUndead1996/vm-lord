//! Waiting until a freshly created guest is actually ready to be used.
//!
//! A VM that HCS reports as running is not a VM anyone can use: the SSH key is
//! installed at cloud-init's init stage, while `packages:` are applied later,
//! at its config stage. Probing the SSH port therefore answers "ready" in the
//! middle of the work. The guest's own `cloud-init status --wait` is what
//! actually knows, so that is what this module asks -- over Windows' own
//! OpenSSH client, because every maintained Rust SSH client is async-only and
//! this project has no async runtime.
//!
//! How far a wait can get depends on how the VM was told to let VMLord in. With
//! the key VMLord deployed, it can ask that question unattended. With a password
//! it cannot: the credential lives in the head of whoever created the VM, and a
//! build is not a moment anyone is at a prompt. Such a wait ends at an open
//! port and says so, rather than inventing a way to type the password or
//! reporting a completeness it never checked.

use std::{
    fmt,
    fs::File,
    io::Read,
    net::{IpAddr, SocketAddr, TcpStream},
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use vmlord_core::{
    BuildMonitor, GuestReadinessTimeouts, RepositoryError, SshAuthentication, SshEndpoint,
};

use crate::{layout, metadata::VmComputeSystemMapping, ssh, ssh::SshInvocation};

/// Keeps the readiness command from flashing a console window of its own.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// How often a running `ssh.exe` is looked at.
const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How often an unfinished phase looks again.
///
/// Two seconds rather than a tighter loop: nothing here becomes true faster
/// than a guest boots, and every poll of the address costs an HNS call.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// What the guest is asked, once a connection to it is possible.
///
/// `--wait` blocks until cloud-init is done; `--long` makes the answer name the
/// module that failed rather than only report that one did.
const READINESS_COMMAND: &str = "cloud-init status --wait --long";

/// How many lines of a transcript or a console log are worth carrying into a
/// diagnostic: enough for the failing unit and its context, short enough to
/// read in a message box.
pub(crate) const DIAGNOSTIC_TAIL_LINES: usize = 40;

/// A guest that has finished booting, with or without complaints.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum GuestReady {
    Ready,
    /// cloud-init finished, but a module of it failed. The guest is usable; the
    /// detail says what is missing from it.
    Degraded {
        detail: String,
    },
    /// The guest answers on its SSH port, and that is everything this wait was
    /// able to establish. A successful outcome -- the VM is up and a person can
    /// log into it -- carrying what was *not* checked, so that "ready" never
    /// silently means two different things.
    Unverified {
        detail: String,
    },
}

/// Every way waiting for a guest can end badly, each naming its own cause.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReadinessFailure {
    NoSshClient,
    NoAddress,
    /// The VM's SSH configuration is not one anything can connect with -- a
    /// user name that would be read as flags, say. Separate from a guest that
    /// does not answer: nothing was tried, and nothing will be until the stored
    /// configuration is fixed.
    Unusable {
        detail: String,
    },
    Unreachable {
        last_error: String,
    },
    CloudInitFailed {
        detail: String,
    },
    TimedOut,
    Cancelled,
}

impl fmt::Display for ReadinessFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSshClient => write!(
                formatter,
                "Windows has no OpenSSH client at \
                 %SystemRoot%\\System32\\OpenSSH\\ssh.exe; install the \
                 \"OpenSSH Client\" optional feature"
            ),
            Self::NoAddress => write!(
                formatter,
                "the VM started but was never given an address on the VMLord network"
            ),
            Self::Unusable { detail } => write!(
                formatter,
                "the VM's SSH configuration cannot be connected with: {detail}"
            ),
            Self::Unreachable { last_error } => write!(
                formatter,
                "the guest did not accept an SSH connection: {last_error}"
            ),
            Self::CloudInitFailed { detail } => {
                write!(formatter, "cloud-init failed inside the guest: {detail}")
            }
            Self::TimedOut => write!(
                formatter,
                "cloud-init did not finish configuring the guest in time"
            ),
            Self::Cancelled => write!(formatter, "waiting for the guest was cancelled"),
        }
    }
}

/// Turns what `ssh.exe` left behind into what is known about the guest.
///
/// `cloud-init status --wait` answers `0` when it is done, `1` when it failed
/// and `2` when it finished degraded. `255` is OpenSSH's own code, meaning the
/// command never ran. No code at all means the child was killed -- which is
/// what this module does to one that outlives its deadline.
pub(crate) fn outcome(
    exit_code: Option<i32>,
    transcript_tail: &str,
) -> Result<GuestReady, ReadinessFailure> {
    let detail = || {
        let text = transcript_tail.trim();
        if text.is_empty() {
            "no output".to_owned()
        } else {
            text.to_owned()
        }
    };

    match exit_code {
        Some(0) => Ok(GuestReady::Ready),
        Some(2) => Ok(GuestReady::Degraded { detail: detail() }),
        Some(1) => Err(ReadinessFailure::CloudInitFailed { detail: detail() }),
        Some(255) => Err(ReadinessFailure::Unreachable {
            last_error: detail(),
        }),
        Some(other) => Err(ReadinessFailure::CloudInitFailed {
            detail: format!("the SSH command exited with code {other}: {}", detail()),
        }),
        None => Err(ReadinessFailure::TimedOut),
    }
}

/// The timeouts of a wait, as durations rather than as settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReadinessTimeouts {
    pub(crate) address: Duration,
    pub(crate) ssh_port: Duration,
    pub(crate) cloud_init: Duration,
    pub(crate) connect: Duration,
}

impl From<GuestReadinessTimeouts> for ReadinessTimeouts {
    fn from(settings: GuestReadinessTimeouts) -> Self {
        Self {
            address: Duration::from_secs(settings.address_secs),
            ssh_port: Duration::from_secs(settings.ssh_port_secs),
            cloud_init: Duration::from_secs(settings.cloud_init_secs),
            connect: Duration::from_secs(settings.connect_timeout_secs),
        }
    }
}

impl Default for ReadinessTimeouts {
    fn default() -> Self {
        GuestReadinessTimeouts::default().into()
    }
}

/// How a run of `ssh.exe` ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SshRun {
    Exited {
        code: Option<i32>,
        transcript_tail: String,
    },
    /// The build was cancelled while the command ran, and the child was killed
    /// for it. Distinct from a deadline -- an `Exited` with no code -- because
    /// the two mean different things to whoever asked.
    Cancelled,
}

type AddressSource =
    Box<dyn Fn(&VmComputeSystemMapping) -> Result<Option<IpAddr>, RepositoryError> + Send + Sync>;
/// Answers `Err(reason)` while the port is not open; the reason is what the
/// failure quotes if the port never opens at all.
type PortProbe = Box<dyn Fn(IpAddr, u16, Duration) -> Result<(), String> + Send + Sync>;
type SshRunner = Box<
    dyn Fn(&ReadinessCommand, Duration, &BuildMonitor) -> Result<SshRun, RepositoryError>
        + Send
        + Sync,
>;
/// Time elapsed since the wait began, so tests can move it by hand.
type Clock = Box<dyn Fn() -> Duration + Send + Sync>;
type Sleeper = Box<dyn Fn(Duration) + Send + Sync>;

/// Waits until a guest is ready, or says why it is not.
///
/// Every seam is `Send + Sync`: the wait runs on the build thread.
pub(crate) struct GuestReadiness {
    timeouts: ReadinessTimeouts,
    /// `None` when this Windows installation has no OpenSSH client. Resolved
    /// once, at construction, so that a wait fails at once instead of spending
    /// twenty minutes on a guest it could never have asked.
    client: Option<PathBuf>,
    address: AddressSource,
    port: PortProbe,
    ssh: SshRunner,
    now: Clock,
    sleep: Sleeper,
}

impl GuestReadiness {
    /// A readiness backed by HNS, a real TCP socket and Windows' OpenSSH.
    #[must_use]
    pub(crate) fn production(timeouts: ReadinessTimeouts) -> Self {
        Self {
            timeouts,
            client: ssh::client_path(),
            address: Box::new(ssh::guest_address),
            port: Box::new(probe_port_at),
            ssh: Box::new(run_ssh),
            now: Box::new(elapsed_since_start()),
            sleep: Box::new(std::thread::sleep),
        }
    }

    /// Waits for the guest of `mapping`, in phases with a timeout each.
    ///
    /// Everything about the connection comes from the VM's own stored
    /// configuration: the user name cloud-init created, the port the daemon was
    /// configured with, and the credential VMLord holds. Nothing is guessed
    /// from the request the VM was built out of -- the wait runs against the VM
    /// that exists, not against the one that was asked for.
    pub(crate) fn wait(
        &self,
        mapping: &VmComputeSystemMapping,
        vm_directory: &Path,
        monitor: &BuildMonitor,
    ) -> Result<GuestReady, ReadinessFailure> {
        // A VM created with SSH switched off runs no daemon: there is no port
        // to wait for and nobody in the guest to ask. The build succeeds, and
        // says what it could not check.
        let Some(config) = mapping.ssh.clone() else {
            tracing::warn!(
                "VM \"{}\" was created without SSH access, so nothing can be asked whether \
                 cloud-init has finished",
                mapping.vm_name
            );
            return Ok(GuestReady::Unverified {
                detail: "the VM was created with SSH switched off, so there is nothing to ask \
                         whether cloud-init has finished"
                    .to_owned(),
            });
        };
        // Refused here rather than after the address arrives: a stored
        // configuration nothing can connect with is not going to become one,
        // and a wait that spends its address timeout first says the same thing
        // ninety seconds later.
        config.validate().map_err(|error| {
            tracing::error!(
                "VM \"{}\" cannot be asked anything over SSH: {error}",
                mapping.vm_name
            );
            ReadinessFailure::Unusable {
                detail: error.to_string(),
            }
        })?;
        let question = self.question(config.authentication)?;

        let ip = self.wait_for_address(mapping, monitor)?;
        let endpoint = SshEndpoint::new(mapping.vm_id, &config, ip).map_err(|error| {
            ReadinessFailure::Unusable {
                detail: error.to_string(),
            }
        })?;
        tracing::debug!(
            "VM \"{}\" has address {ip}; waiting for it to answer on port {}",
            mapping.vm_name,
            endpoint.port
        );
        // The configured port and no other. A cloud image ships its daemon on
        // 22 and cloud-init moves it, so a probe of 22 would answer during the
        // very window this wait exists to sit through.
        self.wait_for_port(mapping, ip, endpoint.port.get(), monitor)?;

        let client = match question {
            // Nobody is at a keyboard during a build, and there is no key to
            // offer instead: an open port is the whole of what a password-mode
            // wait can establish, and it establishes it honestly.
            Question::PortOnly => {
                tracing::warn!(
                    "VM \"{}\" answers on port {}, but it has no deploy key, so cloud-init was \
                     not asked whether it has finished",
                    mapping.vm_name,
                    endpoint.port
                );
                return Ok(GuestReady::Unverified {
                    detail: format!(
                        "the VM logs in by password, which nobody can type during a build, so \
                         only its SSH port was checked: the guest answers on port {}, and \
                         cloud-init may still be configuring it",
                        endpoint.port
                    ),
                });
            }
            Question::CloudInit(client) => client,
        };

        tracing::debug!(
            "VM \"{}\" answers on port {}; asking cloud-init whether it has finished",
            mapping.vm_name,
            endpoint.port
        );
        let command = readiness_command(&client, &endpoint, vm_directory, self.timeouts.connect);
        let run = (self.ssh)(&command, self.timeouts.cloud_init, monitor).map_err(|error| {
            tracing::error!(
                "could not ask VM \"{}\" whether cloud-init has finished: {error}",
                mapping.vm_name
            );
            ReadinessFailure::Unreachable {
                last_error: error.to_string(),
            }
        })?;

        let result = match run {
            SshRun::Cancelled => Err(ReadinessFailure::Cancelled),
            SshRun::Exited {
                code,
                transcript_tail,
            } => outcome(code, &transcript_tail),
        };
        match &result {
            Ok(GuestReady::Ready) => {
                tracing::info!("VM \"{}\" reports that cloud-init is done", mapping.vm_name);
            }
            Ok(GuestReady::Degraded { detail }) => tracing::warn!(
                "VM \"{}\" is up, but cloud-init finished degraded: {detail}",
                mapping.vm_name
            ),
            Ok(GuestReady::Unverified { detail }) => tracing::warn!(
                "VM \"{}\" is up, but its readiness was not verified: {detail}",
                mapping.vm_name
            ),
            Err(failure) => tracing::error!("VM \"{}\" is not ready: {failure}", mapping.vm_name),
        }
        result
    }

    /// What this wait will be able to establish, decided before it waits.
    ///
    /// Key mode needs `ssh.exe`, so an installation without one fails at once
    /// rather than after six minutes of waiting for a guest it could never have
    /// asked. Password mode never runs a client, so a missing one is not this
    /// wait's problem -- it is the interactive launcher's, which says so when a
    /// person asks for a shell.
    fn question(&self, authentication: SshAuthentication) -> Result<Question, ReadinessFailure> {
        match authentication {
            SshAuthentication::Password => Ok(Question::PortOnly),
            SshAuthentication::VmlordKey => match self.client.clone() {
                Some(client) => Ok(Question::CloudInit(client)),
                None => {
                    tracing::error!("{}", ReadinessFailure::NoSshClient);
                    Err(ReadinessFailure::NoSshClient)
                }
            },
        }
    }

    /// Phase one: the endpoint has to be given an address.
    fn wait_for_address(
        &self,
        mapping: &VmComputeSystemMapping,
        monitor: &BuildMonitor,
    ) -> Result<IpAddr, ReadinessFailure> {
        let deadline = (self.now)() + self.timeouts.address;
        loop {
            if monitor.is_cancelled() {
                return Err(ReadinessFailure::Cancelled);
            }
            match (self.address)(mapping) {
                Ok(Some(ip)) => return Ok(ip),
                Ok(None) => {}
                Err(error) => tracing::debug!(
                    "the address of VM \"{}\" is not readable yet: {error}",
                    mapping.vm_name
                ),
            }
            if (self.now)() >= deadline {
                return Err(ReadinessFailure::NoAddress);
            }
            (self.sleep)(POLL_INTERVAL);
        }
    }

    /// Phase two: something has to answer on the port the guest was configured
    /// with.
    ///
    /// A closed port and a refused connection are the same fact here -- the
    /// guest has not raised sshd yet -- and the last reason is kept so that the
    /// failure can quote it.
    fn wait_for_port(
        &self,
        mapping: &VmComputeSystemMapping,
        ip: IpAddr,
        port: u16,
        monitor: &BuildMonitor,
    ) -> Result<(), ReadinessFailure> {
        let deadline = (self.now)() + self.timeouts.ssh_port;
        // Deliberately uninitialised: the only way out of this loop that reads
        // it is one that has probed at least once and been refused.
        let mut last_error;
        loop {
            if monitor.is_cancelled() {
                return Err(ReadinessFailure::Cancelled);
            }
            match (self.port)(ip, port, self.timeouts.connect) {
                Ok(()) => return Ok(()),
                Err(error) => {
                    tracing::debug!(
                        "VM \"{}\" does not answer on {ip}:{port} yet: {error}",
                        mapping.vm_name
                    );
                    last_error = error;
                }
            }
            if (self.now)() >= deadline {
                return Err(ReadinessFailure::Unreachable { last_error });
            }
            (self.sleep)(POLL_INTERVAL);
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        timeouts: ReadinessTimeouts,
        address: impl Fn(&VmComputeSystemMapping) -> Result<Option<IpAddr>, RepositoryError>
        + Send
        + Sync
        + 'static,
        port: impl Fn(IpAddr, u16, Duration) -> Result<(), String> + Send + Sync + 'static,
        ssh: impl Fn(&ReadinessCommand, Duration, &BuildMonitor) -> Result<SshRun, RepositoryError>
        + Send
        + Sync
        + 'static,
        now: impl Fn() -> Duration + Send + Sync + 'static,
        sleep: impl Fn(Duration) + Send + Sync + 'static,
    ) -> Self {
        Self {
            timeouts,
            client: Some(PathBuf::from(r"C:\Windows\System32\OpenSSH\ssh.exe")),
            address: Box::new(address),
            port: Box::new(port),
            ssh: Box::new(ssh),
            now: Box::new(now),
            sleep: Box::new(sleep),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test_without_client(
        timeouts: ReadinessTimeouts,
        now: impl Fn() -> Duration + Send + Sync + 'static,
    ) -> Self {
        Self {
            timeouts,
            client: None,
            address: Box::new(|_| panic!("nothing may be asked without an ssh client")),
            port: Box::new(|_, _, _| panic!("nothing may be probed without an ssh client")),
            ssh: Box::new(|_, _, _| panic!("ssh cannot run without an ssh client")),
            now: Box::new(now),
            sleep: Box::new(|_| panic!("nothing may be waited for without an ssh client")),
        }
    }

    /// A host with no OpenSSH client whose guests are otherwise reachable: what
    /// a password-mode wait, which runs no client, still has to get through.
    #[cfg(test)]
    pub(crate) fn for_test_without_client_but_reachable(
        timeouts: ReadinessTimeouts,
        now: impl Fn() -> Duration + Send + Sync + 'static,
        sleep: impl Fn(Duration) + Send + Sync + 'static,
    ) -> Self {
        Self {
            timeouts,
            client: None,
            address: Box::new(|_| Ok(Some("10.0.0.2".parse().expect("a literal address")))),
            port: Box::new(|_, _, _| Ok(())),
            ssh: Box::new(|_, _, _| panic!("ssh cannot run without an ssh client")),
            now: Box::new(now),
            sleep: Box::new(sleep),
        }
    }
}

/// What a wait can find out about a guest, and what it needs to find it out.
enum Question {
    /// The guest can be asked cloud-init's own answer, with this client.
    CloudInit(PathBuf),
    /// Nothing can be asked: the wait ends where the port opens.
    PortOnly,
}

/// Everything one readiness command needs, decided without running it.
///
/// The command itself is the shared one every SSH connection to a VM uses; what
/// this adds is the only thing particular to a readiness wait, which is where
/// the long transcript of `--wait` goes.
pub(crate) struct ReadinessCommand {
    pub(crate) invocation: SshInvocation,
    /// Where the child's output goes. A file rather than a pipe: `--wait`
    /// prints a dot a second for as long as twenty minutes, and a pipe nobody
    /// drains fills up and deadlocks the child against the parent polling it.
    pub(crate) transcript: PathBuf,
}

/// Builds the command that asks the guest whether cloud-init has finished.
///
/// Only the question is decided here. Which key, which port, which user and
/// which host-key file are the endpoint's, and the arguments carrying them are
/// the shared builder's: a readiness wait is one more connection to the guest,
/// not a second way of connecting to it.
pub(crate) fn readiness_command(
    client: &Path,
    endpoint: &SshEndpoint,
    vm_directory: &Path,
    connect_timeout: Duration,
) -> ReadinessCommand {
    ReadinessCommand {
        invocation: ssh::invocation(
            client,
            endpoint,
            vm_directory,
            Some(connect_timeout),
            Some(READINESS_COMMAND),
        ),
        transcript: layout::cloud_init_status_log_path(vm_directory),
    }
}

/// The last `lines` lines of `text`, trimmed.
pub(crate) fn tail(text: &str, lines: usize) -> String {
    let kept: Vec<&str> = text.trim_end().lines().collect();
    let start = kept.len().saturating_sub(lines);
    kept[start..].join("\n").trim().to_owned()
}

/// Whether anything answers a TCP connection at `ip:port`.
fn probe_port_at(ip: IpAddr, port: u16, timeout: Duration) -> Result<(), String> {
    TcpStream::connect_timeout(&SocketAddr::new(ip, port), timeout)
        .map(drop)
        .map_err(|error| error.to_string())
}

/// Runs one readiness command, killing it at `deadline` or on cancellation.
fn run_ssh(
    command: &ReadinessCommand,
    deadline: Duration,
    monitor: &BuildMonitor,
) -> Result<SshRun, RepositoryError> {
    let invocation = &command.invocation;
    let transcript = File::create(&command.transcript).map_err(|error| {
        let error = RepositoryError::new(format!(
            "failed to open the readiness transcript {}: {error}",
            command.transcript.display()
        ));
        tracing::error!("{error}");
        error
    })?;
    let errors = transcript.try_clone().map_err(|error| {
        let error = RepositoryError::new(format!(
            "failed to capture the errors of the readiness command: {error}"
        ));
        tracing::error!("{error}");
        error
    })?;

    tracing::debug!(
        "running {} to wait for cloud-init; its output goes to {}",
        invocation.program.display(),
        command.transcript.display()
    );
    let mut child = Command::new(&invocation.program)
        .args(&invocation.args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(transcript))
        .stderr(Stdio::from(errors))
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|error| {
            let error = RepositoryError::new(format!(
                "failed to run {}: {error}",
                invocation.program.display()
            ));
            tracing::error!("{error}");
            error
        })?;

    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Ok(SshRun::Exited {
                    code: status.code(),
                    transcript_tail: transcript_tail(&command.transcript),
                });
            }
            Ok(None) => {}
            Err(error) => {
                let error = RepositoryError::new(format!(
                    "failed to wait for {}: {error}",
                    invocation.program.display()
                ));
                tracing::error!("{error}");
                return Err(error);
            }
        }

        if monitor.is_cancelled() {
            tracing::warn!("killing the readiness command because the build was cancelled");
            kill(&mut child);
            return Ok(SshRun::Cancelled);
        }
        if started.elapsed() >= deadline {
            tracing::error!(
                "the readiness command did not finish within {} seconds; killing it",
                deadline.as_secs()
            );
            kill(&mut child);
            return Ok(SshRun::Exited {
                code: None,
                transcript_tail: transcript_tail(&command.transcript),
            });
        }
        std::thread::sleep(CHILD_POLL_INTERVAL);
    }
}

/// Kills a child and reaps it, so that no zombie handle is left behind.
fn kill(child: &mut Child) {
    if let Err(error) = child.kill() {
        tracing::warn!("the readiness command could not be killed: {error}");
        return;
    }
    if let Err(error) = child.wait() {
        tracing::warn!("the killed readiness command could not be reaped: {error}");
    }
}

/// The end of a file, or nothing when there is nothing to read.
fn file_tail(path: &Path) -> Option<String> {
    let mut text = String::new();
    File::open(path).ok()?.read_to_string(&mut text).ok()?;
    Some(tail(&text, DIAGNOSTIC_TAIL_LINES))
}

fn transcript_tail(path: &Path) -> String {
    file_tail(path).unwrap_or_default()
}

/// The end of the VM's serial console log, for a diagnostic about a guest that
/// never became ready. A VM with no log yet has nothing to add rather than an
/// error to report.
pub(crate) fn com1_tail(vm_directory: &Path) -> Option<String> {
    let captured = file_tail(&layout::com1_log_path(vm_directory))?;
    (!captured.is_empty()).then_some(captured)
}

fn elapsed_since_start() -> impl Fn() -> Duration + Send + Sync {
    let start = std::time::Instant::now();
    move || start.elapsed()
}

#[cfg(test)]
mod tests {
    use std::{
        net::IpAddr,
        path::Path,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
        },
        time::Duration,
    };

    use uuid::Uuid;
    use vmlord_core::{
        BuildMonitor, BuildStep, NetworkMode, RepositoryError, SshAuthentication, SshConfig,
        SshEndpoint, SshPort,
    };

    use super::{
        GuestReadiness, GuestReady, ReadinessCommand, ReadinessFailure, ReadinessTimeouts, SshRun,
        outcome, readiness_command, tail,
    };
    use crate::metadata::VmComputeSystemMapping;

    fn config() -> SshConfig {
        SshConfig {
            username: "machi".to_owned(),
            port: SshPort::DEFAULT,
            authentication: SshAuthentication::VmlordKey,
        }
    }

    fn mapping_with(ssh: Option<SshConfig>) -> VmComputeSystemMapping {
        VmComputeSystemMapping {
            vm_id: Uuid::nil(),
            vm_name: "dev-linux".to_owned(),
            hcs_compute_system_id: "vmlord-test".to_owned(),
            disk_gb: 20,
            endpoint_id: None,
            network_mode: NetworkMode::Nat,
            ssh,
            gpu_mode: vmlord_core::GpuMode::None,
            desktop_profile: vmlord_core::DesktopProfile::Headless,
            display_provisioning: vmlord_core::DisplayProvisioning::NotRequested,
            display_mode: None,
            guest_target: None,
        }
    }

    /// The ordinary VM these tests wait for: a cloud image with the key VMLord
    /// deployed into it.
    fn mapping() -> VmComputeSystemMapping {
        mapping_with(Some(config()))
    }

    fn monitor() -> BuildMonitor {
        BuildMonitor::new(BuildStep::AwaitingGuest)
    }

    fn timeouts() -> ReadinessTimeouts {
        ReadinessTimeouts {
            address: Duration::from_secs(90),
            ssh_port: Duration::from_secs(300),
            cloud_init: Duration::from_secs(1200),
            connect: Duration::from_secs(10),
        }
    }

    fn address() -> IpAddr {
        "10.0.0.2".parse().unwrap()
    }

    fn vm_directory() -> &'static Path {
        Path::new(r"C:\VMs\dev-linux")
    }

    /// A clock that only moves when something sleeps on it, so a twenty-minute
    /// timeout costs a test no time at all.
    #[derive(Clone, Default)]
    struct FakeClock(Arc<AtomicU64>);

    impl FakeClock {
        fn elapsed(&self) -> Duration {
            Duration::from_millis(self.0.load(Ordering::Relaxed))
        }

        fn now(&self) -> impl Fn() -> Duration + Send + Sync + use<> {
            let millis = Arc::clone(&self.0);
            move || Duration::from_millis(millis.load(Ordering::Relaxed))
        }

        fn sleeper(&self) -> impl Fn(Duration) + Send + Sync + use<> {
            let millis = Arc::clone(&self.0);
            move |duration: Duration| {
                millis.fetch_add(
                    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
                    Ordering::Relaxed,
                );
            }
        }
    }

    #[test]
    fn a_guest_that_answers_at_once_is_ready() {
        let clock = FakeClock::default();
        let readiness = GuestReadiness::for_test(
            timeouts(),
            |_| Ok(Some(address())),
            |_, _, _| Ok(()),
            |_, _, _| {
                Ok(SshRun::Exited {
                    code: Some(0),
                    transcript_tail: "status: done".to_owned(),
                })
            },
            clock.now(),
            clock.sleeper(),
        );

        let ready = readiness
            .wait(&mapping(), vm_directory(), &monitor())
            .unwrap();

        assert_eq!(ready, GuestReady::Ready);
    }

    #[test]
    fn an_address_that_never_arrives_fails_on_its_own_timeout() {
        let clock = FakeClock::default();
        let readiness = GuestReadiness::for_test(
            timeouts(),
            |_| Ok(None),
            |_, _, _| panic!("the port must not be probed before an address exists"),
            |_, _, _| panic!("ssh must not run before an address exists"),
            clock.now(),
            clock.sleeper(),
        );

        let failure = readiness
            .wait(&mapping(), vm_directory(), &monitor())
            .unwrap_err();

        assert_eq!(failure, ReadinessFailure::NoAddress);
        // Its own timeout and no more: the later phases have theirs.
        assert!(
            clock.elapsed() >= Duration::from_secs(90),
            "{:?}",
            clock.elapsed()
        );
        assert!(
            clock.elapsed() < Duration::from_secs(120),
            "{:?}",
            clock.elapsed()
        );
    }

    #[test]
    fn a_port_that_never_opens_reports_the_last_connection_error() {
        let clock = FakeClock::default();
        let readiness = GuestReadiness::for_test(
            timeouts(),
            |_| Ok(Some(address())),
            |_, _, _| Err("connection refused".to_owned()),
            |_, _, _| panic!("ssh must not run before the guest's SSH port opens"),
            clock.now(),
            clock.sleeper(),
        );

        let failure = readiness
            .wait(&mapping(), vm_directory(), &monitor())
            .unwrap_err();

        assert_eq!(
            failure,
            ReadinessFailure::Unreachable {
                last_error: "connection refused".to_owned(),
            }
        );
        assert!(
            clock.elapsed() >= Duration::from_secs(300),
            "{:?}",
            clock.elapsed()
        );
    }

    #[test]
    fn a_cancellation_while_waiting_for_an_address_stops_the_wait() {
        let clock = FakeClock::default();
        let monitor = monitor();
        let readiness = GuestReadiness::for_test(
            timeouts(),
            {
                let monitor = monitor.clone();
                move |_| {
                    monitor.cancel();
                    Ok(None)
                }
            },
            |_, _, _| panic!("the port must not be probed after a cancellation"),
            |_, _, _| panic!("ssh must not run after a cancellation"),
            clock.now(),
            clock.sleeper(),
        );

        let failure = readiness
            .wait(&mapping(), vm_directory(), &monitor)
            .unwrap_err();

        assert_eq!(failure, ReadinessFailure::Cancelled);
    }

    #[test]
    fn a_cancellation_while_waiting_for_the_port_stops_the_wait() {
        let clock = FakeClock::default();
        let monitor = monitor();
        let readiness = GuestReadiness::for_test(
            timeouts(),
            |_| Ok(Some(address())),
            {
                let monitor = monitor.clone();
                move |_, _, _| {
                    monitor.cancel();
                    Err("not yet".to_owned())
                }
            },
            |_, _, _| panic!("ssh must not run after a cancellation"),
            clock.now(),
            clock.sleeper(),
        );

        let failure = readiness
            .wait(&mapping(), vm_directory(), &monitor)
            .unwrap_err();

        assert_eq!(failure, ReadinessFailure::Cancelled);
    }

    #[test]
    fn a_cancelled_ssh_run_is_reported_as_a_cancellation_not_a_failure() {
        let clock = FakeClock::default();
        let readiness = GuestReadiness::for_test(
            timeouts(),
            |_| Ok(Some(address())),
            |_, _, _| Ok(()),
            |_, _, _| Ok(SshRun::Cancelled),
            clock.now(),
            clock.sleeper(),
        );

        let failure = readiness
            .wait(&mapping(), vm_directory(), &monitor())
            .unwrap_err();

        assert_eq!(failure, ReadinessFailure::Cancelled);
    }

    #[test]
    fn a_killed_ssh_child_is_a_timeout_not_a_cloud_init_failure() {
        let clock = FakeClock::default();
        let readiness = GuestReadiness::for_test(
            timeouts(),
            |_| Ok(Some(address())),
            |_, _, _| Ok(()),
            |_, _, _| {
                Ok(SshRun::Exited {
                    code: None,
                    transcript_tail: "....".to_owned(),
                })
            },
            clock.now(),
            clock.sleeper(),
        );

        let failure = readiness
            .wait(&mapping(), vm_directory(), &monitor())
            .unwrap_err();

        assert_eq!(failure, ReadinessFailure::TimedOut);
    }

    #[test]
    fn an_address_lookup_that_fails_outright_is_reported_as_no_address() {
        // HNS refusing to answer is not the same as an address that has not
        // arrived yet, but from the guest's side the outcome is the same, and
        // the underlying error has already gone to the log.
        let clock = FakeClock::default();
        let readiness = GuestReadiness::for_test(
            timeouts(),
            |_| Err(RepositoryError::new("HNS is unavailable")),
            |_, _, _| panic!("the port must not be probed without an address"),
            |_, _, _| panic!("ssh must not run without an address"),
            clock.now(),
            clock.sleeper(),
        );

        let failure = readiness
            .wait(&mapping(), vm_directory(), &monitor())
            .unwrap_err();

        assert_eq!(failure, ReadinessFailure::NoAddress);
    }

    #[test]
    fn a_missing_ssh_client_is_reported_before_anything_is_waited_for() {
        let clock = FakeClock::default();
        let readiness = GuestReadiness::for_test_without_client(timeouts(), clock.now());

        let failure = readiness
            .wait(&mapping(), vm_directory(), &monitor())
            .unwrap_err();

        assert_eq!(failure, ReadinessFailure::NoSshClient);
        assert_eq!(clock.elapsed(), Duration::ZERO, "nothing may be waited for");
    }

    #[test]
    fn the_com1_tail_is_the_end_of_the_log_and_nothing_more() {
        let directory =
            std::env::temp_dir().join(format!("vmlord-com1-tail-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let lines = (1..=100)
            .map(|n| format!("boot line {n}"))
            .collect::<Vec<_>>();
        std::fs::write(crate::layout::com1_log_path(&directory), lines.join("\n")).unwrap();

        let captured = super::com1_tail(&directory).unwrap();

        assert!(captured.contains("boot line 100"), "{captured}");
        assert!(
            !captured.contains("boot line 59"),
            "only the last {} lines belong in a diagnostic: {captured}",
            super::DIAGNOSTIC_TAIL_LINES
        );
        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn a_vm_without_a_serial_log_has_no_tail_rather_than_an_error() {
        // A VM whose console never opened still has to produce a diagnostic
        // about its readiness; a missing log is simply nothing to add to one.
        assert_eq!(super::com1_tail(Path::new(r"C:\VMs\never-existed")), None);
    }

    #[test]
    fn a_probe_of_a_port_nobody_listens_on_fails_rather_than_hangs() {
        // Port 9 is discard, which nothing serves on a developer machine: the
        // probe has to come back with a reason rather than block.
        let probed =
            super::probe_port_at("127.0.0.1".parse().unwrap(), 9, Duration::from_millis(200));

        assert!(probed.is_err(), "an unserved port must not report success");
    }

    #[test]
    fn the_timeouts_come_from_the_settings_unchanged() {
        let converted = ReadinessTimeouts::from(vmlord_core::GuestReadinessTimeouts {
            address_secs: 1,
            ssh_port_secs: 2,
            cloud_init_secs: 3,
            connect_timeout_secs: 4,
        });

        assert_eq!(
            converted,
            ReadinessTimeouts {
                address: Duration::from_secs(1),
                ssh_port: Duration::from_secs(2),
                cloud_init: Duration::from_secs(3),
                connect: Duration::from_secs(4),
            }
        );
        assert_eq!(ReadinessTimeouts::default(), timeouts());
    }

    fn arguments(command: &ReadinessCommand) -> Vec<String> {
        command
            .invocation
            .args
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn the_readiness_command_asks_cloud_init_and_keeps_its_answer_beside_the_vm() {
        let endpoint = SshEndpoint::new(
            Uuid::nil(),
            &config(),
            "172.22.42.7".parse::<IpAddr>().unwrap(),
        )
        .unwrap();
        let command = readiness_command(
            Path::new(r"C:\Windows\System32\OpenSSH\ssh.exe"),
            &endpoint,
            Path::new(r"C:\VMs\dev-linux"),
            Duration::from_secs(10),
        );
        let args = arguments(&command);

        assert_eq!(
            command.invocation.program,
            Path::new(r"C:\Windows\System32\OpenSSH\ssh.exe")
        );
        assert_eq!(
            args.last().map(String::as_str),
            Some("cloud-init status --wait --long"),
            "{args:?}"
        );
        assert_eq!(
            command.transcript,
            Path::new(r"C:\VMs\dev-linux\cloud-init-status.log")
        );
        // The rest of the arguments are the shared builder's, and are its tests
        // to make; what a readiness wait cannot survive is a prompt, so that one
        // is checked here too.
        assert!(args.contains(&"BatchMode=\"yes\"".to_owned()), "{args:?}");
        assert!(
            args.contains(&"ConnectTimeout=\"10\"".to_owned()),
            "{args:?}"
        );
    }

    #[test]
    fn a_user_name_the_guest_would_refuse_stops_the_wait_before_anything_is_waited_for() {
        let clock = FakeClock::default();
        let damaged = SshConfig {
            username: "root -oProxyCommand=calc".to_owned(),
            ..config()
        };
        let readiness = GuestReadiness::for_test(
            timeouts(),
            |_| panic!("a configuration nothing can connect with is not worth an address"),
            |_, _, _| panic!("nor a port"),
            |_, _, _| panic!("such a name must never reach ssh.exe"),
            clock.now(),
            clock.sleeper(),
        );

        let failure = readiness
            .wait(&mapping_with(Some(damaged)), vm_directory(), &monitor())
            .unwrap_err();

        let ReadinessFailure::Unusable { detail } = failure else {
            panic!("a stored configuration nothing can connect with is its own failure");
        };
        assert!(detail.contains("user"), "{detail}");
        assert_eq!(clock.elapsed(), Duration::ZERO, "nothing may be waited for");
    }

    /// A cloud image ships its daemon on 22 and cloud-init moves it to the port
    /// the VM was created with. A wait that probed 22 would therefore answer
    /// during the very window it exists to sit through -- and then ask the
    /// question on a port nothing listens on.
    #[test]
    fn the_configured_port_is_the_only_one_a_key_mode_wait_looks_at() {
        let clock = FakeClock::default();
        let probed = Arc::new(Mutex::new(Vec::new()));
        let asked = Arc::new(Mutex::new(Vec::new()));
        let readiness = GuestReadiness::for_test(
            timeouts(),
            |_| Ok(Some(address())),
            {
                let probed = Arc::clone(&probed);
                move |ip, port, _| {
                    probed.lock().unwrap().push((ip, port));
                    Ok(())
                }
            },
            {
                let asked = Arc::clone(&asked);
                move |command: &ReadinessCommand, _, _| {
                    *asked.lock().unwrap() = arguments(command);
                    Ok(SshRun::Exited {
                        code: Some(0),
                        transcript_tail: "status: done".to_owned(),
                    })
                }
            },
            clock.now(),
            clock.sleeper(),
        );
        let mapping = mapping_with(Some(SshConfig {
            port: SshPort::new(2222).unwrap(),
            ..config()
        }));

        let ready = readiness
            .wait(&mapping, vm_directory(), &monitor())
            .unwrap();

        assert_eq!(ready, GuestReady::Ready);
        assert_eq!(
            *probed.lock().unwrap(),
            [(address(), 2222)],
            "the temporary listener on 22 is not this guest's SSH port"
        );
        let asked = asked.lock().unwrap().clone();
        let port_flag = asked.iter().position(|argument| argument == "-p").unwrap();
        assert_eq!(
            asked[port_flag + 1],
            "2222",
            "the port that was probed is the port the question is asked on: {asked:?}"
        );
    }

    /// Password mode has no credential VMLord can present unattended: the
    /// password is in a person's head, and a build is not a moment anyone is at
    /// a prompt. The wait ends where the port opens, and says what it did not
    /// check rather than reporting a readiness it never established.
    #[test]
    fn a_password_only_guest_is_waited_for_up_to_its_port_and_no_further() {
        let clock = FakeClock::default();
        let probed = Arc::new(Mutex::new(Vec::new()));
        let readiness = GuestReadiness::for_test(
            timeouts(),
            |_| Ok(Some(address())),
            {
                let probed = Arc::clone(&probed);
                move |ip, port, _| {
                    probed.lock().unwrap().push((ip, port));
                    Ok(())
                }
            },
            |_, _, _| panic!("a password nobody can type must not be attempted"),
            clock.now(),
            clock.sleeper(),
        );
        let mapping = mapping_with(Some(SshConfig {
            port: SshPort::new(2222).unwrap(),
            authentication: SshAuthentication::Password,
            ..config()
        }));

        let ready = readiness
            .wait(&mapping, vm_directory(), &monitor())
            .unwrap();

        let GuestReady::Unverified { detail } = ready else {
            panic!("a guest whose cloud-init was never asked is not a verified one");
        };
        assert!(
            detail.contains("password") && detail.contains("cloud-init"),
            "the warning has to say what was not checked: {detail}"
        );
        assert!(detail.contains("2222"), "{detail}");
        assert_eq!(
            *probed.lock().unwrap(),
            [(address(), 2222)],
            "the configured port is waited for in both modes"
        );
    }

    /// Only key mode runs a client. A password-mode build must not fail over a
    /// Windows feature it was never going to use -- the interactive launcher is
    /// where a missing `ssh.exe` becomes a person's problem.
    #[test]
    fn a_password_only_guest_needs_no_openssh_client_on_the_host() {
        let clock = FakeClock::default();
        let readiness = GuestReadiness::for_test_without_client_but_reachable(
            timeouts(),
            clock.now(),
            clock.sleeper(),
        );
        let mapping = mapping_with(Some(SshConfig {
            authentication: SshAuthentication::Password,
            ..config()
        }));

        let ready = readiness
            .wait(&mapping, vm_directory(), &monitor())
            .unwrap();

        assert!(matches!(ready, GuestReady::Unverified { .. }));
    }

    /// A VM created with SSH switched off runs no daemon: there is no port to
    /// wait for and nobody to ask. The build succeeds -- the VM is there, and
    /// its console is how a person gets in -- and says what it could not check.
    #[test]
    fn a_vm_created_without_ssh_is_not_waited_for_at_all() {
        let clock = FakeClock::default();
        let readiness = GuestReadiness::for_test(
            timeouts(),
            |_| panic!("a VM with no SSH server has no address worth waiting for"),
            |_, _, _| panic!("nor a port"),
            |_, _, _| panic!("nor anything to ask"),
            clock.now(),
            clock.sleeper(),
        );

        let ready = readiness
            .wait(&mapping_with(None), vm_directory(), &monitor())
            .unwrap();

        let GuestReady::Unverified { detail } = ready else {
            panic!("a guest nothing can be asked of is not a verified one");
        };
        assert!(detail.contains("SSH"), "{detail}");
        assert_eq!(clock.elapsed(), Duration::ZERO, "nothing may be waited for");
    }

    #[test]
    fn a_zero_exit_means_the_guest_is_ready() {
        assert!(matches!(
            outcome(Some(0), "status: done"),
            Ok(GuestReady::Ready)
        ));
    }

    #[test]
    fn a_two_exit_means_ready_but_degraded_and_keeps_the_detail() {
        // cloud-init answers 2 when a module of it failed while the system
        // still came up. One broken module must not turn a working VM into a
        // failed build.
        let Ok(GuestReady::Degraded { detail }) = outcome(
            Some(2),
            "status: degraded done\nerror: cc_package_update failed",
        ) else {
            panic!("exit code 2 must report a degraded but ready guest");
        };

        assert!(detail.contains("cc_package_update"), "{detail}");
    }

    #[test]
    fn a_one_exit_is_a_cloud_init_failure_and_keeps_the_detail() {
        let Err(ReadinessFailure::CloudInitFailed { detail }) =
            outcome(Some(1), "status: error\nerror: no such file")
        else {
            panic!("exit code 1 must report a cloud-init failure");
        };

        assert!(detail.contains("no such file"), "{detail}");
    }

    #[test]
    fn exit_code_255_is_ssh_itself_failing_not_cloud_init() {
        // 255 is OpenSSH's own code: the command was never run, so nothing is
        // known about cloud-init and everything about the connection.
        let Err(ReadinessFailure::Unreachable { last_error }) =
            outcome(Some(255), "Permission denied (publickey).")
        else {
            panic!("exit code 255 must report an unreachable guest");
        };

        assert!(last_error.contains("Permission denied"), "{last_error}");
    }

    #[test]
    fn no_exit_code_means_the_child_was_killed_at_its_deadline() {
        assert_eq!(outcome(None, ""), Err(ReadinessFailure::TimedOut));
    }

    #[test]
    fn an_unknown_exit_code_names_itself_rather_than_being_swallowed() {
        let Err(ReadinessFailure::CloudInitFailed { detail }) = outcome(Some(42), "") else {
            panic!("an unrecognised exit code must still fail the wait");
        };

        assert!(detail.contains("42"), "{detail}");
    }

    #[test]
    fn the_tail_keeps_the_last_lines_and_nothing_before_them() {
        let text = (1..=10)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");

        assert_eq!(tail(&text, 3), "line 8\nline 9\nline 10");
    }

    #[test]
    fn a_text_shorter_than_the_tail_is_kept_whole_and_trimmed() {
        assert_eq!(tail("only one\n", 40), "only one");
    }

    #[test]
    fn every_failure_names_its_cause_rather_than_a_code() {
        // The style `error.rs` sets: the message says what happened, not which
        // number happened.
        let messages = [
            ReadinessFailure::NoSshClient.to_string(),
            ReadinessFailure::NoAddress.to_string(),
            ReadinessFailure::Unusable {
                detail: "the user name is not one Linux would accept".into(),
            }
            .to_string(),
            ReadinessFailure::Unreachable {
                last_error: "refused".into(),
            }
            .to_string(),
            ReadinessFailure::CloudInitFailed {
                detail: "boom".into(),
            }
            .to_string(),
            ReadinessFailure::TimedOut.to_string(),
            ReadinessFailure::Cancelled.to_string(),
        ];

        assert!(messages[0].contains("OpenSSH"), "{}", messages[0]);
        assert!(messages[1].contains("address"), "{}", messages[1]);
        assert!(messages[2].contains("user name"), "{}", messages[2]);
        assert!(messages[3].contains("refused"), "{}", messages[3]);
        assert!(messages[4].contains("boom"), "{}", messages[4]);
        assert!(messages[5].contains("did not finish"), "{}", messages[5]);
        assert!(messages[6].contains("cancelled"), "{}", messages[6]);
    }
}
