//! The contract between VMLord on the host and `vmlord-agent` in a guest.
//!
//! Both sides depend on this crate, so it is portable by construction: no
//! Windows APIs, no Linux syscalls, no transport. It knows what a message is
//! and how one is delimited on a byte stream; opening the HvSocket that
//! carries the bytes belongs to `vmlord-platform` on the host and to
//! `vmlord-agent` in the guest.
//!
//! The schema lives in `proto/vmlord/agent/v1/agent.proto` and is compiled at
//! build time. [`FILE_DESCRIPTOR_SET`] is the same schema in the form other
//! tools read, checked in beside the `.proto` so that a change to the wire
//! format shows up in a diff.

pub mod auth;
pub mod backoff;
pub mod envelope;
pub mod frame;
pub mod handshake;

/// The generated types for `vmlord.agent.v1`.
///
/// The whole schema is one version module. A `v2` would be a second module
/// beside it rather than an edit of this one: the host has to be able to keep
/// talking to agents it has already installed while it learns a new major.
pub mod v1 {
    // Generated code is not written to this repository's standards and cannot
    // be, so it is not linted against them.
    #![allow(clippy::all, clippy::pedantic, missing_docs)]

    include!(concat!(env!("OUT_DIR"), "/vmlord.agent.v1.rs"));
}

/// The compiled schema, for tools that read descriptor sets rather than Rust.
///
/// Checked in rather than embedded from the build directory: it is the
/// artifact a reviewer can point at to say what the wire format was at a given
/// commit, and `tests/descriptor.rs` fails if it stops matching the `.proto`.
pub const FILE_DESCRIPTOR_SET: &[u8] = include_bytes!("../proto/agent.descriptor.bin");

#[cfg(test)]
mod tests {
    use crate::v1::DisplayUpdateOutcome;

    #[test]
    fn an_installed_update_that_needs_a_reboot_has_its_own_outcome() {
        assert_eq!(
            DisplayUpdateOutcome::from_str_name("DISPLAY_UPDATE_OUTCOME_REBOOT_REQUIRED")
                .map(|outcome| outcome.as_str_name()),
            Some("DISPLAY_UPDATE_OUTCOME_REBOOT_REQUIRED")
        );
    }
}
