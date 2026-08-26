//! Fetching the image a release means and opening it as a disk, in one call.

mod support;

use std::{fs, path::PathBuf, sync::atomic::AtomicBool};

use sha2::{Digest, Sha256};
use support::TestServer;
use vmlord_core::{ProgressPublisher, ubuntu};
use vmlord_image::{DistroProfile, open_cloud_image};

/// Bigger than the fixture's disk, so capacity is not what is under test.
const CAPACITY: u64 = 1024 * 1024;

fn fixture() -> Vec<u8> {
    fs::read(
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/qcow2"))
            .join("sparse.qcow2"),
    )
    .expect("the qcow2 fixture should be readable")
}

/// The checksum list the server publishes, computed from the fixture itself so
/// the two cannot drift apart.
fn checksums(image: &[u8], file_name: &str) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(image);
    let sum: String = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{sum} *{file_name}\n").into_bytes()
}

fn cache_directory(tag: &str) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("vmlord-open-{tag}-{unique}"));
    fs::create_dir_all(&path).expect("the cache directory should be created");
    path
}

/// A profile pointing at the loopback server instead of the internet.
fn profile_for(server: &TestServer) -> DistroProfile {
    DistroProfile {
        directory_template: format!("{}{{release}}/", server.base_url()),
        ..ubuntu()
    }
}

fn served(image: &[u8]) -> Vec<(String, Vec<u8>)> {
    let file_name = ubuntu().file_name("24.04");
    vec![
        ("SHA256SUMS".to_owned(), checksums(image, &file_name)),
        (file_name, image.to_vec()),
    ]
}

#[test]
fn a_release_becomes_an_open_disk() {
    let image = fixture();
    let server = TestServer::start_directory(served(&image));
    let directory = cache_directory("open");

    let opened = open_cloud_image(
        &profile_for(&server),
        "24.04",
        &directory,
        CAPACITY,
        &ProgressPublisher::default(),
        &AtomicBool::new(false),
    )
    .expect("the server publishes this release");

    assert_eq!(opened.virtual_size(), 64 * 1024 + 512);
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn a_release_the_server_does_not_publish_is_reported_by_name() {
    let server = TestServer::start_directory(Vec::new());
    let directory = cache_directory("missing");

    let error = open_cloud_image(
        &profile_for(&server),
        "24.04",
        &directory,
        CAPACITY,
        &ProgressPublisher::default(),
        &AtomicBool::new(false),
    )
    .expect_err("there is no checksum list to read");

    assert!(error.to_string().contains("404"), "got {error}");
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn an_image_that_does_not_hash_to_what_the_list_says_is_refused() {
    let image = fixture();
    let mut served = served(&image);
    // The list stays as it is; the body served under that name changes.
    served[1].1 = vec![0; image.len()];
    let server = TestServer::start_directory(served);
    let directory = cache_directory("mismatch");

    let error = open_cloud_image(
        &profile_for(&server),
        "24.04",
        &directory,
        CAPACITY,
        &ProgressPublisher::default(),
        &AtomicBool::new(false),
    )
    .expect_err("the body is not the image the list names");

    assert!(error.to_string().contains("hashes to"), "got {error}");
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn an_image_too_big_for_the_disk_is_refused_before_a_byte_is_copied() {
    let image = fixture();
    let server = TestServer::start_directory(served(&image));
    let directory = cache_directory("capacity");

    let error = open_cloud_image(
        &profile_for(&server),
        "24.04",
        &directory,
        64 * 1024,
        &ProgressPublisher::default(),
        &AtomicBool::new(false),
    )
    .expect_err("the image's disk does not fit in 64 KiB");

    // The brief's own assertion checks for "64" in the message, expecting it to
    // appear in the capacity figure. The real `Qcow2Error::TooLarge` message
    // reports 65536 and 66048 -- neither contains "64" as a substring -- so this
    // asserts on "does not fit", the wording that is actually unique to the
    // capacity refusal (as opposed to, say, a checksum or network error).
    assert!(error.to_string().contains("does not fit"), "got {error}");
    fs::remove_dir_all(&directory).unwrap();
}
