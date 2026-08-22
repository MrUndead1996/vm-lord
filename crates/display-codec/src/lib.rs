//! The lossless desktop codec of VMLord's display stack.
//!
//! Turns a captured guest framebuffer into the opaque payloads the display
//! protocol's frame channel carries, and turns them back into pixels. It knows
//! nothing of capture, of DRM, of sockets or of Windows: the guest services
//! and the host viewer are both built against it unchanged.
//!
//! What the record header already carries -- sequence, base, checksum,
//! generation -- is not repeated here, and geometry arrives out of band in a
//! `StreamConfig`, which is why [`Geometry`] is constructor input rather than
//! something a payload may change.

mod container;
pub mod error;
pub mod geometry;
mod varint;
mod zrle;

pub use error::CodecError;
pub use geometry::{Geometry, MAX_DIMENSION, PixelFormat, Rect, TileSize};
