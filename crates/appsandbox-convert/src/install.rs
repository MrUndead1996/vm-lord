//! Everything VMLord's own that an imported guest has to have.
//!
//! This runs before anything of AppSandbox's is taken away, so that the guest
//! is at no point left with neither stack.

use std::{
    fs,
    io::Write,
    os::unix::fs::{self as unix_fs, PermissionsExt},
    path::Path,
};

use vmlord_agent_protocol::auth::GUEST_SECRET_PATH;
use vmlord_seed::{AGENT_BINARY_PATH, AGENT_UNIT, AGENT_UNIT_NAME, AGENT_UNIT_PATH};

use crate::{Conversion, ConvertError, facts::GuestFacts, root::guest_path};

/// Where VMLord's network configuration goes.
///
/// Numbered like any other file rather than above the source application's,
/// because that one is removed rather than outranked: netplan merges what it
/// finds, and two documents claiming one interface is the failure that
/// outranking would leave in place.
pub(crate) const NETPLAN_PATH: &str = "/etc/netplan/90-vmlord.yaml";
const WANTS_DIRECTORY: &str = "/etc/systemd/system/multi-user.target.wants";

pub(crate) fn run(conversion: &Conversion, facts: &GuestFacts) -> Result<(), ConvertError> {
    authorized_key(conversion, facts)?;
    agent(conversion)?;
    netplan(conversion, facts)?;
    hostname(conversion)?;
    ssh(conversion)?;
    first_boot_repair(conversion)
}

/// The VM's own key, and only it.
///
/// Written whole rather than appended to: the file holds the source
/// application's key and nothing else -- its agent wrote it with `"w"` on
/// every boot -- so there is nothing in it to preserve, and appending would
/// leave a key whose private half VMLord does not hold.
fn authorized_key(conversion: &Conversion, facts: &GuestFacts) -> Result<(), ConvertError> {
    let directory = facts.home.join(".ssh");
    let keys = directory.join("authorized_keys");
    create_dir_all(&directory)?;
    write(
        &keys,
        format!("{}\n", conversion.vmlord_public_key).as_bytes(),
    )?;
    set_mode(&directory, 0o700)?;
    set_mode(&keys, 0o600)?;
    own(&directory, facts.uid, facts.gid)?;
    own(&keys, facts.uid, facts.gid)
}

fn agent(conversion: &Conversion) -> Result<(), ConvertError> {
    let binary = guest_path(&conversion.root, AGENT_BINARY_PATH)?;
    create_dir_all(binary.parent().expect("a parent"))?;
    let bytes = fs::read(&conversion.agent_binary).map_err(|error| {
        ConvertError::new(format!(
            "the agent at {} could not be read: {error}",
            conversion.agent_binary.display()
        ))
    })?;
    write(&binary, &bytes)?;
    set_mode(&binary, 0o755)?;
    own(&binary, 0, 0)?;

    let secret = guest_path(&conversion.root, GUEST_SECRET_PATH)?;
    create_dir_all(secret.parent().expect("a parent"))?;
    // Created, narrowed, and only then written: the secret never exists under
    // permissions wider than the ones it ends up with.
    let mut file = fs::File::create(&secret).map_err(|error| {
        ConvertError::new(format!(
            "{} could not be created: {error}",
            secret.display()
        ))
    })?;
    set_mode(&secret, 0o600)?;
    file.write_all(format!("{}\n", conversion.agent_secret).as_bytes())
        .and_then(|()| file.flush())
        .map_err(|error| {
            ConvertError::new(format!(
                "{} could not be written: {error}",
                secret.display()
            ))
        })?;
    own(&secret, 0, 0)?;

    let unit = guest_path(&conversion.root, AGENT_UNIT_PATH)?;
    create_dir_all(unit.parent().expect("a parent"))?;
    write(&unit, AGENT_UNIT.as_bytes())?;
    set_mode(&unit, 0o644)?;
    own(&unit, 0, 0)?;

    // The symlink *is* the enablement: `systemctl enable` writes exactly this
    // and nothing else, so an offline conversion writes it directly rather
    // than asking a systemd that is not running.
    let wants = guest_path(&conversion.root, WANTS_DIRECTORY)?;
    create_dir_all(&wants)?;
    let link = wants.join(AGENT_UNIT_NAME);
    let _ = fs::remove_file(&link);
    unix_fs::symlink(AGENT_UNIT_PATH, &link).map_err(|error| {
        ConvertError::new(format!("{} could not be linked: {error}", link.display()))
    })
}

fn netplan(conversion: &Conversion, facts: &GuestFacts) -> Result<(), ConvertError> {
    let path = guest_path(&conversion.root, NETPLAN_PATH)?;
    create_dir_all(path.parent().expect("a parent"))?;
    let document = format!(
        "network:\n  version: 2\n  renderer: {}\n  ethernets:\n    vmlordnic:\n      match: {{ name: \"e*\" }}\n      dhcp4: true\n      dhcp6: false\n",
        facts.renderer.name()
    );
    write(&path, document.as_bytes())?;
    set_mode(&path, 0o600)
}

fn hostname(conversion: &Conversion) -> Result<(), ConvertError> {
    let path = guest_path(&conversion.root, "/etc/hostname")?;
    write(&path, format!("{}\n", conversion.hostname).as_bytes())?;

    let hosts_path = guest_path(&conversion.root, "/etc/hosts")?;
    let hosts = fs::read_to_string(&hosts_path).unwrap_or_default();
    let mut lines: Vec<String> = hosts
        .lines()
        .filter(|line| !line.starts_with("127.0.1.1"))
        .map(ToOwned::to_owned)
        .collect();
    lines.push(format!("127.0.1.1\t{}", conversion.hostname));
    write(&hosts_path, format!("{}\n", lines.join("\n")).as_bytes())
}

fn ssh(conversion: &Conversion) -> Result<(), ConvertError> {
    let Some(drop_ins) = &conversion.ssh else {
        return Ok(());
    };
    let config = guest_path(&conversion.root, &drop_ins.config_drop_in)?;
    create_dir_all(config.parent().expect("a parent"))?;
    write(&config, format!("Port {}\n", drop_ins.port).as_bytes())?;

    if let Some(socket_drop_in) = &drop_ins.socket_drop_in {
        let socket = guest_path(&conversion.root, socket_drop_in)?;
        create_dir_all(socket.parent().expect("a parent"))?;
        // The empty `ListenStream=` is not a stray line: systemd appends to a
        // list setting, so without it the socket would listen on the
        // distribution's port as well as this one.
        write(
            &socket,
            format!(
                "[Socket]\nListenStream=\nListenStream=0.0.0.0:{port}\nListenStream=[::]:{port}\n",
                port = drop_ins.port
            )
            .as_bytes(),
        )?;
    }
    Ok(())
}

/// Where the one-shot that finishes the conversion inside the guest goes.
pub(crate) const REPAIR_UNIT_PATH: &str = "/etc/systemd/system/vmlord-import-repair.service";
pub(crate) const REPAIR_UNIT_NAME: &str = "vmlord-import-repair.service";
/// The marker whose absence is what makes it run once and never again.
pub(crate) const REPAIR_MARKER: &str = "/var/lib/vmlord/import-repaired";

/// The two things an offline conversion cannot do to a guest, done by the
/// guest on its first boot.
///
/// The initramfs still carries the source application's kernel modules: they
/// were built into it when they were installed, and removing the modules from
/// `/lib/modules` does not remove the copies inside the image. The guest boots
/// with `asb_drm` loaded and `systemd-modules-load` failing, on a driver whose
/// files are gone.
///
/// The kernel command line still carries that application's `video=` options
/// for the same reason: the drop-in they came from is removed, but `grub.cfg`
/// was generated while it was there.
///
/// Both are regenerated by programs that need the guest's own kernel, its
/// module tree and a working `/proc` -- a chroot at conversion time rather
/// than a boot. So the conversion installs the work instead of doing it, the
/// way the source application installed its own first-boot service, and the
/// unit removes itself once it has run.
fn first_boot_repair(conversion: &Conversion) -> Result<(), ConvertError> {
    let unit = guest_path(&conversion.root, REPAIR_UNIT_PATH)?;
    create_dir_all(unit.parent().expect("a parent"))?;
    write(
        &unit,
        format!(
            "[Unit]\n             Description=Finish VMLord's import of this guest\n             ConditionPathExists=!{REPAIR_MARKER}\n             DefaultDependencies=no\n             After=local-fs.target\n             Before=multi-user.target\n\n             [Service]\n             Type=oneshot\n             RemainAfterExit=yes\n             TimeoutStartSec=600\n             ExecStart=/usr/sbin/update-initramfs -u\n             ExecStart=/usr/sbin/update-grub\n             ExecStart=/usr/bin/mkdir -p /var/lib/vmlord\n             ExecStart=/usr/bin/touch {REPAIR_MARKER}\n             ExecStart=/usr/bin/systemctl disable {REPAIR_UNIT_NAME}\n\n             [Install]\n             WantedBy=multi-user.target\n"
        )
        .as_bytes(),
    )?;
    set_mode(&unit, 0o644)?;
    own(&unit, 0, 0)?;

    let wants = guest_path(&conversion.root, WANTS_DIRECTORY)?;
    create_dir_all(&wants)?;
    let link = wants.join(REPAIR_UNIT_NAME);
    let _ = fs::remove_file(&link);
    unix_fs::symlink(REPAIR_UNIT_PATH, &link).map_err(|error| {
        ConvertError::new(format!("{} could not be linked: {error}", link.display()))
    })
}

fn create_dir_all(path: &Path) -> Result<(), ConvertError> {
    fs::create_dir_all(path).map_err(|error| {
        ConvertError::new(format!("{} could not be created: {error}", path.display()))
    })
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), ConvertError> {
    fs::write(path, bytes).map_err(|error| {
        ConvertError::new(format!("{} could not be written: {error}", path.display()))
    })
}

fn set_mode(path: &Path, mode: u32) -> Result<(), ConvertError> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|error| {
        ConvertError::new(format!(
            "{} could not be given mode {mode:o}: {error}",
            path.display()
        ))
    })
}

/// Ownership, where the conversion runs with the privilege to set it.
///
/// A conversion run without it -- a fixture in a test, a dry run by hand --
/// still writes every file and every mode; only the owner is left as it was,
/// and it is reported rather than silently skipped.
fn own(path: &Path, uid: u32, gid: u32) -> Result<(), ConvertError> {
    match unix_fs::chown(path, Some(uid), Some(gid)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            tracing::warn!("{} keeps its owner: {error}", path.display());
            Ok(())
        }
        Err(error) => Err(ConvertError::new(format!(
            "{} could not be given owner {uid}:{gid}: {error}",
            path.display()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt};

    use super::{NETPLAN_PATH, run};
    use crate::{facts, fixture::AppSandboxGuest};

    fn converted(guest: &AppSandboxGuest) -> crate::Conversion {
        let conversion = guest.conversion();
        let facts = facts::read(&conversion).expect("facts");
        run(&conversion, &facts).expect("installed");
        conversion
    }

    #[test]
    fn vmlords_key_replaces_the_one_the_source_left_behind() {
        let guest = AppSandboxGuest::new();
        converted(&guest);
        let keys = fs::read_to_string(guest.root().join("home/agromov/.ssh/authorized_keys"))
            .expect("read");
        assert!(keys.contains("ssh-ed25519 AAAAC3Nz vmlord"), "{keys}");
        assert!(!keys.contains("appsandbox"), "{keys}");
        let mode = fs::metadata(guest.root().join("home/agromov/.ssh/authorized_keys"))
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn the_agent_its_secret_and_its_unit_are_installed_at_the_seeds_paths() {
        let guest = AppSandboxGuest::new();
        converted(&guest);
        let root = guest.root();
        assert_eq!(
            fs::read(root.join("usr/local/lib/vmlord/vmlord-agent")).expect("read"),
            b"agent"
        );
        assert_eq!(
            fs::read_to_string(root.join("etc/vmlord/agent.secret")).expect("read"),
            "c2VjcmV0\n"
        );
        assert_eq!(
            fs::read_to_string(root.join("etc/systemd/system/vmlord-agent.service")).expect("read"),
            vmlord_seed::AGENT_UNIT
        );
    }

    #[test]
    fn the_secret_is_readable_by_root_alone_and_the_binary_is_executable() {
        let guest = AppSandboxGuest::new();
        converted(&guest);
        let mode = |path: &str| {
            fs::metadata(guest.root().join(path))
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode("etc/vmlord/agent.secret"), 0o600);
        assert_eq!(mode("usr/local/lib/vmlord/vmlord-agent"), 0o755);
        assert_eq!(mode("etc/systemd/system/vmlord-agent.service"), 0o644);
    }

    #[test]
    fn the_unit_is_enabled_by_the_symlink_systemd_reads() {
        let guest = AppSandboxGuest::new();
        converted(&guest);
        let link = guest
            .root()
            .join("etc/systemd/system/multi-user.target.wants/vmlord-agent.service");
        assert_eq!(
            fs::read_link(&link).expect("a symlink"),
            std::path::Path::new(vmlord_seed::AGENT_UNIT_PATH)
        );
    }

    #[test]
    fn the_netplan_asks_for_an_address_and_names_the_renderer_that_runs() {
        let guest = AppSandboxGuest::new().with_network_manager();
        converted(&guest);
        let netplan = fs::read_to_string(guest.root().join(NETPLAN_PATH.trim_start_matches('/')))
            .expect("read");
        assert!(netplan.contains("dhcp4: true"), "{netplan}");
        assert!(netplan.contains("renderer: NetworkManager"), "{netplan}");
        assert!(!netplan.contains("$RENDERER"), "{netplan}");
    }

    #[test]
    fn the_hostname_becomes_the_one_the_document_names() {
        let guest = AppSandboxGuest::new();
        converted(&guest);
        assert_eq!(
            fs::read_to_string(guest.root().join("etc/hostname")).expect("read"),
            "imported\n"
        );
        let hosts = fs::read_to_string(guest.root().join("etc/hosts")).expect("read");
        assert!(hosts.contains("127.0.1.1\timported"), "{hosts}");
    }

    #[test]
    fn installing_twice_leaves_what_installing_once_did() {
        let guest = AppSandboxGuest::new();
        let conversion = guest.conversion();
        let facts = facts::read(&conversion).expect("facts");
        run(&conversion, &facts).expect("installed");
        run(&conversion, &facts).expect("installed again");
        let keys = fs::read_to_string(guest.root().join("home/agromov/.ssh/authorized_keys"))
            .expect("read");
        assert_eq!(keys.matches("vmlord").count(), 1, "{keys}");
    }

    #[test]
    fn a_one_shot_is_installed_to_finish_the_conversion_inside_the_guest() {
        let guest = AppSandboxGuest::new();
        converted(&guest);
        let unit = fs::read_to_string(
            guest
                .root()
                .join("etc/systemd/system/vmlord-import-repair.service"),
        )
        .expect("read");

        // The initramfs still carries the source application's modules, and
        // the kernel command line its video options; both are regenerated by
        // programs that need the guest's own kernel.
        assert!(unit.contains("update-initramfs -u"), "{unit}");
        assert!(unit.contains("update-grub"), "{unit}");
        // Once, and never again.
        assert!(
            unit.contains("ConditionPathExists=!/var/lib/vmlord/import-repaired"),
            "{unit}"
        );
        assert!(unit.contains("systemctl disable"), "{unit}");
        assert_eq!(
            fs::read_link(
                guest.root().join(
                    "etc/systemd/system/multi-user.target.wants/vmlord-import-repair.service"
                )
            )
            .expect("a symlink"),
            std::path::Path::new(super::REPAIR_UNIT_PATH)
        );
    }

    #[test]
    fn the_ssh_drop_ins_are_written_only_when_the_document_asks_for_them() {
        let guest = AppSandboxGuest::new();
        converted(&guest);
        assert!(
            !guest
                .root()
                .join("etc/ssh/sshd_config.d/10-vmlord.conf")
                .exists()
        );

        let guest = AppSandboxGuest::new();
        let mut conversion = guest.conversion();
        conversion.ssh = Some(crate::SshDropIns {
            config_drop_in: "/etc/ssh/sshd_config.d/10-vmlord.conf".to_owned(),
            socket_drop_in: Some("/etc/systemd/system/ssh.socket.d/10-vmlord.conf".to_owned()),
            port: 2222,
        });
        let facts = facts::read(&conversion).expect("facts");
        run(&conversion, &facts).expect("installed");
        assert_eq!(
            fs::read_to_string(guest.root().join("etc/ssh/sshd_config.d/10-vmlord.conf"))
                .expect("read"),
            "Port 2222\n"
        );
        let socket = fs::read_to_string(
            guest
                .root()
                .join("etc/systemd/system/ssh.socket.d/10-vmlord.conf"),
        )
        .expect("read");
        assert!(
            socket.contains("ListenStream=\nListenStream=0.0.0.0:2222"),
            "{socket}"
        );
    }
}
