//! An AppSandbox-shaped guest in a temporary directory.
//!
//! Every test in this crate runs against one of these rather than against a
//! mounted disk: the conversion is a function over a directory, so a directory
//! is the whole of what it needs to be exercised on.

use std::{
    fs,
    os::unix::fs as unix_fs,
    path::{Path, PathBuf},
};

use tempfile::TempDir;

use crate::Conversion;

/// The files an AppSandbox guest is recognised and converted by.
const FILES: [(&str, &str); 47] = [
    (
        "/etc/os-release",
        "NAME=\"Ubuntu\"\nVERSION_ID=\"26.04\"\nID=ubuntu\n",
    ),
    (
        "/etc/passwd",
        "root:x:0:0:root:/root:/bin/bash\nagromov:x:1000:1000::/home/agromov:/bin/bash\n",
    ),
    ("/etc/hostname", "ubuntu\n"),
    ("/etc/hosts", "127.0.0.1\tlocalhost\n127.0.1.1\tubuntu\n"),
    (
        "/etc/netplan/99-appsandbox.yaml",
        "network:\n  version: 2\n",
    ),
    (
        "/etc/cloud/cloud.cfg.d/99-disable-network-config.cfg",
        "network: {config: disabled}\n",
    ),
    (
        "/home/agromov/.ssh/authorized_keys",
        "ssh-ed25519 AAAAC3Nz appsandbox\n",
    ),
    ("/usr/local/bin/appsandbox-agent", "ELF"),
    ("/usr/local/bin/appsandbox-audio", "ELF"),
    ("/usr/local/bin/appsandbox-clipboard", "ELF"),
    ("/usr/local/bin/appsandbox-display", "ELF"),
    ("/usr/local/bin/appsandbox-input", "ELF"),
    ("/usr/local/bin/appsandbox-gpu", "#!/bin/sh\n"),
    ("/usr/local/bin/appsandbox-firstboot.sh", "#!/bin/bash\n"),
    ("/etc/systemd/system/appsandbox-agent.service", "[Unit]\n"),
    ("/etc/systemd/user/appsandbox-clipboard.service", "[Unit]\n"),
    ("/etc/modprobe.d/asb_drm.conf", "blacklist hyperv_drm\n"),
    ("/etc/modules-load.d/asb_drm.conf", "asb_drm\n"),
    (
        "/etc/systemd/user-environment-generators/50-appsandbox-gpu",
        "#!/bin/sh\n",
    ),
    ("/opt/appsandbox/appsandbox-gpu", "#!/bin/sh\n"),
    ("/var/lib/appsandbox-firstboot.done", ""),
    ("/etc/systemd/system/appsandbox-audio.service", "[Unit]\n"),
    ("/etc/systemd/system/appsandbox-display.service", "[Unit]\n"),
    ("/etc/systemd/system/appsandbox-input.service", "[Unit]\n"),
    (
        "/etc/systemd/system/asb-evict-simpledrm.service",
        "[Unit]\n",
    ),
    (
        "/etc/systemd/system/appsandbox-firstboot.service",
        "[Unit]\n",
    ),
    (
        "/etc/systemd/user/org.gnome.Shell@.service.d/no-gpu.conf",
        "[Service]\nUnsetEnvironment=LD_LIBRARY_PATH\n",
    ),
    ("/etc/modules-load.d/dxgkrnl.conf", "dxgkrnl\n"),
    ("/etc/modules-load.d/snd-aloop.conf", "snd-aloop\n"),
    ("/etc/ld.so.conf.d/wsl-mesa.conf", "/opt/wsl-mesa/lib\n"),
    (
        "/etc/ld.so.conf.d/appsandbox-wsl-deps.conf",
        "/opt/appsandbox/wsl-deps\n",
    ),
    ("/etc/ld.so.conf.d/wsl.conf", "/usr/lib/wsl/lib\n"),
    ("/etc/vulkan/icd.d/dzn_icd.x86_64.json", "{}"),
    (
        "/etc/apt/appsandbox-sources.list.d/appsandbox-local.list",
        "deb [trusted=yes] file:/opt/appsandbox/local-apt noble main\n",
    ),
    (
        "/etc/default/grub.d/99-appsandbox-no-efifb.cfg",
        "GRUB_CMDLINE_LINUX_DEFAULT=\"$GRUB_CMDLINE_LINUX_DEFAULT video=efifb:off\"\n",
    ),
    ("/etc/appsandbox-admin-user", "agromov"),
    ("/etc/appsandbox-ssh-enabled", ""),
    ("/opt/appsandbox/local-apt/pool/main/dkms.deb", "deb"),
    ("/opt/wsl-mesa/lib/x86_64-linux-gnu/libGL.so", "ELF"),
    ("/usr/src/asb_drm-1.0.0/dkms.conf", "PACKAGE_NAME=asb_drm\n"),
    ("/usr/src/dxgkrnl-1.0.0/dkms.conf", "PACKAGE_NAME=dxgkrnl\n"),
    ("/var/lib/dkms/asb_drm/1.0.0/source", ""),
    ("/var/lib/dkms/dxgkrnl/1.0.0/source", ""),
    (
        "/lib/modules/6.14.0-24-generic/updates/dkms/asb_drm.ko",
        "ELF",
    ),
    (
        "/lib/modules/6.14.0-24-generic/updates/dkms/dxgkrnl.ko",
        "ELF",
    ),
    ("/usr/local/bin/nvidia-smi", "ELF"),
    ("/etc/fstab", "UUID=abc / ext4 rw,relatime 0 1\n"),
];

/// The units the first-boot script enabled, whose symlinks the fixture has for
/// the same reason it has the files: the symlink is the enablement.
const ENABLED: [&str; 5] = [
    "appsandbox-agent.service",
    "appsandbox-audio.service",
    "appsandbox-display.service",
    "appsandbox-input.service",
    "asb-evict-simpledrm.service",
];

pub(crate) struct AppSandboxGuest {
    directory: TempDir,
    agent: PathBuf,
}

impl AppSandboxGuest {
    pub(crate) fn new() -> Self {
        let directory = TempDir::new().expect("a temporary directory");
        let root = directory.path();
        for (path, contents) in FILES {
            let target = root.join(path.trim_start_matches('/'));
            fs::create_dir_all(target.parent().expect("a parent")).expect("mkdir");
            fs::write(&target, contents).expect("write");
        }
        // The enablement is the symlink, so the fixture has the ones a
        // converted guest has to be rid of.
        let wants = root.join("etc/systemd/system/multi-user.target.wants");
        fs::create_dir_all(&wants).expect("mkdir");
        for unit in ENABLED {
            unix_fs::symlink(format!("/etc/systemd/system/{unit}"), wants.join(unit))
                .expect("symlink");
        }
        unix_fs::symlink(
            "/etc/systemd/system/appsandbox-firstboot.service",
            wants.join("appsandbox-firstboot.service"),
        )
        .expect("symlink");
        let agent = root.join("vmlord-agent-source");
        fs::write(&agent, b"agent").expect("write");
        Self { directory, agent }
    }

    pub(crate) fn root(&self) -> &Path {
        self.directory.path()
    }

    /// Enables NetworkManager the way a guest running it has it enabled.
    pub(crate) fn with_network_manager(self) -> Self {
        let unit = self
            .root()
            .join("etc/systemd/system/multi-user.target.wants/NetworkManager.service");
        unix_fs::symlink("/usr/lib/systemd/system/NetworkManager.service", unit).expect("symlink");
        self
    }

    /// Removes one guest path, for the tests that assert a refusal.
    pub(crate) fn without(self, guest_path: &str) -> Self {
        let _ = fs::remove_file(self.root().join(guest_path.trim_start_matches('/')));
        self
    }

    pub(crate) fn conversion(&self) -> Conversion {
        Conversion::from_json(&format!(
            r#"{{
                "root": {root:?},
                "guest_username": "agromov",
                "vmlord_public_key": "ssh-ed25519 AAAAC3Nz vmlord",
                "agent_secret": "c2VjcmV0",
                "agent_binary": {agent:?},
                "hostname": "imported",
                "ssh": null
            }}"#,
            root = self.root(),
            agent = self.agent,
        ))
        .expect("a valid document")
    }
}
