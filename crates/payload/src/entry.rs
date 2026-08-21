//! Where the mechanism meets a kind of payload.
//!
//! Everything the shared half does -- verify an archive, expand it under
//! limits, cache it by content, stage it into a VM -- needs to identify a
//! payload, know what it may cost, and read the two documents at the root of
//! it. That is this trait, and it is deliberately no larger: what a target is,
//! what provenance means, which guest an entry applies to and which of several
//! entries wins are questions only the payload's own crate can answer, and a
//! method here would be this crate pretending to.

use crate::{PayloadError, PreparedFile, Sha256Digest};

pub trait PayloadEntry: serde::Serialize + Sized {
    /// The parsed `payload.json` of this kind of payload.
    type Manifest: PayloadFiles;

    /// The parsed `sources.json`: provenance, and whatever else a kind says a
    /// payload owes its sources. The shared half writes it into the cache's
    /// provenance record and gives it one look at the manifest; what makes one
    /// valid is the kind's business.
    type Sources: PayloadSources<Self::Manifest>;

    /// The directory this kind of payload owns, in the cache and in a release.
    ///
    /// Two kinds must not share it even where their digests could not collide:
    /// quarantining a broken generation of one must not reach into the other.
    const NAMESPACE: &'static str;

    /// Reads one entry document, as a release carries it beside its archive.
    ///
    /// # Errors
    ///
    /// [`PayloadError::InvalidCatalog`] for a document that does not parse or
    /// does not pass the kind's own validation.
    fn from_json(bytes: &[u8]) -> Result<Self, PayloadError>;

    fn payload_id(&self) -> &str;

    fn archive_sha256(&self) -> &Sha256Digest;

    fn payload_manifest_sha256(&self) -> &Sha256Digest;

    fn expanded_size_limit(&self) -> u64;

    fn file_count_limit(&self) -> u64;

    /// Parses `payload.json` and cross-checks it against this entry.
    ///
    /// # Errors
    ///
    /// [`PayloadError::InvalidManifest`] when the document does not parse or
    /// does not describe this entry.
    fn parse_manifest(&self, bytes: &[u8]) -> Result<Self::Manifest, PayloadError>;

    /// Parses `sources.json` and cross-checks it against this entry.
    ///
    /// # Errors
    ///
    /// [`PayloadError::InvalidManifest`] when the provenance does not match
    /// what the entry claims.
    fn parse_sources(&self, bytes: &[u8]) -> Result<Self::Sources, PayloadError>;
}

/// What every payload manifest has in common: the files it declares.
pub trait PayloadFiles {
    fn files(&self) -> &[PreparedFile];
}

/// A payload's provenance, and the one question the mechanism asks of it.
///
/// Expansion ends by handing the sources the manifest, because a kind may owe
/// its own cross-check between the two -- GPU checks that every overlay it
/// claims is declared with the digest it claims. A kind with nothing to say
/// answers `Ok(())`.
pub trait PayloadSources<M: PayloadFiles>: serde::Serialize {
    /// # Errors
    ///
    /// [`PayloadError::InvalidManifest`] when the provenance and the manifest
    /// disagree about a file.
    fn validate_prepared_files(&self, manifest: &M) -> Result<(), PayloadError>;
}
