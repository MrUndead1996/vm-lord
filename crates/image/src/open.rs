//! Getting from "Ubuntu 24.04" to a disk that can be read cluster by cluster.
//!
//! Three steps that are written separately and belong together: read the
//! checksum list to learn which file a release means, fetch that file into the
//! cache, open it as the guest's disk. Joining them here rather than in the
//! composition root keeps the root a place where dependencies are assembled and
//! leaves this testable against the fixture server the other tests already use.

use std::{path::Path, sync::atomic::AtomicBool};

use vmlord_core::{DistroProfile, DownloadPhase, ProgressPublisher, RepositoryError};

use crate::{
    download::{ImageDownloadRequest, fetch_image},
    qcow2::Qcow2Image,
    resolve::resolve_image,
};

/// Fetches the image `release` means and opens it as the guest's disk.
///
/// `capacity` is the size of the VM's disk, not of the image: opening refuses
/// an image whose disk would not fit, before a byte is copied anywhere.
///
/// `progress` and `cancel` are passed through to the download, which is the
/// only step long enough to have either. #61 hands in a publisher nobody reads
/// and a flag nobody sets; #64 hands in the real ones without this signature
/// changing.
///
/// The typed errors of the three steps end here: the caller is the creation
/// pipeline, whose contract across the project is `RepositoryError`. Nothing is
/// lost -- each error's `Display` names its own cause, and the crate logs it
/// where it happens.
pub fn open_cloud_image(
    profile: &DistroProfile,
    release: &str,
    cache_directory: &Path,
    capacity: u64,
    progress: &ProgressPublisher<DownloadPhase>,
    cancel: &AtomicBool,
) -> Result<Qcow2Image, RepositoryError> {
    log::debug!(
        "preparing {} {release} for a {capacity}-byte disk, cached in {}",
        profile.name,
        cache_directory.display()
    );

    let resolved = resolve_image(profile, release).map_err(at_the_boundary)?;
    let path = fetch_image(
        ImageDownloadRequest {
            url: &resolved.url,
            expected_sha256: &resolved.sha256,
            cache_directory,
        },
        progress,
        cancel,
    )
    .map_err(at_the_boundary)?;

    let image = Qcow2Image::open(&path, capacity).map_err(at_the_boundary)?;
    log::debug!(
        "opened {} as a {}-byte disk",
        path.display(),
        image.virtual_size()
    );
    Ok(image)
}

fn at_the_boundary(error: impl std::fmt::Display) -> RepositoryError {
    RepositoryError::new(error.to_string())
}
