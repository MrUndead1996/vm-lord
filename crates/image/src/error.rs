//! Everything that can go wrong fetching an image, and how it reads.

use std::{
    fmt, io,
    path::{Path, PathBuf},
};

/// A failure fetching a distribution image.
#[derive(Debug)]
pub enum DownloadError {
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    /// The transport failed: connection refused, TLS rejected, body cut short.
    Http(String),
    UnexpectedStatus {
        status: u16,
    },
    ChecksumMismatch {
        expected: String,
        actual: String,
    },
    /// Another downloader holds the lock on this image's partial file.
    AlreadyInProgress {
        path: PathBuf,
    },
    Cancelled,
    /// The caller supplied something that is not a SHA256.
    InvalidChecksum(String),
}

impl fmt::Display for DownloadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                operation,
                path,
                source,
            } => write!(
                formatter,
                "failed to {operation} at {}: {source}",
                path.display()
            ),
            Self::Http(message) => write!(formatter, "the image request failed: {message}"),
            Self::UnexpectedStatus { status } => {
                write!(formatter, "the image server answered with status {status}")
            }
            Self::ChecksumMismatch { expected, actual } => write!(
                formatter,
                "the downloaded image hashes to {actual} instead of the expected {expected}"
            ),
            Self::AlreadyInProgress { path } => write!(
                formatter,
                "another download of this image is already running; it holds {}",
                path.display()
            ),
            Self::Cancelled => formatter.write_str("the image download was cancelled"),
            Self::InvalidChecksum(value) => {
                write!(formatter, "{value:?} is not a SHA256 checksum")
            }
        }
    }
}

impl std::error::Error for DownloadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Http(_)
            | Self::UnexpectedStatus { .. }
            | Self::ChecksumMismatch { .. }
            | Self::AlreadyInProgress { .. }
            | Self::Cancelled
            | Self::InvalidChecksum(_) => None,
        }
    }
}

/// Builds the `Io` variant for a fallible filesystem call.
///
/// Written as a closure factory so call sites read
/// `.map_err(io_error("open the partial download", &path))?`.
pub(crate) fn io_error(
    operation: &'static str,
    path: &Path,
) -> impl FnOnce(io::Error) -> DownloadError + use<> {
    let path = path.to_path_buf();
    move |source| DownloadError::Io {
        operation,
        path,
        source,
    }
}

/// A failure working out which image a release means.
///
/// Separate from `DownloadError` on purpose: the two have different callers and
/// tell the user different things. "the server published no checksum list for
/// 24.04" and "the image that arrived does not match its checksum" are
/// different accidents, and merging them would force every caller to match
/// variants that cannot occur where it stands.
#[derive(Debug)]
pub enum ResolveError {
    /// The caller supplied something that is not a release version.
    InvalidRelease(String),
    /// The transport failed: connection refused, TLS rejected, body cut short.
    Http(String),
    UnexpectedStatus {
        status: u16,
    },
    /// The body arrived but is not a list of checksums -- typically an HTML
    /// error page served with status 200.
    MalformedChecksums {
        url: String,
    },
    /// The list is a list, but this distribution does not publish that image.
    ImageNotListed {
        file_name: String,
        url: String,
    },
}

impl fmt::Display for ResolveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRelease(value) => {
                write!(
                    formatter,
                    "{value:?} is not a release version like \"24.04\""
                )
            }
            Self::Http(message) => write!(formatter, "the release lookup failed: {message}"),
            Self::UnexpectedStatus { status } => write!(
                formatter,
                "the image server answered with status {status} for the checksum list"
            ),
            Self::MalformedChecksums { url } => {
                write!(formatter, "{url} is not a list of checksums")
            }
            Self::ImageNotListed { file_name, url } => {
                write!(formatter, "{url} lists no image named {file_name}")
            }
        }
    }
}

impl std::error::Error for ResolveError {}

/// A failure checking for, or downloading, an application update.
#[derive(Debug)]
pub enum UpdateDownloadError {
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    Http(String),
    UnexpectedStatus {
        status: u16,
    },
    UnpublishedRelease,
    ManifestAssetMissing,
    ResponseTooLarge {
        limit: u64,
    },
    MalformedRelease(String),
    SizeMismatch {
        expected: u64,
        actual: u64,
    },
    ChecksumMismatch {
        expected: String,
        actual: String,
    },
    Cancelled,
}

impl fmt::Display for UpdateDownloadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                operation,
                path,
                source,
            } => write!(
                formatter,
                "failed to {operation} at {}: {source}",
                path.display()
            ),
            Self::Http(message) => write!(formatter, "the update request failed: {message}"),
            Self::UnexpectedStatus { status } => {
                write!(formatter, "the update server answered with status {status}")
            }
            Self::UnpublishedRelease => {
                formatter.write_str("the update server returned a draft or prerelease")
            }
            Self::ManifestAssetMissing => {
                formatter.write_str("the update release has no release-manifest.json asset")
            }
            Self::ResponseTooLarge { limit } => {
                write!(
                    formatter,
                    "the update response exceeds its {limit}-byte limit"
                )
            }
            Self::MalformedRelease(message) => {
                write!(
                    formatter,
                    "the update release response is malformed: {message}"
                )
            }
            Self::SizeMismatch { expected, actual } => write!(
                formatter,
                "the downloaded installer is {actual} bytes instead of the expected {expected}"
            ),
            Self::ChecksumMismatch { expected, actual } => write!(
                formatter,
                "the downloaded installer hashes to {actual} instead of the expected {expected}"
            ),
            Self::Cancelled => formatter.write_str("the update download was cancelled"),
        }
    }
}

impl std::error::Error for UpdateDownloadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// A refusal to read a qcow2 image, or a failure part-way through reading one.
///
/// Most variants are refusals rather than failures, and that is the point: the
/// file arrived over the network, and every feature of the format we cannot
/// account for is answered with a name and a number rather than with whatever
/// an unmaintained parser happens to do with it.
#[derive(Debug)]
pub enum Qcow2Error {
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    /// The file does not begin with the qcow magic.
    NotQcow2 { path: PathBuf },
    /// Version 1, or a version from the future. Version 1 has no feature bits
    /// at all, so there is no way to learn what an image is asking of us.
    UnsupportedVersion { version: u32 },
    /// A bit is set in the incompatible-features field that this reader does
    /// not implement, or does not know at all. The spec requires refusing.
    UnsupportedFeatures { bits: u64 },
    /// The compression type field names a codec we have no decoder for.
    UnsupportedCompression { compression_type: u8 },
    /// Cluster contents are encrypted; without the key they are noise.
    Encrypted { crypt_method: u32 },
    /// The image is an overlay: its holes mean "read the parent", not "zero".
    BackingFile,
    /// The image carries internal snapshots, so which L1 table is current is a
    /// question we would rather not answer on an untrusted file.
    Snapshots { count: u32 },
    /// A cluster size outside 512 bytes .. 2 MiB, which is what every writer of
    /// the format keeps to and what bounds our own allocations.
    UnsupportedClusterSize { cluster_bits: u32 },
    /// The image is bigger than the disk it was going to be written to.
    TooLarge { virtual_size: u64, capacity: u64 },
    /// The header is self-contradictory, or the parser could not make sense of
    /// the file. Untrusted input reaching this is expected; the message says
    /// what disagreed with what.
    Malformed(String),
}

impl fmt::Display for Qcow2Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                operation,
                path,
                source,
            } => write!(
                formatter,
                "failed to {operation} at {}: {source}",
                path.display()
            ),
            Self::NotQcow2 { path } => {
                write!(formatter, "{} is not a qcow2 image", path.display())
            }
            Self::UnsupportedVersion { version } => write!(
                formatter,
                "the image is qcow version {version}; only versions 2 and 3 are supported"
            ),
            Self::UnsupportedFeatures { bits } => write!(
                formatter,
                "the image requires qcow2 features this reader does not implement \
                 (incompatible feature bits {bits:#018x})"
            ),
            Self::UnsupportedCompression { compression_type } => write!(
                formatter,
                "the image uses compression type {compression_type}; only zlib and zstd are \
                 supported"
            ),
            Self::Encrypted { crypt_method } => write!(
                formatter,
                "the image is encrypted (crypt method {crypt_method}) and cannot be read"
            ),
            Self::BackingFile => formatter.write_str(
                "the image has a backing file, so it is an overlay rather than a whole disk",
            ),
            Self::Snapshots { count } => write!(
                formatter,
                "the image carries {count} internal snapshots and cannot be read"
            ),
            Self::UnsupportedClusterSize { cluster_bits } => write!(
                formatter,
                "the image declares a cluster size of 2^{cluster_bits} bytes, which is outside \
                 the supported 512 bytes to 2 MiB"
            ),
            Self::TooLarge {
                virtual_size,
                capacity,
            } => write!(
                formatter,
                "the image holds a {virtual_size}-byte disk, which does not fit in the \
                 {capacity} bytes it was to be written to"
            ),
            Self::Malformed(message) => write!(formatter, "the image is malformed: {message}"),
        }
    }
}

impl std::error::Error for Qcow2Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Builds the `Io` variant for a fallible call on the image file.
pub(crate) fn qcow2_io_error(
    operation: &'static str,
    path: &Path,
) -> impl FnOnce(io::Error) -> Qcow2Error + use<> {
    let path = path.to_path_buf();
    move |source| Qcow2Error::Io {
        operation,
        path,
        source,
    }
}
