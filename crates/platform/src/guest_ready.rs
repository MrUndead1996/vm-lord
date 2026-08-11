//! Waiting until a freshly created guest is actually ready to be used.
//!
//! A VM that HCS reports as running is not a VM anyone can use: the SSH key is
//! installed at cloud-init's init stage, while `packages:` are applied later,
//! at its config stage. Probing port 22 therefore answers "ready" in the middle
//! of the work. The guest's own `cloud-init status --wait` is what actually
//! knows, so that is what this module asks -- over Windows' own OpenSSH client,
//! because every maintained Rust SSH client is async-only and this project has
//! no async runtime.

use std::{
    ffi::OsString,
    fmt,
    fs::File,
    io::Read,
    net::{IpAddr, SocketAddr, TcpStream},
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use vmlord_core::{BuildMonitor, GuestReadinessTimeouts, RepositoryError};

use crate::{hcn_endpoint::HcnEndpoint, layout, metadata::VmComputeSystemMapping};

/// Keeps the readiness command from flashing a console window of its own.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// How often a running `ssh.exe` is looked at.
const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How often an unfinished phase looks again.
///
/// Two seconds rather than a tighter loop: nothing here becomes true faster
/// than a guest boots, and every poll of the address costs an HNS call.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// The port a Linux guest answers SSH on. Fixed, because the seed does not
/// move it.
const SSH_PORT: u16 = 22;

/// Where Windows keeps its OpenSSH client.
const SSH_CLIENT_RELATIVE_PATH: &str = r"System32\OpenSSH\ssh.exe";

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
}

/// Every way waiting for a guest can end badly, each naming its own cause.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReadinessFailure {
    NoSshClient,
    NoAddress,
    Unreachable { last_error: String },
    CloudInitFailed { detail: String },
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
type PortProbe = Box<dyn Fn(IpAddr, Duration) -> Result<(), String> + Send + Sync>;
type SshRunner = Box<
    dyn Fn(&SshInvocation, Duration, &BuildMonitor) -> Result<SshRun, RepositoryError>
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
            client: ssh_client_path(),
            address: Box::new(endpoint_address),
            port: Box::new(probe_port),
            ssh: Box::new(run_ssh),
            now: Box::new(elapsed_since_start()),
            sleep: Box::new(std::thread::sleep),
        }
    }

    /// Waits for the guest of `mapping`, in three phases with a timeout each.
    pub(crate) fn wait(
        &self,
        mapping: &VmComputeSystemMapping,
        vm_directory: &Path,
        username: &str,
        monitor: &BuildMonitor,
    ) -> Result<GuestReady, ReadinessFailure> {
        let Some(client) = self.client.clone() else {
            log::error!("{}", ReadinessFailure::NoSshClient);
            return Err(ReadinessFailure::NoSshClient);
        };

        let ip = self.wait_for_address(mapping, monitor)?;
        log::debug!(
            "VM \"{}\" has address {ip}; waiting for it to answer on port {SSH_PORT}",
            mapping.vm_name
        );
        self.wait_for_port(mapping, ip, monitor)?;
        log::debug!(
            "VM \"{}\" answers on port {SSH_PORT}; asking cloud-init whether it has finished",
            mapping.vm_name
        );

        let invocation = ssh_invocation(&client, vm_directory, username, ip, self.timeouts.connect);
        let run = (self.ssh)(&invocation, self.timeouts.cloud_init, monitor).map_err(|error| {
            log::error!(
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
                log::info!("VM \"{}\" reports that cloud-init is done", mapping.vm_name);
            }
            Ok(GuestReady::Degraded { detail }) => log::warn!(
                "VM \"{}\" is up, but cloud-init finished degraded: {detail}",
                mapping.vm_name
            ),
            Err(failure) => log::error!("VM \"{}\" is not ready: {failure}", mapping.vm_name),
        }
        result
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
                Err(error) => log::debug!(
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

    /// Phase two: something has to answer on port 22.
    ///
    /// A closed port and a refused connection are the same fact here -- the
    /// guest has not raised sshd yet -- and the last reason is kept so that the
    /// failure can quote it.
    fn wait_for_port(
        &self,
        mapping: &VmComputeSystemMapping,
        ip: IpAddr,
        monitor: &BuildMonitor,
    ) -> Result<(), ReadinessFailure> {
        let deadline = (self.now)() + self.timeouts.ssh_port;
        let mut last_error = "the guest never answered".to_owned();
        loop {
            if monitor.is_cancelled() {
                return Err(ReadinessFailure::Cancelled);
            }
            match (self.port)(ip, self.timeouts.connect) {
                Ok(()) => return Ok(()),
                Err(error) => {
                    log::debug!(
                        "VM \"{}\" does not answer on {ip}:{SSH_PORT} yet: {error}",
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
    fn for_test(
        timeouts: ReadinessTimeouts,
        address: impl Fn(&VmComputeSystemMapping) -> Result<Option<IpAddr>, RepositoryError>
        + Send
        + Sync
        + 'static,
        port: impl Fn(IpAddr, Duration) -> Result<(), String> + Send + Sync + 'static,
        ssh: impl Fn(&SshInvocation, Duration, &BuildMonitor) -> Result<SshRun, RepositoryError>
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
    fn for_test_without_client(
        timeouts: ReadinessTimeouts,
        now: impl Fn() -> Duration + Send + Sync + 'static,
    ) -> Self {
        Self {
            timeouts,
            client: None,
            address: Box::new(|_| panic!("nothing may be asked without an ssh client")),
            port: Box::new(|_, _| panic!("nothing may be probed without an ssh client")),
            ssh: Box::new(|_, _, _| panic!("ssh cannot run without an ssh client")),
            now: Box::new(now),
            sleep: Box::new(|_| panic!("nothing may be waited for without an ssh client")),
        }
    }
}

/// Everything one readiness command needs, decided without running it.
///
/// Separate from running it so that the decisions -- which key, which
/// known-hosts file, which timeout -- are testable without a guest, a network,
/// or an `ssh.exe` to run.
pub(crate) struct SshInvocation {
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<OsString>,
    /// Where the child's output goes. A file rather than a pipe: `--wait`
    /// prints a dot a second for as long as twenty minutes, and a pipe nobody
    /// drains fills up and deadlocks the child against the parent polling it.
    pub(crate) transcript: PathBuf,
}

/// Windows' own OpenSSH client, if this installation has one.
///
/// Optional Windows features can be absent, so this is a state to report rather
/// than a reason to panic: [`ReadinessFailure::NoSshClient`] names the feature
/// a person has to install.
pub(crate) fn ssh_client_path() -> Option<PathBuf> {
    let root = std::env::var_os("SystemRoot")?;
    let path = PathBuf::from(root).join(SSH_CLIENT_RELATIVE_PATH);
    path.is_file().then_some(path)
}

/// Builds the command that asks the guest whether cloud-init has finished.
pub(crate) fn ssh_invocation(
    client: &Path,
    vm_directory: &Path,
    username: &str,
    ip: IpAddr,
    connect_timeout: Duration,
) -> SshInvocation {
    let mut args = vec![
        OsString::from("-i"),
        layout::ssh_key_path(vm_directory).into_os_string(),
    ];
    for option in [
        "BatchMode=yes".to_owned(),
        "IdentitiesOnly=yes".to_owned(),
        "StrictHostKeyChecking=accept-new".to_owned(),
        format!(
            "UserKnownHostsFile={}",
            vm_directory.join("known_hosts").display()
        ),
        format!("ConnectTimeout={}", connect_timeout.as_secs()),
    ] {
        args.push(OsString::from("-o"));
        args.push(OsString::from(option));
    }
    args.push(OsString::from("-l"));
    args.push(OsString::from(username));
    args.push(OsString::from(ip.to_string()));
    args.push(OsString::from(READINESS_COMMAND));

    SshInvocation {
        program: client.to_path_buf(),
        args,
        transcript: layout::cloud_init_status_log_path(vm_directory),
    }
}

/// The last `lines` lines of `text`, trimmed.
pub(crate) fn tail(text: &str, lines: usize) -> String {
    let kept: Vec<&str> = text.trim_end().lines().collect();
    let start = kept.len().saturating_sub(lines);
    kept[start..].join("\n").trim().to_owned()
}

/// The address HNS has given the VM's endpoint, if it has one yet.
///
/// The endpoint is where a guest's address is known on the host side: HNS
/// assigns it and VMLord's DHCP server offers the guest that one and no other.
/// An address HNS spells in a way that does not parse is reported as no address
/// rather than as an error -- it is exactly as unusable as none at all, and the
/// phase's own timeout is what turns that into a failure.
fn endpoint_address(mapping: &VmComputeSystemMapping) -> Result<Option<IpAddr>, RepositoryError> {
    let Some(endpoint_id) = mapping.endpoint_id else {
        return Ok(None);
    };
    let Some(endpoint) = HcnEndpoint::open_if_present(endpoint_id)? else {
        return Ok(None);
    };
    let Some(address) = endpoint.address()? else {
        return Ok(None);
    };
    match address.ip_address.parse() {
        Ok(ip) => Ok(Some(ip)),
        Err(error) => {
            log::debug!(
                "HNS reported \"{}\" as the address of VM \"{}\", which is not an IP address: \
                 {error}",
                address.ip_address,
                mapping.vm_name
            );
            Ok(None)
        }
    }
}

/// Whether anything answers a TCP connection at `ip:port`.
fn probe_port_at(ip: IpAddr, port: u16, timeout: Duration) -> Result<(), String> {
    TcpStream::connect_timeout(&SocketAddr::new(ip, port), timeout)
        .map(drop)
        .map_err(|error| error.to_string())
}

fn probe_port(ip: IpAddr, timeout: Duration) -> Result<(), String> {
    probe_port_at(ip, SSH_PORT, timeout)
}

/// Runs one readiness command, killing it at `deadline` or on cancellation.
fn run_ssh(
    invocation: &SshInvocation,
    deadline: Duration,
    monitor: &BuildMonitor,
) -> Result<SshRun, RepositoryError> {
    let transcript = File::create(&invocation.transcript).map_err(|error| {
        let error = RepositoryError::new(format!(
            "failed to open the readiness transcript {}: {error}",
            invocation.transcript.display()
        ));
        log::error!("{error}");
        error
    })?;
    let errors = transcript.try_clone().map_err(|error| {
        let error = RepositoryError::new(format!(
            "failed to capture the errors of the readiness command: {error}"
        ));
        log::error!("{error}");
        error
    })?;

    log::debug!(
        "running {} to wait for cloud-init; its output goes to {}",
        invocation.program.display(),
        invocation.transcript.display()
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
            log::error!("{error}");
            error
        })?;

    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Ok(SshRun::Exited {
                    code: status.code(),
                    transcript_tail: transcript_tail(&invocation.transcript),
                });
            }
            Ok(None) => {}
            Err(error) => {
                let error = RepositoryError::new(format!(
                    "failed to wait for {}: {error}",
                    invocation.program.display()
                ));
                log::error!("{error}");
                return Err(error);
            }
        }

        if monitor.is_cancelled() {
            log::warn!("killing the readiness command because the build was cancelled");
            kill(&mut child);
            return Ok(SshRun::Cancelled);
        }
        if started.elapsed() >= deadline {
            log::error!(
                "the readiness command did not finish within {} seconds; killing it",
                deadline.as_secs()
            );
            kill(&mut child);
            return Ok(SshRun::Exited {
                code: None,
                transcript_tail: transcript_tail(&invocation.transcript),
            });
        }
        std::thread::sleep(CHILD_POLL_INTERVAL);
    }
}

/// Kills a child and reaps it, so that no zombie handle is left behind.
fn kill(child: &mut Child) {
    if let Err(error) = child.kill() {
        log::warn!("the readiness command could not be killed: {error}");
        return;
    }
    if let Err(error) = child.wait() {
        log::warn!("the killed readiness command could not be reaped: {error}");
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
            Arc,
            atomic::{AtomicU64, Ordering},
        },
        time::Duration,
    };

    use uuid::Uuid;
    use vmlord_core::{BuildMonitor, BuildStep, NetworkMode, RepositoryError};

    use super::{
        GuestReadiness, GuestReady, ReadinessFailure, ReadinessTimeouts, SshInvocation, SshRun,
        outcome, ssh_invocation, tail,
    };
    use crate::metadata::VmComputeSystemMapping;

    fn mapping() -> VmComputeSystemMapping {
        VmComputeSystemMapping {
            vm_id: Uuid::nil(),
            vm_name: "dev-linux".to_owned(),
            hcs_compute_system_id: "vmlord-test".to_owned(),
            disk_gb: 20,
            endpoint_id: None,
            network_mode: NetworkMode::Nat,
        }
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
            |_, _| Ok(()),
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
            .wait(&mapping(), vm_directory(), "machi", &monitor())
            .unwrap();

        assert_eq!(ready, GuestReady::Ready);
    }

    #[test]
    fn an_address_that_never_arrives_fails_on_its_own_timeout() {
        let clock = FakeClock::default();
        let readiness = GuestReadiness::for_test(
            timeouts(),
            |_| Ok(None),
            |_, _| panic!("the port must not be probed before an address exists"),
            |_, _, _| panic!("ssh must not run before an address exists"),
            clock.now(),
            clock.sleeper(),
        );

        let failure = readiness
            .wait(&mapping(), vm_directory(), "machi", &monitor())
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
            |_, _| Err("connection refused".to_owned()),
            |_, _, _| panic!("ssh must not run before port 22 opens"),
            clock.now(),
            clock.sleeper(),
        );

        let failure = readiness
            .wait(&mapping(), vm_directory(), "machi", &monitor())
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
            |_, _| panic!("the port must not be probed after a cancellation"),
            |_, _, _| panic!("ssh must not run after a cancellation"),
            clock.now(),
            clock.sleeper(),
        );

        let failure = readiness
            .wait(&mapping(), vm_directory(), "machi", &monitor)
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
                move |_, _| {
                    monitor.cancel();
                    Err("not yet".to_owned())
                }
            },
            |_, _, _| panic!("ssh must not run after a cancellation"),
            clock.now(),
            clock.sleeper(),
        );

        let failure = readiness
            .wait(&mapping(), vm_directory(), "machi", &monitor)
            .unwrap_err();

        assert_eq!(failure, ReadinessFailure::Cancelled);
    }

    #[test]
    fn a_cancelled_ssh_run_is_reported_as_a_cancellation_not_a_failure() {
        let clock = FakeClock::default();
        let readiness = GuestReadiness::for_test(
            timeouts(),
            |_| Ok(Some(address())),
            |_, _| Ok(()),
            |_, _, _| Ok(SshRun::Cancelled),
            clock.now(),
            clock.sleeper(),
        );

        let failure = readiness
            .wait(&mapping(), vm_directory(), "machi", &monitor())
            .unwrap_err();

        assert_eq!(failure, ReadinessFailure::Cancelled);
    }

    #[test]
    fn a_killed_ssh_child_is_a_timeout_not_a_cloud_init_failure() {
        let clock = FakeClock::default();
        let readiness = GuestReadiness::for_test(
            timeouts(),
            |_| Ok(Some(address())),
            |_, _| Ok(()),
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
            .wait(&mapping(), vm_directory(), "machi", &monitor())
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
            |_, _| panic!("the port must not be probed without an address"),
            |_, _, _| panic!("ssh must not run without an address"),
            clock.now(),
            clock.sleeper(),
        );

        let failure = readiness
            .wait(&mapping(), vm_directory(), "machi", &monitor())
            .unwrap_err();

        assert_eq!(failure, ReadinessFailure::NoAddress);
    }

    #[test]
    fn a_missing_ssh_client_is_reported_before_anything_is_waited_for() {
        let clock = FakeClock::default();
        let readiness = GuestReadiness::for_test_without_client(timeouts(), clock.now());

        let failure = readiness
            .wait(&mapping(), vm_directory(), "machi", &monitor())
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

    fn arguments(invocation: &SshInvocation) -> Vec<String> {
        invocation
            .args
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn the_ssh_invocation_can_neither_prompt_nor_touch_the_users_known_hosts() {
        let invocation = ssh_invocation(
            Path::new(r"C:\Windows\System32\OpenSSH\ssh.exe"),
            Path::new(r"C:\VMs\dev-linux"),
            "machi",
            "172.22.42.7".parse::<IpAddr>().unwrap(),
            Duration::from_secs(10),
        );
        let args = arguments(&invocation);

        assert_eq!(
            invocation.program,
            Path::new(r"C:\Windows\System32\OpenSSH\ssh.exe")
        );
        // No prompt can hang a build, and no key of the user's own agent can be
        // tried in place of the VM's.
        assert!(args.contains(&"BatchMode=yes".to_owned()), "{args:?}");
        assert!(args.contains(&"IdentitiesOnly=yes".to_owned()), "{args:?}");
        assert!(
            args.contains(&"StrictHostKeyChecking=accept-new".to_owned()),
            "{args:?}"
        );
        assert!(
            args.contains(&r"UserKnownHostsFile=C:\VMs\dev-linux\known_hosts".to_owned()),
            "the VM's host key must not land in the user's own known_hosts: {args:?}"
        );
        assert!(args.contains(&"ConnectTimeout=10".to_owned()), "{args:?}");
        assert!(
            args.contains(&r"C:\VMs\dev-linux\keys\id_ed25519".to_owned()),
            "{args:?}"
        );
        assert_eq!(
            args.last().map(String::as_str),
            Some("cloud-init status --wait --long"),
            "{args:?}"
        );
        assert_eq!(
            invocation.transcript,
            Path::new(r"C:\VMs\dev-linux\cloud-init-status.log")
        );
    }

    #[test]
    fn the_user_and_the_address_are_separate_arguments_not_a_joined_string() {
        // `-l user host` rather than `user@host`: a username containing an `@`
        // would otherwise be split in the wrong place by ssh itself.
        let invocation = ssh_invocation(
            Path::new("ssh.exe"),
            Path::new(r"C:\VMs\dev"),
            "a@b",
            "10.0.0.2".parse::<IpAddr>().unwrap(),
            Duration::from_secs(5),
        );
        let args = arguments(&invocation);
        let user_flag = args.iter().position(|argument| argument == "-l").unwrap();

        assert_eq!(args[user_flag + 1], "a@b");
        assert_eq!(args[user_flag + 2], "10.0.0.2");
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
        assert!(messages[2].contains("refused"), "{}", messages[2]);
        assert!(messages[3].contains("boom"), "{}", messages[3]);
        assert!(messages[4].contains("did not finish"), "{}", messages[4]);
        assert!(messages[5].contains("cancelled"), "{}", messages[5]);
    }
}
