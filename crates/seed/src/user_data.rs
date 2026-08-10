//! `user-data`: what cloud-init is asked to do on the first boot.
//!
//! Printed by hand rather than serialised. The document is small and fixed, and
//! what it must be is known exactly -- including the `#cloud-config` line, which
//! is a comment to YAML and the format marker to cloud-init.

use crate::{SeedRequest, scalar};

/// The indentation a block scalar's content sits at inside `write_files`.
const FILE_INDENT: &str = "      ";

/// Prints the document.
pub(crate) fn render(request: &SeedRequest<'_>) -> String {
    let mut document = String::from("#cloud-config\nusers:\n");

    document.push_str(&format!("  - name: {}\n", scalar::yaml(request.username)));
    document.push_str("    shell: '/bin/bash'\n");
    document.push_str(&format!(
        "    groups: [{}]\n",
        scalar::yaml(request.admin_group)
    ));
    // cloud-init writes this into /etc/sudoers.d itself, so the rule holds
    // whatever the administrative group is called. It never asks for a
    // password: a key-only login has none to give.
    document.push_str("    sudo: 'ALL=(ALL) NOPASSWD:ALL'\n");
    match request.password_hash {
        Some(hash) => {
            document.push_str("    lock_passwd: false\n");
            document.push_str(&format!("    hashed_passwd: {}\n", scalar::yaml(hash)));
        }
        None => document.push_str("    lock_passwd: true\n"),
    }
    if let Some(key) = request.authorized_key {
        document.push_str("    ssh_authorized_keys:\n");
        document.push_str(&format!("      - {}\n", scalar::yaml(key)));
    }

    document.push_str(&format!("ssh_pwauth: {}\n", password_login_allowed(request)));
    document.push_str(&format!("locale: {}\n", scalar::yaml(request.locale)));
    document.push_str(&format!("timezone: {}\n", scalar::yaml(request.timezone)));
    document.push_str(&keyboard_file(request.keyboard));
    document.push_str("growpart:\n  mode: auto\n  devices: ['/']\nresize_rootfs: true\n");

    document
}

/// Whether the SSH daemon accepts a password.
///
/// Both halves matter: without a hash there is no password to accept, and with
/// SSH off the setting has nobody to apply to.
fn password_login_allowed(request: &SeedRequest<'_>) -> bool {
    matches!(request.ssh, vmlord_core::SshAccess::Enabled { .. }) && request.password_hash.is_some()
}

/// The `write_files` entry that sets the console keyboard layout.
///
/// `/etc/default/keyboard` is Debian-family: Fedora keeps the same setting in
/// `/etc/vconsole.conf` under different keys, which is a different mechanism
/// rather than a different value, so it waits for a second distribution.
///
/// The layout is escaped for the shell, not for YAML: this file is read with
/// `source`, where an unescaped `$` or quote is code.
fn keyboard_file(layout: &str) -> String {
    let layout = scalar::shell(layout);
    format!(
        "write_files:\n  - path: '/etc/default/keyboard'\n    permissions: '0644'\n    content: |\n\
         {FILE_INDENT}XKBMODEL=\"pc105\"\n\
         {FILE_INDENT}XKBLAYOUT=\"{layout}\"\n\
         {FILE_INDENT}XKBVARIANT=\"\"\n\
         {FILE_INDENT}XKBOPTIONS=\"\"\n\
         {FILE_INDENT}BACKSPACE=\"guess\"\n"
    )
}

#[cfg(test)]
mod tests {
    use super::render;
    use crate::SeedRequest;
    use serde_yaml_ng::Value;
    use vmlord_core::SshAccess;

    const HASH: &str = "$6$rounds=4096$salt$hash";
    const KEY: &str = "ssh-ed25519 AAAAC3Nz vmlord";

    fn request() -> SeedRequest<'static> {
        SeedRequest {
            vm_name: "my-vm",
            instance_id: "vmlord-4f1c0e5a",
            username: "dev",
            password_hash: Some(HASH),
            authorized_key: Some(KEY),
            ssh: SshAccess::Enabled { deploy_key: true },
            locale: "en_US.UTF-8",
            keyboard: "us",
            timezone: "Europe/Moscow",
            admin_group: "sudo",
            ssh_units: &[],
        }
    }

    fn parsed(document: &str) -> Value {
        serde_yaml_ng::from_str(document).expect("cloud-init reads this with a YAML parser too")
    }

    /// cloud-init recognises the format by this line, and a YAML parser reads
    /// it as a comment -- so nothing but a byte comparison can check it.
    #[test]
    fn the_first_line_is_the_marker_cloud_init_looks_for() {
        assert!(render(&request()).starts_with("#cloud-config\n"));
    }

    #[test]
    fn the_user_is_created_with_a_password_a_key_and_administrative_rights() {
        let document = parsed(&render(&request()));
        let user = &document["users"][0];

        assert_eq!(user["name"], Value::from("dev"));
        assert_eq!(user["shell"], Value::from("/bin/bash"));
        assert_eq!(user["groups"], Value::from(vec!["sudo"]));
        assert_eq!(user["sudo"], Value::from("ALL=(ALL) NOPASSWD:ALL"));
        assert_eq!(user["hashed_passwd"], Value::from(HASH));
        assert_eq!(user["lock_passwd"], Value::from(false));
        assert_eq!(user["ssh_authorized_keys"], Value::from(vec![KEY]));
        assert_eq!(document["ssh_pwauth"], Value::from(true));
    }

    /// The group comes from the distribution profile, so a profile naming
    /// `wheel` must reach the document unchanged.
    #[test]
    fn the_administrative_group_comes_from_the_profile() {
        let document = parsed(&render(&SeedRequest {
            admin_group: "wheel",
            ..request()
        }));

        assert_eq!(document["users"][0]["groups"], Value::from(vec!["wheel"]));
    }

    #[test]
    fn the_guest_gets_the_locale_and_the_timezone_it_was_asked_for() {
        let document = parsed(&render(&request()));

        assert_eq!(document["locale"], Value::from("en_US.UTF-8"));
        assert_eq!(document["timezone"], Value::from("Europe/Moscow"));
    }

    #[test]
    fn the_keyboard_layout_is_written_into_the_debian_configuration_file() {
        let document = parsed(&render(&SeedRequest {
            keyboard: "ru",
            ..request()
        }));
        let file = &document["write_files"][0];

        assert_eq!(file["path"], Value::from("/etc/default/keyboard"));
        assert_eq!(file["permissions"], Value::from("0644"));
        assert_eq!(
            file["content"],
            Value::from(
                "XKBMODEL=\"pc105\"\nXKBLAYOUT=\"ru\"\nXKBVARIANT=\"\"\n\
                 XKBOPTIONS=\"\"\nBACKSPACE=\"guess\"\n"
            )
        );
    }

    /// The file is read with `source`, so the layout is escaped for the shell
    /// on top of being inside a YAML block scalar.
    #[test]
    fn a_layout_that_would_run_a_command_arrives_escaped() {
        let document = parsed(&render(&SeedRequest {
            keyboard: "us$(reboot)",
            ..request()
        }));

        assert!(
            document["write_files"][0]["content"]
                .as_str()
                .expect("the file content is a string")
                .contains("XKBLAYOUT=\"us\\$(reboot)\"")
        );
    }

    /// Growing the root filesystem to `disk_gb` is a VMLord promise, so it is
    /// stated in the document rather than left to cloud-init's defaults.
    #[test]
    fn the_root_filesystem_is_grown_to_fill_the_disk() {
        let document = parsed(&render(&request()));

        assert_eq!(document["growpart"]["mode"], Value::from("auto"));
        assert_eq!(document["growpart"]["devices"], Value::from(vec!["/"]));
        assert_eq!(document["resize_rootfs"], Value::from(true));
    }

    /// A value with an apostrophe is the one that breaks naive quoting.
    #[test]
    fn a_value_with_an_apostrophe_survives_the_round_trip() {
        let document = parsed(&render(&SeedRequest {
            timezone: "Europe/O'Hare",
            ..request()
        }));

        assert_eq!(document["timezone"], Value::from("Europe/O'Hare"));
    }
}
