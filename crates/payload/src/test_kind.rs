//! A payload kind that exists only so the mechanism can be tested as one.
//!
//! The shared half must be exercised without a real payload, or its tests
//! would be tests of whichever kind was borrowed for them -- and the point of
//! [`PayloadEntry`](crate::PayloadEntry) is that no kind is privileged. This
//! is the smallest thing that satisfies the trait: an identity, two digests,
//! two limits, a manifest that is its file list and provenance that claims
//! nothing.

use serde::{Deserialize, Serialize};

use crate::{PayloadEntry, PayloadError, PayloadFiles, PayloadSources, PreparedFile, Sha256Digest};

#[derive(Serialize)]
pub struct TestEntry {
    payload_id: String,
    archive_sha256: Sha256Digest,
    payload_manifest_sha256: Sha256Digest,
    expanded_size_limit: u64,
    file_count_limit: u64,
}

impl TestEntry {
    #[must_use]
    pub fn new(
        archive_sha256: Sha256Digest,
        payload_manifest_sha256: Sha256Digest,
        expanded_size_limit: u64,
        file_count_limit: u64,
    ) -> Self {
        Self {
            payload_id: "test".to_owned(),
            archive_sha256,
            payload_manifest_sha256,
            expanded_size_limit,
            file_count_limit,
        }
    }
}

/// The declared files, and whatever else the document happens to carry.
///
/// Deliberately lenient about unknown fields: what a manifest must say beyond
/// its files is each kind's own rule, and enforcing one here would make the
/// mechanism's tests depend on a rule the mechanism does not have.
#[derive(Deserialize)]
pub struct TestManifest {
    files: Vec<PreparedFile>,
}

impl PayloadFiles for TestManifest {
    fn files(&self) -> &[PreparedFile] {
        &self.files
    }
}

#[derive(Deserialize, Serialize)]
pub struct TestSources {
    schema_version: u32,
}

impl PayloadSources<TestManifest> for TestSources {
    /// Nothing: a kind with no cross-check between provenance and manifest is
    /// a kind that answers `Ok`, which is what makes the hook optional in
    /// practice without being optional in the trait.
    fn validate_prepared_files(&self, _manifest: &TestManifest) -> Result<(), PayloadError> {
        Ok(())
    }
}

impl PayloadEntry for TestEntry {
    type Manifest = TestManifest;
    type Sources = TestSources;
    const NAMESPACE: &'static str = "test-payload";

    fn from_json(bytes: &[u8]) -> Result<Self, PayloadError> {
        #[derive(Deserialize)]
        struct Document {
            payload_id: String,
            archive_sha256: Sha256Digest,
            payload_manifest_sha256: Sha256Digest,
            expanded_size_limit: u64,
            file_count_limit: u64,
        }

        let document: Document = serde_json::from_slice(bytes)
            .map_err(|error| PayloadError::InvalidCatalog(error.to_string()))?;
        if document.payload_id.is_empty() {
            return Err(PayloadError::InvalidCatalog("empty payload ID".into()));
        }
        Ok(Self {
            payload_id: document.payload_id,
            archive_sha256: document.archive_sha256,
            payload_manifest_sha256: document.payload_manifest_sha256,
            expanded_size_limit: document.expanded_size_limit,
            file_count_limit: document.file_count_limit,
        })
    }

    fn payload_id(&self) -> &str {
        &self.payload_id
    }

    fn archive_sha256(&self) -> &Sha256Digest {
        &self.archive_sha256
    }

    fn payload_manifest_sha256(&self) -> &Sha256Digest {
        &self.payload_manifest_sha256
    }

    fn expanded_size_limit(&self) -> u64 {
        self.expanded_size_limit
    }

    fn file_count_limit(&self) -> u64 {
        self.file_count_limit
    }

    fn parse_manifest(&self, bytes: &[u8]) -> Result<Self::Manifest, PayloadError> {
        serde_json::from_slice(bytes)
            .map_err(|error| PayloadError::InvalidManifest(error.to_string()))
    }

    fn parse_sources(&self, bytes: &[u8]) -> Result<Self::Sources, PayloadError> {
        serde_json::from_slice(bytes)
            .map_err(|error| PayloadError::InvalidManifest(error.to_string()))
    }
}
