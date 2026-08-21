//! Mounting the one display payload share.
//!
//! One share and one mount point, which is what makes this shorter than its
//! GPU neighbour: there is nothing to reconcile, no manifest to diff and no
//! linker to tell. What it does share is the 9p mount itself, which is a
//! property of Hyper-V's Plan9 server rather than of what is being carried.

use std::{fs, path::Path};

use vmlord_agent_protocol::v1::{DisplayMount, DisplayMountState, DisplayShare};

use crate::{
    display_kernel::PAYLOAD_MOUNT,
    gpu_mountinfo,
    gpu_mounts::{MOUNTINFO, mount_plan9_share},
};

/// The only share name this agent will mount a display payload from.
pub const DISPLAY_PAYLOAD_SHARE: &str = "vmlord.display.payload";

/// Mounts the display payload share, or says why it did not.
///
/// A share by any other name is refused rather than mounted: the host names
/// what it offers, and a guest that mounts whatever it is handed is a guest
/// with no boundary at all.
pub fn attach(share: &DisplayShare) -> DisplayMount {
    if share.name != DISPLAY_PAYLOAD_SHARE {
        return DisplayMount {
            name: share.name.clone(),
            mount_point: String::new(),
            state: i32::from(DisplayMountState::Refused),
            message: format!(
                "this agent mounts {DISPLAY_PAYLOAD_SHARE} and no other display share"
            ),
        };
    }

    let path = Path::new(PAYLOAD_MOUNT);
    if is_mounted(share.name.as_str()) {
        return DisplayMount {
            name: share.name.clone(),
            mount_point: PAYLOAD_MOUNT.to_owned(),
            state: i32::from(DisplayMountState::AlreadyMounted),
            message: format!("{PAYLOAD_MOUNT} already carries this share"),
        };
    }

    if let Err(error) = fs::create_dir_all(path) {
        return DisplayMount {
            name: share.name.clone(),
            mount_point: PAYLOAD_MOUNT.to_owned(),
            state: i32::from(DisplayMountState::Failed),
            message: format!("{PAYLOAD_MOUNT} could not be created: {error}"),
        };
    }

    match mount_plan9_share(&share.name, path) {
        Ok(()) => DisplayMount {
            name: share.name.clone(),
            mount_point: PAYLOAD_MOUNT.to_owned(),
            state: i32::from(DisplayMountState::Mounted),
            message: format!("mounted at {PAYLOAD_MOUNT}"),
        },
        Err(error) => DisplayMount {
            name: share.name.clone(),
            mount_point: PAYLOAD_MOUNT.to_owned(),
            state: i32::from(DisplayMountState::Failed),
            message: format!("{PAYLOAD_MOUNT} could not be mounted: {error}"),
        },
    }
}

/// Whether the kernel already has this share at the mount point.
fn is_mounted(share: &str) -> bool {
    let mountinfo = fs::read_to_string(MOUNTINFO).unwrap_or_default();
    gpu_mountinfo::parse(&mountinfo)
        .into_iter()
        .any(|mounted| mounted.path == Path::new(PAYLOAD_MOUNT) && mounted.share == share)
}

#[cfg(test)]
mod tests {
    use vmlord_agent_protocol::v1::{DisplayMountState, DisplayShare};

    use super::{DISPLAY_PAYLOAD_SHARE, attach};

    #[test]
    fn a_share_by_another_name_is_refused_rather_than_mounted() {
        let mount = attach(&DisplayShare {
            name: "vmlord.gpu.payload".to_owned(),
        });

        assert_eq!(mount.state(), DisplayMountState::Refused);
        assert!(mount.mount_point.is_empty(), "nothing was mounted anywhere");
        assert!(mount.message.contains(DISPLAY_PAYLOAD_SHARE));
    }

    #[test]
    fn the_share_this_agent_mounts_is_named_once() {
        assert_eq!(DISPLAY_PAYLOAD_SHARE, "vmlord.display.payload");
    }
}
