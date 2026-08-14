use std::{fmt, io, path::PathBuf};
use crate::{GuestTarget, Sha256Digest};

#[derive(Debug)]
pub enum PayloadError {
    InvalidDigest(String), InvalidCatalog(String), UnsupportedTarget(GuestTarget), InvalidManifest(String),
    AlreadyInProgress { path: PathBuf }, ArchiveSizeMismatch { expected: u64, actual: u64 },
    DigestMismatch { subject: String, expected: Sha256Digest, actual: Sha256Digest }, UnsafeArchive(String),
    LimitExceeded { subject: &'static str, limit: u64, actual: u64 }, Cancelled, Http(String),
    Io { operation: &'static str, path: PathBuf, source: io::Error }, Archive(String), ConflictingGeneration { path: PathBuf },
}
impl PayloadError { pub(crate) fn io(operation: &'static str, path: PathBuf, source: io::Error) -> Self { Self::Io { operation, path, source } } }
impl fmt::Display for PayloadError { fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { match self { Self::InvalidDigest(value) => write!(f, "invalid SHA-256 digest: {value}"), Self::InvalidCatalog(message) | Self::InvalidManifest(message) | Self::UnsafeArchive(message) | Self::Http(message) | Self::Archive(message) => f.write_str(message), Self::UnsupportedTarget(target) => write!(f, "unsupported GPU payload target: {target:?}"), Self::AlreadyInProgress { path } => write!(f, "GPU payload preparation already in progress: {}", path.display()), Self::ArchiveSizeMismatch { expected, actual } => write!(f, "archive size mismatch: expected {expected}, got {actual}"), Self::DigestMismatch { subject, expected, actual } => write!(f, "digest mismatch for {subject}: expected {expected}, got {actual}"), Self::LimitExceeded { subject, limit, actual } => write!(f, "{subject} limit {limit} exceeded by {actual}"), Self::Cancelled => f.write_str("GPU payload operation cancelled"), Self::Io { operation, path, source } => write!(f, "could not {operation} {}: {source}", path.display()), Self::ConflictingGeneration { path } => write!(f, "conflicting GPU payload generation: {}", path.display()) } } }
impl std::error::Error for PayloadError { fn source(&self) -> Option<&(dyn std::error::Error + 'static)> { match self { Self::Io { source, .. } => Some(source), _ => None } } }
