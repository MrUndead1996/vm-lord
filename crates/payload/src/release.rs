//! Where a release keeps the payload archives it ships.
//!
//! One rule, written once: `cargo dist` copies to it and the running
//! application reads from it, and a disagreement between the two would be a
//! release whose payload is invisible with nothing to say so.
//!
//! The subdirectory is a parameter rather than a constant, because it is the
//! only thing about this layout that differs between kinds of payload.

use std::path::{Path, PathBuf};

/// The archive for `payload_id` below `directory`.
///
/// `directory` is the one holding the executable. It is a parameter rather
/// than read from `current_exe` here so that this can be tested, and so that
/// the build tool -- which is placing files into a distribution rather than
/// running from one -- can use the same rule.
#[must_use]
pub fn archive_path(directory: &Path, subdirectory: &str, payload_id: &str) -> PathBuf {
    payload_directory(directory, subdirectory).join(format!("{payload_id}.zip"))
}

/// The entry document for `payload_id` below `directory`.
///
/// The pair is named by the payload's own ID: one directory listing then says
/// which payloads a release carries, and an entry cannot describe an archive
/// other than the one beside it.
#[must_use]
pub fn entry_path(directory: &Path, subdirectory: &str, payload_id: &str) -> PathBuf {
    payload_directory(directory, subdirectory).join(format!("{payload_id}.json"))
}

/// The directory a release keeps one kind of payload's pairs in.
#[must_use]
pub fn payload_directory(directory: &Path, subdirectory: &str) -> PathBuf {
    directory.join(subdirectory)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{archive_path, entry_path};

    #[test]
    fn each_payload_kind_keeps_its_pair_in_its_own_directory() {
        assert_eq!(
            archive_path(
                Path::new("dist"),
                "display-payload",
                "display-ubuntu-24.04-amd64-0.1.0"
            ),
            PathBuf::from("dist")
                .join("display-payload")
                .join("display-ubuntu-24.04-amd64-0.1.0.zip")
        );
        assert_eq!(
            entry_path(
                Path::new("dist"),
                "gpu-payload",
                "ubuntu-26.04-amd64-7.0.0-28-v2"
            ),
            PathBuf::from("dist")
                .join("gpu-payload")
                .join("ubuntu-26.04-amd64-7.0.0-28-v2.json")
        );
    }
}
