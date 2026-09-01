//! What VMLord promises to deliver into a Linux guest, and what it refuses to
//! promise.
//!
//! Installation media and cloud images are not two ways of doing one thing. A
//! local ISO means a human installs the system by hand; a cloud image means
//! cloud-init reads a seed VMLord wrote. Provisioning is meaningful only in the
//! second case, so it lives inside that variant rather than beside it: "a local
//! ISO with a password" is then not a state to be rejected at run time but one
//! that cannot be spelled.

use std::fmt;

use crate::{
    RepositoryError,
    display::DesktopProfile,
    distro::{DistroProfile, SshDaemon},
    ssh::{SshAuthentication, SshConfig, SshPort},
};

/// The longest user name Linux tools accept without complaint.
const MAX_USERNAME_LENGTH: usize = 32;

/// The longest host name a DNS label may carry.
const MAX_VM_NAME_LENGTH: usize = 63;

/// Where a new VM's system comes from.
///
/// The cloud variant is some three hundred bytes larger than the local one,
/// which clippy would rather see boxed. It is not: one of these is built per VM
/// creation and moved a handful of times, so the indirection would buy nothing
/// and would stand between every caller and the fields it reads.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum VmSource {
    /// Installation media. The guest system is installed by hand, so VMLord
    /// promises nothing about the user inside it.
    LocalMedia { path: String },
    /// A cloud image, provisioned by cloud-init from a seed VMLord writes.
    CloudImage {
        image: CloudImage,
        provisioning: Provisioning,
    },
    /// A disk that already holds a system, brought to VMLord's contract by
    /// something other than a seed -- the offline conversion of a guest
    /// imported from AppSandbox.
    ///
    /// It carries a `Provisioning` because the record still has to say which
    /// account VMLord's key went into and on what port the daemon answers, and
    /// an `SshDaemon` because no distribution profile was chosen for it: the
    /// guest was observed rather than selected.
    ExistingDisk {
        path: String,
        provisioning: Provisioning,
        ssh_daemon: SshDaemon,
    },
}

/// Which release of which distribution to boot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloudImage {
    pub profile: DistroProfile,
    pub release: String,
}

/// The configuration cloud-init is asked to apply on the first boot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provisioning {
    pub username: String,
    /// Absent means there is no password at all: the guest is reachable by key
    /// only, and the seed turns password authentication off.
    pub password: Option<Password>,
    pub ssh: SshAccess,
    pub locale: String,
    pub keyboard: String,
    pub timezone: String,
    /// The desktop cloud-init is asked to install, if any.
    ///
    /// Here rather than beside the source, for the reason provisioning itself
    /// is here: a desktop is something a seed installs, and installation media
    /// gets no seed -- so "a local ISO with GNOME" is a state that cannot be
    /// spelled rather than one to be rejected later. Changing it after the VM
    /// exists means installing a desktop into a booted guest, which is its own
    /// task (#127).
    pub desktop: DesktopProfile,
}

/// Whether the guest runs an SSH server, on what port, and whether VMLord puts
/// a key in it.
///
/// The port lives inside `Enabled` rather than beside it, for the reason
/// provisioning itself lives inside `VmSource::CloudImage`: a port for a guest
/// that runs no daemon is not a value to be ignored later but one that cannot
/// be spelled. It is chosen once, when the VM is created -- changing it
/// afterwards means reconfiguring a guest that is already installed, which is
/// its own task (#72).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SshAccess {
    Disabled,
    Enabled { deploy_key: bool, port: SshPort },
}

/// A password in the clear, on its way to being hashed.
///
/// Hashing happens as the seed is built; until then the plaintext travels
/// inside `VmCreateRequest`, which several call sites print with `{:?}`. The
/// manual `Debug` is what makes that harmless, and the absence of `Display`
/// keeps it from being printed by accident in any other way.
#[derive(Clone, PartialEq, Eq)]
pub struct Password(String);

impl Password {
    #[must_use]
    pub fn new(password: impl Into<String>) -> Self {
        Self(password.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Password {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Password(<redacted>)")
    }
}

impl VmSource {
    /// Checks the source, and the provisioning it carries, before any
    /// filesystem or Windows API side effect is attempted.
    pub fn validate(&self) -> Result<(), RepositoryError> {
        match self {
            Self::LocalMedia { path } => {
                if path.trim().is_empty() {
                    return Err(rejected("VM image path must not be empty"));
                }
                Ok(())
            }
            Self::CloudImage {
                image,
                provisioning,
            } => {
                if image.release.trim().is_empty() {
                    return Err(rejected("distribution release must not be empty"));
                }
                provisioning.validate()
            }
            Self::ExistingDisk {
                path, provisioning, ..
            } => {
                if path.trim().is_empty() {
                    return Err(rejected("the disk to adopt must be named"));
                }
                provisioning.validate()
            }
        }
    }
}

impl Provisioning {
    /// Checks the configuration the guest will be asked to apply.
    ///
    /// The rules come from the UI, which used to own them although AGENTS.md
    /// puts business logic outside that layer; they are not duplicated there.
    pub fn validate(&self) -> Result<(), RepositoryError> {
        validate_username(&self.username)?;

        if let Some(password) = &self.password
            && password.as_str().is_empty()
        {
            return Err(rejected(
                "a password must not be empty; leave it unset for a key-only login",
            ));
        }
        if self.password.is_none() && self.ssh == SshAccess::Disabled {
            return Err(rejected(
                "a VM with neither a password nor SSH cannot be logged into",
            ));
        }
        // The two SSH modes are the whole list: VMLord's own key, or the
        // password. An SSH server with neither is a server nobody can get
        // through -- there is no third credential to fall back on.
        if self.password.is_none()
            && matches!(
                self.ssh,
                SshAccess::Enabled {
                    deploy_key: false,
                    ..
                }
            )
        {
            return Err(rejected(
                "an SSH server without a deployed key needs a password to log in with",
            ));
        }

        validate_guest_setting("locale", &self.locale)?;
        validate_guest_setting("keyboard layout", &self.keyboard)?;
        validate_guest_setting("timezone", &self.timezone)?;

        tracing::debug!(
            "provisioning user \"{}\" ({}, {}, {}), password {}, {}",
            self.username,
            self.locale,
            self.keyboard,
            self.timezone,
            if self.password.is_some() {
                "set"
            } else {
                "unset"
            },
            match self.ssh {
                SshAccess::Disabled => "SSH off".to_owned(),
                SshAccess::Enabled {
                    deploy_key: false,
                    port,
                } => format!("SSH on port {port}"),
                SshAccess::Enabled {
                    deploy_key: true,
                    port,
                } => format!("SSH on port {port} with a deployed key"),
            }
        );
        Ok(())
    }

    /// The SSH access this provisioning leaves behind, as it will be recorded
    /// against the VM.
    ///
    /// `None` when the guest runs no SSH server: that is what "SSH is off"
    /// means everywhere afterwards, rather than a flag beside a port. The mode
    /// follows from what the guest is given -- a deployed key, or the password
    /// [`Provisioning::validate`] insists on when there is no key -- and the
    /// port is the one cloud-init is about to configure the daemon with, so
    /// what is stored and what the guest listens on cannot drift apart.
    #[must_use]
    pub fn ssh_config(&self) -> Option<SshConfig> {
        let (authentication, port) = match self.ssh {
            SshAccess::Disabled => return None,
            SshAccess::Enabled {
                deploy_key: true,
                port,
            } => (SshAuthentication::VmlordKey, port),
            SshAccess::Enabled {
                deploy_key: false,
                port,
            } => (SshAuthentication::Password, port),
        };
        Some(SshConfig {
            username: self.username.clone(),
            port,
            authentication,
        })
    }
}

/// The locale, keyboard layout and timezone a new VM is offered before anyone
/// edits them.
///
/// A domain type rather than three strings the UI holds: the form is not
/// allowed to invent what a guest's locale should be, and these three values
/// are read from the host -- which is a Windows lookup, so it belongs to the
/// composition root rather than to the layer drawing the fields.
///
/// [`Default`] is the fallback the epic's decision names: when the host's
/// settings cannot be read, or have no counterpart in the guest, a VM is
/// created in a state that works rather than not created at all. #60 replaces
/// the values VMLord passes in with the ones mapped from Windows; nothing else
/// changes when it does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuestDefaults {
    pub locale: String,
    pub keyboard: String,
    pub timezone: String,
}

impl Default for GuestDefaults {
    fn default() -> Self {
        Self {
            locale: "en_US.UTF-8".into(),
            keyboard: "us".into(),
            timezone: "Etc/UTC".into(),
        }
    }
}

/// Accepts the names Linux accepts as a host name, which is what a VM's name
/// becomes inside the guest.
///
/// Lives here rather than in the create form for the reason the user-name rule
/// does: it is the guest that refuses these names, so the refusal is a fact
/// about the domain and not about the dialog that happens to type them.
pub fn validate_vm_name(name: &str) -> Result<(), RepositoryError> {
    let shaped = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-';

    if name.trim().is_empty() {
        return Err(rejected("the VM name must not be empty"));
    }
    if name.len() > MAX_VM_NAME_LENGTH
        || name.starts_with('-')
        || name.ends_with('-')
        || !name.bytes().all(shaped)
    {
        return Err(rejected(
            "the VM name must be a lowercase Linux host name of up to 63 characters, \
             made of letters, digits and inner hyphens",
        ));
    }
    Ok(())
}

/// Accepts the user names `useradd` accepts and cloud-init passes on unchanged.
///
/// Public because a user name outlives the request that typed it: it is stored
/// with the VM and read back for every SSH connection, and what reaches an
/// `ssh -l` argument has to pass the same rule then as it did in the form.
pub fn validate_username(username: &str) -> Result<(), RepositoryError> {
    let shaped = |(index, byte): (usize, u8)| match index {
        0 => byte.is_ascii_lowercase() || byte == b'_',
        _ => byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-'),
    };

    if username.is_empty()
        || username.len() > MAX_USERNAME_LENGTH
        || !username.bytes().enumerate().all(shaped)
    {
        return Err(rejected(
            "the user name must be a lowercase Linux user name of up to 32 characters, \
             starting with a letter or an underscore",
        ));
    }
    Ok(())
}

/// Refuses a setting that is missing, and one that would break out of the
/// document it is written into.
fn validate_guest_setting(field: &str, value: &str) -> Result<(), RepositoryError> {
    if value.trim().is_empty() {
        return Err(rejected(format!("the {field} must not be empty")));
    }
    if value.chars().any(char::is_control) {
        return Err(rejected(format!(
            "the {field} must not contain control characters"
        )));
    }
    Ok(())
}

/// Builds the error and records the refusal in one place.
///
/// `warn` rather than `error`: a request that fails validation is a person
/// mistyping a field, not the system failing.
fn rejected(message: impl Into<String>) -> RepositoryError {
    let error = RepositoryError::new(message);
    tracing::warn!("rejected VM request: {error}");
    error
}

#[cfg(test)]
mod tests {
    use super::{CloudImage, DesktopProfile, Password, Provisioning, SshAccess, VmSource};
    use crate::{distro::ubuntu, ssh::SshPort};

    fn provisioning() -> Provisioning {
        Provisioning {
            username: "user".into(),
            password: Some(Password::new("secret")),
            ssh: SshAccess::Enabled {
                deploy_key: true,
                port: SshPort::DEFAULT,
            },
            locale: "en_US.UTF-8".into(),
            keyboard: "us".into(),
            timezone: "Europe/Moscow".into(),
            desktop: DesktopProfile::Gnome,
        }
    }

    /// An adopted disk carries what a seed would otherwise have applied,
    /// because the record still has to say which account holds VMLord's key
    /// and on what port the guest's daemon answers.
    #[test]
    fn an_existing_disk_carries_the_provisioning_a_seed_would_have_applied() {
        let source = VmSource::ExistingDisk {
            path: "D:\\vms\\imported\\disk.vhdx".to_owned(),
            provisioning: provisioning(),
            ssh_daemon: ubuntu().ssh,
        };

        source.validate().expect("a valid adoption");
        let VmSource::ExistingDisk { provisioning, .. } = &source else {
            panic!("the variant just built");
        };
        assert_eq!(provisioning.username, "user");
    }

    #[test]
    fn an_adoption_that_names_no_disk_is_refused() {
        let source = VmSource::ExistingDisk {
            path: "   ".to_owned(),
            provisioning: provisioning(),
            ssh_daemon: ubuntu().ssh,
        };

        let error = source.validate().expect_err("refused");
        assert!(error.to_string().contains("disk"), "{error}");
    }

    fn cloud_source() -> VmSource {
        VmSource::CloudImage {
            image: CloudImage {
                profile: ubuntu(),
                release: "24.04".into(),
            },
            provisioning: provisioning(),
        }
    }

    #[test]
    fn a_fully_populated_cloud_source_is_accepted() {
        assert!(cloud_source().validate().is_ok());
    }

    #[test]
    fn local_media_needs_a_path() {
        assert!(
            VmSource::LocalMedia {
                path: "C:\\images\\ubuntu.iso".into()
            }
            .validate()
            .is_ok()
        );
        assert!(
            VmSource::LocalMedia { path: "  ".into() }
                .validate()
                .unwrap_err()
                .to_string()
                .contains("image path")
        );
    }

    #[test]
    fn a_cloud_image_needs_a_release() {
        let source = VmSource::CloudImage {
            image: CloudImage {
                profile: ubuntu(),
                release: " ".into(),
            },
            provisioning: provisioning(),
        };

        assert!(
            source
                .validate()
                .unwrap_err()
                .to_string()
                .contains("release")
        );
    }

    #[test]
    fn a_vm_name_the_guest_can_carry_as_its_host_name_is_kept() {
        for candidate in ["ubuntu", "dev-linux", "vm2", "a"] {
            assert!(
                super::validate_vm_name(candidate).is_ok(),
                "{candidate:?} is a valid host name"
            );
        }
    }

    #[test]
    fn a_vm_name_linux_would_refuse_as_a_host_name_is_rejected() {
        for candidate in [
            "",
            "   ",
            "Dev",
            "dev linux",
            "-dev",
            "dev-",
            "dev_linux",
            "dev.linux",
            "a/b",
            "имя",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            assert!(
                super::validate_vm_name(candidate).is_err(),
                "{candidate:?} must be refused"
            );
        }
    }

    #[test]
    fn a_username_in_the_shape_linux_accepts_is_kept() {
        for candidate in ["user", "_svc", "ubuntu-1", "a", "u_2-x"] {
            let provisioning = Provisioning {
                username: candidate.into(),
                ..provisioning()
            };
            assert!(
                provisioning.validate().is_ok(),
                "{candidate:?} is a valid Linux user name"
            );
        }
    }

    #[test]
    fn a_username_linux_would_refuse_never_reaches_cloud_init() {
        for candidate in [
            "",
            "  ",
            "1user",
            "-user",
            "User",
            "user name",
            "user\n",
            "пользователь",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            let provisioning = Provisioning {
                username: candidate.into(),
                ..provisioning()
            };
            assert!(
                provisioning
                    .validate()
                    .unwrap_err()
                    .to_string()
                    .contains("user name"),
                "{candidate:?} must be refused"
            );
        }
    }

    #[test]
    fn a_password_that_is_present_but_empty_is_a_mistake_not_a_key_only_login() {
        let provisioning = Provisioning {
            password: Some(Password::new("")),
            ..provisioning()
        };

        assert!(
            provisioning
                .validate()
                .unwrap_err()
                .to_string()
                .contains("password")
        );
    }

    #[test]
    fn a_key_only_login_needs_no_password() {
        let provisioning = Provisioning {
            password: None,
            ssh: SshAccess::Enabled {
                deploy_key: true,
                port: SshPort::DEFAULT,
            },
            ..provisioning()
        };

        assert!(provisioning.validate().is_ok());
    }

    #[test]
    fn a_vm_nobody_can_log_into_is_refused() {
        let provisioning = Provisioning {
            password: None,
            ssh: SshAccess::Disabled,
            ..provisioning()
        };

        let message = provisioning.validate().unwrap_err().to_string();
        assert!(message.contains("password"), "got {message:?}");
        assert!(message.contains("SSH"), "got {message:?}");
    }

    #[test]
    fn ssh_without_a_deploy_key_is_a_valid_choice() {
        let provisioning = Provisioning {
            ssh: SshAccess::Enabled {
                deploy_key: false,
                port: SshPort::DEFAULT,
            },
            ..provisioning()
        };

        assert!(provisioning.validate().is_ok());
    }

    /// The epic's range is `1..=65535` and nothing narrower: a port that some
    /// other service on the host happens to use is still a port a guest can
    /// listen on, and a list of "bad" ports would refuse working VMs.
    #[test]
    fn any_port_an_ssh_server_can_listen_on_is_accepted() {
        for candidate in [1_u16, 22, 80, 445, 1080, 3389, 8080, 65535] {
            let provisioning = Provisioning {
                ssh: SshAccess::Enabled {
                    deploy_key: true,
                    port: SshPort::new(candidate).unwrap(),
                },
                ..provisioning()
            };

            assert!(
                provisioning.validate().is_ok(),
                "port {candidate} must not be refused"
            );
        }
    }

    #[test]
    fn an_ssh_server_with_neither_a_key_nor_a_password_is_refused() {
        let provisioning = Provisioning {
            password: None,
            ssh: SshAccess::Enabled {
                deploy_key: false,
                port: SshPort::DEFAULT,
            },
            ..provisioning()
        };

        let message = provisioning.validate().unwrap_err().to_string();
        assert!(message.contains("password"), "got {message:?}");
    }

    #[test]
    fn the_ssh_access_a_guest_is_left_with_is_what_gets_recorded() {
        use crate::ssh::{SshAuthentication, SshConfig};

        let port = SshPort::new(2222).unwrap();
        let chosen = Provisioning {
            ssh: SshAccess::Enabled {
                deploy_key: true,
                port,
            },
            ..provisioning()
        };

        assert_eq!(
            chosen.ssh_config(),
            Some(SshConfig {
                username: "user".into(),
                port,
                authentication: SshAuthentication::VmlordKey,
            }),
            "what is recorded is the port cloud-init is about to configure"
        );
        assert_eq!(
            Provisioning {
                ssh: SshAccess::Enabled {
                    deploy_key: false,
                    port,
                },
                ..provisioning()
            }
            .ssh_config()
            .unwrap()
            .authentication,
            SshAuthentication::Password
        );
        assert_eq!(
            provisioning().ssh_config().unwrap().port,
            SshPort::DEFAULT,
            "a VM nobody chose a port for listens where a guest normally does"
        );
        assert_eq!(
            Provisioning {
                ssh: SshAccess::Disabled,
                ..provisioning()
            }
            .ssh_config(),
            None,
            "SSH being off is the absence of a configuration, not a flag inside one"
        );
    }

    #[test]
    fn a_password_only_login_is_a_valid_choice() {
        let provisioning = Provisioning {
            ssh: SshAccess::Disabled,
            ..provisioning()
        };

        assert!(provisioning.validate().is_ok());
    }

    #[test]
    fn locale_keyboard_and_timezone_must_be_present() {
        for provisioning in [
            Provisioning {
                locale: String::new(),
                ..provisioning()
            },
            Provisioning {
                keyboard: "  ".into(),
                ..provisioning()
            },
            Provisioning {
                timezone: String::new(),
                ..provisioning()
            },
        ] {
            assert!(provisioning.validate().is_err());
        }
    }

    /// These three are written into a YAML document and into
    /// `/etc/default/keyboard`; a newline in one of them is not a typo but an
    /// injection into the document.
    #[test]
    fn no_control_character_reaches_the_cloud_config_document() {
        for provisioning in [
            Provisioning {
                locale: "en_US.UTF-8\nruncmd:".into(),
                ..provisioning()
            },
            Provisioning {
                keyboard: "us\r".into(),
                ..provisioning()
            },
            Provisioning {
                timezone: "Europe/\u{7}Moscow".into(),
                ..provisioning()
            },
        ] {
            assert!(provisioning.validate().is_err());
        }
    }

    #[test]
    fn a_password_never_prints_itself() {
        let password = Password::new("hunter2");

        assert_eq!(format!("{password:?}"), "Password(<redacted>)");
        assert_eq!(password.as_str(), "hunter2");
    }

    #[test]
    fn a_whole_source_never_prints_a_password() {
        assert!(
            !format!("{:?}", cloud_source()).contains("secret"),
            "the request is logged with {{:?}} in several places"
        );
    }
}
