//! Reading the payloads a release carries beside its executable.
//!
//! Four rules, and they are rules about releases rather than about what a
//! payload holds, which is why they are here and selection is not: a missing
//! directory is an empty catalog, a file that is there and wrong fails the
//! whole read, an entry must be named for the payload it declares, and its
//! archive must be beside it.

use std::{ffi::OsStr, fs, path::Path};

use crate::{PayloadEntry, PayloadError, release};

/// Every entry of one kind a release carries, in no particular order.
///
/// A child that is not there, cannot be listed, or holds no entry reads as no
/// entries rather than as an error: a build without a payload is a build
/// without that feature, and both features this serves are best effort.
///
/// A file that *is* there and is wrong fails the read. That is a broken
/// release, and a silent absence is the worst way to learn of one. An archive
/// nothing claims is ignored, because failing over a leftover file would be a
/// rule worse than the problem.
///
/// # Errors
///
/// [`PayloadError::InvalidCatalog`] for an entry that does not parse, does not
/// validate, is not named for its payload ID, or has no archive beside it;
/// [`PayloadError::Io`] for a file that is listed and cannot be read.
pub fn read_release_directory<E: PayloadEntry>(
    directory: &Path,
    subdirectory: &str,
) -> Result<Vec<E>, PayloadError> {
    let payloads = release::payload_directory(directory, subdirectory);
    let Ok(listing) = fs::read_dir(&payloads) else {
        return Ok(Vec::new());
    };

    let mut entries = Vec::new();
    for item in listing {
        let Ok(item) = item else {
            continue;
        };
        let path = item.path();
        if path.extension().and_then(OsStr::to_str) != Some("json") {
            continue;
        }
        let bytes = fs::read(&path)
            .map_err(|error| PayloadError::io("read payload entry", path.clone(), error))?;
        let entry = E::from_json(&bytes)?;
        if path.file_stem().and_then(OsStr::to_str) != Some(entry.payload_id()) {
            return Err(PayloadError::InvalidCatalog(format!(
                "{} does not name its payload ID {}",
                path.display(),
                entry.payload_id()
            )));
        }
        let archive = release::archive_path(directory, subdirectory, entry.payload_id());
        if !archive.is_file() {
            return Err(PayloadError::InvalidCatalog(format!(
                "payload {} has no archive at {}",
                entry.payload_id(),
                archive.display()
            )));
        }
        entries.push(entry);
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use crate::{PayloadError, test_kind::TestEntry};

    use super::read_release_directory;

    const ZERO: &str = "0000000000000000000000000000000000000000000000000000000000000000";
    static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

    struct TemporaryDirectory(PathBuf);

    impl TemporaryDirectory {
        fn new(label: &str) -> Self {
            let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "vmlord-payload-catalog-{label}-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TemporaryDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn entry_json(payload_id: &str) -> String {
        format!(
            r#"{{"payload_id":"{payload_id}","archive_sha256":"{ZERO}",
             "payload_manifest_sha256":"{ZERO}","expanded_size_limit":16,"file_count_limit":4}}"#
        )
    }

    fn write_pair(directory: &Path, subdirectory: &str, file_stem: &str, document: &str) {
        let payloads = directory.join(subdirectory);
        fs::create_dir_all(&payloads).unwrap();
        fs::write(payloads.join(format!("{file_stem}.json")), document).unwrap();
        fs::write(payloads.join(format!("{file_stem}.zip")), b"archive").unwrap();
    }

    #[test]
    fn a_release_without_this_kind_of_payload_reads_as_an_empty_list() {
        let temporary = TemporaryDirectory::new("absent");

        let entries: Vec<TestEntry> =
            read_release_directory(temporary.path(), "display-payload").unwrap();

        assert!(entries.is_empty(), "nothing is an answer, not an error");
    }

    #[test]
    fn an_archive_nothing_claims_is_ignored() {
        let temporary = TemporaryDirectory::new("stray");
        fs::create_dir(temporary.path().join("display-payload")).unwrap();
        fs::write(
            temporary.path().join("display-payload").join("stray.zip"),
            b"archive",
        )
        .unwrap();

        let entries: Vec<TestEntry> =
            read_release_directory(temporary.path(), "display-payload").unwrap();

        assert!(entries.is_empty());
    }

    #[test]
    fn an_entry_file_that_is_there_and_wrong_fails_the_read() {
        let unreadable = TemporaryDirectory::new("broken-json");
        write_pair(unreadable.path(), "display-payload", "a", "{not json");
        assert!(read_release_directory::<TestEntry>(unreadable.path(), "display-payload").is_err());

        let misnamed = TemporaryDirectory::new("misnamed");
        write_pair(
            misnamed.path(),
            "display-payload",
            "not-its-id",
            &entry_json("real-id"),
        );
        assert!(matches!(
            read_release_directory::<TestEntry>(misnamed.path(), "display-payload"),
            Err(PayloadError::InvalidCatalog(_))
        ));

        let archiveless = TemporaryDirectory::new("no-archive");
        write_pair(
            archiveless.path(),
            "display-payload",
            "real-id",
            &entry_json("real-id"),
        );
        fs::remove_file(
            archiveless
                .path()
                .join("display-payload")
                .join("real-id.zip"),
        )
        .unwrap();
        assert!(matches!(
            read_release_directory::<TestEntry>(archiveless.path(), "display-payload"),
            Err(PayloadError::InvalidCatalog(_))
        ));
    }

    #[test]
    fn one_kind_does_not_read_anothers_directory() {
        let temporary = TemporaryDirectory::new("two-kinds");
        write_pair(
            temporary.path(),
            "gpu-payload",
            "real-id",
            &entry_json("real-id"),
        );

        let entries: Vec<TestEntry> =
            read_release_directory(temporary.path(), "display-payload").unwrap();

        assert!(entries.is_empty());
    }
}
