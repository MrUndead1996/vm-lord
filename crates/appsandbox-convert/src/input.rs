//! The document the conversion takes every outside value from.
//!
//! Every name, path and key the conversion acts on comes from here, so that
//! nothing a person chose is ever a literal inside the program.

use std::{fmt, path::PathBuf};

use serde::Deserialize;

use crate::ConvertError;

/// One conversion of one guest.
#[derive(Deserialize)]
pub struct Conversion {
    /// Where the guest's filesystem root is mounted on this machine.
    pub root: PathBuf,
    /// The interactive account VMLord's key goes into.
    pub guest_username: String,
    pub vmlord_public_key: String,
    /// The VM's agent secret, base64 as the host stored it.
    pub agent_secret: String,
    /// The agent binary on *this* machine, copied into the guest.
    pub agent_binary: PathBuf,
    /// What the converted guest calls itself.
    pub hostname: String,
    /// Absent when the guest's daemon is already on the port VMLord recorded.
    pub ssh: Option<SshDropIns>,
}

/// The two drop-ins that move the daemon, named by the distribution profile.
#[derive(Deserialize)]
pub struct SshDropIns {
    pub config_drop_in: String,
    /// Absent where the daemon opens its own port instead of a socket.
    pub socket_drop_in: Option<String>,
    pub port: u16,
}

impl Conversion {
    /// Reads a document, refusing anything it cannot name a value for.
    pub fn from_json(document: &str) -> Result<Self, ConvertError> {
        serde_json::from_str(document).map_err(|error| {
            ConvertError::new(format!("the conversion document is not one: {error}"))
        })
    }
}

/// Everything but the secret, which has no rendering that shows it.
impl fmt::Debug for Conversion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Conversion")
            .field("root", &self.root)
            .field("guest_username", &self.guest_username)
            .field("hostname", &self.hostname)
            .field("agent_binary", &self.agent_binary)
            .field("agent_secret", &"<redacted>")
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::Conversion;

    const DOCUMENT: &str = r#"{
        "root": "/mnt/guest",
        "guest_username": "agromov",
        "vmlord_public_key": "ssh-ed25519 AAAAC3Nz vmlord",
        "agent_secret": "c2VjcmV0",
        "agent_binary": "/tmp/vmlord-agent",
        "hostname": "ubuntu",
        "ssh": { "config_drop_in": "/etc/ssh/sshd_config.d/10-vmlord.conf",
                 "socket_drop_in": "/etc/systemd/system/ssh.socket.d/10-vmlord.conf",
                 "port": 2222 }
    }"#;

    #[test]
    fn a_document_names_every_value_the_conversion_takes_from_outside() {
        let conversion = Conversion::from_json(DOCUMENT).expect("a valid document");
        assert_eq!(conversion.guest_username, "agromov");
        assert_eq!(conversion.hostname, "ubuntu");
        assert_eq!(conversion.ssh.as_ref().expect("ssh").port, 2222);
    }

    #[test]
    fn a_document_without_ssh_is_a_guest_vmlord_leaves_the_daemon_alone_in() {
        let document = DOCUMENT.replace(
            r#""ssh": { "config_drop_in": "/etc/ssh/sshd_config.d/10-vmlord.conf",
                 "socket_drop_in": "/etc/systemd/system/ssh.socket.d/10-vmlord.conf",
                 "port": 2222 }"#,
            r#""ssh": null"#,
        );
        assert!(
            Conversion::from_json(&document)
                .expect("valid")
                .ssh
                .is_none()
        );
    }

    #[test]
    fn a_missing_field_is_refused_rather_than_defaulted() {
        let error = Conversion::from_json(r#"{"root": "/mnt/guest"}"#).expect_err("refused");
        assert!(error.to_string().contains("guest_username"), "{error}");
    }

    #[test]
    fn the_secret_is_not_in_the_documents_debug_rendering() {
        let conversion = Conversion::from_json(DOCUMENT).expect("a valid document");
        assert!(!format!("{conversion:?}").contains("c2VjcmV0"));
    }
}
