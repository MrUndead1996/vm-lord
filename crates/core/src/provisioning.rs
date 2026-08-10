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

use crate::{RepositoryError, distro::DistroProfile};

/// The longest user name Linux tools accept without complaint.
const MAX_USERNAME_LENGTH: usize = 32;

/// Where a new VM's system comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VmSource {
    /// Installation media. The guest system is installed by hand, so VMLord
    /// promises nothing about the user inside it.
    LocalMedia { path: String },
    /// A cloud image, provisioned by cloud-init from a seed VMLord writes.
    CloudImage {
        image: CloudImage,
        provisioning: Provisioning,
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
}

/// Whether the guest runs an SSH server, and whether VMLord puts a key in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SshAccess {
    Disabled,
    Enabled { deploy_key: bool },
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

        validate_guest_setting("locale", &self.locale)?;
        validate_guest_setting("keyboard layout", &self.keyboard)?;
        validate_guest_setting("timezone", &self.timezone)?;

        log::debug!(
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
                SshAccess::Disabled => "SSH off",
                SshAccess::Enabled { deploy_key: false } => "SSH on",
                SshAccess::Enabled { deploy_key: true } => "SSH on with a deployed key",
            }
        );
        Ok(())
    }
}

/// Accepts the user names `useradd` accepts and cloud-init passes on unchanged.
fn validate_username(username: &str) -> Result<(), RepositoryError> {
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
    log::warn!("rejected VM request: {error}");
    error
}

#[cfg(test)]
mod tests {
    use super::{CloudImage, Password, Provisioning, SshAccess, VmSource};
    use crate::distro::ubuntu;

    fn provisioning() -> Provisioning {
        Provisioning {
            username: "user".into(),
            password: Some(Password::new("secret")),
            ssh: SshAccess::Enabled { deploy_key: true },
            locale: "en_US.UTF-8".into(),
            keyboard: "us".into(),
            timezone: "Europe/Moscow".into(),
        }
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
            ssh: SshAccess::Enabled { deploy_key: true },
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
            ssh: SshAccess::Enabled { deploy_key: false },
            ..provisioning()
        };

        assert!(provisioning.validate().is_ok());
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
