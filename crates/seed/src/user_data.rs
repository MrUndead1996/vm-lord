//! `user-data`: what cloud-init is asked to do on the first boot.
//!
//! Printed by hand rather than serialised. The document is small and fixed, and
//! what it must be is known exactly -- including the `#cloud-config` line, which
//! is a comment to YAML and the format marker to cloud-init.

use vmlord_agent_protocol::auth::GUEST_SECRET_PATH;
use vmlord_core::{SshAccess, SshPort, SshUnits};

use crate::{AGENT_FILE, SeedRequest, scalar};

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
    document.push_str(&packages(request));
    document.push_str(&write_files(request));
    document.push_str("growpart:\n  mode: auto\n  devices: ['/']\nresize_rootfs: true\n");
    document.push_str(&runcmd(request));

    document
}

/// The desktop the first boot installs, as cloud-init's own `packages` block,
/// or nothing when no desktop was asked for.
///
/// cloud-init's key rather than an `apt-get` line in `runcmd`: it runs before
/// the commands, it refreshes the package lists itself, and it comes from the
/// distribution's configured archives -- so a guest ends up with the desktop
/// its vendor publishes and VMLord adds no repository of its own.
///
/// A failure here does not stop the boot. cloud-init reports it and carries
/// on, which is exactly what a host with no working network must leave behind:
/// a VM that runs, that SSH answers on, and whose desktop is missing and can
/// be installed again.
fn packages(request: &SeedRequest<'_>) -> String {
    if request.desktop_packages.is_empty() {
        return String::new();
    }
    let mut block = String::from("package_update: true\npackages:\n");
    for package in request.desktop_packages {
        block.push_str(&format!("  - {}\n", scalar::yaml(package)));
    }
    block
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
        if let SshUnits::SocketActivated { socket_drop_in, .. } = &request.ssh_daemon.units {
            files.push_str(&file(socket_drop_in, &socket_settings(port)));
        }
    }
    if let Some(secret) = request.agent_secret {
        // Root alone, unlike everything else here: the other files describe
        // the guest, and this one is what lets a process claim to be its
        // agent.
        files.push_str(&restricted_file(
            GUEST_SECRET_PATH,
            &format!("{secret}\n"),
            "root:root",
        ));
        files.push_str(&file(AGENT_SERVICE_PATH, AGENT_SERVICE));
    }
    files
}

/// The commands the first boot runs, as one `runcmd` block, or nothing when
/// there are none.
///
/// SSH is the only thing here, and it is one of two opposite jobs: switching
/// the daemon off, or moving it to the port the VM was created with.
fn runcmd(request: &SeedRequest<'_>) -> String {
    let mut commands = match request.ssh {
        SshAccess::Disabled => disable_ssh(request),
        SshAccess::Enabled { .. } => apply_ssh_configuration(request),
    };
    if request.agent_secret.is_some() {
        commands.extend(agent_install_commands());
    }
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

fn agent_install_commands() -> Vec<Vec<String>> {
    vec![
        vec!["mkdir".into(), "-p".into(), "/run/vmlord-tools".into()],
        vec!["mkdir".into(), "-p".into(), "/usr/local/lib/vmlord".into()],
        vec![
            "mount".into(),
            "-o".into(),
            "ro".into(),
            "-L".into(),
            "VMLTOOLS".into(),
            "/run/vmlord-tools".into(),
        ],
        vec![
            "install".into(),
            "-m".into(),
            "0755".into(),
            "-o".into(),
            "root".into(),
            "-g".into(),
            "root".into(),
            format!("/run/vmlord-tools/{AGENT_FILE}"),
            "/usr/local/lib/vmlord/vmlord-agent".into(),
        ],
        vec!["umount".into(), "/run/vmlord-tools".into()],
        vec!["systemctl".into(), "daemon-reload".into()],
        vec![
            "systemctl".into(),
            "enable".into(),
            "--now".into(),
            "vmlord-agent.service".into(),
        ],
    ]
}

/// Stops the SSH daemon and keeps it stopped.
///
/// The unit names come from the profile rather than from here: Debian-family
/// systems socket-activate `ssh.socket`, Fedora and SUSE name both `sshd`. A
/// unit that does not exist on a given release makes `systemctl` return
/// non-zero, which `runcmd` does not treat as fatal.
fn disable_ssh(request: &SeedRequest<'_>) -> Vec<Vec<String>> {
    let units = request.ssh_daemon.units.all();

    tracing::debug!("the seed disables the SSH daemon: {}", units.join(", "));
    let mut command = vec![
        "systemctl".to_owned(),
        "disable".to_owned(),
        "--now".to_owned(),
    ];
    command.extend(units.into_iter().map(ToOwned::to_owned));
    vec![command]
}

/// Makes the daemon read the drop-ins `write_files` has just left behind.
///
/// `daemon-reload` first, because a unit's override is only a file until
/// systemd has re-read it. What follows depends on how the distribution runs
/// the daemon, because the two shapes fail in opposite ways.
///
/// Where the daemon opens its own port, `try-restart` is the whole answer: it
/// restarts a running daemon and does nothing to one the release keeps stopped.
///
/// Where a socket owns the port, the service must not be restarted at all --
/// and `try-restart` is not enough to promise that. A socket-activated
/// `ssh.service` is inactive only until *something connects*, and on a guest
/// created with the default port something does: the image already listens on
/// 22 from `sockets.target`, so VMLord's own readiness probe and its
/// `cloud-init status --wait` connect during the first boot and activate the
/// service before this command ever runs. `try-restart` then restarts a daemon
/// that is now standalone, binding the port out of `sshd_config` while the
/// socket still holds it -- two listeners fighting over one port, which is
/// exactly what a guest created on 22 used to end up with (#105).
///
/// So the socket-activated branch decides in the guest, where the answer is
/// known: if the socket is the listener, the service is *stopped* -- the next
/// connection brings it back through the socket -- and only then is the socket
/// restarted onto its new port. Stopping first is not an ordering preference:
/// restarting the socket while the service holds the port is the same fight
/// from the other side. Releases that ship the socket unit without enabling it
/// -- Ubuntu 22.04 does -- take the `else`, and their running service is
/// restarted as before.
///
/// One `sh -c` rather than a list of commands because the decision is one:
/// `runcmd` has no way to spell "and only if the first answered yes", and
/// splitting it would mean guessing on the host what only the guest can see.
fn apply_ssh_configuration(request: &SeedRequest<'_>) -> Vec<Vec<String>> {
    let mut commands = vec![vec!["systemctl".to_owned(), "daemon-reload".to_owned()]];
    commands.push(match &request.ssh_daemon.units {
        SshUnits::Service { unit } => vec![
            "systemctl".to_owned(),
            "try-restart".to_owned(),
            unit.to_owned(),
        ],
        SshUnits::SocketActivated {
            socket, service, ..
        } => vec![
            "sh".to_owned(),
            "-c".to_owned(),
            format!(
                "if systemctl is-active --quiet {socket}; \
                 then systemctl stop {service}; systemctl restart {socket}; \
                 else systemctl try-restart {service}; fi"
            ),
        ],
    });
    commands
}

/// The socket unit override that moves the listener.
///
/// The empty `ListenStream=` is not a stray line: systemd appends to a list
/// setting, so without it the socket would listen on the distribution's port
/// *and* on the chosen one, and the VM would answer where it was not supposed
/// to.
///
/// Both address families are then named, because clearing the list threw both
/// of the distribution's own entries away. Ubuntu's `ssh.socket` listens on
/// `0.0.0.0:22` and `[::]:22` *and* sets `BindIPv6Only=ipv6-only`, which a
/// drop-in that only replaces the addresses leaves in force -- so a bare
/// `ListenStream=<n>` binds one IPv6 socket that refuses every IPv4 connection,
/// which is how a guest ended up answering nothing on the address VMLord
/// reaches it at (#105). Naming both is right whatever that setting says: with
/// `ipv6-only` they are the two listeners the guest needs, and without it the
/// IPv4 entry is the redundant half of a dual-stack socket rather than a
/// second one.
fn socket_settings(port: SshPort) -> String {
    format!("[Socket]\nListenStream=\nListenStream=0.0.0.0:{port}\nListenStream=[::]:{port}\n")
}

/// Whether the SSH daemon accepts a password.
///
/// Both halves matter: without a hash there is no password to accept, and with
/// SSH off the setting has nobody to apply to.
fn password_login_allowed(request: &SeedRequest<'_>) -> bool {
    matches!(request.ssh, vmlord_core::SshAccess::Enabled { .. }) && request.password_hash.is_some()
}

/// One `write_files` entry for a file the whole guest may read.
fn file(path: &str, content: &str) -> String {
    entry(path, content, "0644", None)
}

/// One `write_files` entry for a file nobody but its owner may read.
fn restricted_file(path: &str, content: &str, owner: &str) -> String {
    entry(path, content, "0600", Some(owner))
}

/// A `write_files` entry: a path, its permissions and owner, and the content
/// as a block scalar.
fn entry(path: &str, content: &str, permissions: &str, owner: Option<&str>) -> String {
    let body = content
        .lines()
        .map(|line| format!("{FILE_INDENT}{line}\n"))
        .collect::<String>();
    let owner = owner
        .map(|owner| format!("    owner: {}\n", scalar::yaml(owner)))
        .unwrap_or_default();

    format!(
        "  - path: {}\n    permissions: '{permissions}'\n{owner}    content: |\n{body}",
        scalar::yaml(path)
    )
}

/// Where the console keyboard layout is configured.
///
/// Debian-family: Fedora keeps the same setting in `/etc/vconsole.conf` under
/// different keys, which is a different mechanism rather than a different
/// value, so it waits for a second distribution.
const KEYBOARD_PATH: &str = "/etc/default/keyboard";

const AGENT_SERVICE_PATH: &str = "/etc/systemd/system/vmlord-agent.service";
const AGENT_SERVICE: &str = "[Unit]\nDescription=VMLord guest agent\nConditionPathExists=/etc/vmlord/agent.secret\n\n[Service]\nExecStart=/usr/local/lib/vmlord/vmlord-agent\nUser=root\nRestart=always\nRestartSec=5\n\n[Install]\nWantedBy=multi-user.target\n";

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
    use super::{GUEST_SECRET_PATH, render};
    use crate::{SeedRequest, UBUNTU_SSH};
    use serde_yaml_ng::Value;
    use vmlord_core::{SshAccess, SshDaemon, SshPort, SshUnits};

    const HASH: &str = "$6$rounds=4096$salt$hash";
    const KEY: &str = "ssh-ed25519 AAAAC3Nz vmlord";
    const AGENT_SECRET: &str = "Zm9ydHktdHdvIGJ5dGVzIG9mIHNlY3JldCBoZXJlIQ==";
    const SSHD_DROP_IN: &str = "/etc/ssh/sshd_config.d/10-vmlord.conf";
    const SOCKET_DROP_IN: &str = "/etc/systemd/system/ssh.socket.d/10-vmlord.conf";

    /// A subscriber that keeps every record, for the test below.
    mod capture {
        use std::{
            fmt,
            sync::{Arc, Mutex},
        };

        use tracing::{
            Event, Subscriber,
            field::{Field, Visit},
        };
        use tracing_subscriber::{Layer, layer::Context, layer::SubscriberExt as _};

        /// Runs `body` with every record it writes captured, and hands back the
        /// records, joined.
        pub(super) fn capture(body: impl FnOnce()) -> String {
            let records = Arc::new(Mutex::new(Vec::new()));
            let subscriber = tracing_subscriber::registry().with(Capture(Arc::clone(&records)));
            tracing::subscriber::with_default(subscriber, body);
            records
                .lock()
                .expect("no test panics while holding the records")
                .join("\n")
        }

        struct Capture(Arc<Mutex<Vec<String>>>);

        impl<S: Subscriber> Layer<S> for Capture {
            fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
                let mut rendered = String::new();
                event.record(&mut Everything(&mut rendered));
                self.0
                    .lock()
                    .expect("no test panics while holding the records")
                    .push(rendered);
            }
        }

        /// Every field, not only the message: a secret that leaked as a field
        /// would be just as leaked.
        struct Everything<'a>(&'a mut String);

        impl Visit for Everything<'_> {
            fn record_str(&mut self, field: &Field, value: &str) {
                self.0.push_str(&format!(" {}={value}", field.name()));
            }

            fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
                self.0.push_str(&format!(" {}={value:?}", field.name()));
            }
        }
    }

    /// The rule this guards: a secret has neither a `Display` nor a `Debug`
    /// that shows its value, so it cannot reach a record at all.
    ///
    /// It passes on the first run, because `SeedRequest`, `Seed` and
    /// `auth::Secret` are already built that way. It is here to stop passing
    /// the moment one of them grows a `Debug` and something records it -- which
    /// would otherwise break nothing and be noticed by nobody.
    #[test]
    fn building_the_documents_records_no_secret() {
        let mut request = request();
        request.agent_secret = Some(AGENT_SECRET);

        let text = capture::capture(|| {
            let _seed = crate::build(&request);
        });

        assert!(
            !text.contains(HASH),
            "no crypt entry may be recorded: {text}"
        );
        assert!(
            !text.contains(AGENT_SECRET),
            "no agent secret may be recorded: {text}"
        );
    }

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
            // The fixture is a VM with no agent, so that the tests counting
            // written files stay about the files they are named after. The
            // two tests about the secret set it themselves.
            agent_secret: None,
            desktop_packages: &[],
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
    fn a_headless_vm_asks_for_no_packages_at_all() {
        let document = parsed(&render(&request()));
        assert!(document.get("packages").is_none());
        assert!(document.get("package_update").is_none());
    }

    #[test]
    fn a_desktop_vm_installs_its_packages_from_the_distribution_s_archives() {
        let packages = ["ubuntu-desktop-minimal".to_owned()];
        let document = parsed(&render(&SeedRequest {
            desktop_packages: &packages,
            ..request()
        }));

        assert_eq!(
            document["packages"],
            Value::from(vec!["ubuntu-desktop-minimal"])
        );
        assert_eq!(document["package_update"], Value::from(true));
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
                Value::from(format!(
                    "[Socket]\nListenStream=\nListenStream=0.0.0.0:{port}\n\
                     ListenStream=[::]:{port}\n"
                )),
                "the empty entry clears the ports the unit already listens on, \
                 and both families are named because clearing took both away"
            );
            assert_eq!(
                file(&document, SSHD_DROP_IN)["permissions"],
                Value::from("0644")
            );
        }
    }

    /// The bug behind the bug: Ubuntu's socket unit sets
    /// `BindIPv6Only=ipv6-only` and names one listener per family, so a drop-in
    /// that clears the list and gives back a bare port number leaves the guest
    /// answering on IPv6 alone -- and VMLord reaches its guests over IPv4.
    #[test]
    fn the_socket_listens_on_ipv4_whatever_port_it_is_moved_to() {
        for port in [22, 222, 65535] {
            let document = parsed(&render(&SeedRequest {
                ssh: SshAccess::Enabled {
                    deploy_key: true,
                    port: SshPort::new(port).unwrap(),
                },
                ..request()
            }));

            let settings = file(&document, SOCKET_DROP_IN)["content"]
                .as_str()
                .expect("the drop-in is a string")
                .to_owned();

            assert!(
                settings.contains(&format!("ListenStream=0.0.0.0:{port}\n")),
                "port {port} has no IPv4 listener: {settings:?}"
            );
            assert!(
                settings.contains(&format!("ListenStream=[::]:{port}\n")),
                "port {port} has no IPv6 listener: {settings:?}"
            );
        }
    }

    /// A file written into `/etc` changes nothing until systemd has read it,
    /// and on a socket-activated release the service must be stopped rather
    /// than restarted: the socket owns the port, and a service restarted beside
    /// it binds the same port a second time.
    #[test]
    fn a_socket_activated_daemon_is_moved_by_its_socket_alone() {
        let document = parsed(&render(&request()));

        assert_eq!(
            commands(&document),
            [
                vec!["systemctl", "daemon-reload"],
                vec![
                    "sh",
                    "-c",
                    "if systemctl is-active --quiet ssh.socket; \
                     then systemctl stop ssh.service; systemctl restart ssh.socket; \
                     else systemctl try-restart ssh.service; fi",
                ],
            ]
        );
    }

    /// The bug this shape exists for: a VM created on 22 answers on 22 from
    /// `sockets.target`, so VMLord itself activates the service before the seed
    /// reconfigures it. Whatever the port, the seed must never hand the guest a
    /// command that restarts that service into a second listener.
    #[test]
    fn no_port_makes_the_seed_restart_a_socket_activated_service() {
        for port in [22, 222, 65535] {
            let document = parsed(&render(&SeedRequest {
                ssh: SshAccess::Enabled {
                    deploy_key: true,
                    port: SshPort::new(port).unwrap(),
                },
                ..request()
            }));
            let commands = commands(&document);

            assert!(
                !commands
                    .iter()
                    .any(|command| command.contains(&"try-restart".to_owned())
                        && command.contains(&"ssh.service".to_owned())),
                "port {port} restarts the service beside its socket: {commands:?}"
            );
        }
    }

    /// A distribution whose daemon opens its own port names no socket unit, and
    /// then there is nothing to override and nothing to fight over: the running
    /// daemon is simply restarted onto the port its own configuration now says.
    #[test]
    fn a_profile_without_socket_activation_gets_no_socket_drop_in() {
        let daemon = SshDaemon {
            units: SshUnits::Service {
                unit: "sshd.service".into(),
            },
            config_drop_in: "/etc/ssh/sshd_config.d/10-vmlord.conf".into(),
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

    /// The agent's secret is the one file in the document no other account may
    /// read: anything that can read it can open an authenticated session as
    /// this VM's agent.
    #[test]
    fn the_agent_secret_is_written_where_only_root_can_read_it() {
        let document = parsed(&render(&SeedRequest {
            agent_secret: Some(AGENT_SECRET),
            ..request()
        }));

        let file = file(&document, GUEST_SECRET_PATH);
        assert_eq!(file["content"], Value::from(format!("{AGENT_SECRET}\n")));
        assert_eq!(file["permissions"], Value::from("0600"));
        assert_eq!(file["owner"], Value::from("root:root"));
    }

    #[test]
    fn a_vm_with_no_agent_secret_has_no_such_file() {
        let document = parsed(&render(&SeedRequest {
            agent_secret: None,
            ..request()
        }));

        assert!(
            document["write_files"]
                .as_sequence()
                .expect("write_files is a list")
                .iter()
                .all(|file| file["path"].as_str() != Some(GUEST_SECRET_PATH))
        );
    }

    #[test]
    fn an_agent_seed_writes_and_enables_the_agent_service() {
        let daemon = SshDaemon {
            units: SshUnits::Service {
                unit: "sshd.service".into(),
            },
            config_drop_in: "/etc/ssh/sshd_config.d/10-vmlord.conf".into(),
        };
        let document = parsed(&render(&SeedRequest {
            agent_secret: Some(AGENT_SECRET),
            ssh: SshAccess::Disabled,
            ssh_daemon: &daemon,
            ..request()
        }));

        let service = file(&document, "/etc/systemd/system/vmlord-agent.service");
        assert_eq!(service["permissions"], Value::from("0644"));
        let content = service["content"]
            .as_str()
            .expect("unit content is a string");
        for line in [
            "ConditionPathExists=/etc/vmlord/agent.secret",
            "ExecStart=/usr/local/lib/vmlord/vmlord-agent",
            "User=root",
            "Restart=always",
            "RestartSec=5",
            "WantedBy=multi-user.target",
        ] {
            assert!(content.contains(line), "unit should contain {line}");
        }

        assert_eq!(
            commands(&document),
            [
                vec!["systemctl", "disable", "--now", "sshd.service"],
                vec!["mkdir", "-p", "/run/vmlord-tools"],
                vec!["mkdir", "-p", "/usr/local/lib/vmlord"],
                vec!["mount", "-o", "ro", "-L", "VMLTOOLS", "/run/vmlord-tools"],
                vec![
                    "install",
                    "-m",
                    "0755",
                    "-o",
                    "root",
                    "-g",
                    "root",
                    "/run/vmlord-tools/vmlord-agent",
                    "/usr/local/lib/vmlord/vmlord-agent",
                ],
                vec!["umount", "/run/vmlord-tools"],
                vec!["systemctl", "daemon-reload"],
                vec!["systemctl", "enable", "--now", "vmlord-agent.service"],
            ]
        );
    }

    #[test]
    fn a_seed_without_an_agent_has_no_unit_or_commands_when_ssh_is_disabled() {
        let daemon = SshDaemon {
            units: SshUnits::Service {
                unit: "sshd.service".into(),
            },
            config_drop_in: "/etc/ssh/sshd_config.d/10-vmlord.conf".into(),
        };
        let document = parsed(&render(&SeedRequest {
            agent_secret: None,
            ssh: SshAccess::Disabled,
            ssh_daemon: &daemon,
            ..request()
        }));

        assert!(
            document["write_files"]
                .as_sequence()
                .expect("write_files is a list")
                .iter()
                .all(|file| file["path"].as_str()
                    != Some("/etc/systemd/system/vmlord-agent.service"))
        );
        assert_eq!(
            commands(&document),
            [vec!["systemctl", "disable", "--now", "sshd.service"]],
            "a guest that runs no agent still has its daemon switched off"
        );
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
