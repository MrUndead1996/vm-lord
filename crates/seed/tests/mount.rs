//! Hands the writer's output to the reader that will actually read it.
//!
//! The unit tests parse the image with a parser of our own, which cannot
//! disprove the one deliberate deviation from ECMA-119: file identifiers
//! written literally, lowercase and hyphenated, with no version suffix. Only a
//! Linux kernel can, and the one in WSL2 is the same driver the guest runs.
//! Windows is no substitute: CDFS will never open this image in production.
//!
//! Run with:
//!
//! ```text
//! sudo -E $(which cargo) test -p vmlord-seed --test mount -- --ignored --nocapture
//! ```
//!
//! `sudo` drops the caller's `PATH`, which is why `cargo` is spelled out.

#![cfg(target_os = "linux")]

use std::{fs, path::PathBuf, process::Command};

use vmlord_core::{SshAccess, SshPort, ubuntu};
use vmlord_seed::{Seed, SeedRequest, build, image, tools_image};

/// A directory that unmounts and deletes itself however the test ends.
struct Scratch {
    directory: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!("vmlord-seed-{}", std::process::id()));
        fs::create_dir_all(directory.join("mnt")).expect("a scratch directory should be creatable");
        fs::create_dir_all(directory.join("tools-mnt"))
            .expect("a tools scratch directory should be creatable");
        Self { directory }
    }

    fn image_path(&self) -> PathBuf {
        self.directory.join("seed.iso")
    }

    fn mount_point(&self) -> PathBuf {
        self.directory.join("mnt")
    }

    fn tools_image_path(&self) -> PathBuf {
        self.directory.join("tools.iso")
    }

    fn tools_mount_point(&self) -> PathBuf {
        self.directory.join("tools-mnt")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = Command::new("umount").arg(self.mount_point()).status();
        let _ = Command::new("umount")
            .arg(self.tools_mount_point())
            .status();
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn seed() -> Seed {
    build(&SeedRequest {
        vm_name: "my-vm",
        instance_id: "vmlord-4f1c0e5a",
        username: "dev",
        password_hash: Some("$6$rounds=4096$salt$hash"),
        authorized_key: Some("ssh-ed25519 AAAA vmlord"),
        ssh: SshAccess::Enabled {
            deploy_key: true,
            port: SshPort::DEFAULT,
        },
        locale: "en_US.UTF-8",
        keyboard: "us",
        keyboard_files: &ubuntu().keyboard,
        timezone: "Europe/Moscow",
        admin_group: "sudo",
        ssh_daemon: &ubuntu().ssh,
        agent_secret: None,
        desktop_packages: &[],
    })
}

#[test]
#[ignore = "mounts a loop device; run as root: sudo -E cargo test -p vmlord-seed --test mount -- --ignored"]
fn a_linux_kernel_reads_the_seed_the_way_cloud_init_will() {
    let seed = seed();
    let scratch = Scratch::new();
    fs::write(scratch.image_path(), image(&seed)).expect("the image should be written");
    let agent = b"vmlord-agent test bytes";
    fs::write(scratch.tools_image_path(), tools_image(agent))
        .expect("the tools image should be written");

    // blkid is how cloud-init finds the seed: by label, not by device name.
    let label = Command::new("blkid")
        .args(["-s", "LABEL", "-o", "value"])
        .arg(scratch.image_path())
        .output()
        .expect("blkid should be installed");
    assert_eq!(
        String::from_utf8_lossy(&label.stdout).trim(),
        "CIDATA",
        "blkid stderr: {}",
        String::from_utf8_lossy(&label.stderr)
    );

    let mounted = Command::new("mount")
        .args(["-o", "loop,ro"])
        .arg(scratch.image_path())
        .arg(scratch.mount_point())
        .status()
        .expect("mount should be installed");
    assert!(mounted.success(), "mount failed; this test needs root");

    let mount_point = scratch.mount_point();
    assert_eq!(
        fs::read_to_string(mount_point.join("user-data")).expect("user-data should be readable"),
        seed.user_data
    );
    assert_eq!(
        fs::read_to_string(mount_point.join("meta-data")).expect("meta-data should be readable"),
        seed.meta_data
    );

    let tools_label = Command::new("blkid")
        .args(["-s", "LABEL", "-o", "value"])
        .arg(scratch.tools_image_path())
        .output()
        .expect("blkid should be installed");
    assert_eq!(
        String::from_utf8_lossy(&tools_label.stdout).trim(),
        "VMLTOOLS",
        "blkid stderr: {}",
        String::from_utf8_lossy(&tools_label.stderr)
    );

    let tools_mounted = Command::new("mount")
        .args(["-o", "loop,ro"])
        .arg(scratch.tools_image_path())
        .arg(scratch.tools_mount_point())
        .status()
        .expect("mount should be installed");
    assert!(
        tools_mounted.success(),
        "tools mount failed; this test needs root"
    );
    assert_eq!(
        fs::read(scratch.tools_mount_point().join("vmlord-agent"))
            .expect("the agent should be readable"),
        agent
    );
}
