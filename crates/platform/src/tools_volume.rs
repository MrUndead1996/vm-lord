//! The volume a guest installs the agent from, and keeping it current.
//!
//! The agent is the one part of VMLord that lives inside the guest, and until
//! this module it arrived exactly once: `create` wrote it onto `tools.iso` and
//! the first boot's cloud-init copied it out. Every release after that shipped
//! a host that talked to an agent frozen at the VM's creation date -- and a
//! feature whose guest half lives in the agent never reached a VM that already
//! existed.
//!
//! The volume itself was always the right place for the answer: it is attached
//! to every VM that has one for the life of that VM, not only for its first
//! boot, and what it means has never been anything but "what the host hands
//! the guest to install". So a start rewrites it when the agent beside the
//! executable is not the one already on it, and the agent in the guest
//! installs from it. Neither half needs anything that is not already there.
//!
//! Rewritten rather than added to: the image is one file and a deterministic
//! build of it, so "does this volume already carry this agent" is a byte
//! comparison rather than a version anybody has to maintain.

use std::{fs, path::Path};

use crate::layout;

/// The agent's name beside the VMLord executable, which is also its name
/// inside the volume.
const AGENT_FILE_NAME: &str = "vmlord-agent";

/// Reads the guest agent bundled beside the running VMLord executable.
///
/// A missing binary is a packaging problem but not a reason to reject a cloud
/// VM: its normal cloud-init provisioning can still complete without the
/// optional agent service.
pub(crate) fn agent_beside_executable() -> Option<Vec<u8>> {
    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(error) => {
            tracing::warn!(
                "cannot locate the VMLord executable to find {AGENT_FILE_NAME}: {error}"
            );
            return None;
        }
    };
    let agent_path = executable.with_file_name(AGENT_FILE_NAME);
    match fs::read(&agent_path) {
        Ok(agent) => Some(agent),
        Err(error) => {
            tracing::warn!(
                "cannot read the guest agent at {}: {error}; cloud VMs will not include a tools volume",
                agent_path.display()
            );
            None
        }
    }
}

/// Puts this release's agent on the VM's tools volume, so that the guest
/// installs it on its next boot.
///
/// Best effort and never a reason a VM does not start: a volume that could not
/// be rewritten leaves the guest running the agent it already has, which is
/// what it would have done anyway.
///
/// A VM with no `tools.iso` is left alone rather than given one. The volume is
/// an attachment of the compute system, written into its configuration when
/// the VM was created; a VM created from local media has no such attachment,
/// and a file nothing is attached to is a file nobody would ever read.
pub(crate) fn refresh(vm_name: &str, vm_directory: &Path) {
    let path = layout::tools_path(vm_directory);
    let Ok(present) = fs::read(&path) else {
        return;
    };
    let Some(agent) = agent_beside_executable() else {
        return;
    };
    let Some(image) = image_to_write(&present, &agent) else {
        return;
    };

    match fs::write(&path, &image) {
        Ok(()) => tracing::info!(
            "the tools volume of VM \"{vm_name}\" now carries this release's guest agent, which \
             the guest installs on its next boot"
        ),
        Err(error) => tracing::warn!(
            "the tools volume of VM \"{vm_name}\" at {} could not be rewritten: {error}; its \
             guest keeps the agent it has",
            path.display()
        ),
    }
}

/// The image to write over `present`, or `None` when it already carries
/// `agent`.
///
/// The whole image is compared rather than the file inside it: the build is
/// deterministic and has no timestamps, so two volumes carrying the same agent
/// are the same bytes -- and comparing what is actually written is one fact
/// rather than two that could disagree.
fn image_to_write(present: &[u8], agent: &[u8]) -> Option<Vec<u8>> {
    let image = vmlord_seed::tools_image(agent);
    (image != present).then_some(image)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::{image_to_write, refresh};
    use crate::layout;

    static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

    struct TemporaryDirectory(PathBuf);

    impl TemporaryDirectory {
        fn new(label: &str) -> Self {
            let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "vmlord-tools-volume-{label}-{}-{sequence}",
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
    fn a_volume_that_already_carries_this_agent_is_left_alone() {
        let present = vmlord_seed::tools_image(b"the agent this release ships");

        assert_eq!(
            image_to_write(&present, b"the agent this release ships"),
            None,
            "a start rewrites nothing when the guest would install what it already has"
        );
    }

    #[test]
    fn a_volume_carrying_an_older_agent_is_rewritten_with_this_one() {
        let present = vmlord_seed::tools_image(b"the agent this VM was created with");

        assert_eq!(
            image_to_write(&present, b"the agent this release ships"),
            Some(vmlord_seed::tools_image(b"the agent this release ships")),
            "the volume is rewritten with the image this release's agent makes"
        );
    }

    #[test]
    fn a_vm_without_a_tools_volume_is_not_given_one() {
        let directory = TemporaryDirectory::new("no-volume");

        refresh("local-media", &directory.0);

        assert!(
            !layout::tools_path(&directory.0).exists(),
            "the volume is an attachment of the compute system, and a VM created from local \
             media has none"
        );
    }
}
