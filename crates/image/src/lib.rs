//! Getting a distribution's cloud image onto disk, intact.
//!
//! The cache is addressed by content: a file is named after the SHA256 it is
//! expected to have. Two releases can therefore never collide on a name, and a
//! file whose name and content disagree cannot exist.
//!
//! Trust comes from HTTPS, not from the checksum. A list of sums downloaded
//! from the same server as the image proves nothing about authenticity:
//! whoever could swap the image could swap the list. The checksum is an
//! integrity check, and above all the one defence against a file left truncated
//! by an interrupted download.

mod cache;
mod checksums;
mod distro;
mod download;
mod error;
mod http;
mod part;
mod resolve;

pub use distro::{DistroProfile, UBUNTU};
pub use download::{ImageDownloadRequest, fetch_image};
pub use error::{DownloadError, ResolveError};
pub use resolve::{ResolvedImage, resolve_image};
