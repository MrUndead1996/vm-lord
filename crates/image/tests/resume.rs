mod support;

use std::{fs, path::Path, path::PathBuf, sync::atomic::AtomicBool};

use support::{Behaviour, TestServer};
use vmlord_core::ProgressPublisher;
use vmlord_image::{DownloadError, ImageDownloadRequest, fetch_image};

fn image_body() -> Vec<u8> {
    (0u8..=255).cycle().take(64 * 1024 + 17).collect()
}

fn image_sum() -> String {
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
    let path = std::env::temp_dir().join(format!("vmlord-resume-{tag}-{unique}"));
    fs::create_dir_all(&path).unwrap();
    path
}

/// Leaves `prefix` bytes of the image in the partial file, as an interrupted
/// download would have.
fn seed_partial(directory: &Path, sum: &str, prefix: usize) {
    fs::write(
        directory.join(format!("{sum}.img.part")),
        &image_body()[..prefix],
    )
    .unwrap();
}

#[test]
fn an_interrupted_download_asks_only_for_the_rest() {
    let directory = cache_directory("resume");
    let sum = image_sum();
    seed_partial(&directory, &sum, 1000);
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

    assert_eq!(fs::read(&path).unwrap(), image_body());
    assert_eq!(
        server.ranges_seen(),
        vec![Some("bytes=1000-".to_owned())],
        "the whole point is not to fetch the first 1000 bytes twice"
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_server_that_ignores_the_range_still_yields_a_correct_image() {
    let directory = cache_directory("ignored");
    let sum = image_sum();
    seed_partial(&directory, &sum, 1000);
    let server = TestServer::start(image_body(), Behaviour::IgnoresRange);

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

    assert_eq!(
        fs::read(&path).unwrap(),
        image_body(),
        "appending a whole body to a partial one would double the first 1000 bytes"
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_range_the_server_rejects_leads_to_one_clean_restart() {
    let directory = cache_directory("rejected");
    let sum = image_sum();
    seed_partial(&directory, &sum, 1000);
    let server = TestServer::start(image_body(), Behaviour::RejectsRange);

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
        server.ranges_seen(),
        vec![Some("bytes=1000-".to_owned()), None],
        "exactly one retry, and it asks for the whole file"
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_body_cut_short_fails_and_keeps_what_arrived() {
    let directory = cache_directory("cut");
    let sum = image_sum();
    let server = TestServer::start(image_body(), Behaviour::Truncated { bytes: 4096 });

    let error = fetch_image(
        ImageDownloadRequest {
            url: server.url(),
            expected_sha256: &sum,
            cache_directory: &directory,
        },
        &ProgressPublisher::default(),
        &AtomicBool::new(false),
    )
    .expect_err("a body that stops early is not an image");

    assert!(matches!(error, DownloadError::Http(_)), "got {error:?}");
    assert_eq!(
        fs::metadata(directory.join(format!("{sum}.img.part")))
            .unwrap()
            .len(),
        4096,
        "the bytes that did arrive are kept so the next attempt can resume them"
    );
    assert!(!directory.join(format!("{sum}.img")).exists());

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_status_that_is_not_a_download_is_reported_as_itself() {
    let directory = cache_directory("not-found");
    let sum = image_sum();
    let server = TestServer::start(image_body(), Behaviour::NotFound);

    let error = fetch_image(
        ImageDownloadRequest {
            url: server.url(),
            expected_sha256: &sum,
            cache_directory: &directory,
        },
        &ProgressPublisher::default(),
        &AtomicBool::new(false),
    )
    .expect_err("404 is not an image");

    assert!(
        matches!(error, DownloadError::UnexpectedStatus { status: 404 }),
        "a wrong URL should say so plainly rather than as an opaque transport failure; got {error:?}"
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn the_next_attempt_resumes_what_the_cut_body_left() {
    let directory = cache_directory("cut-then-resume");
    let sum = image_sum();
    let cut = TestServer::start(image_body(), Behaviour::Truncated { bytes: 4096 });
    let _ = fetch_image(
        ImageDownloadRequest {
            url: cut.url(),
            expected_sha256: &sum,
            cache_directory: &directory,
        },
        &ProgressPublisher::default(),
        &AtomicBool::new(false),
    );

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

    assert_eq!(fs::read(&path).unwrap(), image_body());
    assert_eq!(server.ranges_seen(), vec![Some("bytes=4096-".to_owned())]);

    fs::remove_dir_all(directory).unwrap();
}
