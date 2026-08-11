//! Copying an image onto a disk, and proving afterwards that it landed.

use std::{
    io::Read,
    sync::atomic::{AtomicBool, Ordering},
};

use vmlord_core::RepositoryError;

use crate::import::plan::{digest, fill_chunk, is_zero};

/// The disk an image is written onto: `\\.\PhysicalDriveN` in production, an
/// array of bytes in the tests.
///
/// Reads exist for one reason -- reading back what was written. See
/// [`copy_image`] for why that is not optional here.
pub(crate) trait DiskBlocks {
    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<(), RepositoryError>;
    fn read_at(&mut self, offset: u64, bytes: &mut [u8]) -> Result<(), RepositoryError>;
    /// Waits until everything written is on the disk rather than on its way
    /// there. Reading back before this would be asking the same layer that
    /// swallowed a write whether it swallowed it.
    fn flush(&mut self) -> Result<(), RepositoryError>;
}

/// What an import moved, and what it left alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ImportSummary {
    /// The size of the disk inside the image: every byte the guest can see.
    pub image_bytes: u64,
    /// The bytes actually written. The rest were holes.
    pub written_bytes: u64,
    /// The bytes recognised as holes and skipped, which is what keeps the VHDX
    /// smaller than the disk it presents.
    pub skipped_bytes: u64,
    /// The bytes read back off the disk and matched against what was written.
    pub verified_bytes: u64,
}

/// One chunk that was written, kept so it can be recognised on the way back.
struct Written {
    offset: u64,
    length: usize,
    digest: u64,
}

/// Writes the disk inside `source` onto `disk`, then reads back every chunk it
/// wrote and checks it against what it sent.
///
/// Three things here are not optimisations:
///
/// Holes are skipped. The reader hands out zeros for every cluster the image
/// never allocated; writing them back would allocate the whole disk and turn a
/// 600 MB image into a 64 GB file.
///
/// The first chunk goes last. Once sector zero carries the image's GPT, the
/// volume manager can recognise the disk, mount what it finds and take it
/// exclusively -- after which writes stop arriving without failing. AppSandbox
/// hit exactly this and worked around it by delaying
/// `IOCTL_DISK_UPDATE_PROPERTIES` to the very end
/// (`tools/iso-patch/ubuntu_vhdx.c:204-208`). Holding back the one chunk that
/// makes the disk look like a disk is the same defence, one step earlier.
///
/// Everything written is read back. This is the failure that has no error
/// message: a write that does not land leaves a VHDX that is the right size, in
/// the right place, with a log full of successes, and a VM that does not boot.
///
/// `chunk_bytes` is the unit all of this works in; production passes
/// [`CHUNK_BYTES`](super::plan::CHUNK_BYTES), and the tests pass kilobytes.
///
/// `cancel` is polled once per chunk. A build is cancelled by the user or by
/// VMLord shutting down, and a copy that only noticed afterwards would hold
/// both for as long as a disk takes to write.
pub(crate) fn copy_image(
    source: &mut dyn Read,
    disk: &mut dyn DiskBlocks,
    capacity: u64,
    chunk_bytes: usize,
    cancel: &AtomicBool,
) -> Result<ImportSummary, RepositoryError> {
    let mut chunk = vec![0u8; chunk_bytes];
    // The one chunk that is not written where it is read. It is held whole
    // rather than re-read at the end, because the source is a stream and
    // rewinding it would mean decompressing the first clusters twice.
    let mut first_chunk = vec![0u8; chunk_bytes];
    let mut first_length = 0;

    let mut written: Vec<Written> = Vec::new();
    let mut summary = ImportSummary::default();
    let mut offset = 0u64;

    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(repository_error(
                "writing the disk was cancelled".to_owned(),
            ));
        }
        let used = fill_chunk(source, &mut chunk)
            .map_err(|error| repository_error(format!("reading the image failed: {error}")))?;
        if used == 0 {
            break;
        }
        let end = offset + used as u64;
        if end > capacity {
            return Err(repository_error(format!(
                "the image does not fit the disk: {end} bytes of image against {capacity} bytes of disk"
            )));
        }
        summary.image_bytes = end;

        if offset == 0 {
            first_chunk[..used].copy_from_slice(&chunk[..used]);
            first_length = used;
        } else if is_zero(&chunk[..used]) {
            summary.skipped_bytes += used as u64;
            log::debug!("skipped the hole at {offset} ({used} bytes)");
        } else {
            disk.write_at(offset, &chunk[..used])?;
            written.push(Written {
                offset,
                length: used,
                digest: digest(&chunk[..used]),
            });
            summary.written_bytes += used as u64;
        }
        offset = end;
    }

    if first_length > 0 {
        let first = &first_chunk[..first_length];
        if is_zero(first) {
            summary.skipped_bytes += first_length as u64;
        } else {
            disk.write_at(0, first)?;
            written.push(Written {
                offset: 0,
                length: first_length,
                digest: digest(first),
            });
            summary.written_bytes += first_length as u64;
        }
    }

    log::info!(
        "wrote {} of {} image bytes, skipping {} as holes",
        summary.written_bytes,
        summary.image_bytes,
        summary.skipped_bytes
    );

    disk.flush()?;
    summary.verified_bytes = verify(disk, &written, &mut chunk)?;
    Ok(summary)
}

/// Reads every written chunk back off the disk and checks it against what was
/// sent, returning how many bytes came back intact.
fn verify(
    disk: &mut dyn DiskBlocks,
    written: &[Written],
    buffer: &mut [u8],
) -> Result<u64, RepositoryError> {
    let mut verified = 0;
    for record in written {
        let read_back = &mut buffer[..record.length];
        disk.read_at(record.offset, read_back)?;
        if digest(read_back) != record.digest {
            let error = repository_error(format!(
                "the {} bytes written at {} did not read back: the disk took the write and did not keep it",
                record.length, record.offset
            ));
            log::error!("{error}");
            return Err(error);
        }
        verified += record.length as u64;
    }
    log::info!("read back and matched {verified} bytes");
    Ok(verified)
}

fn repository_error(message: String) -> RepositoryError {
    log::error!("{message}");
    RepositoryError::new(message)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use vmlord_core::RepositoryError;

    use super::{DiskBlocks, copy_image};

    const CHUNK: usize = 4096;

    /// The flag of a copy nobody has cancelled.
    fn running() -> std::sync::atomic::AtomicBool {
        std::sync::atomic::AtomicBool::new(false)
    }

    /// A disk that remembers not only what it holds but which of its bytes were
    /// ever written to, so a test can tell a skipped hole from a chunk of zeros
    /// that was written out in full.
    struct MemoryDisk {
        bytes: Vec<u8>,
        touched: Vec<(u64, usize)>,
        /// Writes at or past this offset are accepted and dropped, the way they
        /// are once the volume manager has taken the disk.
        swallow_from: Option<u64>,
        flushed: bool,
        read_before_flush: bool,
    }

    impl MemoryDisk {
        fn new(capacity: usize) -> Self {
            Self {
                bytes: vec![0; capacity],
                touched: Vec::new(),
                swallow_from: None,
                flushed: false,
                read_before_flush: false,
            }
        }

        fn write_offsets(&self) -> Vec<u64> {
            self.touched.iter().map(|(offset, _)| *offset).collect()
        }
    }

    impl DiskBlocks for MemoryDisk {
        fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<(), RepositoryError> {
            self.touched.push((offset, bytes.len()));
            if self.swallow_from.is_some_and(|from| offset >= from) {
                return Ok(());
            }
            let start = offset as usize;
            self.bytes[start..start + bytes.len()].copy_from_slice(bytes);
            Ok(())
        }

        fn read_at(&mut self, offset: u64, bytes: &mut [u8]) -> Result<(), RepositoryError> {
            self.read_before_flush |= !self.flushed;
            let start = offset as usize;
            bytes.copy_from_slice(&self.bytes[start..start + bytes.len()]);
            Ok(())
        }

        fn flush(&mut self) -> Result<(), RepositoryError> {
            self.flushed = true;
            Ok(())
        }
    }

    /// An image of `chunks` chunks whose byte at guest offset `n` is
    /// `(n / 7 + 1) as u8 | 1`, so no chunk is accidentally a hole and a chunk
    /// written to the wrong offset cannot pass for the right one.
    fn image(chunks: usize) -> Vec<u8> {
        (0..chunks * CHUNK)
            .map(|offset| ((offset / 7 + 1) as u8) | 1)
            .collect()
    }

    /// Writing a disk is the second-longest step of creating a VM, and a
    /// cancellation that only takes effect after it would not be a
    /// cancellation.
    #[test]
    fn a_cancelled_copy_stops_and_reports_why() {
        let cancel = std::sync::atomic::AtomicBool::new(true);
        let mut source = Cursor::new(vec![7u8; CHUNK * 4]);
        let mut disk = MemoryDisk::new(CHUNK * 4);

        let error = copy_image(&mut source, &mut disk, (CHUNK * 4) as u64, CHUNK, &cancel)
            .expect_err("a cancelled copy must not report success");

        assert!(error.to_string().contains("cancelled"), "got {error}");
        assert!(disk.write_offsets().is_empty());
    }

    #[test]
    fn every_byte_of_the_image_lands_at_the_offset_it_came_from() {
        let source = image(3);
        let mut disk = MemoryDisk::new(8 * CHUNK);

        let summary = copy_image(
            &mut Cursor::new(source.clone()),
            &mut disk,
            8 * CHUNK as u64,
            CHUNK,
            &running(),
        )
        .expect("the import should succeed");

        assert_eq!(&disk.bytes[..source.len()], source.as_slice());
        assert!(disk.bytes[source.len()..].iter().all(|byte| *byte == 0));
        assert_eq!(summary.image_bytes, 3 * CHUNK as u64);
        assert_eq!(summary.written_bytes, 3 * CHUNK as u64);
        assert_eq!(summary.skipped_bytes, 0);
        assert_eq!(summary.verified_bytes, 3 * CHUNK as u64);
    }

    #[test]
    fn a_hole_in_the_middle_of_the_image_is_never_written() {
        let mut source = image(4);
        source[CHUNK..2 * CHUNK].fill(0);
        let mut disk = MemoryDisk::new(8 * CHUNK);

        let summary = copy_image(
            &mut Cursor::new(source.clone()),
            &mut disk,
            8 * CHUNK as u64,
            CHUNK,
            &running(),
        )
        .expect("the import should succeed");

        assert!(
            !disk.write_offsets().contains(&(CHUNK as u64)),
            "the hole was written out: {:?}",
            disk.write_offsets()
        );
        assert_eq!(&disk.bytes[..source.len()], source.as_slice());
        assert_eq!(summary.skipped_bytes, CHUNK as u64);
        assert_eq!(summary.written_bytes, 3 * CHUNK as u64);
    }

    #[test]
    fn an_image_that_begins_with_a_hole_writes_nothing_at_sector_zero() {
        let mut source = image(2);
        source[..CHUNK].fill(0);
        let mut disk = MemoryDisk::new(8 * CHUNK);

        let summary = copy_image(&mut Cursor::new(source), &mut disk, 8 * CHUNK as u64, CHUNK, &running())
            .expect("the import should succeed");

        assert_eq!(disk.write_offsets(), vec![CHUNK as u64]);
        assert_eq!(summary.skipped_bytes, CHUNK as u64);
    }

    #[test]
    fn the_chunk_that_makes_the_disk_recognisable_is_written_last() {
        let mut disk = MemoryDisk::new(8 * CHUNK);

        copy_image(
            &mut Cursor::new(image(4)),
            &mut disk,
            8 * CHUNK as u64,
            CHUNK,
            &running(),
        )
        .expect("the import should succeed");

        assert_eq!(
            disk.write_offsets(),
            vec![CHUNK as u64, 2 * CHUNK as u64, 3 * CHUNK as u64, 0],
            "sector zero must not carry a GPT until every other byte is down"
        );
    }

    #[test]
    fn a_final_chunk_shorter_than_the_rest_is_written_whole() {
        let mut source = image(3);
        source.truncate(2 * CHUNK + 17);
        let mut disk = MemoryDisk::new(8 * CHUNK);

        let summary = copy_image(
            &mut Cursor::new(source.clone()),
            &mut disk,
            8 * CHUNK as u64,
            CHUNK,
            &running(),
        )
        .expect("the import should succeed");

        assert_eq!(disk.touched.last(), Some(&(0, CHUNK)));
        assert!(disk.touched.contains(&(2 * CHUNK as u64, 17)));
        assert_eq!(&disk.bytes[..source.len()], source.as_slice());
        assert_eq!(summary.image_bytes, 2 * CHUNK as u64 + 17);
    }

    #[test]
    fn an_image_larger_than_the_disk_is_refused_rather_than_wrapped() {
        let mut disk = MemoryDisk::new(8 * CHUNK);

        let error = copy_image(
            &mut Cursor::new(image(4)),
            &mut disk,
            2 * CHUNK as u64,
            CHUNK,
            &running(),
        )
        .expect_err("an image that does not fit must be refused");

        assert!(error.to_string().contains("does not fit"), "{error}");
    }

    #[test]
    fn writes_that_stop_landing_are_caught_by_the_read_back() {
        let mut disk = MemoryDisk::new(8 * CHUNK);
        disk.swallow_from = Some(2 * CHUNK as u64);

        let error = copy_image(
            &mut Cursor::new(image(4)),
            &mut disk,
            8 * CHUNK as u64,
            CHUNK,
            &running(),
        )
        .expect_err("a write that did not land must be reported");

        assert!(
            error.to_string().contains(&format!("{}", 2 * CHUNK)),
            "{error}"
        );
    }

    #[test]
    fn nothing_is_read_back_before_the_disk_has_been_flushed() {
        let mut disk = MemoryDisk::new(8 * CHUNK);

        copy_image(
            &mut Cursor::new(image(3)),
            &mut disk,
            8 * CHUNK as u64,
            CHUNK,
            &running(),
        )
        .expect("the import should succeed");

        assert!(disk.flushed);
        assert!(
            !disk.read_before_flush,
            "a read-back before the flush proves nothing about what reached the disk"
        );
    }

    #[test]
    fn a_disk_that_reads_back_as_it_was_written_is_reported_as_verified() {
        let mut disk = MemoryDisk::new(8 * CHUNK);
        let mut source = image(3);
        source[CHUNK..2 * CHUNK].fill(0);

        let summary = copy_image(&mut Cursor::new(source), &mut disk, 8 * CHUNK as u64, CHUNK, &running())
            .expect("the import should succeed");

        assert_eq!(summary.verified_bytes, summary.written_bytes);
        assert_eq!(summary.verified_bytes, 2 * CHUNK as u64);
    }
}
