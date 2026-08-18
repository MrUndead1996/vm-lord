//! Where a release keeps the payload archives it ships.
//!
//! One rule, written once: `cargo dist` copies to it and the running
//! application reads from it, and a disagreement between the two would be a
//! release whose payload is invisible with nothing to say so.

use std::path::{Path, PathBuf};

/// The child of the executable's directory holding shipped archives.
pub const LOCAL_ARCHIVE_DIRECTORY: &str = "gpu-payload";

/// The archive for `payload_id` below `directory`.
///
/// `directory` is the one holding the executable. It is a parameter rather
/// than read from `current_exe` here so that this can be tested, and so that
/// the build tool -- which is placing files into a distribution rather than
/// running from one -- can use the same rule.
pub fn local_archive_path(directory: &Path, payload_id: &str) -> PathBuf {
    directory
        .join(LOCAL_ARCHIVE_DIRECTORY)
        .join(format!("{payload_id}.zip"))
}

/// The entry document for `payload_id` below `directory`.
///
/// The pair is named by the payload's own ID: one directory listing then says
/// which payloads a release carries, and an entry cannot describe an archive
/// other than the one beside it.
pub fn local_entry_path(directory: &Path, payload_id: &str) -> PathBuf {
    directory
        .join(LOCAL_ARCHIVE_DIRECTORY)
        .join(format!("{payload_id}.json"))
}

/// The directory a release keeps its payload pairs in.
pub(crate) fn local_payload_directory(directory: &Path) -> PathBuf {
    directory.join(LOCAL_ARCHIVE_DIRECTORY)
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
