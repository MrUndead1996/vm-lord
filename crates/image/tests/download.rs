mod support;

use std::{fs, path::PathBuf, sync::atomic::AtomicBool};

use support::{Behaviour, TestServer};
use vmlord_core::{DownloadPhase, ProgressPublisher};
use vmlord_image::{DownloadError, ImageDownloadRequest, fetch_image};

/// The bytes every test downloads, and the sum they hash to.
fn image_body() -> Vec<u8> {
    (0u8..=255).cycle().take(64 * 1024 + 17).collect()
}

fn image_sum() -> String {
    // Computed by the test itself so the fixture and the expectation cannot
    // drift apart.
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(image_body());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn cache_directory(tag: &str) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("vmlord-download-{tag}-{unique}"));
    fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn an_image_is_downloaded_verified_and_named_after_its_checksum() {
    let directory = cache_directory("fresh");
    let server = TestServer::start(image_body(), Behaviour::Ranged);
    let sum = image_sum();
    let publisher = ProgressPublisher::default();

    let path = fetch_image(
        ImageDownloadRequest {
            url: server.url(),
            expected_sha256: &sum,
            cache_directory: &directory,
        },
        &publisher,
        &AtomicBool::new(false),
    )
    .unwrap();

    assert_eq!(path, directory.join(format!("{sum}.img")));
    assert_eq!(fs::read(&path).unwrap(), image_body());
    assert!(
        !directory.join(format!("{sum}.img.part")).exists(),
        "the partial file is renamed into place, not left behind holding a copy"
    );
    assert_eq!(publisher.snapshot(), Some(DownloadPhase::Completed));

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_cached_image_is_used_without_touching_the_network() {
    let directory = cache_directory("hit");
    let server = TestServer::start(image_body(), Behaviour::Ranged);
    let sum = image_sum();
    fs::write(directory.join(format!("{sum}.img")), image_body()).unwrap();
    let publisher = ProgressPublisher::default();

    let path = fetch_image(
        ImageDownloadRequest {
            url: server.url(),
            expected_sha256: &sum,
            cache_directory: &directory,
        },
        &publisher,
        &AtomicBool::new(false),
    )
    .unwrap();

    assert_eq!(path, directory.join(format!("{sum}.img")));
    assert!(
        server.ranges_seen().is_empty(),
        "a cache hit must not make a request at all"
    );
    assert_eq!(publisher.snapshot(), Some(DownloadPhase::Completed));

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_cached_image_left_truncated_by_an_earlier_run_is_replaced() {
    let directory = cache_directory("truncated-cache");
    let server = TestServer::start(image_body(), Behaviour::Ranged);
    let sum = image_sum();
    let cached = directory.join(format!("{sum}.img"));
    fs::write(&cached, &image_body()[..1000]).unwrap();

    let path = fetch_image(
        ImageDownloadRequest {
            url: server.url(),
            expected_sha256: &sum,
            cache_directory: &directory,
        },
        &ProgressPublisher::default(),
        &AtomicBool::new(false),
    )
    .unwrap();

    assert_eq!(fs::read(&path).unwrap(), image_body());
    assert_eq!(
        server.ranges_seen().len(),
        1,
        "checking the sum on every cache hit is the only thing that catches this"
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn an_image_that_does_not_hash_to_what_was_promised_is_refused() {
    let directory = cache_directory("mismatch");
    let server = TestServer::start(image_body(), Behaviour::Ranged);
    let wrong_sum = "0".repeat(64);

    let error = fetch_image(
        ImageDownloadRequest {
            url: server.url(),
            expected_sha256: &wrong_sum,
            cache_directory: &directory,
        },
        &ProgressPublisher::default(),
        &AtomicBool::new(false),
    )
    .expect_err("an image that hashes differently must never enter the cache");

    assert!(
        matches!(error, DownloadError::ChecksumMismatch { .. }),
        "got {error:?}"
    );
    assert!(!directory.join(format!("{wrong_sum}.img")).exists());
    assert_eq!(
        fs::metadata(directory.join(format!("{wrong_sum}.img.part")))
            .unwrap()
            .len(),
        0,
        "the bad bytes are dropped, but the .part keeps its lock and its name"
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_checksum_that_is_not_one_is_refused_before_any_request() {
    let directory = cache_directory("bad-sum");
    let server = TestServer::start(image_body(), Behaviour::Ranged);

    let error = fetch_image(
        ImageDownloadRequest {
            url: server.url(),
            expected_sha256: "not-a-checksum",
            cache_directory: &directory,
        },
        &ProgressPublisher::default(),
        &AtomicBool::new(false),
    )
    .expect_err("the caller should learn this in milliseconds, not after 600 MB");

    assert!(matches!(error, DownloadError::InvalidChecksum(_)));
    assert!(server.ranges_seen().is_empty());

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn an_image_another_downloader_finished_first_is_adopted() {
    let directory = cache_directory("race-rename");
    let sum = image_sum();
    // The winner's file is already in place; our download has to notice rather
    // than replace a file the importer may have open.
    fs::write(directory.join(format!("{sum}.img")), image_body()).unwrap();
    let server = TestServer::start(image_body(), Behaviour::Ranged);

    let path = fetch_image(
        ImageDownloadRequest {
            url: server.url(),
            expected_sha256: &sum,
            cache_directory: &directory,
        },
        &ProgressPublisher::default(),
        &AtomicBool::new(false),
    )
    .unwrap();

    assert_eq!(path, directory.join(format!("{sum}.img")));
    assert_eq!(fs::read(&path).unwrap(), image_body());

    fs::remove_dir_all(directory).unwrap();
}
