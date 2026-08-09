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
