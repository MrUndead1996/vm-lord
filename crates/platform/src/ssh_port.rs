//! Moving the SSH port of a guest that already exists.
//!
//! Creating a VM puts the port in two places at once: the seed tells cloud-init
//! where the daemon is to listen, and the mapping records what it was told.
//! Changing the port afterwards has neither of those. The seed was consumed on
//! the first boot, and nothing on the host can edit a file inside a running
//! guest's disk -- Hyper-V holds the VHDX open, and a VM whose file system was
//! written behind its back would not read the change anyway.
//!
//! So the change is made where the file lives: inside the guest, over the same
//! SSH connection everything else uses, on the port the daemon answers on
//! *now*. The files written are the same drop-ins the seed wrote, at the paths
//! the VM's own distribution profile named -- recorded in the mapping at
//! creation precisely because this module would otherwise have to guess which
//! distribution answered.
//!
//! Two commands and not one, because the second one cuts the branch it sits on.
//! Writing the drop-ins and reloading systemd is safe and its exit status means
//! what it says. Restarting the daemon is not: on a socket-activated guest the
//! session VMLord is giving the order through *is* `ssh.service`, so stopping
//! it kills the connection that asked -- `ssh.exe` then reports a broken
//! session rather than the command's status, whether or not the guest did as it
//! was told. The restart is therefore handed to `systemd-run`, which runs it in
//! a transient unit of its own -- outside the session's cgroup, so it survives
//! the session's death -- and its outcome is not read from an exit code at all
//! but from the only thing that settles the question: whether the guest answers
//! on the new port afterwards.

use std::{
    fmt,
    fs::File,
    io::Read,
    net::{IpAddr, SocketAddr, TcpStream},
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use vmlord_core::{RepositoryError, SshAuthentication, SshDaemon, SshEndpoint, SshPort, SshUnits};

use crate::{layout, metadata::VmComputeSystemMapping, ssh, ssh::SshInvocation};

/// Keeps the reconfiguration from flashing a console window of its own.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// How long one connection to the guest may take to establish.
///
/// The VM is running and a person is waiting for a dialog to close, so this is
/// not a readiness wait: a guest on a local virtual switch that needs longer
/// than five seconds to accept a connection has trouble a port move is not
/// going to fix.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long one remote command may run before it is killed.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);

/// How long a single probe of the new port waits for an answer.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// How often the new port is probed after the restart was asked for.
const PROBE_INTERVAL: Duration = Duration::from_secs(1);

/// How many times it is probed before the move is called unfinished.
///
/// Ten seconds' worth. A socket unit is moved in milliseconds; what this waits
/// out is systemd getting round to the transient unit and the daemon letting go
/// of the old listener.
const PROBE_ATTEMPTS: usize = 10;

/// How many lines of a transcript are worth carrying into an error message.
const TRANSCRIPT_TAIL_LINES: usize = 20;

/// What became of a port that was asked to move.
///
/// Both variants mean the guest's own configuration now names the new port:
/// the file was written and systemd re-read it. They differ in whether the
/// running daemon has caught up, which is the difference between "you can
/// connect now" and "you can connect after a restart".
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PortMove {
    /// The guest answers on the new port already.
    Applied,
    /// The drop-ins are in place, and nothing answers on the new port yet. The
    /// detail says what was last seen, so that a person deciding whether to
    /// restart the VM has the reason in front of them.
    RestartNeeded { detail: String },
}

/// Every reason a port move is not attempted, or does not get as far as
/// changing anything.
///
/// Each variant is a different thing for a person to do -- install a Windows
/// feature, start the VM, create it differently, or read what the guest said --
/// which is why they are variants and not one string. Every one of them means
/// the guest is exactly as it was: nothing here is reported after the drop-ins
/// have been written.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PortMoveFailure {
    NoSshClient,
    /// The VM has no SSH access at all, so there is no daemon to move.
    Disabled,
    /// VMLord did not configure this guest's SSH daemon and does not know
    /// where its files are -- a VM installed by hand from local media.
    UnknownDaemon,
    /// The VM logs in by password, which nobody can type into a command VMLord
    /// runs on its own.
    PasswordMode,
    /// The stored SSH configuration is not one anything can connect with.
    Unusable {
        detail: String,
    },
    /// The VM is not running, or has not been given an address yet.
    NoAddress,
    /// The guest was asked and refused, or could not be asked at all.
    Refused {
        detail: String,
    },
}

impl fmt::Display for PortMoveFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSshClient => write!(
                formatter,
                "Windows has no OpenSSH client at \
                 %SystemRoot%\\System32\\OpenSSH\\ssh.exe; install the \
                 \"OpenSSH Client\" optional feature"
            ),
            Self::Disabled => write!(
                formatter,
                "the VM was created without SSH access, so it has no SSH port to move"
            ),
            Self::UnknownDaemon => write!(
                formatter,
                "VMLord did not configure this VM's SSH daemon, so it cannot reconfigure it"
            ),
            Self::PasswordMode => write!(
                formatter,
                "the VM logs in by password, which nobody can type into a command VMLord runs \
                 on its own; change the port inside the guest instead"
            ),
            Self::Unusable { detail } => write!(
                formatter,
                "the VM's SSH configuration cannot be connected with: {detail}"
            ),
            Self::NoAddress => write!(
                formatter,
                "the SSH port is changed inside the running guest, and this VM has no address \
                 on the VMLord network; start it first"
            ),
            Self::Refused { detail } => {
                write!(formatter, "the guest did not accept the change: {detail}")
            }
        }
    }
}

/// What one run of `ssh.exe` left behind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteRun {
    /// The remote command's exit status, or `None` for a client that was
    /// killed or never got that far.
    pub(crate) code: Option<i32>,
    pub(crate) transcript_tail: String,
}

type AddressSource =
    Box<dyn Fn(&VmComputeSystemMapping) -> Result<Option<IpAddr>, RepositoryError> + Send + Sync>;
/// Runs one remote command and reports what it left behind.
type CommandRunner = Box<
    dyn Fn(&SshInvocation, &Path, Duration) -> Result<RemoteRun, RepositoryError> + Send + Sync,
>;
/// Answers `Err(reason)` while nothing is listening.
type PortProbe = Box<dyn Fn(IpAddr, u16, Duration) -> Result<(), String> + Send + Sync>;
type Sleeper = Box<dyn Fn(Duration) + Send + Sync>;

/// Moves the SSH port of running guests.
pub(crate) struct SshPortMover {
    /// `None` when this Windows installation has no OpenSSH client.
    client: Option<PathBuf>,
    address: AddressSource,
    run: CommandRunner,
    probe: PortProbe,
    sleep: Sleeper,
}

impl SshPortMover {
    /// The mover VMLord runs with: Windows' own OpenSSH, HNS and a real socket.
    #[must_use]
    pub(crate) fn production() -> Self {
        Self {
            client: ssh::client_path(),
            address: Box::new(ssh::guest_address),
            run: Box::new(run_command),
            probe: Box::new(probe_port_at),
            sleep: Box::new(std::thread::sleep),
        }
    }

    /// Moves the guest of `mapping` to `port`, or says what stopped it.
    ///
    /// Everything that can be refused without touching the guest is refused
    /// first, in the order that spends the least time doing it. After that
    /// there is exactly one point of no return -- the command that writes the
    /// drop-ins -- and every outcome past it is an `Ok`, because the guest's
    /// configuration has changed and the stored one has to change with it.
    pub(crate) fn move_port(
        &self,
        mapping: &VmComputeSystemMapping,
        vm_directory: &Path,
        port: SshPort,
    ) -> Result<PortMove, PortMoveFailure> {
        let client = self.client.clone().ok_or(PortMoveFailure::NoSshClient)?;
        let config = mapping.ssh.clone().ok_or(PortMoveFailure::Disabled)?;
        let daemon = mapping
            .ssh_daemon
            .clone()
            .ok_or(PortMoveFailure::UnknownDaemon)?;
        if config.authentication != SshAuthentication::VmlordKey {
            return Err(PortMoveFailure::PasswordMode);
        }

        let address = match (self.address)(mapping) {
            Ok(Some(address)) => address,
            Ok(None) => return Err(PortMoveFailure::NoAddress),
            Err(error) => {
                tracing::error!(
                    "the address of VM \"{}\" could not be read: {error}",
                    mapping.vm_name
                );
                return Err(PortMoveFailure::NoAddress);
            }
        };
        // The endpoint of the guest as it is now: the move is asked for over
        // the port the daemon currently answers on, not over the one it is
        // being given.
        let endpoint = SshEndpoint::new(mapping.vm_id, &config, address).map_err(|error| {
            PortMoveFailure::Unusable {
                detail: error.to_string(),
            }
        })?;

        tracing::info!(
            "asking VM \"{}\" at {endpoint} to move its SSH daemon to port {port}",
            mapping.vm_name
        );
        let transcript = layout::ssh_port_log_path(vm_directory);
        let write = self.command(
            &client,
            &endpoint,
            vm_directory,
            &write_command(&daemon, port),
        );
        let run = (self.run)(&write, &transcript, COMMAND_TIMEOUT).map_err(|error| {
            PortMoveFailure::Refused {
                detail: error.to_string(),
            }
        })?;
        if run.code != Some(0) {
            return Err(PortMoveFailure::Refused {
                detail: refusal(&run, &transcript),
            });
        }

        // Past here the guest's own configuration names the new port, so
        // nothing below is a failure of the move -- only of the daemon
        // catching up with it while the VM runs.
        let restart = self.command(&client, &endpoint, vm_directory, &restart_command(&daemon));
        // The exit status is deliberately not read: this command ends the
        // session it arrives on. What it says is worth a log line and nothing
        // more; the port itself is the answer.
        match (self.run)(&restart, &transcript, COMMAND_TIMEOUT) {
            Ok(run) => tracing::debug!(
                "VM \"{}\" was asked to restart its SSH daemon; the client exited with {:?}",
                mapping.vm_name,
                run.code
            ),
            Err(error) => tracing::warn!(
                "VM \"{}\" was asked to restart its SSH daemon, and the client could not be \
                 run: {error}",
                mapping.vm_name
            ),
        }

        Ok(self.verify(mapping, address, port))
    }

    /// Waits for the guest to answer on the port it was just given.
    fn verify(&self, mapping: &VmComputeSystemMapping, address: IpAddr, port: SshPort) -> PortMove {
        let mut last_error = String::new();
        for attempt in 0..PROBE_ATTEMPTS {
            if attempt > 0 {
                (self.sleep)(PROBE_INTERVAL);
            }
            match (self.probe)(address, port.get(), PROBE_TIMEOUT) {
                Ok(()) => {
                    tracing::info!("VM \"{}\" now answers on port {port}", mapping.vm_name);
                    return PortMove::Applied;
                }
                Err(error) => {
                    tracing::debug!(
                        "VM \"{}\" does not answer on {address}:{port} yet: {error}",
                        mapping.vm_name
                    );
                    last_error = error;
                }
            }
        }
        tracing::warn!(
            "VM \"{}\" is configured for port {port} and does not answer there yet: {last_error}",
            mapping.vm_name
        );
        PortMove::RestartNeeded { detail: last_error }
    }

    /// One run of the client, asking the guest to run `command`.
    fn command(
        &self,
        client: &Path,
        endpoint: &SshEndpoint,
        vm_directory: &Path,
        command: &str,
    ) -> SshInvocation {
        ssh::invocation(
            client,
            endpoint,
            vm_directory,
            Some(CONNECT_TIMEOUT),
            Some(command),
            // A port move runs a command and reads what it printed: this
            // caller keeps the client's output itself, so there is nothing for
            // a log file to answer.
            None,
        )
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        address: impl Fn(&VmComputeSystemMapping) -> Result<Option<IpAddr>, RepositoryError>
        + Send
        + Sync
        + 'static,
        run: impl Fn(&SshInvocation, &Path, Duration) -> Result<RemoteRun, RepositoryError>
        + Send
        + Sync
        + 'static,
        probe: impl Fn(IpAddr, u16, Duration) -> Result<(), String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            client: Some(PathBuf::from(r"C:\Windows\System32\OpenSSH\ssh.exe")),
            address: Box::new(address),
            run: Box::new(run),
            probe: Box::new(probe),
            sleep: Box::new(|_| {}),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test_without_client() -> Self {
        Self {
            client: None,
            address: Box::new(|_| panic!("nothing may be asked without an ssh client")),
            run: Box::new(|_, _, _| panic!("nothing may be run without an ssh client")),
            probe: Box::new(|_, _, _| panic!("nothing may be probed without an ssh client")),
            sleep: Box::new(|_| panic!("nothing may be waited for without an ssh client")),
        }
    }
}

/// What a refused command is worth saying about itself.
fn refusal(run: &RemoteRun, transcript: &Path) -> String {
    let status = match run.code {
        Some(code) => format!("the command exited with status {code}"),
        None => "the command did not finish".to_owned(),
    };
    if run.transcript_tail.is_empty() {
        format!("{status}; see {}", transcript.display())
    } else {
        format!("{status}: {}", run.transcript_tail)
    }
}

/// The command that writes the guest's drop-ins and makes systemd read them.
///
/// The same files the seed writes, at the same paths, with the same content:
/// a guest created on a port and a guest moved to one must not be two different
/// configurations. `install -D` creates the directory the drop-in lives in --
/// `/etc/systemd/system/ssh.socket.d` does not exist on a guest created on the
/// default port -- and fixes the mode before anything is written into it.
///
/// `set -e` is what makes the exit status mean something: without it the
/// command's status would be `daemon-reload`'s alone, and a drop-in that could
/// not be written would be reported as a change that was made.
fn write_command(daemon: &SshDaemon, port: SshPort) -> String {
    let mut script = String::from("set -e\n");
    script.push_str(&write_file(
        &daemon.config_drop_in,
        &[format!("Port {port}")],
    ));
    if let SshUnits::SocketActivated { socket_drop_in, .. } = &daemon.units {
        // Both address families named and the inherited list cleared first,
        // exactly as the seed does it -- see `vmlord_seed`, which explains at
        // length why either half missing leaves a guest answering nowhere.
        script.push_str(&write_file(
            socket_drop_in,
            &[
                "[Socket]".to_owned(),
                "ListenStream=".to_owned(),
                format!("ListenStream=0.0.0.0:{port}"),
                format!("ListenStream=[::]:{port}"),
            ],
        ));
    }
    script.push_str("systemctl daemon-reload\n");
    privileged(&script)
}

/// The command that puts the new port into effect on a running guest.
///
/// The decision is the seed's, for the reasons the seed gives: where a socket
/// owns the port, restarting the service beside it leaves two listeners
/// fighting over one port, so an active socket means stopping the service and
/// restarting the socket, and only a guest whose socket is not the listener has
/// its service restarted.
///
/// What is added here is `systemd-run`: this command kills the very session it
/// is running in, and a shell backgrounded inside that session would be killed
/// along with it -- leaving a guest with a stopped socket and no daemon at all.
/// A transient unit is systemd's own child, so the restart finishes whatever
/// happens to the session that asked for it, and `--no-block` means the answer
/// comes back before the connection dies rather than instead of it.
fn restart_command(daemon: &SshDaemon) -> String {
    let decision = match &daemon.units {
        SshUnits::Service { unit } => format!("systemctl try-restart {unit}"),
        SshUnits::SocketActivated {
            socket, service, ..
        } => format!(
            "if systemctl is-active --quiet {socket}; \
             then systemctl stop {service}; systemctl restart {socket}; \
             else systemctl try-restart {service}; fi"
        ),
    };
    privileged(&format!(
        "systemd-run --collect --no-block /bin/sh -c {}\n",
        quote(&decision)
    ))
}

/// One file, written whole.
///
/// `printf '%s\n' <line>...` rather than a here-document or an `echo`: every
/// line is an argument, so nothing in it is a format string, an escape
/// sequence, or something the shell reads twice.
fn write_file(path: &str, lines: &[String]) -> String {
    let quoted_path = quote(path);
    let content = lines
        .iter()
        .map(|line| quote(line))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "install -D -m 0644 /dev/null {quoted_path}\nprintf '%s\\n' {content} > {quoted_path}\n"
    )
}

/// A script, run as root without a password prompt.
///
/// `-n` rather than a prompt nobody is at: the guest user cloud-init created
/// has passwordless `sudo`, and a guest where that has been taken away is one
/// that must refuse the change rather than sit waiting to be typed at.
fn privileged(script: &str) -> String {
    format!("sudo -n /bin/sh -c {}", quote(script))
}

/// One word for a POSIX shell, whatever it contains.
///
/// The guest's shell parses the remote command once, and the paths and unit
/// names inside it come from a document on disk. Single quotes suspend every
/// meaning a shell has except the closing quote, and the closing quote itself
/// is spelled by leaving the quotes, escaping it, and going back in.
fn quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// Whether anything answers a TCP connection at `ip:port`.
fn probe_port_at(ip: IpAddr, port: u16, timeout: Duration) -> Result<(), String> {
    TcpStream::connect_timeout(&SocketAddr::new(ip, port), timeout)
        .map(drop)
        .map_err(|error| error.to_string())
}

/// Runs one remote command, killing it at `deadline`.
///
/// The output goes to a file rather than a pipe, like the readiness wait's: it
/// is what a person reads when a guest refuses a change nobody was watching it
/// refuse, and a file survives the process that wrote it.
fn run_command(
    invocation: &SshInvocation,
    transcript: &Path,
    deadline: Duration,
) -> Result<RemoteRun, RepositoryError> {
    let output = File::create(transcript).map_err(|error| {
        let error = RepositoryError::new(format!(
            "failed to open the SSH port transcript {}: {error}",
            transcript.display()
        ));
        tracing::error!("{error}");
        error
    })?;
    let errors = output.try_clone().map_err(|error| {
        let error = RepositoryError::new(format!(
            "failed to capture the errors of the SSH port command: {error}"
        ));
        tracing::error!("{error}");
        error
    })?;

    tracing::debug!("running {}", invocation.command_line());
    let mut child = Command::new(&invocation.program)
        .args(&invocation.args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(output))
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
                return Ok(RemoteRun {
                    code: status.code(),
                    transcript_tail: transcript_tail(transcript),
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
        if started.elapsed() >= deadline {
            tracing::error!(
                "the SSH port command did not finish within {} seconds; killing it",
                deadline.as_secs()
            );
            if let Err(error) = child.kill() {
                tracing::warn!("the SSH port command could not be killed: {error}");
            } else if let Err(error) = child.wait() {
                tracing::warn!("the killed SSH port command could not be reaped: {error}");
            }
            return Ok(RemoteRun {
                code: None,
                transcript_tail: transcript_tail(transcript),
            });
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn transcript_tail(path: &Path) -> String {
    let mut text = String::new();
    match File::open(path).and_then(|mut file| file.read_to_string(&mut text)) {
        Ok(_) => crate::guest_ready::tail(&text, TRANSCRIPT_TAIL_LINES),
        Err(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::{IpAddr, Ipv4Addr},
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
        time::Duration,
    };

    use uuid::Uuid;
    use vmlord_core::{
        NetworkMode, RepositoryError, SshAuthentication, SshConfig, SshDaemon, SshPort, SshUnits,
        distro,
    };

    use super::{
        PortMove, PortMoveFailure, RemoteRun, SshPortMover, quote, restart_command, write_command,
    };
    use crate::{metadata::VmComputeSystemMapping, ssh::SshInvocation};

    fn address() -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(172, 30, 0, 5))
    }

    fn config() -> SshConfig {
        SshConfig {
            username: "ubuntu".into(),
            port: SshPort::DEFAULT,
            authentication: SshAuthentication::VmlordKey,
        }
    }

    fn service_daemon() -> SshDaemon {
        SshDaemon {
            units: SshUnits::Service {
                unit: "sshd.service".into(),
            },
            config_drop_in: "/etc/ssh/sshd_config.d/10-vmlord.conf".into(),
        }
    }

    fn mapping(ssh: Option<SshConfig>, daemon: Option<SshDaemon>) -> VmComputeSystemMapping {
        VmComputeSystemMapping {
            vm_id: Uuid::from_u128(7),
            vm_name: "dev".into(),
            hcs_compute_system_id: "vmlord-dev".into(),
            disk_gb: 20,
            endpoint_id: Some(Uuid::from_u128(8)),
            network_mode: NetworkMode::Nat,
            ssh,
            ssh_daemon: daemon,
            gpu_mode: vmlord_core::GpuMode::None,
            desktop_profile: vmlord_core::DesktopProfile::Headless,
            display_provisioning: vmlord_core::DisplayProvisioning::NotRequested,
            display_mode: None,
            guest_target: None,
        }
    }

    /// A mover whose guest runs everything it is asked and answers afterwards.
    fn mover(commands: &Arc<Mutex<Vec<String>>>) -> SshPortMover {
        let recorded = Arc::clone(commands);
        SshPortMover::for_test(
            |_| Ok(Some(address())),
            move |invocation, _, _| {
                recorded
                    .lock()
                    .expect("the recorded commands are not poisoned")
                    .push(remote_command(invocation));
                Ok(RemoteRun {
                    code: Some(0),
                    transcript_tail: String::new(),
                })
            },
            |_, _, _| Ok(()),
        )
    }

    /// The remote command of an invocation: its last argument, which is what
    /// the builder puts there.
    fn remote_command(invocation: &SshInvocation) -> String {
        invocation
            .args
            .last()
            .expect("an invocation carries arguments")
            .to_string_lossy()
            .into_owned()
    }

    fn moved(
        mover: &SshPortMover,
        mapping: &VmComputeSystemMapping,
        port: u16,
    ) -> Result<PortMove, PortMoveFailure> {
        mover.move_port(
            mapping,
            Path::new(r"C:\VMLord\dev"),
            SshPort::new(port).expect("a port"),
        )
    }

    #[test]
    fn a_quoted_word_survives_every_meaning_a_shell_has() {
        assert_eq!(
            quote("/etc/ssh/sshd_config.d/10-vmlord.conf"),
            "'/etc/ssh/sshd_config.d/10-vmlord.conf'"
        );
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("$(id)"), "'$(id)'");
        assert_eq!(quote("it's"), r"'it'\''s'");
    }

    /// The seed writes `Port` into the daemon's own drop-in on every
    /// distribution, and the socket's listener only where a socket owns it.
    #[test]
    fn the_write_command_writes_the_same_drop_ins_the_seed_does() {
        let command = write_command(&distro::ubuntu().ssh, SshPort::new(2222).unwrap());

        assert!(command.starts_with("sudo -n /bin/sh -c "), "got {command}");
        assert!(command.contains(r"set -e"), "got {command}");
        assert!(
            command.contains(r"'/etc/ssh/sshd_config.d/10-vmlord.conf'"),
            "got {command}"
        );
        assert!(command.contains(r"'Port 2222'"), "got {command}");
        assert!(
            command.contains(r"'/etc/systemd/system/ssh.socket.d/10-vmlord.conf'"),
            "got {command}"
        );
        assert!(command.contains(r"'ListenStream='"), "got {command}");
        assert!(
            command.contains(r"'ListenStream=0.0.0.0:2222'")
                && command.contains(r"'ListenStream=[::]:2222'"),
            "both address families have to be named: {command}"
        );
        assert!(command.contains("systemctl daemon-reload"), "got {command}");
    }

    /// A daemon that opens its own port has no socket to move, and a socket
    /// drop-in written for it would be a file nothing reads.
    #[test]
    fn a_daemon_that_owns_its_port_gets_one_drop_in() {
        let command = write_command(&service_daemon(), SshPort::new(2222).unwrap());

        assert!(command.contains(r"'Port 2222'"), "got {command}");
        assert!(!command.contains("ListenStream"), "got {command}");
    }

    #[test]
    fn the_restart_is_decided_inside_the_guest_and_run_outside_the_session() {
        let command = restart_command(&distro::ubuntu().ssh);

        assert!(
            command.contains("systemd-run --collect --no-block"),
            "the restart must outlive the session it kills: {command}"
        );
        assert!(
            command.contains("is-active --quiet ssh.socket"),
            "only the guest knows whether the socket is the listener: {command}"
        );
        assert!(
            command.contains("systemctl stop ssh.service")
                && command.contains("systemctl restart ssh.socket"),
            "got {command}"
        );
        assert!(
            command.contains("try-restart ssh.service"),
            "a release that ships the socket without enabling it still has a service: {command}"
        );
    }

    #[test]
    fn a_daemon_that_owns_its_port_is_simply_restarted() {
        let command = restart_command(&service_daemon());

        assert!(command.contains("systemd-run"), "got {command}");
        assert!(
            command.contains("try-restart sshd.service"),
            "got {command}"
        );
        assert!(!command.contains("is-active"), "got {command}");
    }

    #[test]
    fn a_guest_that_answers_on_the_new_port_has_had_the_change_applied() {
        let commands = Arc::new(Mutex::new(Vec::new()));

        let outcome = moved(
            &mover(&commands),
            &mapping(Some(config()), Some(distro::ubuntu().ssh)),
            2222,
        );

        assert_eq!(outcome, Ok(PortMove::Applied));
        let commands = commands.lock().unwrap();
        assert_eq!(commands.len(), 2, "the write and the restart are separate");
        assert!(commands[0].contains("'Port 2222'"), "got {:?}", commands[0]);
        assert!(commands[1].contains("systemd-run"), "got {:?}", commands[1]);
    }

    /// The connection is made over the port the guest answers on now, not the
    /// one it is being given: the daemon has not moved yet.
    #[test]
    fn the_change_is_asked_for_over_the_port_the_guest_still_listens_on() {
        let ports = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&ports);
        let mover = SshPortMover::for_test(
            |_| Ok(Some(address())),
            move |invocation, _, _| {
                let args: Vec<String> = invocation
                    .args
                    .iter()
                    .map(|argument| argument.to_string_lossy().into_owned())
                    .collect();
                let port = args
                    .iter()
                    .position(|argument| argument == "-p")
                    .map(|index| args[index + 1].clone())
                    .expect("every invocation names a port");
                recorded.lock().unwrap().push(port);
                Ok(RemoteRun {
                    code: Some(0),
                    transcript_tail: String::new(),
                })
            },
            |_, _, _| Ok(()),
        );

        moved(
            &mover,
            &mapping(Some(config()), Some(distro::ubuntu().ssh)),
            2222,
        )
        .expect("the guest accepts the change");

        assert_eq!(ports.lock().unwrap().as_slice(), ["22", "22"]);
    }

    /// The restart ends the session that carries it, so its exit status says
    /// nothing about whether the guest obeyed. The port does.
    #[test]
    fn a_restart_whose_session_died_is_not_a_failed_move() {
        let mover = SshPortMover::for_test(
            |_| Ok(Some(address())),
            |invocation, _, _| {
                Ok(RemoteRun {
                    code: if remote_command(invocation).contains("systemd-run") {
                        Some(255)
                    } else {
                        Some(0)
                    },
                    transcript_tail: "Connection to 172.30.0.5 closed by remote host.".into(),
                })
            },
            |_, _, _| Ok(()),
        );

        let outcome = moved(
            &mover,
            &mapping(Some(config()), Some(distro::ubuntu().ssh)),
            2222,
        );

        assert_eq!(outcome, Ok(PortMove::Applied));
    }

    #[test]
    fn a_guest_that_never_answers_leaves_a_change_waiting_for_a_restart() {
        let mover = SshPortMover::for_test(
            |_| Ok(Some(address())),
            |_, _, _| {
                Ok(RemoteRun {
                    code: Some(0),
                    transcript_tail: String::new(),
                })
            },
            |_, _, _| Err("connection refused".to_owned()),
        );

        let outcome = moved(
            &mover,
            &mapping(Some(config()), Some(distro::ubuntu().ssh)),
            2222,
        );

        assert_eq!(
            outcome,
            Ok(PortMove::RestartNeeded {
                detail: "connection refused".into()
            })
        );
    }

    /// A guest that refused the write is a guest nothing was changed in, so
    /// the stored port must stay what it was -- which is what an `Err` here
    /// tells the repository.
    #[test]
    fn a_guest_that_refuses_the_write_changes_nothing() {
        let mover = SshPortMover::for_test(
            |_| Ok(Some(address())),
            |_, _, _| {
                Ok(RemoteRun {
                    code: Some(1),
                    transcript_tail: "sudo: a password is required".into(),
                })
            },
            |_, _, _| panic!("nothing may be probed after a refused write"),
        );

        let error = moved(
            &mover,
            &mapping(Some(config()), Some(distro::ubuntu().ssh)),
            2222,
        )
        .unwrap_err();

        assert!(
            matches!(&error, PortMoveFailure::Refused { detail }
                if detail.contains("status 1") && detail.contains("a password is required")),
            "got {error:?}"
        );
    }

    #[test]
    fn a_vm_without_ssh_has_no_port_to_move() {
        let commands = Arc::new(Mutex::new(Vec::new()));

        let error = moved(&mover(&commands), &mapping(None, None), 2222).unwrap_err();

        assert_eq!(error, PortMoveFailure::Disabled);
        assert!(commands.lock().unwrap().is_empty());
    }

    /// A guest VMLord did not configure has an SSH daemon of somebody else's,
    /// whose files are in places VMLord would be guessing at.
    #[test]
    fn a_guest_vmlord_did_not_configure_is_not_reconfigured() {
        let commands = Arc::new(Mutex::new(Vec::new()));

        let error = moved(&mover(&commands), &mapping(Some(config()), None), 2222).unwrap_err();

        assert_eq!(error, PortMoveFailure::UnknownDaemon);
        assert!(commands.lock().unwrap().is_empty());
    }

    #[test]
    fn a_password_login_cannot_be_typed_into_a_command_vmlord_runs() {
        let commands = Arc::new(Mutex::new(Vec::new()));
        let password = SshConfig {
            authentication: SshAuthentication::Password,
            ..config()
        };

        let error = moved(
            &mover(&commands),
            &mapping(Some(password), Some(distro::ubuntu().ssh)),
            2222,
        )
        .unwrap_err();

        assert_eq!(error, PortMoveFailure::PasswordMode);
        assert!(commands.lock().unwrap().is_empty());
    }

    #[test]
    fn a_vm_with_no_address_is_a_vm_nothing_can_be_asked_of() {
        let mover = SshPortMover::for_test(
            |_| Ok(None),
            |_, _, _| panic!("nothing may be run without an address"),
            |_, _, _| panic!("nothing may be probed without an address"),
        );

        let error = moved(
            &mover,
            &mapping(Some(config()), Some(distro::ubuntu().ssh)),
            2222,
        )
        .unwrap_err();

        assert_eq!(error, PortMoveFailure::NoAddress);
        assert!(error.to_string().contains("start it first"), "got {error}");
    }

    #[test]
    fn a_host_without_an_ssh_client_asks_nothing() {
        let error = moved(
            &SshPortMover::for_test_without_client(),
            &mapping(Some(config()), Some(distro::ubuntu().ssh)),
            2222,
        )
        .unwrap_err();

        assert_eq!(error, PortMoveFailure::NoSshClient);
    }

    /// A stored configuration nothing can connect with is refused while it is
    /// still data, exactly as the readiness wait and the launcher refuse it.
    #[test]
    fn a_damaged_configuration_is_refused_before_anything_runs() {
        let commands = Arc::new(Mutex::new(Vec::new()));
        let damaged = SshConfig {
            username: "root -oProxyCommand=calc".into(),
            ..config()
        };

        let error = moved(
            &mover(&commands),
            &mapping(Some(damaged), Some(distro::ubuntu().ssh)),
            2222,
        )
        .unwrap_err();

        assert!(
            matches!(error, PortMoveFailure::Unusable { .. }),
            "got {error:?}"
        );
        assert!(commands.lock().unwrap().is_empty());
    }

    /// The transcript is the VM's own, so two VMs moved at once do not
    /// overwrite each other's account of what the guest said.
    #[test]
    fn the_transcript_belongs_to_the_vm() {
        let paths = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&paths);
        let mover = SshPortMover::for_test(
            |_| Ok(Some(address())),
            move |_, transcript, _| {
                recorded.lock().unwrap().push(transcript.to_path_buf());
                Ok(RemoteRun {
                    code: Some(0),
                    transcript_tail: String::new(),
                })
            },
            |_, _, _| Ok(()),
        );

        moved(
            &mover,
            &mapping(Some(config()), Some(distro::ubuntu().ssh)),
            2222,
        )
        .expect("the guest accepts the change");

        assert_eq!(
            paths.lock().unwrap().as_slice(),
            [
                PathBuf::from(r"C:\VMLord\dev\ssh-port.log"),
                PathBuf::from(r"C:\VMLord\dev\ssh-port.log")
            ]
        );
    }

    /// The runner is handed a deadline rather than being trusted to invent
    /// one: a guest that never answers must not hold the dialog open forever.
    #[test]
    fn every_command_carries_a_deadline() {
        let deadlines = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&deadlines);
        let mover = SshPortMover::for_test(
            |_| Ok(Some(address())),
            move |_, _, deadline| {
                recorded.lock().unwrap().push(deadline);
                Ok(RemoteRun {
                    code: Some(0),
                    transcript_tail: String::new(),
                })
            },
            |_, _, _| Ok(()),
        );

        moved(
            &mover,
            &mapping(Some(config()), Some(distro::ubuntu().ssh)),
            2222,
        )
        .expect("the guest accepts the change");

        for deadline in deadlines.lock().unwrap().iter() {
            assert!(
                *deadline > Duration::ZERO && *deadline <= Duration::from_secs(60),
                "got {deadline:?}"
            );
        }
    }

    /// Not a test of behaviour but of a seam: the address source is the same
    /// one every other SSH path reads, so a VM whose endpoint is gone is
    /// reported the same way here as there.
    #[test]
    fn the_address_source_is_the_repositorys_own() {
        let _: fn(&VmComputeSystemMapping) -> Result<Option<IpAddr>, RepositoryError> =
            crate::ssh::guest_address;
    }
}
