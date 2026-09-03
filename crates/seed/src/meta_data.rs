//! `meta-data`: who this instance is, and what it calls itself.

use crate::{SeedRequest, scalar};

/// Prints the document.
///
/// `instance-id` comes from the VM's identifier and never changes: the seed
/// stays attached for the life of the VM, and cloud-init reads it on every
/// boot, so a changing identifier would re-run the per-instance modules and
/// re-create the user on each start.
pub(crate) fn render(request: &SeedRequest<'_>) -> String {
    format!(
        "instance-id: {}\nlocal-hostname: {}\n",
        scalar::yaml(request.instance_id),
        scalar::yaml(request.vm_name),
    )
}

#[cfg(test)]
mod tests {
    use super::render;
    use crate::{SeedRequest, UBUNTU_KEYBOARD, UBUNTU_SSH};
    use serde_yaml_ng::Value;
    use vmlord_core::{PackageRefresh, SshAccess, SshPort};

    fn request() -> SeedRequest<'static> {
        SeedRequest {
            vm_name: "my-vm",
            instance_id: "vmlord-4f1c0e5a",
            username: "dev",
            password_hash: Some("$6$rounds=4096$salt$hash"),
            authorized_key: Some("ssh-ed25519 AAAAC3Nz vmlord"),
            ssh: SshAccess::Enabled {
                deploy_key: true,
                port: SshPort::DEFAULT,
            },
            locale: "en_US.UTF-8",
            keyboard: "us",
            keyboard_files: &UBUNTU_KEYBOARD,
            timezone: "Europe/Moscow",
            admin_group: "sudo",
            ssh_daemon: &UBUNTU_SSH,
            agent_secret: None,
            desktop_packages: &[],
            package_refresh: PackageRefresh::Lists,
        }
    }

    fn parsed(document: &str) -> Value {
        serde_yaml_ng::from_str(document).expect("cloud-init reads this with a YAML parser too")
    }

    #[test]
    fn meta_data_carries_the_instance_id_and_the_hostname() {
        let document = parsed(&render(&request()));

        assert_eq!(document["instance-id"], Value::from("vmlord-4f1c0e5a"));
        assert_eq!(document["local-hostname"], Value::from("my-vm"));
    }

    /// A VM name is only checked for emptiness in the domain, so the quoting
    /// is what keeps a name from becoming structure. YAML folds a newline
    /// inside a quoted scalar into a space, so the value arrives folded rather
    /// than byte-identical -- what matters is that it arrives as one value and
    /// the document still has exactly two keys.
    #[test]
    fn a_hostile_name_stays_inside_its_scalar() {
        let document = parsed(&render(&SeedRequest {
            vm_name: "vm'\nruncmd: ['reboot']",
            ..request()
        }));

        assert_eq!(
            document["local-hostname"],
            Value::from("vm' runcmd: ['reboot']")
        );
        assert_eq!(document.as_mapping().expect("a mapping").len(), 2);
    }
}
