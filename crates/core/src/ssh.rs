//! What VMLord knows about logging into a guest over SSH.
//!
//! SSH used to be a port number beside a VM: `Option<u32>`, where `Some` meant
//! "there is an SSH server, and also VMLord knows how to reach it" and `None`
//! meant any of "SSH is off", "the VM is not running yet" and "this backend
//! cannot answer". Everything that had to act on it -- enabling a button,
//! building a command line -- guessed the rest from elsewhere.
//!
//! These types spell the whole thing instead. A VM either has an SSH access
//! VMLord configured, in which case the user name, the port and the way in are
//! all known together, or it has none; and a connection needs, on top of that,
//! an address the guest currently answers at. The two are separate because they
//! are learned at different times: the configuration is written once, when the
//! VM is created, and the address exists only while the VM runs.

use std::{fmt, net::IpAddr, num::NonZeroU16};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{RepositoryError, provisioning::validate_username};

/// The port a guest's SSH server listens on unless someone chooses another.
const DEFAULT_SSH_PORT: NonZeroU16 = NonZeroU16::new(22).expect("22 is not zero");

/// How VMLord proves who it is to a guest.
///
/// Two modes and no third: VMLord either offers the key pair it generated for
/// this VM, or the password the guest user was provisioned with. The user's own
/// keys and any running agent are deliberately not in this list -- a login that
/// silently succeeded through some other credential would be a login VMLord
/// cannot reproduce. Key mode does not fall back to a password either: a key
/// that stopped working is something to see, not something to route around.
///
/// Serializable because it is recorded per VM at creation: the variant names
/// are an on-disk format, so renaming one changes what stored VMs read back as.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SshAuthentication {
    /// The key pair VMLord generated for this VM, whose public half cloud-init
    /// installed in the guest.
    VmlordKey,
    /// The password the guest user was provisioned with. Never stored: it is
    /// hashed into the seed and typed by whoever connects.
    Password,
}

impl fmt::Display for SshAuthentication {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::VmlordKey => "key",
            Self::Password => "password",
        })
    }
}

/// A port an SSH server can actually be reached on.
///
/// A [`NonZeroU16`] rather than a `u16`, so that port 0 -- which means "any
/// free port" to a listener and nothing at all to a client -- is not a value to
/// be checked for at every use, but one that cannot be built or read back from
/// a stored document. `1..=65535` is the range the epic names, and it is
/// exactly what this type holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SshPort(NonZeroU16);

impl SshPort {
    /// The port a VM gets when nobody asks for another one.
    pub const DEFAULT: Self = Self(DEFAULT_SSH_PORT);

    /// Accepts a port an SSH server can be reached on, and refuses zero.
    pub fn new(port: u16) -> Result<Self, RepositoryError> {
        NonZeroU16::new(port).map(Self).ok_or_else(|| {
            let error =
                RepositoryError::new("the SSH port must be between 1 and 65535, and 0 is not one");
            tracing::warn!("rejected SSH port: {error}");
            error
        })
    }

    #[must_use]
    pub const fn get(self) -> u16 {
        self.0.get()
    }
}

impl Default for SshPort {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl fmt::Display for SshPort {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// The SSH access VMLord configured into a guest, as it was configured.
///
/// Written once, when the VM is created, and read for every connection after
/// that. What is *not* here matters as much as what is: no address, because the
/// guest takes a new one from HNS on every start and a stored one would be a
/// lie; and no password, because a password on disk is a password leaked --
/// [`SshAuthentication::Password`] records only that a person will type one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshConfig {
    /// The guest user cloud-init created, which is also who connects.
    pub username: String,
    pub port: SshPort,
    pub authentication: SshAuthentication,
}

impl SshConfig {
    /// Checks a configuration that has come back from disk before anything is
    /// built out of it.
    ///
    /// The user name goes through the same validator the create form's does:
    /// what reaches an `ssh -l` argument is a name Linux would accept, whether
    /// it was typed a minute ago or read from a document someone has since
    /// edited by hand. The port needs no check of its own -- [`SshPort`] cannot
    /// hold an invalid one, and a stored `0` fails to parse.
    pub fn validate(&self) -> Result<(), RepositoryError> {
        validate_username(&self.username)
    }
}

/// Whether a VM in the list can be connected to over SSH at all.
///
/// The replacement for `VmSummary`'s old `ssh_port: Option<u32>`, which was a
/// capability written as a number: every reader had to know that a missing port
/// meant "no SSH here" rather than "the port is not known yet".
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SshAvailability {
    /// This VM has no SSH access: it was created without one, installed by hand
    /// from local media, or comes from a backend that does not configure SSH.
    #[default]
    Disabled,
    Enabled(SshConfig),
}

impl SshAvailability {
    /// The configuration to connect with, if there is one.
    #[must_use]
    pub fn config(&self) -> Option<&SshConfig> {
        match self {
            Self::Disabled => None,
            Self::Enabled(config) => Some(config),
        }
    }

    #[must_use]
    pub fn is_enabled(&self) -> bool {
        matches!(self, Self::Enabled(_))
    }
}

impl From<Option<SshConfig>> for SshAvailability {
    fn from(config: Option<SshConfig>) -> Self {
        config.map_or(Self::Disabled, Self::Enabled)
    }
}

/// One running guest, as an SSH client would have to be told about it.
///
/// The configuration plus the two things only a running VM has: an address, and
/// the identity that address belongs to. [`SshEndpoint::new`] is the only way
/// to build one, so a configuration that came back damaged from disk is refused
/// here -- before a command line is formed out of it, rather than after a
/// process has already been handed the pieces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshEndpoint {
    /// The VM's stable identity, which is what its host key is remembered
    /// under.
    ///
    /// The id rather than the name or the address: both of those change over a
    /// VM's life, and a host key filed under either would either be lost on a
    /// rename or -- worse -- matched against a different guest that inherited
    /// the address.
    pub vm_id: Uuid,
    pub username: String,
    pub address: IpAddr,
    pub port: SshPort,
    pub authentication: SshAuthentication,
}

impl SshEndpoint {
    /// Builds the endpoint of a running VM, refusing a configuration that is
    /// not fit to connect with.
    pub fn new(vm_id: Uuid, config: &SshConfig, address: IpAddr) -> Result<Self, RepositoryError> {
        config.validate()?;
        Ok(Self {
            vm_id,
            username: config.username.clone(),
            address,
            port: config.port,
            authentication: config.authentication,
        })
    }

    /// The name this guest's host key is remembered under.
    ///
    /// Stable across address changes and renames, which is the whole point:
    /// `StrictHostKeyChecking=accept-new` learns a key once, and a changed key
    /// afterwards has to mean "this is not the guest it was" rather than "the
    /// VM moved".
    #[must_use]
    pub fn host_key_alias(&self) -> String {
        self.vm_id.to_string()
    }
}

impl fmt::Display for SshEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}@{}:{} ({})",
            self.username, self.address, self.port, self.authentication
        )
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use uuid::Uuid;

    use super::{SshAuthentication, SshAvailability, SshConfig, SshEndpoint, SshPort};

    fn config() -> SshConfig {
        SshConfig {
            username: "user".into(),
            port: SshPort::DEFAULT,
            authentication: SshAuthentication::VmlordKey,
        }
    }

    fn address() -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(172, 30, 0, 5))
    }

    #[test]
    fn a_port_an_ssh_server_can_be_reached_on_is_kept() {
        for candidate in [1, 22, 2222, 65535] {
            assert_eq!(SshPort::new(candidate).unwrap().get(), candidate);
        }
    }

    #[test]
    fn port_zero_is_not_a_port_to_connect_to() {
        let error = SshPort::new(0).unwrap_err().to_string();

        assert!(error.contains("SSH port"), "got {error:?}");
    }

    #[test]
    fn the_default_port_is_the_one_a_guest_listens_on() {
        assert_eq!(SshPort::default().get(), 22);
    }

    #[test]
    fn a_port_survives_being_written_and_read_back() {
        let document = serde_json::to_string(&SshPort::new(2222).unwrap()).unwrap();

        assert_eq!(document, "2222", "the port is stored as a plain number");
        assert_eq!(
            serde_json::from_str::<SshPort>(&document).unwrap().get(),
            2222
        );
    }

    #[test]
    fn a_stored_port_of_zero_does_not_load() {
        assert!(serde_json::from_str::<SshPort>("0").is_err());
    }

    #[test]
    fn a_configuration_survives_being_written_and_read_back() {
        let stored = SshConfig {
            username: "ubuntu".into(),
            port: SshPort::new(2222).unwrap(),
            authentication: SshAuthentication::Password,
        };

        let document = serde_json::to_string(&stored).unwrap();

        assert_eq!(
            document,
            r#"{"username":"ubuntu","port":2222,"authentication":"Password"}"#
        );
        assert_eq!(
            serde_json::from_str::<SshConfig>(&document).unwrap(),
            stored
        );
    }

    /// The stored names are a format, and a mode nobody wrote is not a mode to
    /// silently accept.
    #[test]
    fn a_stored_authentication_mode_that_does_not_exist_does_not_load() {
        assert!(
            serde_json::from_str::<SshConfig>(
                r#"{"username":"ubuntu","port":22,"authentication":"Agent"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn a_configuration_carries_the_user_name_rule_of_the_create_form() {
        for candidate in ["", "User", "1user", "user name", "user\n"] {
            let config = SshConfig {
                username: candidate.into(),
                ..config()
            };

            assert!(
                config.validate().unwrap_err().to_string().contains("user"),
                "{candidate:?} must be refused"
            );
        }
        assert!(config().validate().is_ok());
    }

    #[test]
    fn availability_says_whether_there_is_anything_to_connect_to() {
        assert_eq!(SshAvailability::default(), SshAvailability::Disabled);
        assert!(!SshAvailability::Disabled.is_enabled());
        assert_eq!(SshAvailability::Disabled.config(), None);

        let available = SshAvailability::from(Some(config()));
        assert!(available.is_enabled());
        assert_eq!(available.config(), Some(&config()));
        assert_eq!(SshAvailability::from(None), SshAvailability::Disabled);
    }

    #[test]
    fn an_endpoint_carries_the_configuration_and_the_address_of_a_running_guest() {
        let vm_id = Uuid::from_u128(0x1234);

        let endpoint = SshEndpoint::new(vm_id, &config(), address()).unwrap();

        assert_eq!(endpoint.username, "user");
        assert_eq!(endpoint.address, address());
        assert_eq!(endpoint.port, SshPort::DEFAULT);
        assert_eq!(endpoint.authentication, SshAuthentication::VmlordKey);
        assert_eq!(endpoint.host_key_alias(), vm_id.to_string());
    }

    /// The endpoint is the last place a stored configuration can be refused
    /// while it is still data rather than the arguments of a process.
    #[test]
    fn a_damaged_configuration_never_becomes_an_endpoint() {
        let damaged = SshConfig {
            username: "root -oProxyCommand=calc".into(),
            ..config()
        };

        assert!(SshEndpoint::new(Uuid::from_u128(1), &damaged, address()).is_err());
    }

    #[test]
    fn an_endpoint_reads_as_the_connection_it_describes() {
        let endpoint = SshEndpoint::new(Uuid::from_u128(1), &config(), address()).unwrap();

        assert_eq!(endpoint.to_string(), "user@172.30.0.5:22 (key)");
    }
}
