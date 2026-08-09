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
fn a_cancelled_download_stops_and_keeps_what_it_had() {
    let directory = cache_directory("cancel");
    let sum = image_sum();
    let server = TestServer::start(image_body(), Behaviour::Ranged);
    // Bytes an earlier attempt had already fetched. Cancelling must leave them
    // alone; truncating here is what would make cancel-then-retry re-download
    // the whole image.
    let part_path = directory.join(format!("{sum}.img.part"));
    fs::write(&part_path, &image_body()[..1000]).unwrap();
    let cancel = AtomicBool::new(true);

    let error = fetch_image(
        ImageDownloadRequest {
            url: server.url(),
            expected_sha256: &sum,
            cache_directory: &directory,
        },
        &ProgressPublisher::default(),
        &cancel,
    )
    .expect_err("a cancelled download must not report success");

    assert!(matches!(error, DownloadError::Cancelled), "got {error:?}");
    assert_eq!(
        fs::read(&part_path).unwrap(),
        image_body()[..1000],
        "the partial file survives cancellation intact so the next run can resume it"
    );
    assert!(!directory.join(format!("{sum}.img")).exists());

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_second_download_of_the_same_image_is_refused_while_the_first_runs() {
    use std::sync::{Arc, Barrier};

    let directory = cache_directory("race");
    let sum = image_sum();
    let server = TestServer::start(image_body(), Behaviour::Ranged);

    // Hold the lock the way a running download holds it: the partial file is
    // opened and locked before any byte moves.
    let held = directory.join(format!("{sum}.img.part"));
    let lock_taken = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let holder = {
        let (lock_taken, release, held) =
            (Arc::clone(&lock_taken), Arc::clone(&release), held.clone());
        std::thread::spawn(move || {
            let file = fs::File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&held)
                .unwrap();
            file.try_lock().unwrap();
            lock_taken.wait();
            release.wait();
            drop(file);
        })
    };
    lock_taken.wait();

    let error = fetch_image(
        ImageDownloadRequest {
            url: server.url(),
            expected_sha256: &sum,
            cache_directory: &directory,
        },
        &ProgressPublisher::default(),
        &AtomicBool::new(false),
    )
    .expect_err("two downloaders must not write into one partial file");

    assert!(
        matches!(error, DownloadError::AlreadyInProgress { .. }),
        "got {error:?}"
    );
    assert!(
        server.ranges_seen().is_empty(),
        "the refusal must come before any bandwidth is spent"
    );

    release.wait();
    holder.join().unwrap();

    // With the lock gone, the same call goes through.
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
