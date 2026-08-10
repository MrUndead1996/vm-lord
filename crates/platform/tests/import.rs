//! Importing an image into a real VHDX, on a real host.
//!
//! Every test here is `#[ignore]`d: each one attaches a virtual disk and writes
//! to the physical drive Windows presents it as, which needs an elevated
//! process and leaves a file behind if it dies half way. They are also the only
//! tests that can fail the way this code fails in production -- a write that is
//! accepted and never lands is invisible to anything short of a real disk.
//!
//! Run them with:
//!
//! ```text
//! cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu --test import -- --ignored --nocapture
//! ```

use std::{
    fs,
    io::Read,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use vmlord_platform::import_image;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

struct TempRoot(PathBuf);

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn temp_root(label: &str) -> TempRoot {
    static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "vmlord-import-test-{label}-{}-{}",
        std::process::id(),
        NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&path).expect("test root should be created");
    TempRoot(path)
}

/// A stand-in for a cloud image: data, then a hole, then data, then nothing at
/// all -- the shape every sparse image has, without needing one on disk.
///
/// The byte at offset `n` is `(n / 7 + 1) as u8 | 1`, so no region is
/// accidentally a hole and a block written to the wrong offset cannot pass for
/// the right one.
struct PatternImage {
    position: u64,
    length: u64,
    hole: std::ops::Range<u64>,
}

impl Read for PatternImage {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self.length.saturating_sub(self.position);
        let wanted = buffer.len().min(remaining as usize);
        for (index, slot) in buffer[..wanted].iter_mut().enumerate() {
            let offset = self.position + index as u64;
            *slot = if self.hole.contains(&offset) {
                0
            } else {
                ((offset / 7 + 1) as u8) | 1
            };
        }
        self.position += wanted as u64;
        Ok(wanted)
    }
}

#[test]
#[ignore = "attaches a virtual disk; requires an elevated process"]
fn writes_an_image_into_a_vhdx_and_leaves_the_holes_unallocated() {
    let root = temp_root("sparse");
    let target = root.0.join("system.vhdx");
    let mut source = PatternImage {
        position: 0,
        length: 64 * MIB,
        hole: 16 * MIB..48 * MIB,
    };

    let summary = import_image(&mut source, &target, GIB).expect("the import should succeed");

    println!("{summary:?}");
    assert_eq!(summary.image_bytes, 64 * MIB);
    assert_eq!(summary.skipped_bytes, 32 * MIB);
    assert_eq!(summary.written_bytes, 32 * MIB);
    assert_eq!(
        summary.verified_bytes, summary.written_bytes,
        "every written byte must be read back"
    );

    let file_bytes = fs::metadata(&target).expect("the VHDX should exist").len();
    println!("the VHDX holding a {GIB}-byte disk is {file_bytes} bytes");
    assert!(
        file_bytes < 96 * MIB,
        "the hole was allocated: {file_bytes} bytes on disk for {} bytes of data",
        summary.written_bytes
    );
}

#[test]
#[ignore = "attaches a virtual disk; requires an elevated process"]
fn refuses_an_image_larger_than_the_disk_without_leaving_a_vhdx_behind() {
    let root = temp_root("too-big");
    let target = root.0.join("system.vhdx");
    let mut source = PatternImage {
        position: 0,
        length: 64 * MIB,
        hole: 0..0,
    };

    let error = import_image(&mut source, &target, 16 * MIB)
        .expect_err("an image that does not fit must be refused");

    println!("{error}");
    assert!(error.to_string().contains("does not fit"));
    assert!(
        !target.exists(),
        "a failed import must not leave a half-written disk behind"
    );
}

/// The real thing: a cloud image, read through `Qcow2Image`, written into a
/// disk the size a VM would get. Point `VMLORD_TEST_CLOUD_IMAGE` at a
/// downloaded `.img` to run it.
#[test]
#[ignore = "needs VMLORD_TEST_CLOUD_IMAGE and an elevated process"]
fn imports_a_real_cloud_image() {
    let Ok(image_path) = std::env::var("VMLORD_TEST_CLOUD_IMAGE") else {
        panic!("set VMLORD_TEST_CLOUD_IMAGE to a downloaded cloud image");
    };
    let capacity = 64 * GIB;
    let root = temp_root("cloud");
    let target = root.0.join("system.vhdx");
    let mut source = vmlord_image::Qcow2Image::open(std::path::Path::new(&image_path), capacity)
        .expect("the cloud image should open");
    let virtual_size = source.virtual_size();

    let summary = import_image(&mut source, &target, capacity).expect("the import should succeed");

    println!("{summary:?}");
    assert_eq!(summary.image_bytes, virtual_size);
    assert_eq!(summary.verified_bytes, summary.written_bytes);
    assert!(
        summary.skipped_bytes > 0,
        "a cloud image is mostly holes; skipping none of it means none were recognised"
    );

    let file_bytes = fs::metadata(&target).expect("the VHDX should exist").len();
    println!("a {capacity}-byte disk landed as a {file_bytes}-byte file");
    assert!(
        file_bytes < summary.written_bytes + GIB,
        "the VHDX is not sparse: {file_bytes} bytes for {} bytes of data",
        summary.written_bytes
    );
}
