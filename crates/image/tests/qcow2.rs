//! Reading the fixtures in `tests/fixtures/qcow2`, which qemu-img wrote.
//!
//! The guest content of the two content fixtures is a pattern rather than a
//! stored copy: `reference` below recomputes what `generate.sh` wrote, so a
//! reader that returns a plausible-looking disk with one cluster out of place
//! still fails. Both fixtures hold the same disk, so the compressed one is
//! checked against the same bytes as the sparse one -- which is the only way to
//! know that inflating a cluster produced the cluster that went in.

use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
};

use vmlord_image::{Qcow2Error, Qcow2Image};

/// Bigger than any fixture's disk, so capacity is not what is under test.
const CAPACITY: u64 = 1024 * 1024;

/// The virtual size every content fixture declares: 64 KiB and one sector, so
/// that the last cluster hangs over the end of the disk.
const VIRTUAL_SIZE: u64 = 64 * 1024 + 512;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/qcow2")).join(name)
}

/// The disk the content fixtures hold, recomputed rather than stored.
///
/// Must agree with the loop in `generate.sh`: 512-byte sectors filled with
/// `(n * 13 + 7) % 256`, except in the 4096-byte clusters 0, 4 and 5, which are
/// zero and which qemu-img therefore left unallocated.
fn reference() -> Vec<u8> {
    const SECTOR: usize = 512;
    const CLUSTER: usize = 4096;
    const HOLES: [usize; 3] = [0, 4, 5];

    let mut disk = Vec::with_capacity(VIRTUAL_SIZE as usize);
    for sector in 0..VIRTUAL_SIZE as usize / SECTOR {
        let byte = if HOLES.contains(&(sector * SECTOR / CLUSTER)) {
            0
        } else {
            ((sector * 13 + 7) % 256) as u8
        };
        disk.extend(std::iter::repeat_n(byte, SECTOR));
    }
    disk
}

fn open(name: &str) -> Result<Qcow2Image, Qcow2Error> {
    Qcow2Image::open(&fixture(name), CAPACITY)
}

fn read_whole_disk(name: &str) -> Vec<u8> {
    let mut image = open(name).expect("the fixture should open");
    let mut disk = Vec::new();
    image
        .read_to_end(&mut disk)
        .expect("the fixture should read");
    disk
}

#[test]
fn the_disk_inside_a_sparse_image_reads_back_byte_for_byte() {
    let image = open("sparse.qcow2").expect("the fixture should open");

    assert_eq!(image.virtual_size(), VIRTUAL_SIZE);
    assert_eq!(image.cluster_size(), 4096);
    assert_eq!(read_whole_disk("sparse.qcow2"), reference());
}

#[test]
fn a_hole_reads_as_the_zeros_the_guest_would_see() {
    let disk = read_whole_disk("sparse.qcow2");

    // Cluster 0 is unallocated, which is the case a reader is most likely to
    // get wrong: it is the one the crate behind this reader reads eagerly, and
    // the one whose absence a lookup keyed on "have I moved cluster" misses.
    assert!(
        disk[..4096].iter().all(|byte| *byte == 0),
        "the first cluster is a hole"
    );
    assert!(
        disk[16384..24576].iter().all(|byte| *byte == 0),
        "clusters four and five are a hole"
    );
    assert!(
        disk[4096..8192].iter().any(|byte| *byte != 0),
        "the cluster after the first hole is not one, or the fixture stopped testing anything"
    );
}

#[test]
fn zlib_clusters_are_decoded_into_the_same_disk() {
    assert_eq!(read_whole_disk("compressed.qcow2"), reference());
}

#[test]
fn zstd_clusters_are_decoded_into_the_same_disk() {
    assert_eq!(read_whole_disk("compressed-zstd.qcow2"), reference());
}

#[test]
fn the_cluster_size_is_read_from_the_image_rather_than_assumed() {
    let sparse = open("sparse.qcow2").expect("the fixture should open");
    let compressed = open("compressed.qcow2").expect("the fixture should open");

    assert_eq!(sparse.cluster_size(), 4096);
    assert_eq!(
        compressed.cluster_size(),
        8192,
        "the fixtures differ here so that a wired-in cluster size fails one of them"
    );
}

/// The property: whatever offset a caller seeks to and whatever length it asks
/// for, the bytes it gets are the bytes of the disk at that offset, and a read
/// never crosses the end of the disk.
///
/// The offsets and lengths come from a fixed seed rather than a property-testing
/// framework -- the workspace has no such dependency, and a failure that cannot
/// be reproduced from the test's own source is worth less than one that can.
#[test]
fn a_read_of_any_length_from_any_offset_agrees_with_the_disk() {
    let disk = reference();
    let mut image = open("sparse.qcow2").expect("the fixture should open");
    let mut random = 0x5eed_1234_9abc_def1u64;
    let mut next = move || {
        random ^= random << 13;
        random ^= random >> 7;
        random ^= random << 17;
        random
    };

    for _ in 0..2_000 {
        // Past the end as often as not, which is where a reader that clamps to
        // a cluster boundary rather than to the disk gives itself away.
        let offset = next() % (VIRTUAL_SIZE + 8192);
        let length = (next() % 9000) as usize;

        image
            .seek(SeekFrom::Start(offset))
            .expect("seeking within a qcow2 image cannot fail");
        let mut read = vec![0u8; length];
        let mut filled = 0;
        while filled < length {
            match image.read(&mut read[filled..]) {
                Ok(0) => break,
                Ok(count) => filled += count,
                Err(error) => panic!("reading {length} bytes at {offset} failed: {error}"),
            }
        }

        let start = (offset as usize).min(disk.len());
        let available = (disk.len() - start).min(length);
        assert_eq!(
            filled,
            available,
            "reading {length} bytes at {offset} of a {}-byte disk",
            disk.len()
        );
        assert_eq!(
            &read[..filled],
            &disk[start..start + filled],
            "the bytes read at {offset} are not the bytes of the disk"
        );
    }
}

#[test]
fn the_stream_ends_at_the_disk_rather_than_at_a_cluster_boundary() {
    let mut image = open("sparse.qcow2").expect("the fixture should open");

    image.seek(SeekFrom::End(-100)).expect("seek to the tail");
    let mut tail = Vec::new();
    image.read_to_end(&mut tail).expect("read the tail");
    assert_eq!(
        tail.len(),
        100,
        "the last cluster of this image reaches past the end of the disk"
    );
    assert_eq!(tail, reference()[VIRTUAL_SIZE as usize - 100..]);

    let mut nothing = [0u8; 16];
    assert_eq!(
        image
            .read(&mut nothing)
            .expect("a read at the end is not a failure"),
        0
    );
    image
        .seek(SeekFrom::Start(VIRTUAL_SIZE * 4))
        .expect("seeking past the end is allowed, as it is for a file");
    assert_eq!(
        image
            .read(&mut nothing)
            .expect("a read past the end is not a failure"),
        0
    );
}

#[test]
fn seeking_before_the_start_of_the_disk_is_an_error_rather_than_a_wrap() {
    let mut image = open("sparse.qcow2").expect("the fixture should open");

    image.seek(SeekFrom::Start(10)).expect("seek forwards");
    assert!(image.seek(SeekFrom::Current(-11)).is_err());
    assert_eq!(
        image.stream_position().expect("the position survives"),
        10,
        "a refused seek leaves the position where it was"
    );
}

#[test]
fn an_overlay_is_refused_because_its_holes_mean_read_the_parent() {
    let error = open("backing-child.qcow2").expect_err("an overlay is not a whole disk");

    assert!(matches!(error, Qcow2Error::BackingFile), "got {error}");
}

#[test]
fn the_legacy_format_is_refused_by_version() {
    let error = open("legacy-v1.qcow").expect_err("version 1 has no feature bits to check");

    assert!(
        matches!(error, Qcow2Error::UnsupportedVersion { version: 1 }),
        "got {error}"
    );
}

#[test]
fn an_image_larger_than_the_disk_it_is_headed_for_is_refused() {
    let error = Qcow2Image::open(&fixture("sparse.qcow2"), VIRTUAL_SIZE - 1)
        .expect_err("the last sector would have nowhere to go");

    assert!(
        matches!(error, Qcow2Error::TooLarge { virtual_size, capacity }
            if virtual_size == VIRTUAL_SIZE && capacity == VIRTUAL_SIZE - 1),
        "got {error}"
    );
}

#[test]
fn a_feature_bit_set_in_an_otherwise_real_image_is_refused() {
    let mut bytes = fs::read(fixture("sparse.qcow2")).expect("the fixture should be readable");
    // Bit 4 of the incompatible features: extended L2 entries, which changes
    // what every L2 entry in the image means.
    bytes[72 + 7] |= 1 << 4;
    let path = temporary("extended-l2.qcow2", &bytes);

    let error = Qcow2Image::open(&path, CAPACITY).expect_err("the bit changes the whole format");

    assert!(
        matches!(error, Qcow2Error::UnsupportedFeatures { bits } if bits == 1 << 4),
        "got {error}"
    );
}

#[test]
fn a_download_truncated_before_its_tables_is_refused_on_opening() {
    let bytes = fs::read(fixture("sparse.qcow2")).expect("the fixture should be readable");
    let path = temporary("truncated-tables.qcow2", &bytes[..8192]);

    let error = Qcow2Image::open(&path, CAPACITY).expect_err("the L1 table is not in the file");

    assert!(matches!(error, Qcow2Error::Malformed(_)), "got {error}");
}

#[test]
fn a_download_truncated_after_its_tables_fails_where_the_data_stops() {
    let bytes = fs::read(fixture("sparse.qcow2")).expect("the fixture should be readable");
    let path = temporary("truncated-data.qcow2", &bytes[..bytes.len() / 2]);

    let mut image = Qcow2Image::open(&path, CAPACITY).expect("the header and tables are intact");
    let error = image
        .read_to_end(&mut Vec::new())
        .expect_err("the clusters the tables point at are missing");

    assert_eq!(
        error.kind(),
        std::io::ErrorKind::Other,
        "a cluster past the end of the file is a malformed image, not an end of stream: {error}"
    );

    // A failed read leaves the cluster buffer half written, so what it holds is
    // no longer the cluster it was filled with. A reader that forgets to say so
    // serves those leftovers to the next caller that comes back to it.
    // The cluster before the one that failed is the one the buffer was holding,
    // so that is the one to ask for again.
    let stopped_at = image.stream_position().expect("the position survives") as usize;
    let previous = stopped_at - 4096;
    assert!(
        reference()[previous..stopped_at].iter().any(|byte| *byte != 0),
        "the cluster this comes back to must not be a hole, or nothing is being tested"
    );

    let mut again = vec![0u8; 4096];
    image
        .seek(SeekFrom::Start(previous as u64))
        .expect("seek back to a cluster the file still contains");
    image
        .read_exact(&mut again)
        .expect("the clusters before the truncation are still readable");
    assert_eq!(again, reference()[previous..stopped_at]);
}

/// Writes `bytes` where the test can open them, and returns the path.
///
/// One shared directory, one file per test, overwritten on each run -- so a run
/// starts from the same state however the previous one ended.
fn temporary(name: &str, bytes: &[u8]) -> PathBuf {
    let directory = std::env::temp_dir().join("vmlord-qcow2-tests");
    fs::create_dir_all(&directory).expect("the temporary directory should be creatable");
    let path = directory.join(name);
    fs::write(&path, bytes).expect("the temporary image should be writable");
    path
}

/// The whole point of the exercise, on an image nobody wrote for a test.
///
/// `cargo test -p vmlord-image --test qcow2 -- --ignored --nocapture`
#[test]
#[ignore = "needs a real cloud image at VMLORD_TEST_CLOUD_IMAGE"]
fn a_real_cloud_image_reads_as_a_partitioned_disk() {
    let Ok(path) = std::env::var("VMLORD_TEST_CLOUD_IMAGE") else {
        panic!("set VMLORD_TEST_CLOUD_IMAGE to a downloaded .img to run this");
    };

    let mut image = Qcow2Image::open(PathBuf::from(path).as_path(), 64 * 1024 * 1024 * 1024)
        .expect("a published cloud image should be readable");
    let virtual_size = image.virtual_size();
    println!(
        "the image holds a {virtual_size}-byte disk in {}-byte clusters",
        image.cluster_size()
    );

    // The GPT header lives in the second sector, and its signature is the one
    // thing a bootable cloud image cannot be missing.
    let mut first_sectors = [0u8; 1024];
    image
        .read_exact(&mut first_sectors)
        .expect("the first two sectors should read");
    assert_eq!(
        &first_sectors[512..520],
        b"EFI PART",
        "the second sector of a cloud image is a GPT header"
    );

    // Reading the whole disk is what the importer does, and the interesting
    // failures -- a cluster whose offset is past the end of the file, a
    // compressed cluster that will not inflate -- are only found by doing it.
    image.rewind().expect("rewind to the start of the disk");
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut total = 0u64;
    let mut zero_reads = 0u64;
    loop {
        let read = image.read(&mut buffer).expect("the disk should read");
        if read == 0 {
            break;
        }
        total += read as u64;
        if buffer[..read].iter().all(|byte| *byte == 0) {
            zero_reads += 1;
        }
    }

    assert_eq!(
        total, virtual_size,
        "the stream should end exactly at the end of the disk"
    );
    println!("{total} bytes read, {zero_reads} of the reads were all zeros");
}
