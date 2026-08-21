#[cfg(feature = "builder")]
pub mod builder;
mod cache;
mod catalog;
mod manifest;
mod release;
mod staging;

#[cfg(test)]
pub(crate) use catalog::test_entry;
pub use catalog::{
    CatalogEntry, GuestSelector, GuestTarget, MesaPolicy, PayloadCatalog, RendererCapability,
};
pub use manifest::{PayloadManifest, ReadyMarker, SourceManifest};
// The primitives every payload shares, re-exported under the names this
// crate's callers already use: what moved is where they are defined.
pub use cache::{PrepareRequest, ReadyGpuPayload, prepare};
pub use release::{LOCAL_ARCHIVE_DIRECTORY, local_archive_path, local_entry_path};
pub use staging::{StagedGpuPayload, ensure_staging_root, stage_payload};
pub use vmlord_payload::{PayloadError, PayloadProgress, PreparedFile, Sha256Digest};
