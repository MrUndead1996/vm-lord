//! Where a release keeps the GPU payload archives it ships.
//!
//! The rule itself is `vmlord_payload::release`'s; what is here is the one
//! name that is the GPU payload's own.

use std::path::{Path, PathBuf};

/// The child of the executable's directory holding shipped archives.
pub const LOCAL_ARCHIVE_DIRECTORY: &str = "gpu-payload";

/// The archive for `payload_id` below `directory`.
#[must_use]
pub fn local_archive_path(directory: &Path, payload_id: &str) -> PathBuf {
    vmlord_payload::release::archive_path(directory, LOCAL_ARCHIVE_DIRECTORY, payload_id)
}

/// The entry document for `payload_id` below `directory`.
#[must_use]
pub fn local_entry_path(directory: &Path, payload_id: &str) -> PathBuf {
    vmlord_payload::release::entry_path(directory, LOCAL_ARCHIVE_DIRECTORY, payload_id)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{local_archive_path, local_entry_path};

    #[test]
    fn a_release_keeps_each_payload_under_its_own_id() {
        assert_eq!(
            local_archive_path(Path::new("dist"), "ubuntu-26.04-amd64-7.0.0-28-v1"),
            PathBuf::from("dist")
                .join("gpu-payload")
                .join("ubuntu-26.04-amd64-7.0.0-28-v1.zip")
        );
    }

    #[test]
    fn a_release_keeps_each_entry_beside_its_archive() {
        assert_eq!(
            local_entry_path(Path::new("dist"), "ubuntu-26.04-amd64-7.0.0-28-v1"),
            PathBuf::from("dist")
                .join("gpu-payload")
                .join("ubuntu-26.04-amd64-7.0.0-28-v1.json")
        );
    }
}
