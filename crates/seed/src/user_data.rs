//! `user-data`: what cloud-init is asked to do on the first boot.
//!
//! Printed by hand rather than serialised. The document is small and fixed, and
//! what it must be is known exactly -- including the `#cloud-config` line, which
//! is a comment to YAML and the format marker to cloud-init.

use vmlord_core::{SshAccess, SshPort};

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

    document.push_str(&format!(
        "ssh_pwauth: {}\n",
        password_login_allowed(request)
    ));
    document.push_str(&format!("locale: {}\n", scalar::yaml(request.locale)));
    document.push_str(&format!("timezone: {}\n", scalar::yaml(request.timezone)));
    document.push_str(&write_files(request));
    document.push_str("growpart:\n  mode: auto\n  devices: ['/']\nresize_rootfs: true\n");
    document.push_str(&runcmd(request));

    document
}

/// Every file the first boot writes, as one `write_files` block.
///
/// One block because YAML has one: a second `write_files` key later in the
/// document would not add to the first, it would replace it.
fn write_files(request: &SeedRequest<'_>) -> String {
    let mut files = format!(
        "write_files:\n{}",
        file(KEYBOARD_PATH, &keyboard_settings(request.keyboard))
    );

    if let SshAccess::Enabled { port, .. } = request.ssh {
        files.push_str(&file(
            &request.ssh_daemon.config_drop_in,
            &format!("Port {port}\n"),
        ));
        if let Some(path) = &request.ssh_daemon.socket_drop_in {
            files.push_str(&file(path, &socket_settings(port)));
        }
    }
    files
}

/// The commands the first boot runs, as one `runcmd` block, or nothing when
/// there are none.
///
/// SSH is the only thing here, and it is one of two opposite jobs: switching
/// the daemon off, or moving it to the port the VM was created with.
fn runcmd(request: &SeedRequest<'_>) -> String {
    let commands = match request.ssh {
        SshAccess::Disabled => disable_ssh(request),
        SshAccess::Enabled { .. } => apply_ssh_configuration(request),
    };
    if commands.is_empty() {
        return String::new();
    }

    let mut block = String::from("runcmd:\n");
    for command in commands {
        block.push_str(&format!(
            "  - [{}]\n",
            command
                .iter()
                .map(|word| scalar::yaml(word))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    block
}

/// Stops the SSH daemon and keeps it stopped.
///
/// The unit names come from the profile rather than from here: Debian-family
/// systems socket-activate `ssh.socket`, Fedora and SUSE name both `sshd`. A
/// unit that does not exist on a given release makes `systemctl` return
/// non-zero, which `runcmd` does not treat as fatal.
fn disable_ssh(request: &SeedRequest<'_>) -> Vec<Vec<String>> {
    let units = &request.ssh_daemon.units;
    if units.is_empty() {
        return Vec::new();
    }

    log::debug!("the seed disables the SSH daemon: {}", units.join(", "));
    let mut command = vec![
        "systemctl".to_owned(),
        "disable".to_owned(),
        "--now".to_owned(),
    ];
    command.extend(units.iter().cloned());
    vec![command]
}

/// Makes the daemon read the drop-ins `write_files` has just left behind.
///
/// `daemon-reload` first, because a socket unit's override is only a file until
/// systemd has re-read it. Then `try-restart` -- not `restart` -- once per
/// unit: it restarts a unit that is running and does nothing to one that is
/// not, which is the whole difference between moving a guest's listener and
/// creating a second one. Ubuntu 24.04 listens through `ssh.socket` and leaves
/// `ssh.service` to be activated on demand; 22.04 ships the same socket unit
/// but does not enable it and runs the service directly. A plain `restart`
/// would start whichever of the two the release deliberately leaves alone, and
/// the guest would end up with a socket and a daemon competing for one port.
///
/// One command per unit rather than one naming all of them, for the reason
/// `runcmd` tolerates a failure: a name a release does not have must not take
/// the other units down with it. The order is the profile's, socket first.
fn apply_ssh_configuration(request: &SeedRequest<'_>) -> Vec<Vec<String>> {
    let units = &request.ssh_daemon.units;
    if units.is_empty() {
        return Vec::new();
    }

    let mut commands = vec![vec!["systemctl".to_owned(), "daemon-reload".to_owned()]];
    commands.extend(units.iter().map(|unit| {
        vec![
            "systemctl".to_owned(),
            "try-restart".to_owned(),
            unit.to_owned(),
        ]
    }));
    commands
}

/// The socket unit override that moves the listener.
///
/// The empty `ListenStream=` is not a stray line: systemd appends to a list
/// setting, so without it the socket would listen on the distribution's port
/// *and* on the chosen one, and the VM would answer where it was not supposed
/// to.
fn socket_settings(port: SshPort) -> String {
    format!("[Socket]\nListenStream=\nListenStream={port}\n")
}

/// Whether the SSH daemon accepts a password.
///
/// Both halves matter: without a hash there is no password to accept, and with
/// SSH off the setting has nobody to apply to.
fn password_login_allowed(request: &SeedRequest<'_>) -> bool {
    matches!(request.ssh, vmlord_core::SshAccess::Enabled { .. }) && request.password_hash.is_some()
}

/// One `write_files` entry: a path, the permissions every file here gets, and
/// the content as a block scalar.
fn file(path: &str, content: &str) -> String {
    let body = content
        .lines()
        .map(|line| format!("{FILE_INDENT}{line}\n"))
        .collect::<String>();
    format!(
        "  - path: {}\n    permissions: '0644'\n    content: |\n{body}",
        scalar::yaml(path)
    )
}

/// Where the console keyboard layout is configured.
///
/// Debian-family: Fedora keeps the same setting in `/etc/vconsole.conf` under
/// different keys, which is a different mechanism rather than a different
/// value, so it waits for a second distribution.
const KEYBOARD_PATH: &str = "/etc/default/keyboard";

/// The content of that file.
///
/// The layout is escaped for the shell, not for YAML: this file is read with
/// `source`, where an unescaped `$` or quote is code.
fn keyboard_settings(layout: &str) -> String {
    let layout = scalar::shell(layout);
    format!(
        "XKBMODEL=\"pc105\"\n\
         XKBLAYOUT=\"{layout}\"\n\
         XKBVARIANT=\"\"\n\
         XKBOPTIONS=\"\"\n\
         BACKSPACE=\"guess\"\n"
    )
}

#[cfg(test)]
mod tests {
    use super::render;
    use crate::{SeedRequest, UBUNTU_SSH};
    use serde_yaml_ng::Value;
    use vmlord_core::{SshAccess, SshDaemon, SshPort};

    const HASH: &str = "$6$rounds=4096$salt$hash";
    const KEY: &str = "ssh-ed25519 AAAAC3Nz vmlord";
    const SSHD_DROP_IN: &str = "/etc/ssh/sshd_config.d/10-vmlord.conf";
    const SOCKET_DROP_IN: &str = "/etc/systemd/system/ssh.socket.d/10-vmlord.conf";

    fn request() -> SeedRequest<'static> {
        SeedRequest {
            vm_name: "my-vm",
            instance_id: "vmlord-4f1c0e5a",
            username: "dev",
            password_hash: Some(HASH),
            authorized_key: Some(KEY),
            ssh: SshAccess::Enabled {
                deploy_key: true,
                port: SshPort::DEFAULT,
            },
            locale: "en_US.UTF-8",
            keyboard: "us",
            timezone: "Europe/Moscow",
            admin_group: "sudo",
            ssh_daemon: &UBUNTU_SSH,
        }
    }

    fn parsed(document: &str) -> Value {
        serde_yaml_ng::from_str(document).expect("cloud-init reads this with a YAML parser too")
    }

    /// The `write_files` entry for a path, whichever position it was written
    /// at: the order of the files is not what any of these tests are about.
    fn file(document: &Value, path: &str) -> Value {
        document["write_files"]
            .as_sequence()
            .expect("write_files is a list")
            .iter()
            .find(|file| file["path"].as_str() == Some(path))
            .unwrap_or_else(|| panic!("no file written at {path}"))
            .clone()
    }

    fn commands(document: &Value) -> Vec<Vec<String>> {
        document["runcmd"]
            .as_sequence()
            .map(|commands| {
                commands
                    .iter()
                    .map(|command| {
                        command
                            .as_sequence()
                            .expect("a command is a list of words")
                            .iter()
                            .map(|word| word.as_str().expect("a word is a string").to_owned())
                            .collect()
                    })
                    .collect()
            })
            .unwrap_or_default()
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
        let file = file(&document, "/etc/default/keyboard");

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
            file(&document, "/etc/default/keyboard")["content"]
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

    /// A VM reachable by key only: the account exists, password login does not.
    #[test]
    fn without_a_hash_the_account_has_no_password_at_all() {
        let document = parsed(&render(&SeedRequest {
            password_hash: None,
            ..request()
        }));
        let user = &document["users"][0];

        assert_eq!(user["lock_passwd"], Value::from(true));
        assert_eq!(user.get("hashed_passwd"), None);
        assert_eq!(document["ssh_pwauth"], Value::from(false));
    }

    #[test]
    fn without_a_key_the_user_has_no_authorized_keys_entry() {
        let document = parsed(&render(&SeedRequest {
            authorized_key: None,
            ..request()
        }));

        assert_eq!(document["users"][0].get("ssh_authorized_keys"), None);
    }

    /// A cloud image ships the SSH daemon enabled, so "SSH off" has to be an
    /// action: silence would leave the daemon running and the choice void.
    #[test]
    fn ssh_turned_off_disables_the_daemon_named_by_the_profile() {
        let document = parsed(&render(&SeedRequest {
            ssh: SshAccess::Disabled,
            authorized_key: None,
            ..request()
        }));

        assert_eq!(
            commands(&document),
            [["systemctl", "disable", "--now", "ssh.socket", "ssh.service"]]
        );
        assert_eq!(document["ssh_pwauth"], Value::from(false));
    }

    /// A guest with no daemon has no port to be told about either: the drop-ins
    /// would configure something that is being switched off.
    #[test]
    fn ssh_turned_off_writes_no_ssh_configuration() {
        let document = parsed(&render(&SeedRequest {
            ssh: SshAccess::Disabled,
            authorized_key: None,
            ..request()
        }));
        let paths = document["write_files"]
            .as_sequence()
            .expect("write_files is a list")
            .iter()
            .map(|file| file["path"].as_str().unwrap_or_default().to_owned())
            .collect::<Vec<_>>();

        assert_eq!(paths, ["/etc/default/keyboard"]);
    }

    /// The port a VM was created with is the port its guest has to answer on --
    /// including the default one, which is written rather than assumed: an
    /// image whose own configuration says something else would otherwise win.
    #[test]
    fn the_chosen_port_is_written_where_the_daemon_and_the_socket_read_it() {
        for port in [22, 2222, 65535] {
            let document = parsed(&render(&SeedRequest {
                ssh: SshAccess::Enabled {
                    deploy_key: true,
                    port: SshPort::new(port).unwrap(),
                },
                ..request()
            }));

            assert_eq!(
                file(&document, SSHD_DROP_IN)["content"],
                Value::from(format!("Port {port}\n")),
                "sshd reads its own configuration"
            );
            assert_eq!(
                file(&document, SOCKET_DROP_IN)["content"],
                Value::from(format!("[Socket]\nListenStream=\nListenStream={port}\n")),
                "the empty entry clears the port the unit already listens on"
            );
            assert_eq!(
                file(&document, SSHD_DROP_IN)["permissions"],
                Value::from("0644")
            );
        }
    }

    /// A file written into `/etc` changes nothing until systemd has read it and
    /// the daemon has been restarted -- and the socket goes first, because it
    /// owns the listening port. `try-restart` leaves a unit the release keeps
    /// stopped alone, so the guest never ends up with two listeners.
    #[test]
    fn the_daemon_is_reloaded_and_restarted_so_the_port_takes_effect() {
        let document = parsed(&render(&request()));

        assert_eq!(
            commands(&document),
            [
                vec!["systemctl", "daemon-reload"],
                vec!["systemctl", "try-restart", "ssh.socket"],
                vec!["systemctl", "try-restart", "ssh.service"],
            ]
        );
    }

    /// A distribution whose daemon opens its own port names no socket unit, and
    /// then there is nothing to override.
    #[test]
    fn a_profile_without_socket_activation_gets_no_socket_drop_in() {
        let daemon = SshDaemon {
            units: vec!["sshd.service".into()],
            config_drop_in: "/etc/ssh/sshd_config.d/10-vmlord.conf".into(),
            socket_drop_in: None,
        };
        let document = parsed(&render(&SeedRequest {
            ssh_daemon: &daemon,
            ..request()
        }));

        assert_eq!(
            document["write_files"]
                .as_sequence()
                .expect("write_files is a list")
                .len(),
            2,
            "the keyboard file and the daemon's own drop-in, and nothing else"
        );
        assert_eq!(
            commands(&document),
            [
                vec!["systemctl", "daemon-reload"],
                vec!["systemctl", "try-restart", "sshd.service"],
            ]
        );
    }

    /// The plaintext password never reaches this crate, and the private key is
    /// never handed to it. The test states that as a property of the output.
    #[test]
    fn no_secret_beyond_the_hash_and_the_public_key_appears_in_the_document() {
        let document = render(&request());

        assert!(!document.contains("hunter2"));
        assert!(!document.contains("PRIVATE KEY"));
        assert!(document.contains(HASH), "the hash is what the guest needs");
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
