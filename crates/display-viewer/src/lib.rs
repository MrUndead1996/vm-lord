//! The host end of one VMLord display session.
//!
//! A process of its own rather than part of VMLord: a session outlives the
//! application that started it, and a crash in either must leave the other
//! standing. What is here is everything but the window -- the launch contract,
//! the session states, the decode path -- and `src/windows/` is the four
//! modules that touch Win32.

#[cfg(test)]
pub(crate) mod duplex;
pub mod input;
pub mod launch;
pub mod live;
pub mod log;
pub mod placement;
pub mod relay;
pub mod status;
pub mod video;

#[cfg(windows)]
pub mod windows;

/// The generated types for the launch pipes' private schema.
pub mod viewer {
    /// One version module, the way the wire contract has one.
    pub mod v1 {
        // Generated code is not written to this repository's standards and
        // cannot be, so it is not linted against them.
        #![allow(clippy::all, clippy::pedantic, missing_docs)]

        include!(concat!(env!("OUT_DIR"), "/vmlord.display.viewer.v1.rs"));
    }
}
