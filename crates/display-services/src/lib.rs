//! The two programs a VMLord guest runs to put its desktop on the wire.
//!
//! `vmlord-display-broker` is privileged and small; `vmlord-display-session`
//! runs hot and holds nothing worth stealing. What crosses between them is
//! [`ipc`], and it is typed operations only: no device descriptor and no ioctl
//! passthrough leaves the broker.

pub mod capture;
pub mod cursor;
pub mod drm;
pub mod ipc;
pub mod pipeline;
pub mod unix;

/// The generated types for the broker's private schema.
mod broker {
    // Generated code is not written to this repository's standards and cannot
    // be, so it is not linted against them.
    #![allow(clippy::all, clippy::pedantic, missing_docs)]

    include!(concat!(env!("OUT_DIR"), "/vmlord.display.broker.rs"));
}
