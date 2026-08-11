//! Where a downloaded image lives on disk and how its integrity is checked.

use std::{
    fs::File,
    io::Read,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

use sha2::{Digest, Sha256};
use vmlord_core::{DownloadPhase, ProgressThrottle};

use crate::error::{DownloadError, io_error};

/// How much is hashed between two cancellation checks.
const HASH_CHUNK: usize = 1024 * 1024;

/// The number of hex characters in a SHA256.
const CHECKSUM_LENGTH: usize = 64;

/// The longest extension taken from a URL, in characters.
const MAX_EXTENSION: usize = 8;

/// Accepts a SHA256 in either case and returns it lowercase.
///
/// Checked before any request goes out: a caller that got the checksum wrong
/// should learn it in milliseconds, not after several hundred megabytes.
pub(crate) fn normalized_checksum(value: &str) -> Result<String, DownloadError> {
    let trimmed = value.trim();
    if trimmed.len() != CHECKSUM_LENGTH || !trimmed.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(DownloadError::InvalidChecksum(trimmed.to_owned()));
    }
    Ok(trimmed.to_ascii_lowercase())
}

/// Names the cache entry for an image: its checksum, plus the extension the URL
/// carried so the file stays recognisable to a human and to tooling.
///
/// The name is the content, so two releases cannot collide and a file whose
/// name disagrees with its content cannot exist.
pub(crate) fn cache_file_name(url: &str, checksum: &str) -> String {
    match url_extension(url) {
        Some(extension) => format!("{checksum}.{extension}"),
        None => checksum.to_owned(),
    }
}

/// The extension of the last path segment of `url`, when it looks like one.
///
/// Anything that is not a short alphanumeric word is rejected rather than
/// pasted into a filename: the URL is attacker-influenced input.
fn url_extension(url: &str) -> Option<&str> {
    let without_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let path = without_scheme
        .split(['?', '#'])
        .next()
        .unwrap_or(without_scheme);
    let segment = path.rsplit('/').next()?;
    let extension = segment.rsplit_once('.')?.1;
    let sane = !extension.is_empty()
        && extension.len() <= MAX_EXTENSION
        && extension
            .chars()
            .all(|character| character.is_ascii_alphanumeric());
    sane.then_some(extension)
}

/// Hashes the file at `path`, reporting progress and honouring cancellation.
///
/// Run on every cache hit, not only after a download: it is the only thing that
/// tells a complete image from one an interrupted download left truncated, and
/// it costs seconds against re-fetching hundreds of megabytes.
pub(crate) fn file_checksum(
    path: &Path,
    progress: &mut ProgressThrottle<DownloadPhase>,
    cancel: &AtomicBool,
) -> Result<String, DownloadError> {
    let mut file = File::open(path).map_err(io_error("open the image for hashing", path))?;
    let total = file
        .metadata()
        .map_err(io_error("measure the image", path))?
        .len();
    checksum_reader(&mut file, total, path, progress, cancel)
}

/// Hashes `total` bytes out of `reader`, reporting progress and honouring
/// cancellation. `path` names the file only so failures can say which one.
///
/// Taken as a reader rather than a path because the partial download must be
/// hashed through the very handle that holds its lock. On Windows `LockFileEx`
/// is mandatory rather than advisory, so opening the same file a second time
/// and reading it fails with `ERROR_LOCK_VIOLATION` -- something no amount of
/// testing on Linux, where `flock` is advisory and the second read simply
/// succeeds, would ever reveal.
pub(crate) fn checksum_reader(
    reader: &mut impl Read,
    total: u64,
    path: &Path,
    progress: &mut ProgressThrottle<DownloadPhase>,
    cancel: &AtomicBool,
) -> Result<String, DownloadError> {
    log::debug!("hashing {} ({total} bytes)", path.display());

    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; HASH_CHUNK];
    let mut hashed = 0u64;
    progress.publish_now(DownloadPhase::Verifying { hashed, total });
    loop {
        if cancel.load(Ordering::Relaxed) {
            log::debug!("hashing {} was cancelled", path.display());
            return Err(DownloadError::Cancelled);
        }
        let read = reader
            .read(&mut buffer)
            .map_err(io_error("read the image for hashing", path))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        hashed += read as u64;
        progress.publish(DownloadPhase::Verifying { hashed, total });
    }
    progress.publish_now(DownloadPhase::Verifying { hashed, total });

    // In sha2 0.11 the digest no longer implements `LowerHex`, so
    // `format!("{digest:x}")` does not compile; the hex is built by hand.
    let digest = hasher.finalize();
    let checksum: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    log::debug!("{} hashes to {checksum}", path.display());
    Ok(checksum)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::atomic::{AtomicBool, Ordering},
        time::Duration,
    };

    use vmlord_core::{DownloadPhase, ProgressPublisher, ProgressThrottle};

    use super::{cache_file_name, file_checksum, normalized_checksum};
    use crate::error::DownloadError;

    const SUM: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn temporary_directory(tag: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("vmlord-image-{tag}-{unique}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn a_checksum_is_accepted_in_either_case_and_kept_lowercase() {
        assert_eq!(normalized_checksum(&SUM.to_uppercase()).unwrap(), SUM);
        assert_eq!(normalized_checksum(&format!("  {SUM}  ")).unwrap(), SUM);
    }

    #[test]
    fn a_checksum_of_the_wrong_shape_is_refused_before_anything_is_downloaded() {
        for candidate in ["", "abc", &SUM[1..], &format!("{}z", &SUM[1..])] {
            assert!(
                matches!(
                    normalized_checksum(candidate),
                    Err(DownloadError::InvalidChecksum(_))
                ),
                "{candidate:?} is not a SHA256"
            );
        }
    }

    #[test]
    fn a_cached_file_is_named_after_its_checksum_and_keeps_the_extension() {
        assert_eq!(
            cache_file_name("https://example.test/path/noble-cloudimg-amd64.img", SUM),
            format!("{SUM}.img")
        );
        assert_eq!(
            cache_file_name("https://example.test/image.qcow2?token=abc#frag", SUM),
            format!("{SUM}.qcow2")
        );
    }

    #[test]
    fn a_url_without_a_usable_extension_gives_a_bare_checksum() {
        assert_eq!(cache_file_name("https://example.test/download", SUM), SUM);
        assert_eq!(cache_file_name("https://example.test/", SUM), SUM);
        assert_eq!(
            cache_file_name("https://example.test/image.tar-gz", SUM),
            SUM,
            "a suffix with punctuation in it is not an extension"
        );
        assert_eq!(
            cache_file_name("https://example.test/image.verylongsuffix", SUM),
            SUM,
            "a suffix too long to be an extension is not pasted into a filename"
        );
    }

    #[test]
    fn only_the_last_suffix_of_a_multi_part_name_is_taken() {
        assert_eq!(
            cache_file_name("https://example.test/archive.tar.gz", SUM),
            format!("{SUM}.gz"),
            "the name is the checksum; the extension is only there to stay recognisable"
        );
    }

    #[test]
    fn hashing_a_file_reports_the_sum_and_the_progress_of_getting_there() {
        let directory = temporary_directory("hash");
        let path = directory.join("payload");
        fs::write(&path, b"vmlord").unwrap();
        let publisher = ProgressPublisher::default();
        let mut throttle = ProgressThrottle::with_interval(publisher.clone(), Duration::ZERO);

        let sum = file_checksum(&path, &mut throttle, &AtomicBool::new(false)).unwrap();

        assert_eq!(
            sum, "c423e3a9d7b4a6f1f03492cfded44b0b9c00c4c63f1ef3c410368e8a9ad3bcd2",
            "the sum of the bytes \"vmlord\""
        );
        assert_eq!(
            publisher.snapshot(),
            Some(DownloadPhase::Verifying {
                hashed: 6,
                total: 6
            })
        );

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn hashing_stops_when_the_download_is_cancelled() {
        let directory = temporary_directory("hash-cancel");
        let path = directory.join("payload");
        fs::write(&path, vec![0u8; 4 * 1024 * 1024]).unwrap();
        let mut throttle =
            ProgressThrottle::with_interval(ProgressPublisher::default(), Duration::ZERO);
        let cancel = AtomicBool::new(false);
        cancel.store(true, Ordering::Relaxed);

        let error = file_checksum(&path, &mut throttle, &cancel)
            .expect_err("a cancelled verification must not report a sum");

        assert!(matches!(error, DownloadError::Cancelled));

        fs::remove_dir_all(directory).unwrap();
    }
}
