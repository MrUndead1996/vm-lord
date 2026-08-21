use std::{fmt, io, path::PathBuf};

use crate::Sha256Digest;

#[derive(Debug)]
pub enum PayloadError {
    InvalidDigest(String),
    InvalidCatalog(String),
    /// A target no entry has, printed by whoever had one.
    ///
    /// A string and not a target type: a GPU tuple carries a kernel and a
    /// display one carries none, and the shared error cannot name either.
    UnsupportedTarget(String),
    InvalidManifest(String),
    NoPayloadForGuest {
        distribution: String,
        release: String,
        architecture: String,
    },
    AlreadyInProgress {
        path: PathBuf,
    },
    ArchiveSizeMismatch {
        expected: u64,
        actual: u64,
    },
    DigestMismatch {
        subject: String,
        expected: Sha256Digest,
        actual: Sha256Digest,
    },
    UnsafeArchive(String),
    LimitExceeded {
        subject: &'static str,
        limit: u64,
        actual: u64,
    },
    Cancelled,
    Http(String),
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    Archive(String),
    ConflictingGeneration {
        path: PathBuf,
    },
}

impl PayloadError {
    /// An I/O failure, named by what was being attempted.
    ///
    /// Public because two crates build payload errors now: the shared
    /// mechanism and whichever payload kind is reading its own documents.
    pub fn io(operation: &'static str, path: PathBuf, source: io::Error) -> Self {
        Self::Io {
            operation,
            path,
            source,
        }
    }
}

impl fmt::Display for PayloadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDigest(value) => write!(formatter, "invalid SHA-256 digest: {value}"),
            Self::InvalidCatalog(message)
            | Self::InvalidManifest(message)
            | Self::UnsafeArchive(message)
            | Self::Http(message)
            | Self::Archive(message) => formatter.write_str(message),
            Self::UnsupportedTarget(target) => {
                write!(formatter, "unsupported payload target: {target}")
            }
            Self::NoPayloadForGuest {
                distribution,
                release,
                architecture,
            } => write!(
                formatter,
                "no payload for {distribution} {release} {architecture}"
            ),
            Self::AlreadyInProgress { path } => write!(
                formatter,
                "payload preparation already in progress: {}",
                path.display()
            ),
            Self::ArchiveSizeMismatch { expected, actual } => write!(
                formatter,
                "archive size mismatch: expected {expected}, got {actual}"
            ),
            Self::DigestMismatch {
                subject,
                expected,
                actual,
            } => write!(
                formatter,
                "digest mismatch for {subject}: expected {expected}, got {actual}"
            ),
            Self::LimitExceeded {
                subject,
                limit,
                actual,
            } => write!(formatter, "{subject} limit {limit} exceeded by {actual}"),
            Self::Cancelled => formatter.write_str("payload operation cancelled"),
            Self::Io {
                operation,
                path,
                source,
            } => write!(
                formatter,
                "could not {operation} {}: {source}",
                path.display()
            ),
            Self::ConflictingGeneration { path } => write!(
                formatter,
                "conflicting payload generation: {}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for PayloadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}
