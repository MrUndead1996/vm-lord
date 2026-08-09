//! The partial download, and the lock that makes it one downloader's business.

use std::{
    fs::{File, TryLockError},
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

use vmlord_core::ProgressThrottle;

use crate::{
    cache::checksum_reader,
    error::{DownloadError, io_error},
};

/// A `.part` file held under an exclusive OS lock for as long as it exists.
///
/// The name of a `.part` is derived from the image's checksum, so two
/// downloaders of one image aim at one file. Interleaving their writes would
/// not corrupt anything a caller can see -- the checksum catches it -- but it
/// wastes the whole download and reports the wrong reason. The lock turns that
/// into an immediate, accurate refusal.
///
/// The lock is the operating system's, taken on the open file: `LockFileEx`
/// with `LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY` on Windows,
/// `flock` elsewhere. It therefore covers two threads of one process as well as
/// two processes, and -- unlike a `.lock` marker file -- it is released when the
/// handle closes, including when the process dies. Nothing has to guess whether
/// a leftover lock is stale.
#[derive(Debug)]
pub(crate) struct PartFile {
    file: File,
    path: PathBuf,
}

impl PartFile {
    /// Opens the partial download and claims it.
    ///
    /// Reports `AlreadyInProgress` rather than waiting: whether it is worth
    /// queueing behind another download is the caller's policy, and a wait
    /// buried in here would need a timeout invented out of nothing.
    pub(crate) fn open_locked(path: PathBuf) -> Result<Self, DownloadError> {
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(io_error("open the partial download", &path))?;

        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                log::debug!("{} is locked by another downloader", path.display());
                return Err(DownloadError::AlreadyInProgress { path });
            }
            Err(TryLockError::Error(source)) => {
                return Err(DownloadError::Io {
                    operation: "lock the partial download",
                    path,
                    source,
                });
            }
        }

        Ok(Self { file, path })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn len(&self) -> Result<u64, DownloadError> {
        Ok(self
            .file
            .metadata()
            .map_err(io_error("measure the partial download", &self.path))?
            .len())
    }

    /// Empties the file, keeping the handle and therefore the lock.
    ///
    /// Deleting and recreating would drop the lock for an instant and, on
    /// Windows, fight the still-open handle. Truncation does neither.
    pub(crate) fn truncate(&mut self) -> Result<(), DownloadError> {
        self.file
            .set_len(0)
            .map_err(io_error("truncate the partial download", &self.path))?;
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(io_error("rewind the partial download", &self.path))?;
        Ok(())
    }

    pub(crate) fn seek_to_end(&mut self) -> Result<u64, DownloadError> {
        self.file
            .seek(SeekFrom::End(0))
            .map_err(io_error("seek the partial download", &self.path))
    }

    pub(crate) fn write_all(&mut self, bytes: &[u8]) -> Result<(), DownloadError> {
        self.file
            .write_all(bytes)
            .map_err(io_error("write the partial download", &self.path))
    }

    /// Hashes what the file holds, reading through the handle that locks it.
    ///
    /// Reopening the path instead would fail on Windows: `LockFileEx` locks are
    /// mandatory, so a second handle reading a locked range gets
    /// `ERROR_LOCK_VIOLATION`. On Linux `flock` is advisory and the second read
    /// would quietly succeed, which is exactly why this has to be deliberate.
    pub(crate) fn checksum(
        &mut self,
        progress: &mut ProgressThrottle,
        cancel: &AtomicBool,
    ) -> Result<String, DownloadError> {
        let total = self.len()?;
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(io_error("rewind the partial download", &self.path))?;
        checksum_reader(&mut self.file, total, &self.path, progress, cancel)
    }

    pub(crate) fn sync(&self) -> Result<(), DownloadError> {
        self.file
            .sync_all()
            .map_err(io_error("flush the partial download", &self.path))
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, thread};

    use super::PartFile;
    use crate::error::DownloadError;

    fn temporary_directory(tag: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("vmlord-part-{tag}-{unique}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn a_fresh_partial_file_starts_empty_and_takes_what_is_written_to_it() {
        let directory = temporary_directory("write");
        let mut part = PartFile::open_locked(directory.join("image.part")).unwrap();

        assert_eq!(part.len().unwrap(), 0);
        part.seek_to_end().unwrap();
        part.write_all(b"first").unwrap();
        part.write_all(b"-second").unwrap();
        part.sync().unwrap();

        assert_eq!(part.len().unwrap(), 12);

        // Read only after the lock is gone: on Windows the lock is mandatory,
        // so a second handle reading the range would fail rather than observe.
        let path = part.path().to_path_buf();
        drop(part);
        assert_eq!(fs::read(&path).unwrap(), b"first-second");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn reopening_a_partial_file_sees_what_the_last_attempt_left() {
        let directory = temporary_directory("resume");
        let path = directory.join("image.part");
        let mut first = PartFile::open_locked(path.clone()).unwrap();
        first.seek_to_end().unwrap();
        first.write_all(b"half").unwrap();
        first.sync().unwrap();
        drop(first);

        let mut second = PartFile::open_locked(path).unwrap();

        assert_eq!(
            second.len().unwrap(),
            4,
            "the whole point of a stable .part name is that the next run can resume it"
        );
        second.seek_to_end().unwrap();
        second.write_all(b"-rest").unwrap();
        second.sync().unwrap();

        let path = second.path().to_path_buf();
        drop(second);
        assert_eq!(fs::read(&path).unwrap(), b"half-rest");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn truncating_empties_the_file_without_dropping_the_lock() {
        let directory = temporary_directory("truncate");
        let mut part = PartFile::open_locked(directory.join("image.part")).unwrap();
        part.seek_to_end().unwrap();
        part.write_all(b"stale bytes").unwrap();

        part.truncate().unwrap();

        assert_eq!(part.len().unwrap(), 0);
        part.seek_to_end().unwrap();
        part.write_all(b"fresh").unwrap();
        part.sync().unwrap();

        let path = part.path().to_path_buf();
        drop(part);
        assert_eq!(fs::read(&path).unwrap(), b"fresh");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_partial_file_can_be_hashed_while_it_is_still_locked() {
        use std::{sync::atomic::AtomicBool, time::Duration};

        use vmlord_core::{ProgressPublisher, ProgressThrottle};

        let directory = temporary_directory("checksum");
        let mut part = PartFile::open_locked(directory.join("image.part")).unwrap();
        part.seek_to_end().unwrap();
        part.write_all(b"vmlord").unwrap();
        part.sync().unwrap();
        let mut throttle =
            ProgressThrottle::with_interval(ProgressPublisher::default(), Duration::ZERO);

        let sum = part
            .checksum(&mut throttle, &AtomicBool::new(false))
            .unwrap();

        assert_eq!(
            sum, "c423e3a9d7b4a6f1f03492cfded44b0b9c00c4c63f1ef3c410368e8a9ad3bcd2",
            "verification happens with the lock still held, so it must read through \
             the locking handle; reopening the path fails on Windows, where the lock \
             is mandatory rather than advisory"
        );

        drop(part);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_second_downloader_of_the_same_image_is_turned_away() {
        let directory = temporary_directory("lock");
        let path = directory.join("image.part");
        let held = PartFile::open_locked(path.clone()).unwrap();

        let error = PartFile::open_locked(path.clone())
            .expect_err("two downloaders must not write into one partial file");

        assert!(
            matches!(error, DownloadError::AlreadyInProgress { .. }),
            "got {error:?}"
        );

        drop(held);
        PartFile::open_locked(path)
            .expect("the lock is released with the file, so the next attempt may resume");

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn the_lock_is_held_against_another_thread_too() {
        let directory = temporary_directory("lock-thread");
        let path = directory.join("image.part");
        let held = PartFile::open_locked(path.clone()).unwrap();

        let contender = path.clone();
        let outcome = thread::spawn(move || PartFile::open_locked(contender).is_err())
            .join()
            .unwrap();

        assert!(
            outcome,
            "the lock has to cover two threads of one process, not just two processes"
        );

        drop(held);
        fs::remove_dir_all(directory).unwrap();
    }
}
