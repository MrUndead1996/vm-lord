//! Turning a copied AppSandbox Linux guest into a VMLord guest, with nothing
//! running from the disk it is on.
//!
//! The conversion is a function over a mounted filesystem root: it knows
//! nothing of VHDX, of Windows or of how the root came to be mounted. That is
//! what lets the same code run under WSL today and inside a service VM later,
//! and what lets every one of its tests run against a directory tree.

mod input;
mod root;

use std::fmt;

pub use input::{Conversion, SshDropIns};

/// A refusal, or a step that could not be completed.
pub struct ConvertError(String);

impl ConvertError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        let error = Self(message.into());
        tracing::error!("{error}");
        error
    }
}

impl fmt::Display for ConvertError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl fmt::Debug for ConvertError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl std::error::Error for ConvertError {}

mod facts;
#[cfg(test)]
mod fixture;
mod install;
mod remove;
mod verify;

pub use verify::verify;

pub use verify::ldconfig::{LdconfigRunner, system as system_ldconfig};

/// Converts a mounted AppSandbox guest into a VMLord guest.
///
/// The order is the whole of the safety: the guest is refused before anything
/// is written, VMLord's own is installed before AppSandbox's is taken away,
/// and the root is read back afterwards rather than reported from what the
/// steps believed.
pub fn convert(conversion: &Conversion, ldconfig: &LdconfigRunner) -> Result<(), ConvertError> {
    tracing::info!("converting the guest at {}", conversion.root.display());
    let facts = facts::read(conversion)?;
    install::run(conversion, &facts)?;
    remove::run(conversion)?;
    verify::ldconfig::run(&conversion.root, ldconfig);
    verify(conversion)?;
    tracing::info!("converted the guest at {}", conversion.root.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{LdconfigRunner, convert, verify};
    use crate::fixture::AppSandboxGuest;

    fn quiet_ldconfig() -> LdconfigRunner {
        Box::new(|_| Ok(()))
    }

    #[test]
    fn a_converted_guest_verifies() {
        let guest = AppSandboxGuest::new();
        convert(&guest.conversion(), &quiet_ldconfig()).expect("converted");
        verify(&guest.conversion()).expect("verified");
    }

    #[test]
    fn a_conversion_refused_by_the_preconditions_changes_nothing() {
        let guest = AppSandboxGuest::new().without("/etc/os-release");
        assert!(convert(&guest.conversion(), &quiet_ldconfig()).is_err());
        assert!(
            guest
                .root()
                .join("etc/systemd/system/appsandbox-agent.service")
                .exists(),
            "a refused conversion removed something anyway"
        );
    }

    #[test]
    fn the_secret_never_reaches_an_error() {
        let guest = AppSandboxGuest::new();
        let mut conversion = guest.conversion();
        conversion.guest_username = "nobody-here".to_owned();
        let error = convert(&conversion, &quiet_ldconfig()).expect_err("refused");
        assert!(!error.to_string().contains("c2VjcmV0"), "{error}");
    }
}
