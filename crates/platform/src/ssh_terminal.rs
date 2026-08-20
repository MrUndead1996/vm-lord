//! Putting an interactive SSH session on screen, and everything that has to be
//! true before one is attempted.
//!
//! A person asking for a shell is asking for a window they can type in, which
//! is the one thing VMLord cannot check afterwards: once `ssh.exe` runs in a
//! terminal of its own, its output goes to that window and nowhere else. So
//! everything knowable in advance is established first -- the client exists, the
//! VM has SSH access at all, it has an address, something answers on its port,
//! and the key it logs in with is still there -- and each of those failures says
//! which one it was. What is left for OpenSSH to report is what only OpenSSH can
//! report: the host key, the credential, and the transport.
//!
//! The session that starts is deliberately not VMLord's. No child handle is
//! kept, nothing is killed when the repository is dropped, and nothing counts
//! how many sessions a VM has: a shell someone opened is theirs to close, and
//! two shells into one guest is an ordinary thing to want. This is the opposite
//! of the COM1 console, where a second reader on one pipe would split the
//! guest's output between two windows.

use std::{
    ffi::OsString,
    fmt, io,
    net::{IpAddr, SocketAddr, TcpStream},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use vmlord_core::{RepositoryError, SshAuthentication};

use crate::{
    com1_terminal::{TerminalCommand, spawn_terminal},
    layout,
    metadata::VmComputeSystemMapping,
    ssh::{self, SshInvocation},
};

/// How long the one preflight connection to the guest's SSH port waits.
///
/// One attempt and three seconds: this is not a readiness wait -- the VM is
/// running and a person is holding a mouse button down. A guest that needs
/// longer than that to answer on a local virtual switch is a guest whose
/// remaining trouble `ssh.exe` will describe better than a probe can.
const PORT_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Every reason an interactive session may not be attempted, each naming its
/// own cause.
///
/// Separate variants rather than one string because each of them is a different
/// thing for a person to do: install a Windows feature, create the VM
/// differently, start it, wait, or find out where the key went.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SshLaunchFailure {
    NoSshClient,
    /// The VM has no SSH access at all: it was created without one, or
    /// installed by hand from local media.
    Disabled,
    /// The stored SSH configuration is not one anything can connect with.
    Unusable {
        detail: String,
    },
    NoAddress,
    Unreachable {
        port: u16,
        detail: String,
    },
    /// Key mode, and the private key the guest trusts is not where VMLord keeps
    /// it. Refused rather than attempted: without it `ssh.exe` has nothing to
    /// offer and `BatchMode` turns that into a bare "Permission denied".
    NoKey {
        path: PathBuf,
    },
    /// Neither terminal host could be started, and both said why.
    NoTerminal {
        refusals: Vec<String>,
    },
}

impl fmt::Display for SshLaunchFailure {
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
                "the VM was created without SSH access, so there is nothing to connect to"
            ),
            Self::Unusable { detail } => write!(
                formatter,
                "the VM's SSH configuration cannot be connected with: {detail}"
            ),
            Self::NoAddress => write!(formatter, "the VM has no address on the VMLord network yet"),
            Self::Unreachable { port, detail } => write!(
                formatter,
                "the guest does not answer on port {port}: {detail}"
            ),
            Self::NoKey { path } => write!(
                formatter,
                "the VM logs in with the key VMLord generated for it, and {} is missing",
                path.display()
            ),
            Self::NoTerminal { refusals } => write!(
                formatter,
                "no terminal could be started for the session ({})",
                refusals.join("; ")
            ),
        }
    }
}

type AddressSource =
    Box<dyn Fn(&VmComputeSystemMapping) -> Result<Option<IpAddr>, RepositoryError> + Send + Sync>;
/// Answers `Err(reason)` when nothing is listening; the reason is what the
/// failure quotes.
type PortProbe = Box<dyn Fn(IpAddr, u16, Duration) -> Result<(), String> + Send + Sync>;
/// Whether the private key is where the VM's directory says it is.
type KeyProbe = Box<dyn Fn(&Path) -> bool + Send + Sync>;
/// Starts one terminal host, which is what a test replaces to keep a launch off
/// screen.
type TerminalSpawner = Arc<dyn Fn(&TerminalCommand) -> io::Result<()> + Send + Sync>;

/// Opens interactive SSH sessions into running guests.
pub(crate) struct SshLauncher {
    /// `None` when this Windows installation has no OpenSSH client. Resolved
    /// once, at construction: a machine that gains the optional feature gains
    /// it for the next run of VMLord, and a launch that probed for it every
    /// time would still be answering the same question.
    client: Option<PathBuf>,
    address: AddressSource,
    port: PortProbe,
    key: KeyProbe,
    spawn: TerminalSpawner,
}

impl SshLauncher {
    /// The launcher VMLord runs with: Windows' own OpenSSH, HNS, a real socket
    /// and a real terminal.
    pub(crate) fn production() -> Self {
        Self {
            client: ssh::client_path(),
            address: Box::new(ssh::guest_address),
            port: Box::new(probe_port_at),
            key: Box::new(|path: &Path| path.is_file()),
            spawn: Arc::new(spawn_terminal),
        }
    }

    /// Opens a shell into the guest of `mapping`, or says what stopped it.
    ///
    /// Whether the VM is running is not asked here: that is a question for HCS,
    /// and the repository has already asked it. Everything else is checked in
    /// the order that spends the least time before refusing.
    ///
    /// What comes back is what was asked of `ssh.exe`. The session says nothing
    /// to VMLord after this -- it is a process in a window of its own -- so the
    /// arguments are the last thing knowable about it, and the repository puts
    /// them where a person can read them.
    pub(crate) fn launch(
        &self,
        mapping: &VmComputeSystemMapping,
        vm_directory: &Path,
    ) -> Result<SshInvocation, SshLaunchFailure> {
        let client = self.client.clone().ok_or(SshLaunchFailure::NoSshClient)?;
        if mapping.ssh.is_none() {
            return Err(SshLaunchFailure::Disabled);
        }

        let address = match (self.address)(mapping) {
            Ok(Some(address)) => address,
            Ok(None) => return Err(SshLaunchFailure::NoAddress),
            Err(error) => {
                log::error!(
                    "the address of VM \"{}\" could not be read: {error}",
                    mapping.vm_name
                );
                return Err(SshLaunchFailure::NoAddress);
            }
        };
        let endpoint = ssh::endpoint(mapping, address)
            .map_err(|error| SshLaunchFailure::Unusable {
                detail: error.to_string(),
            })?
            // `mapping.ssh` was `Some` a moment ago and nothing here can change
            // it: the same absence would have been reported as `Disabled`.
            .ok_or(SshLaunchFailure::Disabled)?;

        let port = endpoint.port.get();
        // One attempt, not a wait. A closed port here usually means the guest
        // is still booting, and saying so is more use than a client that sits
        // in a window until it times out.
        (self.port)(address, port, PORT_PROBE_TIMEOUT).map_err(|detail| {
            SshLaunchFailure::Unreachable {
                port,
                detail: detail.clone(),
            }
        })?;
        // The probe and the session are two connections, so a guest can lose
        // its daemon between them. That race is left alone deliberately: the
        // window is milliseconds wide, and the session that runs into it is in
        // a terminal where OpenSSH explains itself.
        if endpoint.authentication == SshAuthentication::VmlordKey {
            let key = layout::ssh_key_path(vm_directory);
            if !(self.key)(&key) {
                return Err(SshLaunchFailure::NoKey { path: key });
            }
        }

        // No connect timeout: an interactive session is one a person is
        // watching, and a deadline VMLord invented would close their window
        // mid-handshake.
        let invocation = ssh::invocation(&client, &endpoint, vm_directory, None, None);
        self.spawn_somewhere(&invocation, &mapping.vm_name)?;

        log::info!(
            "an SSH session to VM \"{}\" was opened at {endpoint}",
            mapping.vm_name
        );
        Ok(invocation)
    }

    /// Starts the first terminal host that will have it, and reports both if
    /// neither does.
    ///
    /// What "success" means here is narrow and worth being honest about: the
    /// host process started. A Windows Terminal that starts and then fails to
    /// host the session -- a broken profile, a settings file it will not read --
    /// reports that in its own window, and there is nothing left for VMLord to
    /// fall back *from*: the fallback answers a `wt.exe` that could not be
    /// started at all, which is the failure this process can actually see.
    fn spawn_somewhere(
        &self,
        invocation: &SshInvocation,
        vm_name: &str,
    ) -> Result<(), SshLaunchFailure> {
        let mut refusals = Vec::new();
        for command in terminal_commands(invocation, vm_name) {
            match (self.spawn)(&command) {
                Ok(()) => return Ok(()),
                Err(error) => {
                    log::warn!(
                        "could not open an SSH session to VM \"{vm_name}\" with {}: {error}",
                        command.program.display()
                    );
                    refusals.push(format!("{}: {error}", command.program.display()));
                }
            }
        }
        Err(SshLaunchFailure::NoTerminal { refusals })
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        address: impl Fn(&VmComputeSystemMapping) -> Result<Option<IpAddr>, RepositoryError>
        + Send
        + Sync
        + 'static,
        port: impl Fn(IpAddr, u16, Duration) -> Result<(), String> + Send + Sync + 'static,
        key: impl Fn(&Path) -> bool + Send + Sync + 'static,
        spawn: impl Fn(&TerminalCommand) -> io::Result<()> + Send + Sync + 'static,
    ) -> Self {
        Self {
            client: Some(PathBuf::from(r"C:\Windows\System32\OpenSSH\ssh.exe")),
            address: Box::new(address),
            port: Box::new(port),
            key: Box::new(key),
            spawn: Arc::new(spawn),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test_without_client() -> Self {
        Self {
            client: None,
            address: Box::new(|_| panic!("nothing may be asked without an ssh client")),
            port: Box::new(|_, _, _| panic!("nothing may be probed without an ssh client")),
            key: Box::new(|_| panic!("no key matters without an ssh client")),
            spawn: Arc::new(|_| panic!("nothing may be spawned without an ssh client")),
        }
    }
}

/// The two ways an SSH session can be put on screen, best first.
///
/// Windows Terminal gives the session a titled window beside the user's other
/// ones; `ssh.exe` in a console of its own is what remains on a machine without
/// it. Neither of them is a shell VMLord goes through: `wt.exe` is handed the
/// client and its arguments after a `--`, and the fallback is the client
/// itself. Nothing on this path is `powershell.exe` or `cmd.exe`, so no user
/// name, path or address is ever text some other parser gets to interpret.
fn terminal_commands(invocation: &SshInvocation, vm_name: &str) -> Vec<TerminalCommand> {
    let mut windows_terminal = vec![
        // `new` rather than `0`: `0` hands the tab to whatever window Windows
        // Terminal considers current, which is a window VMLord neither owns nor
        // can see, and that delivery has been observed not to be exactly-once.
        // A window of its own is delivered by the process VMLord started.
        OsString::from("-w"),
        OsString::from("new"),
        OsString::from("new-tab"),
        OsString::from("--title"),
        OsString::from(format!("VMLord SSH — {vm_name}")),
        // Everything after this is the command line, not Windows Terminal's own
        // arguments -- an `ssh` option would otherwise be read as one of its.
        OsString::from("--"),
        invocation.program.as_os_str().to_owned(),
    ];
    windows_terminal.extend(invocation.args.iter().cloned());

    vec![
        TerminalCommand {
            program: PathBuf::from("wt.exe"),
            args: windows_terminal,
            // Windows Terminal opens its own window; a console of its own would
            // leave an empty black one behind.
            create_new_console: false,
            raw_args: false,
        },
        TerminalCommand {
            program: invocation.program.clone(),
            args: invocation.args.clone(),
            // Without a console of its own, `ssh.exe` inherits VMLord's -- and
            // VMLord is a windowed application with none, so the session would
            // have nowhere to print and nowhere to be typed at.
            create_new_console: true,
            raw_args: false,
        },
    ]
}

/// Whether anything answers a TCP connection at `ip:port`.
fn probe_port_at(ip: IpAddr, port: u16, timeout: Duration) -> Result<(), String> {
    TcpStream::connect_timeout(&SocketAddr::new(ip, port), timeout)
        .map(drop)
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        net::IpAddr,
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
        time::Duration,
    };

    use uuid::Uuid;
    use vmlord_core::{NetworkMode, RepositoryError, SshAuthentication, SshConfig, SshPort};

    use super::{PORT_PROBE_TIMEOUT, SshLaunchFailure, SshLauncher, TerminalCommand};
    use crate::metadata::VmComputeSystemMapping;

    const CLIENT: &str = r"C:\Windows\System32\OpenSSH\ssh.exe";

    fn vm_id() -> Uuid {
        Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef)
    }

    fn address() -> IpAddr {
        "172.22.42.7".parse().unwrap()
    }

    fn config() -> SshConfig {
        SshConfig {
            username: "machi".to_owned(),
            port: SshPort::DEFAULT,
            authentication: SshAuthentication::VmlordKey,
        }
    }

    fn mapping_with(ssh: Option<SshConfig>) -> VmComputeSystemMapping {
        VmComputeSystemMapping {
            vm_id: vm_id(),
            vm_name: "dev-linux".to_owned(),
            hcs_compute_system_id: "vmlord-test".to_owned(),
            disk_gb: 20,
            endpoint_id: None,
            network_mode: NetworkMode::Nat,
            ssh,
            gpu_mode: vmlord_core::GpuMode::None,
            desktop_profile: vmlord_core::DesktopProfile::Headless,
            display_provisioning: vmlord_core::DisplayProvisioning::NotRequested,
            guest_target: None,
        }
    }

    fn mapping() -> VmComputeSystemMapping {
        mapping_with(Some(config()))
    }

    fn vm_directory() -> &'static Path {
        Path::new(r"C:\VMs\dev-linux")
    }

    /// Every terminal a launch asked for, and what each was answered with.
    #[derive(Clone, Default)]
    struct Attempts {
        commands: Arc<Mutex<Vec<TerminalCommand>>>,
        failures: usize,
    }

    impl Attempts {
        fn spawn(&self) -> impl Fn(&TerminalCommand) -> io::Result<()> + Send + Sync + use<> {
            let commands = Arc::clone(&self.commands);
            let failures = self.failures;
            move |command: &TerminalCommand| {
                let mut commands = commands.lock().unwrap();
                commands.push(command.clone());
                if commands.len() <= failures {
                    return Err(io::Error::other("injected terminal failure"));
                }
                Ok(())
            }
        }

        fn commands(&self) -> Vec<TerminalCommand> {
            self.commands.lock().unwrap().clone()
        }

        fn arguments(&self, attempt: usize) -> Vec<String> {
            self.commands()[attempt]
                .args
                .iter()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect()
        }

        fn count(&self) -> usize {
            self.commands.lock().unwrap().len()
        }
    }

    /// A launcher whose guest is up, answering and holding its key: everything
    /// a launch needs, so that a test can change one thing at a time.
    fn launcher(attempts: &Attempts) -> SshLauncher {
        SshLauncher::for_test(
            |_| Ok(Some(address())),
            |_, _, _| Ok(()),
            |_| true,
            attempts.spawn(),
        )
    }

    #[test]
    fn a_session_opens_in_a_window_of_windows_terminals_own() {
        let attempts = Attempts::default();

        launcher(&attempts)
            .launch(&mapping(), vm_directory())
            .unwrap();

        assert_eq!(attempts.count(), 1, "the first host that starts is the one");
        let command = &attempts.commands()[0];
        assert_eq!(command.program, Path::new("wt.exe"));
        assert!(
            !command.create_new_console,
            "Windows Terminal opens its own window; a console would leave an empty one behind"
        );
        let arguments = attempts.arguments(0);
        let window = arguments
            .iter()
            .position(|argument| argument == "-w")
            .map(|flag| arguments[flag + 1].as_str());
        assert_eq!(
            window,
            Some("new"),
            "`-w 0` hands the tab to a window VMLord does not own: {arguments:?}"
        );
        assert!(
            arguments.contains(&"VMLord SSH — dev-linux".to_owned()),
            "{arguments:?}"
        );
    }

    /// What the launch reports is what it spawned. The repository puts this
    /// line in front of a person, and a log describing a run other than the one
    /// that happened is worse than no log at all.
    #[test]
    fn the_launch_reports_the_command_it_spawned() {
        let attempts = Attempts::default();

        let invocation = launcher(&attempts)
            .launch(&mapping(), vm_directory())
            .unwrap();

        assert_eq!(invocation.program, Path::new(CLIENT));
        let reported: Vec<String> = invocation
            .args
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            reported,
            attempts.arguments(0)[7..],
            "the arguments after Windows Terminal's own are the ones reported back"
        );
    }

    /// The exact argument vector, in order: what Windows Terminal is told about
    /// itself, then `--`, then the client and the arguments the shared builder
    /// produced -- and nothing of VMLord's in between.
    #[test]
    fn windows_terminal_is_handed_the_client_and_its_arguments_after_a_separator() {
        let attempts = Attempts::default();

        launcher(&attempts)
            .launch(&mapping(), vm_directory())
            .unwrap();

        let arguments = attempts.arguments(0);
        assert_eq!(
            arguments[..6],
            [
                "-w",
                "new",
                "new-tab",
                "--title",
                "VMLord SSH — dev-linux",
                "--"
            ]
        );
        assert_eq!(arguments[6], CLIENT);
        assert_eq!(
            arguments[7..],
            [
                "-o",
                r#"HostKeyAlias="01234567-89ab-cdef-0123-456789abcdef""#,
                "-o",
                r#"StrictHostKeyChecking="accept-new""#,
                "-o",
                r#"UserKnownHostsFile="C:\VMs\dev-linux\known_hosts""#,
                "-o",
                r#"IdentitiesOnly="yes""#,
                "-o",
                r#"BatchMode="yes""#,
                "-i",
                r"C:\VMs\dev-linux\keys\id_ed25519",
                "-p",
                "22",
                "-l",
                "machi",
                "172.22.42.7",
            ]
        );
    }

    /// An interactive session is one a person is watching: a deadline VMLord
    /// invented would close their window in the middle of a handshake, and the
    /// guest is asked to run nothing but the shell they came for.
    #[test]
    fn an_interactive_session_carries_no_deadline_and_no_remote_command() {
        let attempts = Attempts::default();

        launcher(&attempts)
            .launch(&mapping(), vm_directory())
            .unwrap();

        let arguments = attempts.arguments(0);
        assert!(
            !arguments
                .iter()
                .any(|argument| argument.contains("ConnectTimeout")),
            "{arguments:?}"
        );
        assert_eq!(
            arguments.last().map(String::as_str),
            Some("172.22.42.7"),
            "the address is the last argument, so nothing was asked of the guest: {arguments:?}"
        );
    }

    /// A machine without Windows Terminal still has an OpenSSH client, and a
    /// console of its own is where that client can be typed at.
    #[test]
    fn a_refused_windows_terminal_falls_back_to_the_client_in_a_new_console() {
        let attempts = Attempts {
            failures: 1,
            ..Attempts::default()
        };

        launcher(&attempts)
            .launch(&mapping(), vm_directory())
            .unwrap();

        assert_eq!(attempts.count(), 2);
        let fallback = &attempts.commands()[1];
        assert_eq!(fallback.program, Path::new(CLIENT));
        assert!(
            fallback.create_new_console,
            "VMLord is a windowed process with no console to inherit"
        );
        assert_eq!(
            attempts.arguments(1),
            attempts.arguments(0)[7..],
            "the fallback is the same connection, just without a host in front of it"
        );
    }

    /// The fallback answers a `wt.exe` that could not be started at all. One
    /// that starts is the session's host, and VMLord does not start a second
    /// one behind its back.
    #[test]
    fn a_windows_terminal_that_started_is_not_second_guessed() {
        let attempts = Attempts::default();

        launcher(&attempts)
            .launch(&mapping(), vm_directory())
            .unwrap();

        assert_eq!(attempts.count(), 1);
    }

    #[test]
    fn a_session_that_could_not_be_hosted_at_all_reports_both_refusals() {
        let attempts = Attempts {
            failures: 2,
            ..Attempts::default()
        };

        let failure = launcher(&attempts)
            .launch(&mapping(), vm_directory())
            .unwrap_err();

        let SshLaunchFailure::NoTerminal { refusals } = &failure else {
            panic!("two refused hosts are their own failure: {failure:?}");
        };
        assert_eq!(refusals.len(), 2, "{refusals:?}");
        let message = failure.to_string();
        assert!(message.contains("wt.exe"), "{message}");
        assert!(message.contains(CLIENT), "{message}");
        assert_eq!(
            message.matches("injected terminal failure").count(),
            2,
            "each host says why it would not start: {message}"
        );
    }

    /// Nothing between VMLord and `ssh.exe` may be a shell: a user name or a
    /// path would then be text some other parser gets to interpret.
    #[test]
    fn no_shell_hosts_the_session() {
        let attempts = Attempts {
            failures: 2,
            ..Attempts::default()
        };

        launcher(&attempts)
            .launch(&mapping(), vm_directory())
            .unwrap_err();

        for command in attempts.commands() {
            let program = command.program.to_string_lossy().to_lowercase();
            assert!(
                !program.contains("powershell") && !program.contains("cmd.exe"),
                "{program}"
            );
            assert!(
                !command.raw_args,
                "a raw command line exists for cmd.exe, and cmd.exe is not here"
            );
        }
    }

    /// Two shells into one guest is an ordinary thing to want, and the second
    /// one has nothing to collide with: the session VMLord started is not
    /// VMLord's to own.
    #[test]
    fn one_vm_may_have_as_many_sessions_as_a_person_opens() {
        let attempts = Attempts::default();
        let launcher = launcher(&attempts);

        for _ in 0..3 {
            launcher.launch(&mapping(), vm_directory()).unwrap();
        }

        assert_eq!(attempts.count(), 3);
    }

    #[test]
    fn a_host_without_an_openssh_client_says_which_feature_is_missing() {
        let failure = SshLauncher::for_test_without_client()
            .launch(&mapping(), vm_directory())
            .unwrap_err();

        assert_eq!(failure, SshLaunchFailure::NoSshClient);
        assert!(failure.to_string().contains("OpenSSH"), "{failure}");
    }

    #[test]
    fn a_vm_created_without_ssh_has_nothing_to_connect_to() {
        let launcher = SshLauncher::for_test(
            |_| panic!("a VM with no SSH server has no address worth asking for"),
            |_, _, _| panic!("nor a port"),
            |_| panic!("nor a key"),
            |_| panic!("nor a terminal"),
        );

        let failure = launcher
            .launch(&mapping_with(None), vm_directory())
            .unwrap_err();

        assert_eq!(failure, SshLaunchFailure::Disabled);
    }

    /// The stored document can be edited by hand between two runs of VMLord,
    /// and a user name that is really a pile of `ssh` flags must not reach a
    /// terminal.
    #[test]
    fn a_stored_configuration_nothing_can_connect_with_is_refused_before_the_port_is_probed() {
        let damaged = SshConfig {
            username: "root -oProxyCommand=calc".to_owned(),
            ..config()
        };
        let launcher = SshLauncher::for_test(
            |_| Ok(Some(address())),
            |_, _, _| panic!("such a name is not worth probing a port for"),
            |_| panic!("nor a key"),
            |_| panic!("such a name must never reach a terminal"),
        );

        let failure = launcher
            .launch(&mapping_with(Some(damaged)), vm_directory())
            .unwrap_err();

        let SshLaunchFailure::Unusable { detail } = &failure else {
            panic!("a configuration nothing can connect with is its own failure: {failure:?}");
        };
        assert!(detail.contains("user"), "{detail}");
    }

    #[test]
    fn a_guest_with_no_address_yet_is_reported_as_such() {
        for address in [Ok(None), Err(RepositoryError::new("HNS is unavailable"))] {
            let address = Mutex::new(Some(address));
            let launcher = SshLauncher::for_test(
                move |_| address.lock().unwrap().take().expect("one lookup"),
                |_, _, _| panic!("a guest with no address has no port to probe"),
                |_| panic!("nor a key worth reading"),
                |_| panic!("nor a session to open"),
            );

            let failure = launcher.launch(&mapping(), vm_directory()).unwrap_err();

            assert_eq!(failure, SshLaunchFailure::NoAddress);
        }
    }

    /// One attempt with a short deadline, on the port the VM was created with:
    /// a guest that is still booting is a thing to say plainly rather than to
    /// wait out in a terminal window.
    #[test]
    fn the_configured_port_is_probed_once_and_briefly() {
        let probed = Arc::new(Mutex::new(Vec::new()));
        let launcher = SshLauncher::for_test(
            |_| Ok(Some(address())),
            {
                let probed = Arc::clone(&probed);
                move |ip, port, timeout| {
                    probed.lock().unwrap().push((ip, port, timeout));
                    Err("connection refused".to_owned())
                }
            },
            |_| panic!("a guest that does not answer is not asked for its key"),
            |_| panic!("nor given a terminal"),
        );
        let mapping = mapping_with(Some(SshConfig {
            port: SshPort::new(2222).unwrap(),
            ..config()
        }));

        let failure = launcher.launch(&mapping, vm_directory()).unwrap_err();

        assert_eq!(
            *probed.lock().unwrap(),
            [(address(), 2222, PORT_PROBE_TIMEOUT)],
            "one attempt, on the port the VM was created with"
        );
        assert_eq!(PORT_PROBE_TIMEOUT, Duration::from_secs(3));
        let SshLaunchFailure::Unreachable { port, detail } = &failure else {
            panic!("a guest that does not answer is its own failure: {failure:?}");
        };
        assert_eq!(*port, 2222);
        assert!(detail.contains("refused"), "{detail}");
        assert!(failure.to_string().contains("2222"), "{failure}");
    }

    #[test]
    fn a_key_mode_vm_whose_key_is_gone_is_refused_by_name() {
        let asked = Arc::new(Mutex::new(Vec::new()));
        let launcher = SshLauncher::for_test(
            |_| Ok(Some(address())),
            |_, _, _| Ok(()),
            {
                let asked = Arc::clone(&asked);
                move |path: &Path| {
                    asked.lock().unwrap().push(path.to_path_buf());
                    false
                }
            },
            |_| panic!("a login with no key to offer must not be attempted"),
        );

        let failure = launcher.launch(&mapping(), vm_directory()).unwrap_err();

        assert_eq!(
            *asked.lock().unwrap(),
            [PathBuf::from(r"C:\VMs\dev-linux\keys\id_ed25519")]
        );
        let SshLaunchFailure::NoKey { path } = &failure else {
            panic!("a missing key is its own failure: {failure:?}");
        };
        assert_eq!(path, Path::new(r"C:\VMs\dev-linux\keys\id_ed25519"));
    }

    /// Password mode has no key by design: whoever connects types a password,
    /// and a file VMLord never wrote must not be a reason to refuse them.
    #[test]
    fn a_password_mode_vm_needs_no_key_on_disk() {
        let attempts = Attempts::default();
        let launcher = SshLauncher::for_test(
            |_| Ok(Some(address())),
            |_, _, _| Ok(()),
            |_| panic!("password mode has no key to look for"),
            attempts.spawn(),
        );
        let mapping = mapping_with(Some(SshConfig {
            authentication: SshAuthentication::Password,
            ..config()
        }));

        launcher.launch(&mapping, vm_directory()).unwrap();

        let arguments = attempts.arguments(0);
        assert!(
            arguments.contains(&r#"PubkeyAuthentication="no""#.to_owned()),
            "{arguments:?}"
        );
        assert!(
            !arguments.iter().any(|argument| argument == "-i"),
            "password mode must not offer a key at all: {arguments:?}"
        );
    }

    #[test]
    fn every_failure_names_its_cause_rather_than_a_code() {
        let messages = [
            SshLaunchFailure::NoSshClient.to_string(),
            SshLaunchFailure::Disabled.to_string(),
            SshLaunchFailure::Unusable {
                detail: "the user name is not one Linux would accept".to_owned(),
            }
            .to_string(),
            SshLaunchFailure::NoAddress.to_string(),
            SshLaunchFailure::Unreachable {
                port: 22,
                detail: "refused".to_owned(),
            }
            .to_string(),
            SshLaunchFailure::NoKey {
                path: PathBuf::from(r"C:\VMs\dev-linux\keys\id_ed25519"),
            }
            .to_string(),
            SshLaunchFailure::NoTerminal {
                refusals: vec!["wt.exe: not found".to_owned()],
            }
            .to_string(),
        ];

        assert!(messages[0].contains("OpenSSH"), "{}", messages[0]);
        assert!(messages[1].contains("without SSH"), "{}", messages[1]);
        assert!(messages[2].contains("user name"), "{}", messages[2]);
        assert!(messages[3].contains("address"), "{}", messages[3]);
        assert!(messages[4].contains("port 22"), "{}", messages[4]);
        assert!(messages[5].contains("id_ed25519"), "{}", messages[5]);
        assert!(messages[6].contains("wt.exe"), "{}", messages[6]);
    }

    #[test]
    fn a_probe_of_a_port_nobody_listens_on_fails_rather_than_hangs() {
        // Port 9 is discard, which nothing serves on a developer machine.
        let probed =
            super::probe_port_at("127.0.0.1".parse().unwrap(), 9, Duration::from_millis(200));

        assert!(probed.is_err(), "an unserved port must not report success");
    }
}
