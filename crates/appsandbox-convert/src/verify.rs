//! Reading the converted root back.
//!
//! A separate pass over the same disk rather than a tally the conversion kept:
//! what the steps believed they wrote is not evidence, and this pass can be
//! run on its own against a root converted at some other time.

use std::{fs, os::unix::fs::PermissionsExt, path::Path};

use vmlord_agent_protocol::auth::GUEST_SECRET_PATH;
use vmlord_seed::{AGENT_BINARY_PATH, AGENT_UNIT_NAME, AGENT_UNIT_PATH};

use crate::{
    Conversion, ConvertError,
    install::NETPLAN_PATH,
    remove::{FILES, TREES, UNITS},
    root::guest_path,
};

/// Rebuilding the linker cache, which naming deleted directories invalidated.
pub(crate) mod ldconfig {
    use std::{path::Path, process::Command};

    /// How the cache is rebuilt.
    ///
    /// A seam rather than a direct call, so that the tests can watch what the
    /// conversion asks for without an `ldconfig` on the machine running them
    /// -- and so that a root mounted somewhere else can be handed a different
    /// way of reaching one.
    pub type LdconfigRunner = Box<dyn Fn(&Path) -> Result<(), String> + Send + Sync>;

    #[must_use]
    pub fn system() -> LdconfigRunner {
        Box::new(|root| {
            let output = Command::new("ldconfig")
                .arg("-r")
                .arg(root)
                .output()
                .map_err(|error| error.to_string())?;
            if output.status.success() {
                Ok(())
            } else {
                Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
            }
        })
    }

    /// A cache that cannot be rebuilt is reported and not fatal: the guest
    /// rebuilds it on its next boot, and refusing the whole conversion over it
    /// would leave a guest that is otherwise converted.
    pub fn run(root: &Path, runner: &LdconfigRunner) {
        match runner(root) {
            Ok(()) => tracing::info!("rebuilt the linker cache under {}", root.display()),
            Err(error) => tracing::warn!(
                "the linker cache under {} was not rebuilt: {error}",
                root.display()
            ),
        }
    }
}

/// Reads the converted root back and reports the first thing that is not as
/// the conversion leaves it.
pub fn verify(conversion: &Conversion) -> Result<(), ConvertError> {
    let root = &conversion.root;

    let installed: [(&str, u32); 3] = [
        (AGENT_BINARY_PATH, 0o755),
        (GUEST_SECRET_PATH, 0o600),
        (AGENT_UNIT_PATH, 0o644),
    ];
    for (path, mode) in installed {
        let file = guest_path(root, path)?;
        let metadata = fs::metadata(&file)
            .map_err(|error| ConvertError::new(format!("{path} is not installed: {error}")))?;
        if metadata.permissions().mode() & 0o777 != mode {
            return Err(ConvertError::new(format!(
                "{path} does not have the permissions the conversion installs it with"
            )));
        }
    }

    let link =
        guest_path(root, "/etc/systemd/system/multi-user.target.wants")?.join(AGENT_UNIT_NAME);
    if fs::read_link(&link).ok().as_deref() != Some(Path::new(AGENT_UNIT_PATH)) {
        return Err(ConvertError::new(format!(
            "{AGENT_UNIT_NAME} is not enabled: {} does not point at it",
            link.display()
        )));
    }

    let netplan = guest_path(root, NETPLAN_PATH)?;
    let document = fs::read_to_string(&netplan)
        .map_err(|error| ConvertError::new(format!("{NETPLAN_PATH} is not there: {error}")))?;
    if !document.contains("dhcp4: true") {
        return Err(ConvertError::new(format!(
            "{NETPLAN_PATH} does not ask for an address"
        )));
    }
    if document.contains("$RENDERER") {
        return Err(ConvertError::new(format!(
            "{NETPLAN_PATH} never had its renderer filled in"
        )));
    }

    let keys = crate::facts::home_of(conversion)?.join(".ssh/authorized_keys");
    let authorized = fs::read_to_string(&keys)
        .map_err(|error| ConvertError::new(format!("{} is not there: {error}", keys.display())))?;
    if !authorized
        .lines()
        .any(|line| line == conversion.vmlord_public_key)
    {
        return Err(ConvertError::new(format!(
            "VMLord's key is not in {}",
            keys.display()
        )));
    }

    for unit in UNITS {
        for directory in ["/etc/systemd/system", "/etc/systemd/user"] {
            if guest_path(root, directory)?.join(unit).exists() {
                return Err(ConvertError::new(format!("{unit} is still installed")));
            }
        }
        let want = guest_path(root, "/etc/systemd/system/multi-user.target.wants")?.join(unit);
        if want.symlink_metadata().is_ok() {
            return Err(ConvertError::new(format!("{unit} is still enabled")));
        }
    }

    for file in FILES {
        if guest_path(root, file)?.exists() {
            return Err(ConvertError::new(format!("{file} is still there")));
        }
    }
    for tree in TREES {
        if guest_path(root, tree)?.exists() {
            return Err(ConvertError::new(format!("{tree} is still there")));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::{Arc, Mutex},
    };

    use super::{ldconfig, verify};
    use crate::{facts, fixture::AppSandboxGuest, install, remove};

    fn fully_converted() -> AppSandboxGuest {
        let guest = AppSandboxGuest::new();
        let conversion = guest.conversion();
        let facts = facts::read(&conversion).expect("facts");
        install::run(&conversion, &facts).expect("installed");
        remove::run(&conversion).expect("removed");
        guest
    }

    #[test]
    fn a_converted_root_passes() {
        let guest = fully_converted();
        verify(&guest.conversion()).expect("a converted guest");
    }

    #[test]
    fn an_unconverted_root_fails() {
        let guest = AppSandboxGuest::new();
        let error = verify(&guest.conversion()).expect_err("not converted");
        assert!(error.to_string().contains("vmlord-agent"), "{error}");
    }

    #[test]
    fn a_root_where_one_appsandbox_unit_came_back_fails() {
        let guest = fully_converted();
        fs::write(
            guest
                .root()
                .join("etc/systemd/system/appsandbox-input.service"),
            "[Unit]\n",
        )
        .expect("write");
        let error = verify(&guest.conversion()).expect_err("still AppSandbox's");
        assert!(
            error.to_string().contains("appsandbox-input.service"),
            "{error}"
        );
    }

    #[test]
    fn a_root_whose_secret_is_readable_by_more_than_root_fails() {
        use std::os::unix::fs::PermissionsExt;
        let guest = fully_converted();
        fs::set_permissions(
            guest.root().join("etc/vmlord/agent.secret"),
            fs::Permissions::from_mode(0o644),
        )
        .expect("chmod");
        let error = verify(&guest.conversion()).expect_err("too readable");
        assert!(error.to_string().contains("agent.secret"), "{error}");
    }

    #[test]
    fn a_root_whose_netplan_asks_for_no_address_fails() {
        let guest = fully_converted();
        fs::write(
            guest.root().join("etc/netplan/90-vmlord.yaml"),
            "network:\n",
        )
        .expect("write");
        let error = verify(&guest.conversion()).expect_err("no address");
        assert!(error.to_string().contains("90-vmlord.yaml"), "{error}");
    }

    #[test]
    fn a_root_without_vmlords_key_in_it_fails() {
        let guest = fully_converted();
        fs::write(
            guest.root().join("home/agromov/.ssh/authorized_keys"),
            "ssh-ed25519 AAAAC3Nz somebody-else\n",
        )
        .expect("write");
        let error = verify(&guest.conversion()).expect_err("not VMLord's key");
        assert!(error.to_string().contains("authorized_keys"), "{error}");
    }

    #[test]
    fn the_linker_cache_is_rebuilt_against_the_root_it_was_given() {
        let guest = AppSandboxGuest::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = seen.clone();
        let runner: ldconfig::LdconfigRunner = Box::new(move |root| {
            recorder.lock().expect("lock").push(root.to_path_buf());
            Ok(())
        });
        ldconfig::run(guest.root(), &runner);
        assert_eq!(
            seen.lock().expect("lock").as_slice(),
            [guest.root().to_path_buf()]
        );
    }

    #[test]
    fn a_linker_cache_that_cannot_be_rebuilt_does_not_fail_the_conversion() {
        let guest = AppSandboxGuest::new();
        let runner: ldconfig::LdconfigRunner = Box::new(|_| Err("no ldconfig here".to_owned()));
        ldconfig::run(guest.root(), &runner);
    }
}
