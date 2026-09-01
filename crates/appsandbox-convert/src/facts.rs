//! What the guest itself says, read off the disk rather than asked of a
//! running system.

use std::path::PathBuf;

use crate::{Conversion, ConvertError, root::guest_path};

/// Which of the two renderers manages the guest's interface.
///
/// A netplan naming the one that is not running makes the one that is stop
/// managing the interface entirely, so this is read rather than assumed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Renderer {
    NetworkManager,
    Networkd,
}

impl Renderer {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::NetworkManager => "NetworkManager",
            Self::Networkd => "networkd",
        }
    }
}

/// Nothing here is a secret -- an account's home, its ids and which renderer
/// is running -- so these are readable in a record.
#[derive(Debug)]
pub(crate) struct GuestFacts {
    pub(crate) renderer: Renderer,
    pub(crate) home: PathBuf,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
}

/// The evidence that a guest is one this conversion can act on.
const APPSANDBOX_EVIDENCE: [&str; 2] = [
    "/opt/appsandbox/appsandbox-gpu",
    "/etc/systemd/system/appsandbox-agent.service",
];

pub(crate) fn read(conversion: &Conversion) -> Result<GuestFacts, ConvertError> {
    let root = &conversion.root;

    let release = guest_path(root, "/etc/os-release")?;
    let release = std::fs::read_to_string(&release).map_err(|error| {
        ConvertError::new(format!("{} could not be read: {error}", release.display()))
    })?;
    if !release.lines().any(|line| line.trim() == "ID=ubuntu") {
        return Err(ConvertError::new(
            "the root's os-release does not name Ubuntu",
        ));
    }

    if !APPSANDBOX_EVIDENCE
        .iter()
        .any(|path| guest_path(root, path).is_ok_and(|path| path.exists()))
    {
        return Err(ConvertError::new(
            "the root holds nothing of AppSandbox's: it is not a guest this converts",
        ));
    }

    if guest_path(root, "/etc/systemd/system/vmlord-agent.service")?.exists() {
        return Err(ConvertError::new(
            "the root already has a vmlord-agent unit: it has been converted before",
        ));
    }

    let (home, uid, gid) = account(conversion)?;
    let renderer = if guest_path(
        root,
        "/etc/systemd/system/multi-user.target.wants/NetworkManager.service",
    )?
    .symlink_metadata()
    .is_ok()
    {
        Renderer::NetworkManager
    } else {
        Renderer::Networkd
    };

    Ok(GuestFacts {
        renderer,
        home: root.join(home.trim_start_matches('/')),
        uid,
        gid,
    })
}

/// The named account's home under the root, for a pass that needs only that.
pub(crate) fn home_of(conversion: &Conversion) -> Result<PathBuf, ConvertError> {
    let (home, _, _) = account(conversion)?;
    Ok(conversion.root.join(home.trim_start_matches('/')))
}

/// The named account's home, uid and gid, out of the guest's own `passwd`.
fn account(conversion: &Conversion) -> Result<(String, u32, u32), ConvertError> {
    let path = guest_path(&conversion.root, "/etc/passwd")?;
    let passwd = std::fs::read_to_string(&path).map_err(|error| {
        ConvertError::new(format!("{} could not be read: {error}", path.display()))
    })?;
    for line in passwd.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() >= 6 && fields[0] == conversion.guest_username {
            let uid = fields[2].parse().map_err(|_| {
                ConvertError::new(format!(
                    "{}'s uid is not a number",
                    conversion.guest_username
                ))
            })?;
            let gid = fields[3].parse().map_err(|_| {
                ConvertError::new(format!(
                    "{}'s gid is not a number",
                    conversion.guest_username
                ))
            })?;
            return Ok((fields[5].to_owned(), uid, gid));
        }
    }
    Err(ConvertError::new(format!(
        "the guest has no account named {}",
        conversion.guest_username
    )))
}

#[cfg(test)]
mod tests {
    use super::{Renderer, read};
    use crate::fixture::AppSandboxGuest;

    #[test]
    fn an_appsandbox_guest_yields_the_account_the_key_goes_to() {
        let guest = AppSandboxGuest::new();
        let facts = read(&guest.conversion()).expect("an AppSandbox guest");
        assert_eq!(facts.home, guest.root().join("home/agromov"));
        assert_eq!(facts.uid, 1000);
        assert_eq!(facts.gid, 1000);
    }

    #[test]
    fn a_guest_with_no_network_manager_unit_is_rendered_by_networkd() {
        let guest = AppSandboxGuest::new();
        assert_eq!(
            read(&guest.conversion()).expect("facts").renderer,
            Renderer::Networkd
        );
    }

    #[test]
    fn a_guest_whose_network_manager_is_enabled_is_rendered_by_it() {
        let guest = AppSandboxGuest::new().with_network_manager();
        assert_eq!(
            read(&guest.conversion()).expect("facts").renderer,
            Renderer::NetworkManager
        );
    }

    #[test]
    fn a_root_that_is_not_ubuntu_is_refused() {
        let guest = AppSandboxGuest::new().without("/etc/os-release");
        let error = read(&guest.conversion()).expect_err("refused");
        assert!(error.to_string().contains("os-release"), "{error}");
    }

    #[test]
    fn a_root_with_nothing_of_appsandbox_in_it_is_refused() {
        let guest = AppSandboxGuest::new()
            .without("/opt/appsandbox/appsandbox-gpu")
            .without("/etc/systemd/system/appsandbox-agent.service");
        let error = read(&guest.conversion()).expect_err("refused");
        assert!(error.to_string().contains("AppSandbox"), "{error}");
    }

    #[test]
    fn a_named_account_that_is_not_in_the_guest_is_refused() {
        let guest = AppSandboxGuest::new();
        let mut conversion = guest.conversion();
        conversion.guest_username = "nobody-here".to_owned();
        let error = read(&conversion).expect_err("refused");
        assert!(error.to_string().contains("nobody-here"), "{error}");
    }

    #[test]
    fn a_guest_vmlord_has_already_converted_is_refused() {
        let guest = AppSandboxGuest::new();
        std::fs::write(
            guest.root().join("etc/systemd/system/vmlord-agent.service"),
            "[Unit]\n",
        )
        .expect("write");
        let error = read(&guest.conversion()).expect_err("refused");
        assert!(error.to_string().contains("vmlord-agent"), "{error}");
    }
}
