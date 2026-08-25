//! Writing a cloud image into a VHDX, through the disk Windows attaches it as.
//!
//! There is no API that writes into a VHDX file. What there is: attach the
//! VHDX, and Windows presents it as `\\.\PhysicalDriveN` like any other disk,
//! which can be written to sector by sector. The image's own bytes -- GPT,
//! partitions, filesystems and all -- go straight down onto it.
//!
//! That route runs past the volume manager, and this is the subtask where
//! things fail silently: a disk it has recognised and taken accepts writes that
//! never arrive, leaving a VHDX of the right size, in the right place, with a
//! log full of successes and a VM that does not boot. AppSandbox met this and
//! left a note (`tools/iso-patch/ubuntu_vhdx.c:204-208`). Three things answer
//! it here -- `IOCTL_DISK_UPDATE_PROPERTIES` is never called, the chunk
//! carrying sector zero is written after everything else, and every byte
//! written is read back off the disk before the import is called a success.

mod copy;
mod drive;
mod plan;

use std::{fs, io::Read, path::Path, sync::atomic::AtomicBool};

use vmlord_core::RepositoryError;

pub use copy::ImportSummary;

use crate::{
    import::{
        copy::copy_image,
        drive::{AttachedDisk, PhysicalDrive},
        plan::CHUNK_BYTES,
    },
    vhd::create_dynamic_vhdx,
};

/// Creates a `disk_size_bytes` VHDX at `target` and writes the disk inside
/// `source` onto it.
///
/// `source` is the guest's disk as a stream of bytes -- `vmlord_image`'s
/// `Qcow2Image` in production. Its holes read as zeros, and those are skipped
/// rather than written, which is what keeps the VHDX file a fraction of the
/// disk it presents.
///
/// The disk is made the size the VM will have, not the size of the image. The
/// image's backup GPT header therefore ends up short of the end of the disk,
/// where the guest's first boot has to move it; that is the next subtask's
/// business, not this one's.
///
/// A failure leaves nothing behind: the disk is detached and the VHDX removed,
/// because a half-written disk that looks complete is the failure this whole
/// module is written against.
///
/// A cancelled import leaves nothing behind either: it takes the same path a
/// failed one takes, because a cancellation here is an ordinary failure.
pub fn import_image(
    source: &mut dyn Read,
    target: &Path,
    disk_size_bytes: u64,
    cancel: &AtomicBool,
) -> Result<ImportSummary, RepositoryError> {
    create_dynamic_vhdx(target, disk_size_bytes)?;
    tracing::info!(
        "importing an image into {} ({disk_size_bytes} bytes)",
        target.display()
    );

    match write_into(source, target, disk_size_bytes, cancel) {
        Ok(summary) => {
            tracing::info!(
                "imported {} bytes into {}, skipping {} bytes of holes",
                summary.written_bytes,
                target.display(),
                summary.skipped_bytes
            );
            Ok(summary)
        }
        Err(error) => Err(discard(target, error)),
    }
}

/// Attaches the disk, writes the image onto it and detaches it again.
///
/// Split out so that every failure from the attach onwards leaves by one path,
/// the one that removes the half-written VHDX.
fn write_into(
    source: &mut dyn Read,
    target: &Path,
    disk_size_bytes: u64,
    cancel: &AtomicBool,
) -> Result<ImportSummary, RepositoryError> {
    let attached = AttachedDisk::attach(target)?;
    let summary = {
        let mut drive = PhysicalDrive::open(attached.physical_path(), CHUNK_BYTES)?;
        copy_image(source, &mut drive, disk_size_bytes, CHUNK_BYTES, cancel)
        // The drive handle closes here, before the disk is detached: detaching
        // a disk somebody still holds open is how a VHDX ends up attached with
        // nothing left to detach it.
    };
    // The disk comes off whether or not the copy worked. A failed import that
    // leaves the VHDX attached leaves a file the caller cannot even delete.
    match (summary, attached.detach()) {
        (Ok(summary), Ok(())) => Ok(summary),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(detach_error)) => Err(RepositoryError::new(format!(
            "{error}; detaching the disk afterwards also failed: {detach_error}"
        ))),
    }
}

/// Removes the VHDX a failed import wrote, keeping the original failure as the
/// error the caller sees.
fn discard(target: &Path, error: RepositoryError) -> RepositoryError {
    match fs::remove_file(target) {
        Ok(()) => {
            tracing::debug!("removed the unfinished {}", target.display());
            error
        }
        Err(remove_error) if remove_error.kind() == std::io::ErrorKind::NotFound => error,
        Err(remove_error) => {
            let error = RepositoryError::new(format!(
                "{error}; the unfinished {} could not be removed either: {remove_error}",
                target.display()
            ));
            tracing::error!("{error}");
            error
        }
    }
}
