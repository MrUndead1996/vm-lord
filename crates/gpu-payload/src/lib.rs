#[cfg(feature = "builder")]
pub mod builder;
mod catalog;
mod manifest;
mod release;

#[cfg(test)]
pub(crate) use catalog::test_entry;
pub use catalog::{
    CatalogEntry, GuestSelector, GuestTarget, MesaPolicy, PayloadCatalog, RendererCapability,
};
pub use manifest::{PayloadManifest, SourceManifest};
// The primitives every payload shares, re-exported under the names this
// crate's callers already use: what moved is where they are defined.
/// A prepared GPU payload: the shared type, named for what it carries.
pub type ReadyGpuPayload = vmlord_payload::ReadyPayload<CatalogEntry>;
pub use release::{LOCAL_ARCHIVE_DIRECTORY, local_archive_path, local_entry_path};
/// A staged GPU payload generation: the shared type under the name this
/// crate's callers use.
pub type StagedGpuPayload = vmlord_payload::StagedPayload;
pub use vmlord_payload::{
    PayloadError, PayloadProgress, PrepareRequest, PreparedFile, ReadyMarker, Sha256Digest, prepare,
};
pub use vmlord_payload::{ensure_staging_root, stage_payload};
