//! The guest side of VMLord's display, as an artifact a release ships.
//!
//! One payload carries everything a guest needs for its display that its own
//! apt cannot provide: the DKMS sources of VMLord's DRM module, and -- from
//! task #115 -- the guest display services. One artifact, one version, and one
//! declared range of display protocol revisions, so that what the host talks
//! to and what the guest runs cannot drift apart unnoticed.
//!
//! The mechanism underneath is `vmlord-payload`'s and is shared with the GPU
//! payload. Nothing about the display's lifecycle is: this crate knows how a
//! display payload is described and which one a guest gets, and stops there.

#[cfg(feature = "builder")]
pub mod builder;
mod catalog;
mod manifest;
mod protocol;
mod version;

pub use catalog::{
    DisplayCatalogEntry, DisplayPayloadCatalog, DisplayTarget, GuestSelector, License, Source,
};
pub use manifest::{DisplayManifest, DisplaySources, MODULE_DIRECTORY, SERVICES_DIRECTORY};
pub use protocol::{ProtocolRange, ProtocolVersionParts};

/// A prepared display payload: the shared type, named for what it carries.
pub type ReadyDisplayPayload = vmlord_payload::ReadyPayload<DisplayCatalogEntry>;
pub use version::PayloadVersion;

/// The child of the executable's directory holding shipped display payloads.
pub const LOCAL_ARCHIVE_DIRECTORY: &str = "display-payload";
