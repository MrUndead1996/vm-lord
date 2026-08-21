mod catalog;
mod manifest;
mod release;
mod archive;
mod cache;
mod staging;
#[cfg(feature = "builder")]
pub mod builder;

pub use catalog::{
    CatalogEntry, GuestSelector, GuestTarget, MesaPolicy, PayloadCatalog, RendererCapability,
};
#[cfg(test)]
pub(crate) use catalog::test_entry;
pub use manifest::{PayloadManifest, ReadyMarker, SourceManifest};
// The primitives every payload shares, re-exported under the names this
// crate's callers already use: what moved is where they are defined.
pub use vmlord_payload::{PayloadError, PayloadProgress, PreparedFile, Sha256Digest};
pub use release::{LOCAL_ARCHIVE_DIRECTORY, local_archive_path, local_entry_path};
pub use cache::{prepare, PrepareRequest, ReadyGpuPayload};
pub use staging::{ensure_staging_root, stage_payload, StagedGpuPayload};
