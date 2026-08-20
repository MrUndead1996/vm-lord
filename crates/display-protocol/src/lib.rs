//! The wire contract between VMLord's display viewer on the host and the
//! display services in a guest.
//!
//! Portable by construction: no Windows APIs, no Linux syscalls, no transport.
//! It knows what a record is, how one is delimited, what proves a peer, and
//! what a session's states are; opening the three HvSocket services that carry
//! the bytes belongs to the host viewer and to the guest services.
//!
//! The schema lives in `proto/vmlord/display/v1/display.proto` and is compiled
//! at build time. [`FILE_DESCRIPTOR_SET`] is the same schema in the form other
//! tools read, checked in beside the `.proto` so that a change to the wire
//! format shows up in a diff.

pub mod handshake;
pub mod keys;
pub mod record;
pub mod session;

/// The generated types for `vmlord.display.v1`.
///
/// The whole schema is one version module. A `v2` would be a second module
/// beside it rather than an edit of this one.
pub mod v1 {
    // Generated code is not written to this repository's standards and cannot
    // be, so it is not linted against them.
    #![allow(clippy::all, clippy::pedantic, missing_docs)]

    include!(concat!(env!("OUT_DIR"), "/vmlord.display.v1.rs"));
}

/// The compiled schema, for tools that read descriptor sets rather than Rust.
pub const FILE_DESCRIPTOR_SET: &[u8] = include_bytes!("../proto/display.descriptor.bin");
