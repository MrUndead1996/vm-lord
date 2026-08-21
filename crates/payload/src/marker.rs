//! What a prepared or staged payload leaves behind to say it is finished.
//!
//! Two records, both written last and both read before anything trusts a
//! directory: the ready marker names the generation a directory holds, and the
//! provenance record says what that generation was made of. Neither is
//! specific to a kind of payload -- what they carry about the kind is the
//! entry itself, serialized whole.

use serde::{Deserialize, Serialize};

use crate::{PayloadEntry, PayloadError, ReadyPayload, Sha256Digest};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReadyMarker {
    schema_version: u32,
    payload_id: String,
    generation: Sha256Digest,
    payload_manifest_sha256: Sha256Digest,
}

impl ReadyMarker {
    #[must_use]
    pub fn new<E: PayloadEntry>(entry: &E) -> Self {
        Self {
            schema_version: 1,
            payload_id: entry.payload_id().to_owned(),
            generation: entry.archive_sha256().clone(),
            payload_manifest_sha256: entry.payload_manifest_sha256().clone(),
        }
    }

    #[must_use]
    pub fn new_for<E: PayloadEntry>(payload: &ReadyPayload<E>) -> Self {
        Self {
            schema_version: 1,
            payload_id: payload.payload_id().to_owned(),
            generation: payload.generation().clone(),
            payload_manifest_sha256: payload.payload_manifest_sha256().clone(),
        }
    }

    /// The marker as it is written: one JSON object and a newline.
    ///
    /// # Errors
    ///
    /// [`PayloadError::InvalidManifest`] if the marker cannot be serialized,
    /// which four owned fields cannot be in practice.
    pub fn to_json_bytes(&self) -> Result<Vec<u8>, PayloadError> {
        let mut bytes = serde_json::to_vec(self)
            .map_err(|error| PayloadError::InvalidManifest(error.to_string()))?;
        bytes.push(b'\n');
        Ok(bytes)
    }
}

/// What a cached generation records about where it came from.
///
/// The whole entry and the whole provenance document, rather than a summary of
/// either: a cache that outlives the release it was filled from must be able
/// to say what it holds without that release being present.
///
/// # Errors
///
/// [`PayloadError::InvalidManifest`] if the record cannot be serialized.
pub fn cache_provenance<E: PayloadEntry>(
    entry: &E,
    sources: &E::Sources,
) -> Result<Vec<u8>, PayloadError> {
    #[derive(Serialize)]
    struct CacheProvenance<'a, E: PayloadEntry> {
        schema_version: u32,
        payload_id: &'a str,
        archive_sha256: &'a Sha256Digest,
        payload_manifest_sha256: &'a Sha256Digest,
        catalog_entry: &'a E,
        sources: &'a E::Sources,
    }

    let value = CacheProvenance {
        schema_version: 1,
        payload_id: entry.payload_id(),
        archive_sha256: entry.archive_sha256(),
        payload_manifest_sha256: entry.payload_manifest_sha256(),
        catalog_entry: entry,
        sources,
    };
    let mut bytes = serde_json::to_vec(&value)
        .map_err(|error| PayloadError::InvalidManifest(error.to_string()))?;
    bytes.push(b'\n');
    Ok(bytes)
}
