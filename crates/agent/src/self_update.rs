//! Installing the agent the host left on the tools volume.
//!
//! The agent used to arrive exactly once, at a VM's first boot: cloud-init
//! mounted `VMLTOOLS`, copied the binary out, and never looked at the volume
//! again. Every release afterwards shipped a host talking to whichever agent
//! the VM happened to be created with, so a feature whose guest half lives
//! here never reached a VM that already existed.
//!
//! The volume is attached for the life of the VM, and the host rewrites it
//! while the VM is down. So this runs before anything else the agent does: it
//! mounts the volume, and if what the host put there is not what is installed,
//! it installs it and asks to be restarted. `Restart=always` in the unit is
//! what starts the new binary -- the same arrangement that recovers from a
//! crash, used for a replacement that is deliberate.
//!
//! Replacing a running program's own file is what `install_file` in
//! `display_kernel` already does for the display services, and for the same
//! reason: an executable that is open cannot be written over, but it can be
//! unlinked and written again. This process keeps running from the inode it
//! started with until it exits.
//!
//! There is no version to compare and none to maintain. The bytes are the
//! answer: what is on the volume is what this host wants installed.

use std::{fs, io, path::Path, time::Duration};

use crate::command;

/// The label the tools volume is mounted by, as cloud-init mounts it.
const TOOLS_VOLUME_LABEL: &str = "VMLTOOLS";
/// Where it is mounted, which is where cloud-init mounts it too.
const TOOLS_MOUNT: &str = "/run/vmlord-tools";
/// The agent's name inside the volume.
const AGENT_FILE: &str = "vmlord-agent";
/// Where the unit starts the agent from, which is what gets replaced.
const INSTALLED_AGENT: &str = "/usr/local/lib/vmlord/vmlord-agent";
/// The mode an installed agent has, as cloud-init installs it.
const AGENT_MODE: u32 = 0o755;

/// A mount and an unmount are two syscalls on a volume that is already
/// attached; a budget this size is for a device that is not answering.
const MOUNT_BUDGET: Duration = Duration::from_secs(15);

/// Installs the agent from the tools volume, and says whether this process
/// should now make way for it.
///
/// Never fatal and never a reason not to serve the host: a guest that could
/// not read the volume keeps the agent it has, which is what it would have
/// kept anyway. What it reports goes to the journal through the unit's
/// standard error, because this runs before there is a host to tell.
pub fn apply() -> Replacement {
    let mounted = mount();
    let source = Path::new(TOOLS_MOUNT).join(AGENT_FILE);
    let replacement = match replace_if_different(&source, Path::new(INSTALLED_AGENT)) {
        Ok(true) => {
            eprintln!(
                "vmlord-agent: installed the agent from {TOOLS_MOUNT} and is making way for it"
            );
            Replacement::Installed
        }
        Ok(false) => Replacement::None,
        Err(reason) => {
            eprintln!("vmlord-agent: {reason}; keeping the agent that is installed");
            Replacement::None
        }
    };
    if mounted {
        unmount();
    }

    replacement
}

/// What [`apply`] found to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Replacement {
    /// A newer agent is on disk, and this process should exit for it.
    Installed,
    /// What is installed is what the host wants installed.
    None,
}

/// Puts `source` in `destination`'s place unless it is already there, and says
/// whether it did.
///
/// # Errors
///
/// What could not be read or written, as a sentence. Every one of them leaves
/// the installed agent as it was: the replacement is a remove and a write, and
/// nothing is removed until the new bytes are in hand.
fn replace_if_different(source: &Path, destination: &Path) -> Result<bool, String> {
    let wanted = fs::read(source)
        .map_err(|error| format!("{} could not be read: {error}", source.display()))?;
    if fs::read(destination).is_ok_and(|installed| installed == wanted) {
        return Ok(false);
    }

    if let Some(directory) = destination.parent() {
        fs::create_dir_all(directory)
            .map_err(|error| format!("{} could not be created: {error}", directory.display()))?;
    }
    // Removed rather than truncated: this process is running from that file,
    // and a write through it would be refused with `ETXTBSY`. Unlinking it
    // leaves the running program its inode and the new bytes their own.
    if let Err(error) = fs::remove_file(destination)
        && error.kind() != io::ErrorKind::NotFound
    {
        return Err(format!(
            "{} could not be removed: {error}",
            destination.display()
        ));
    }
    fs::write(destination, &wanted)
        .map_err(|error| format!("{} could not be written: {error}", destination.display()))?;
    set_mode(destination, AGENT_MODE).map_err(|error| {
        format!(
            "{} could not be made executable: {error}",
            destination.display()
        )
    })?;

    Ok(true)
}

fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

/// Mounts the tools volume read-only, and says whether this call is the one
/// that mounted it.
///
/// A volume that is already mounted -- which is every first boot, where
/// cloud-init mounts it before this agent has ever run -- is not mounted
/// twice and not unmounted afterwards by a caller that did not mount it.
fn mount() -> bool {
    if Path::new(TOOLS_MOUNT).join(AGENT_FILE).exists() {
        return false;
    }
    if let Err(error) = fs::create_dir_all(TOOLS_MOUNT) {
        eprintln!("vmlord-agent: {TOOLS_MOUNT} could not be created: {error}");
        return false;
    }

    let outcome = command::run(
        "mount",
        &["-o", "ro", "-L", TOOLS_VOLUME_LABEL, TOOLS_MOUNT],
        &[],
        MOUNT_BUDGET,
    );
    if !outcome.succeeded() {
        eprintln!(
            "vmlord-agent: the {TOOLS_VOLUME_LABEL} volume could not be mounted at {TOOLS_MOUNT}"
        );
        return false;
    }

    true
}

/// Leaves nothing mounted behind, because nothing here reads the volume again.
fn unmount() {
    let outcome = command::run("umount", &[TOOLS_MOUNT], &[], MOUNT_BUDGET);
    if !outcome.succeeded() {
        eprintln!("vmlord-agent: {TOOLS_MOUNT} could not be unmounted");
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::replace_if_different;

    static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

    struct TemporaryDirectory(PathBuf);

    impl TemporaryDirectory {
        fn new(label: &str) -> Self {
            let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "vmlord-self-update-{label}-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TemporaryDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn an_agent_that_is_already_installed_is_left_running() {
        let directory = TemporaryDirectory::new("unchanged");
        let source = directory.0.join("vmlord-agent");
        let destination = directory.0.join("installed").join("vmlord-agent");
        fs::write(&source, b"the agent this host ships").unwrap();
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::write(&destination, b"the agent this host ships").unwrap();

        assert_eq!(
            replace_if_different(&source, &destination),
            Ok(false),
            "a boot where nothing changed replaces nothing and restarts nothing"
        );
    }

    #[test]
    fn a_newer_agent_replaces_the_installed_one_and_stays_executable() {
        let directory = TemporaryDirectory::new("replaced");
        let source = directory.0.join("vmlord-agent");
        let destination = directory.0.join("installed").join("vmlord-agent");
        fs::write(&source, b"the agent this host ships").unwrap();
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::write(&destination, b"the agent this VM was created with").unwrap();
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(replace_if_different(&source, &destination), Ok(true));
        assert_eq!(
            fs::read(&destination).unwrap(),
            b"the agent this host ships"
        );
        assert_eq!(
            fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
            0o755,
            "a binary systemd cannot execute is worse than the older one it replaced"
        );
    }

    #[test]
    fn an_agent_that_was_never_installed_is_installed() {
        let directory = TemporaryDirectory::new("absent");
        let source = directory.0.join("vmlord-agent");
        let destination = directory.0.join("installed").join("vmlord-agent");
        fs::write(&source, b"the agent this host ships").unwrap();

        assert_eq!(replace_if_different(&source, &destination), Ok(true));
        assert_eq!(
            fs::read(&destination).unwrap(),
            b"the agent this host ships"
        );
    }

    #[test]
    fn a_volume_with_no_agent_on_it_leaves_the_installed_one_alone() {
        let directory = TemporaryDirectory::new("no-source");
        let source = directory.0.join("vmlord-agent");
        let destination = directory.0.join("installed").join("vmlord-agent");
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::write(&destination, b"the agent this VM was created with").unwrap();

        assert!(
            replace_if_different(&source, &destination).is_err(),
            "an unreadable volume is reported, not acted on"
        );
        assert_eq!(
            fs::read(&destination).unwrap(),
            b"the agent this VM was created with",
            "nothing is removed until the new bytes are in hand"
        );
    }
}
