//! Turning what VMLord knows about a VM into a run of Windows' OpenSSH client.
//!
//! Two things happen here and nothing else. A stored [`SshConfig`] plus the
//! address HNS currently gives the VM's endpoint become an [`SshEndpoint`] --
//! the pair of "how to log in" and "where to log in", which are learned at
//! different times and only make sense together. And an endpoint becomes an
//! argument vector for `ssh.exe`.
//!
//! Both the readiness wait and the interactive launcher connect to the same
//! guests with the same key, the same host-key file and the same rules about
//! what may prompt; they differ only in what they ask the guest to do. Writing
//! the arguments twice would let the two drift, and a drift here is not a
//! cosmetic one -- it is one client learning a host key the other refuses, or
//! one of them silently falling back to a credential VMLord did not choose.
//!
//! Nothing in this module builds a command *line*. `ssh.exe` is spawned with an
//! argument vector, so no user name, path or address is ever a substring of
//! something a shell -- or PowerShell, or `cmd` -- would parse. The two places
//! where an outside value does land inside a larger string are the `-o` options
//! that carry a path, and those are quoted and percent-escaped for OpenSSH's
//! own configuration parser rather than for a shell.

use std::{
    ffi::{OsStr, OsString},
    net::IpAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use vmlord_core::{RepositoryError, SshAuthentication, SshEndpoint};

use crate::{hcn_endpoint::HcnEndpoint, layout, metadata::VmComputeSystemMapping};

/// Where Windows keeps its OpenSSH client.
const SSH_CLIENT_RELATIVE_PATH: &str = r"System32\OpenSSH\ssh.exe";

/// Windows' own OpenSSH client, if this installation has one.
///
/// Optional Windows features can be absent, so this is a state to report rather
/// than a reason to panic: whoever asked names the feature a person has to
/// install.
pub(crate) fn client_path() -> Option<PathBuf> {
    let root = std::env::var_os("SystemRoot")?;
    let path = PathBuf::from(root).join(SSH_CLIENT_RELATIVE_PATH);
    path.is_file().then_some(path)
}

/// The address HNS has given the VM's endpoint, if it has one yet.
///
/// The endpoint is where a guest's address is known on the host side: HNS
/// assigns it and VMLord's DHCP server offers the guest that one and no other.
/// An address HNS spells in a way that does not parse is reported as no address
/// rather than as an error -- it is exactly as unusable as none at all, and
/// whoever is waiting has a timeout that turns that into a failure.
pub(crate) fn guest_address(
    mapping: &VmComputeSystemMapping,
) -> Result<Option<IpAddr>, RepositoryError> {
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
            tracing::debug!(
                "HNS reported \"{}\" as the address of VM \"{}\", which is not an IP address: \
                 {error}",
                address.ip_address,
                mapping.vm_name
            );
            Ok(None)
        }
    }
}

/// The endpoint of a running VM: its stored SSH configuration at `address`.
///
/// `None` when the VM has no SSH access at all -- it was created without one,
/// or installed by hand from local media. An error when it has a configuration
/// that cannot be connected with: that is a stored document someone has since
/// edited, and the last moment to refuse it is while it is still data.
///
/// The interactive launcher joins the two halves here; the readiness wait joins
/// them itself, because it reads the configuration before the guest has an
/// address, so that a stored document nothing can connect with is refused
/// instead of spending an address timeout first.
pub(crate) fn endpoint(
    mapping: &VmComputeSystemMapping,
    address: IpAddr,
) -> Result<Option<SshEndpoint>, RepositoryError> {
    mapping
        .ssh
        .as_ref()
        .map(|config| SshEndpoint::new(mapping.vm_id, config, address))
        .transpose()
}

/// Everything one run of `ssh.exe` needs, decided without running it.
///
/// Separate from running it so that the decisions -- which key, which
/// known-hosts file, which port -- are testable without a guest, a network, or
/// an `ssh.exe` to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SshInvocation {
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<OsString>,
}

impl SshInvocation {
    /// The run this describes, as one line to show a person.
    ///
    /// For reading, not for re-running: nothing on this path goes through a
    /// shell, and this is the only place these arguments ever become a single
    /// string. It exists because a session that opened tells VMLord nothing
    /// afterwards -- what was asked of `ssh.exe` is knowable only here, and it
    /// is the first thing anyone needs when a guest refuses a login for a
    /// reason only the client saw.
    ///
    /// A token holding white space is wrapped in quotes so the line can be read
    /// back. The `-o` values already carry quotes of their own -- OpenSSH's
    /// configuration parser put them there -- and are left exactly as they go
    /// to the client, because a log that shows something other than what ran is
    /// worse than no log.
    pub(crate) fn command_line(&self) -> String {
        std::iter::once(self.program.as_os_str())
            .chain(self.args.iter().map(OsString::as_os_str))
            .map(quote_for_reading)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// One token of a command line, quoted only when reading it back needs it.
fn quote_for_reading(token: &OsStr) -> String {
    let text = token.to_string_lossy();
    if text.contains(char::is_whitespace) && !text.contains('"') {
        format!("\"{text}\"")
    } else {
        text.into_owned()
    }
}

/// Builds the command that connects to `endpoint`.
///
/// `remote_command` is what the guest is asked to run, or `None` for the
/// interactive shell an operator gets. It is passed as one argument, so a
/// command with spaces in it stays one command and nothing in it is re-parsed
/// on this side.
///
/// The options are the same for both callers, and each is here for a reason:
///
/// * `HostKeyAlias` is the VM's id, so the guest's key is remembered under
///   something that survives a rename and a new address -- and so that a key
///   that changes afterwards means "not the guest it was" rather than "the VM
///   moved".
/// * `UserKnownHostsFile` is the VM's own file, always. VMLord's automatic
///   learning must not add lines to a file a person maintains by hand.
/// * `StrictHostKeyChecking=accept-new` learns an unknown key and refuses a
///   changed one. Nothing here deletes or rewrites a key that changed: that is
///   a decision for a person who has been shown it.
///
/// `session_log` is where OpenSSH is told to write its own log with `-E`. Only
/// the interactive launcher passes one: it is the sole caller that cannot read
/// the client's standard error, because the client's window is not its own,
/// and the text in that file is what tells a refused credential from a changed
/// host key afterwards.
pub(crate) fn invocation(
    client: &Path,
    endpoint: &SshEndpoint,
    vm_directory: &Path,
    connect_timeout: Option<Duration>,
    remote_command: Option<&str>,
    session_log: Option<&Path>,
) -> SshInvocation {
    let mut args = Vec::new();
    let mut option = |value: OsString| {
        args.push(OsString::from("-o"));
        args.push(value);
    };

    option(config_option(
        "HostKeyAlias",
        OsStr::new(&endpoint.host_key_alias()),
    ));
    option(config_option(
        "StrictHostKeyChecking",
        OsStr::new("accept-new"),
    ));
    option(config_option(
        "UserKnownHostsFile",
        layout::ssh_known_hosts_path(vm_directory).as_os_str(),
    ));
    if let Some(timeout) = connect_timeout {
        option(config_option(
            "ConnectTimeout",
            OsStr::new(&timeout.as_secs().to_string()),
        ));
    }

    match endpoint.authentication {
        // The VM's key and nothing else: `IdentitiesOnly` keeps the keys of a
        // running agent out of it, and `BatchMode` makes a key that stopped
        // working an error rather than a password prompt nobody is there to
        // answer.
        SshAuthentication::VmlordKey => {
            option(config_option("IdentitiesOnly", OsStr::new("yes")));
            option(config_option("BatchMode", OsStr::new("yes")));
            args.push(OsString::from("-i"));
            args.push(identity_file(&layout::ssh_key_path(vm_directory)));
        }
        // A password the person at the keyboard types. No key is offered at
        // all -- an agent key that happened to work would be a login VMLord
        // cannot reproduce -- and no `BatchMode`, because the prompt is the
        // point.
        SshAuthentication::Password => {
            option(config_option("PubkeyAuthentication", OsStr::new("no")));
            option(config_option(
                "PreferredAuthentications",
                OsStr::new("keyboard-interactive,password"),
            ));
        }
    }

    if let Some(log) = session_log {
        args.push(OsString::from("-E"));
        args.push(log.as_os_str().to_owned());
    }

    args.push(OsString::from("-p"));
    args.push(OsString::from(endpoint.port.get().to_string()));
    // `-l user host` rather than `user@host`: a user name containing an `@`
    // would otherwise be split in the wrong place by ssh itself.
    args.push(OsString::from("-l"));
    args.push(OsString::from(&endpoint.username));
    args.push(OsString::from(endpoint.address.to_string()));
    if let Some(command) = remote_command {
        args.push(OsString::from(command));
    }

    SshInvocation {
        program: client.to_path_buf(),
        args,
    }
}

/// One `-o Name=value` option, written so that OpenSSH reads back the value
/// that went in.
///
/// The value of a `-o` is not an opaque argument: `ssh` hands it to the same
/// parser its configuration file goes through. That parser splits on white
/// space, honours double quotes, and expands `%` sequences -- so a VM under
/// `C:\Virtual Machines\` would otherwise lose everything after the space, and
/// one whose name contains `%h` would point at a path made up on the spot. The
/// quotes and the doubled `%` are for that parser, not for a shell; there is no
/// shell anywhere in this path.
fn config_option(name: &str, value: &OsStr) -> OsString {
    let mut option = OsString::from(name);
    option.push("=\"");
    option.push(escape_percent(value));
    option.push("\"");
    option
}

/// The `-i` argument, which is an argument rather than an option value.
///
/// It reaches `ssh` whole, so it needs no quoting; it does go through the same
/// `%` expansion, so it needs the same escaping.
fn identity_file(path: &Path) -> OsString {
    escape_percent(path.as_os_str())
}

/// `%` doubled, which is how OpenSSH's expander spells a literal one.
///
/// Done on the bytes of the `OsStr` through its lossy form only when there is a
/// `%` to escape: a Windows path is UTF-16 and `to_string_lossy` can damage
/// one, so the common path returns it untouched.
fn escape_percent(value: &OsStr) -> OsString {
    let text = value.to_string_lossy();
    if text.contains('%') {
        OsString::from(text.replace('%', "%%"))
    } else {
        value.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::IpAddr,
        path::{Path, PathBuf},
        time::Duration,
    };

    use uuid::Uuid;
    use vmlord_core::{NetworkMode, SshAuthentication, SshConfig, SshEndpoint, SshPort};

    use super::{SshInvocation, endpoint, invocation};
    use crate::metadata::VmComputeSystemMapping;

    fn vm_id() -> Uuid {
        Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef)
    }

    fn address() -> IpAddr {
        "172.22.42.7".parse().unwrap()
    }

    fn config() -> SshConfig {
        SshConfig {
            username: "machi".into(),
            port: SshPort::DEFAULT,
            authentication: SshAuthentication::VmlordKey,
        }
    }

    fn mapping(ssh: Option<SshConfig>) -> VmComputeSystemMapping {
        VmComputeSystemMapping {
            vm_id: vm_id(),
            vm_name: "dev-linux".to_owned(),
            hcs_compute_system_id: "vmlord-test".to_owned(),
            disk_gb: 20,
            endpoint_id: None,
            network_mode: NetworkMode::Nat,
            ssh,
            ssh_daemon: None,
            gpu_mode: vmlord_core::GpuMode::None,
            desktop_profile: vmlord_core::DesktopProfile::Headless,
            display_provisioning: vmlord_core::DisplayProvisioning::NotRequested,
            display_mode: None,
            guest_target: None,
        }
    }

    fn client() -> PathBuf {
        PathBuf::from(r"C:\Windows\System32\OpenSSH\ssh.exe")
    }

    fn vm_directory() -> PathBuf {
        PathBuf::from(r"C:\VMs\dev-linux")
    }

    fn endpoint_with(config: SshConfig) -> SshEndpoint {
        SshEndpoint::new(vm_id(), &config, address()).unwrap()
    }

    fn built(config: SshConfig) -> SshInvocation {
        invocation(
            &client(),
            &endpoint_with(config),
            &vm_directory(),
            Some(Duration::from_secs(10)),
            Some("cloud-init status --wait --long"),
            None,
        )
    }

    fn arguments(invocation: &SshInvocation) -> Vec<String> {
        invocation
            .args
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    /// The value of `-o Name=...`, unquoted, or `None` when it was not passed.
    fn option(invocation: &SshInvocation, name: &str) -> Option<String> {
        let args = arguments(invocation);
        let prefix = format!("{name}=");
        args.iter()
            .zip(args.iter().skip(1))
            .find(|(flag, value)| *flag == "-o" && value.starts_with(&prefix))
            .map(|(_, value)| value[prefix.len()..].trim_matches('"').to_owned())
    }

    fn value_after(invocation: &SshInvocation, flag: &str) -> Option<String> {
        let args = arguments(invocation);
        let at = args.iter().position(|argument| argument == flag)?;
        args.get(at + 1).cloned()
    }

    /// What the diagnostics show after a session opens, since the session
    /// itself says nothing back. It has to be the arguments that ran, not a
    /// tidied version of them.
    #[test]
    fn an_invocation_can_be_read_back_as_the_line_it_is() {
        let line = built(config()).command_line();

        assert!(
            line.starts_with(r"C:\Windows\System32\OpenSSH\ssh.exe "),
            "{line}"
        );
        assert!(line.contains("-p 22"), "{line}");
        assert!(line.contains("-l machi"), "{line}");
        assert!(line.contains("172.22.42.7"), "{line}");
        assert!(
            line.contains(r#"-o UserKnownHostsFile="C:\VMs\dev-linux\known_hosts""#),
            "an option keeps the quotes OpenSSH's own parser needs: {line}"
        );
    }

    /// A path with a space in it is what quoting is for -- and the `-o` values
    /// beside it must not be quoted twice, because what is shown has to be what
    /// was passed.
    #[test]
    fn a_token_with_a_space_is_readable_and_one_that_quotes_itself_is_left_alone() {
        let invocation = invocation(
            Path::new(r"C:\Program Files\OpenSSH\ssh.exe"),
            &endpoint_with(config()),
            Path::new(r"C:\Virtual Machines\dev-linux"),
            None,
            None,
            None,
        );

        let line = invocation.command_line();

        assert!(
            line.starts_with(r#""C:\Program Files\OpenSSH\ssh.exe" "#),
            "{line}"
        );
        assert!(
            line.contains(r#"-i "C:\Virtual Machines\dev-linux\keys\id_ed25519""#),
            "{line}"
        );
        assert!(
            line.contains(r#"-o UserKnownHostsFile="C:\Virtual Machines\dev-linux\known_hosts""#),
            "the option is shown exactly as the client gets it: {line}"
        );
    }

    #[test]
    fn a_vm_with_no_ssh_access_has_no_endpoint_rather_than_an_error() {
        assert_eq!(endpoint(&mapping(None), address()).unwrap(), None);
    }

    #[test]
    fn an_endpoint_joins_the_stored_configuration_to_the_current_address() {
        let built = endpoint(&mapping(Some(config())), address())
            .unwrap()
            .unwrap();

        assert_eq!(built, endpoint_with(config()));
        assert_eq!(built.address, address());
        assert_eq!(built.host_key_alias(), vm_id().to_string());
    }

    /// The stored document can be edited by hand between two runs of VMLord,
    /// and a user name that is really a pile of `ssh` flags must not become
    /// an endpoint anything else would then build arguments out of.
    #[test]
    fn a_stored_configuration_that_cannot_be_connected_with_is_refused() {
        let damaged = SshConfig {
            username: "root -oProxyCommand=calc".into(),
            ..config()
        };

        assert!(endpoint(&mapping(Some(damaged)), address()).is_err());
    }

    #[test]
    fn every_connection_pins_the_host_key_to_the_vm_id_and_the_vms_own_file() {
        let invocation = built(config());

        assert_eq!(invocation.program, client());
        assert_eq!(
            option(&invocation, "HostKeyAlias").as_deref(),
            Some(vm_id().to_string().as_str()),
            "the key must be remembered under something a rename cannot change"
        );
        assert_eq!(
            option(&invocation, "UserKnownHostsFile").as_deref(),
            Some(r"C:\VMs\dev-linux\known_hosts"),
            "the VM's host key must not land in the user's own known_hosts"
        );
        assert_eq!(
            option(&invocation, "StrictHostKeyChecking").as_deref(),
            Some("accept-new")
        );
        assert_eq!(option(&invocation, "ConnectTimeout").as_deref(), Some("10"));
    }

    /// `accept-new` learns a key once; a key that changed after that stops the
    /// login. Nothing in the arguments may quietly undo that -- no
    /// `StrictHostKeyChecking=no`, and no file the client is told to forget.
    #[test]
    fn nothing_offers_to_forget_a_host_key_that_changed() {
        for authentication in [SshAuthentication::VmlordKey, SshAuthentication::Password] {
            let arguments = arguments(&built(SshConfig {
                authentication,
                ..config()
            }));

            for argument in &arguments {
                assert!(
                    !argument.starts_with("StrictHostKeyChecking=")
                        || argument == r#"StrictHostKeyChecking="accept-new""#,
                    "{arguments:?}"
                );
                assert!(
                    !argument.contains("nul") && !argument.contains("/dev/null"),
                    "a known-hosts file that forgets everything is not one: {arguments:?}"
                );
            }
            assert_eq!(
                arguments
                    .iter()
                    .filter(|argument| argument.starts_with("UserKnownHostsFile="))
                    .count(),
                1,
                "exactly one known-hosts file, and it is the VM's: {arguments:?}"
            );
        }
    }

    #[test]
    fn key_mode_offers_the_vms_own_key_and_nothing_else() {
        let invocation = built(config());

        assert_eq!(
            value_after(&invocation, "-i").as_deref(),
            Some(r"C:\VMs\dev-linux\keys\id_ed25519")
        );
        assert_eq!(
            option(&invocation, "IdentitiesOnly").as_deref(),
            Some("yes")
        );
        assert_eq!(
            option(&invocation, "BatchMode").as_deref(),
            Some("yes"),
            "a key that stopped working must fail rather than ask for a password"
        );
        assert_eq!(option(&invocation, "PubkeyAuthentication"), None);
    }

    #[test]
    fn password_mode_offers_no_key_and_lets_a_person_type() {
        let invocation = built(SshConfig {
            authentication: SshAuthentication::Password,
            ..config()
        });

        assert_eq!(
            value_after(&invocation, "-i"),
            None,
            "password mode must not offer a key at all"
        );
        assert_eq!(
            option(&invocation, "PubkeyAuthentication").as_deref(),
            Some("no")
        );
        assert_eq!(
            option(&invocation, "PreferredAuthentications").as_deref(),
            Some("keyboard-interactive,password")
        );
        assert_eq!(
            option(&invocation, "BatchMode"),
            None,
            "the prompt is the whole point of password mode"
        );
    }

    #[test]
    fn the_port_the_vm_was_created_with_is_the_port_it_is_reached_on() {
        let invocation = built(SshConfig {
            port: SshPort::new(2222).unwrap(),
            ..config()
        });

        assert_eq!(value_after(&invocation, "-p").as_deref(), Some("2222"));
        assert_eq!(
            value_after(&built(config()), "-p").as_deref(),
            Some("22"),
            "a VM created on the default port still says which port it means"
        );
    }

    #[test]
    fn the_user_and_the_address_are_separate_arguments_not_a_joined_string() {
        // `-l user host`: a user name containing an `@` would otherwise be
        // split in the wrong place by ssh itself.
        let invocation = built(config());
        let args = arguments(&invocation);
        let user_flag = args.iter().position(|argument| argument == "-l").unwrap();

        assert_eq!(args[user_flag + 1], "machi");
        assert_eq!(args[user_flag + 2], "172.22.42.7");
    }

    #[test]
    fn a_guest_on_ipv6_is_reached_at_the_address_it_has() {
        let address: IpAddr = "fd7a:115c:a1e0::1".parse().unwrap();
        let invocation = invocation(
            &client(),
            &SshEndpoint::new(vm_id(), &config(), address).unwrap(),
            &vm_directory(),
            None,
            None,
            None,
        );
        let args = arguments(&invocation);

        assert_eq!(
            args.last().map(String::as_str),
            Some("fd7a:115c:a1e0::1"),
            "the address is its own argument, so no bracket form is needed: {args:?}"
        );
        assert!(
            !args
                .iter()
                .any(|argument| argument.contains("ConnectTimeout")),
            "a connection nobody put a deadline on gets none: {args:?}"
        );
    }

    /// `C:\Virtual Machines\...` is an ordinary place to keep VMs, and the
    /// value of a `-o` goes through OpenSSH's own tokeniser: unquoted, the
    /// known-hosts file would be `C:\Virtual` and the rest a parse error.
    #[test]
    fn a_path_with_spaces_survives_openssh_s_own_parser() {
        let invocation = invocation(
            &client(),
            &endpoint_with(config()),
            Path::new(r"C:\Virtual Machines\dev linux"),
            None,
            None,
            None,
        );
        let args = arguments(&invocation);

        assert!(
            args.iter().any(|argument| argument
                == r#"UserKnownHostsFile="C:\Virtual Machines\dev linux\known_hosts""#),
            "{args:?}"
        );
        assert_eq!(
            value_after(&invocation, "-i").as_deref(),
            Some(r"C:\Virtual Machines\dev linux\keys\id_ed25519"),
            "-i is an argument of its own and must not grow quotes"
        );
    }

    /// OpenSSH expands `%d`, `%h` and friends inside these values, so a
    /// directory whose name contains a `%` would otherwise name a file nobody
    /// asked for. `%%` is how that parser spells a literal `%`.
    #[test]
    fn a_percent_in_a_path_stays_a_percent() {
        let invocation = invocation(
            &client(),
            &endpoint_with(config()),
            Path::new(r"C:\VMs\100%d"),
            None,
            None,
            None,
        );

        assert_eq!(
            option(&invocation, "UserKnownHostsFile").as_deref(),
            Some(r"C:\VMs\100%%d\known_hosts")
        );
        assert_eq!(
            value_after(&invocation, "-i").as_deref(),
            Some(r"C:\VMs\100%%d\keys\id_ed25519")
        );
    }

    #[test]
    fn the_remote_command_is_one_argument_rather_than_a_command_line() {
        let invocation = built(config());
        let args = arguments(&invocation);

        assert_eq!(
            args.last().map(String::as_str),
            Some("cloud-init status --wait --long"),
            "{args:?}"
        );
        // An interactive session asks for nothing: the guest's own shell is
        // what the person came for.
        let interactive = invocation_without_command();
        let args = arguments(&interactive);
        assert_eq!(
            args.last().map(String::as_str),
            Some("172.22.42.7"),
            "{args:?}"
        );
    }

    fn invocation_without_command() -> SshInvocation {
        invocation(
            &client(),
            &endpoint_with(config()),
            &vm_directory(),
            None,
            None,
            None,
        )
    }

    /// The interactive launcher cannot read the client's standard error -- the
    /// window is not VMLord's -- so OpenSSH is told to write what only it knows
    /// where the launcher can read it afterwards.
    #[test]
    fn a_session_log_is_where_openssh_writes_what_only_it_can_say() {
        let invocation = invocation(
            &client(),
            &endpoint_with(config()),
            &vm_directory(),
            None,
            None,
            Some(Path::new(r"C:\VMs\dev-linux\ssh-sessions\a.log")),
        );

        assert_eq!(
            value_after(&invocation, "-E").as_deref(),
            Some(r"C:\VMs\dev-linux\ssh-sessions\a.log")
        );
        assert!(
            invocation_without_command().args.iter().all(|argument| argument != "-E"),
            "no other caller writes a log: they read the client's output themselves"
        );
    }

    /// The one property the whole module exists for: every value that came
    /// from outside VMLord is a whole argument, or a quoted value inside one,
    /// and never a fragment of a line something else would re-parse.
    #[test]
    fn no_outside_value_is_pasted_into_a_command_line() {
        let invocation = invocation(
            &client(),
            &endpoint_with(config()),
            &vm_directory(),
            None,
            Some("echo one two"),
            None,
        );
        let args = arguments(&invocation);

        // The user name, the address and a command with spaces in it are each
        // an argument of their own rather than pieces of a line.
        assert!(args.contains(&"machi".to_owned()), "{args:?}");
        assert!(args.contains(&"172.22.42.7".to_owned()), "{args:?}");
        assert!(args.contains(&"echo one two".to_owned()), "{args:?}");
        assert!(
            !invocation
                .program
                .as_os_str()
                .to_string_lossy()
                .to_lowercase()
                .contains("powershell"),
            "the client is ssh.exe itself, with no shell between"
        );
    }
}
