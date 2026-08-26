//! What the two peers have to agree on before anything else is sent.
//!
//! Two agreements, answering different questions. The version says whether the
//! peers can talk at all and which revision of the schema they use; the
//! capabilities say which optional parts of that revision are worth sending.
//!
//! These rules are `vmlord-agent-protocol`'s rules, deliberately re-stated
//! here rather than depended on. Sharing twenty lines would tie two contracts
//! that have to be versioned apart: a display major must not drag the agent's
//! schema with it.

use std::{error::Error, fmt};

use crate::v1::{Capability, ProtocolVersion};

/// The revision of the schema this build implements.
pub const CURRENT_VERSION: ProtocolVersion = ProtocolVersion { major: 1, minor: 4 };

impl ProtocolVersion {
    /// The revision this build implements.
    #[must_use]
    pub const fn current() -> Self {
        CURRENT_VERSION
    }
}

/// Settles on the revision both peers can speak.
///
/// # Errors
///
/// [`VersionMismatch`] if the majors differ. A major bump means an existing
/// message changed meaning, so there is nothing to negotiate down to and the
/// session is refused with
/// [`ErrorCode::UnsupportedVersion`](crate::v1::ErrorCode::UnsupportedVersion).
pub fn negotiate_version(
    local: ProtocolVersion,
    remote: ProtocolVersion,
) -> Result<ProtocolVersion, VersionMismatch> {
    if local.major != remote.major {
        return Err(VersionMismatch { local, remote });
    }

    Ok(ProtocolVersion {
        major: local.major,
        minor: local.minor.min(remote.minor),
    })
}

/// Checks the revision a peer answered a hello with.
///
/// # Errors
///
/// [`VersionMismatch`] if the majors differ or `chosen` is newer than
/// `local` -- a revision this side never claimed to speak is one it cannot be
/// held to, and there is no third round in this handshake.
pub fn confirm_version(
    local: ProtocolVersion,
    chosen: ProtocolVersion,
) -> Result<ProtocolVersion, VersionMismatch> {
    if local.major != chosen.major || chosen.minor > local.minor {
        return Err(VersionMismatch {
            local,
            remote: chosen,
        });
    }

    Ok(chosen)
}

/// The capabilities both peers have, in `local`'s order.
///
/// `remote` is raw wire values because that is what the generated field holds:
/// a newer peer may announce a capability this build has never heard of, and
/// the only sane reading of an unknown number is that it is not something both
/// sides have. Unspecified is dropped for the same reason -- it is proto3's
/// "absent", not a capability.
#[must_use]
pub fn agreed_capabilities(local: &[Capability], remote: &[i32]) -> Vec<Capability> {
    let mut agreed: Vec<_> = local
        .iter()
        .copied()
        .filter(|capability| *capability != Capability::Unspecified)
        .filter(|capability| remote.contains(&i32::from(*capability)))
        .collect();
    if !agreed.contains(&Capability::Clipboard) {
        agreed.retain(|capability| *capability != Capability::FileClipboard);
    }
    agreed
}

/// Removes capabilities introduced after the negotiated protocol revision.
#[must_use]
pub fn capabilities_at(
    version: ProtocolVersion,
    mut capabilities: Vec<Capability>,
) -> Vec<Capability> {
    if version.major != 1 || version.minor < 3 {
        capabilities.retain(|capability| *capability != Capability::FileClipboard);
    }
    if version.major != 1 || version.minor < 4 {
        capabilities.retain(|capability| *capability != Capability::HostDisplayModes);
    }
    capabilities
}

/// Checks the capabilities a peer answered a hello with.
///
/// # Errors
///
/// [`UnofferedCapability`] carrying the first value this side never offered.
/// A peer claiming otherwise is claiming the session may carry messages
/// nothing here answers, which is worse than a session without that
/// capability.
pub fn confirm_capabilities(
    local: &[Capability],
    chosen: &[i32],
) -> Result<Vec<Capability>, UnofferedCapability> {
    chosen
        .iter()
        .map(|value| {
            Capability::try_from(*value)
                .ok()
                .filter(|capability| *capability != Capability::Unspecified)
                .filter(|capability| local.contains(capability))
                .ok_or(UnofferedCapability { value: *value })
        })
        .collect()
}

/// A peer that agreed on a capability this side never announced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnofferedCapability {
    /// The wire value, which may be one this build cannot name.
    pub value: i32,
}

impl fmt::Display for UnofferedCapability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "the peer agreed on capability {}, which this build did not offer",
            self.value
        )
    }
}

impl Error for UnofferedCapability {}

/// Two peers whose major versions leave nothing to talk about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VersionMismatch {
    /// What this build implements.
    pub local: ProtocolVersion,
    /// What the peer offered or chose.
    pub remote: ProtocolVersion,
}

impl fmt::Display for VersionMismatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "this build speaks display protocol {}.{} and the peer speaks {}.{}",
            self.local.major, self.local.minor, self.remote.major, self.remote.minor
        )
    }
}

impl Error for VersionMismatch {}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(major: u32, minor: u32) -> ProtocolVersion {
        ProtocolVersion { major, minor }
    }

    #[test]
    fn a_session_runs_at_the_older_peers_minor() {
        assert_eq!(
            negotiate_version(version(1, 4), version(1, 2)),
            Ok(version(1, 2))
        );
        assert_eq!(
            negotiate_version(version(1, 2), version(1, 4)),
            Ok(version(1, 2))
        );
    }

    #[test]
    fn differing_majors_leave_nothing_to_negotiate() {
        assert!(negotiate_version(version(1, 0), version(2, 0)).is_err());
    }

    #[test]
    fn a_revision_newer_than_this_build_claimed_is_not_one_it_can_be_held_to() {
        assert!(confirm_version(version(1, 2), version(1, 3)).is_err());
        assert_eq!(
            confirm_version(version(1, 2), version(1, 1)),
            Ok(version(1, 1))
        );
    }

    #[test]
    fn only_capabilities_both_peers_have_are_agreed() {
        let agreed = agreed_capabilities(
            &[Capability::CursorStream, Capability::DynamicResolution],
            &[i32::from(Capability::DynamicResolution)],
        );

        assert_eq!(agreed, vec![Capability::DynamicResolution]);
    }

    #[test]
    fn a_capability_this_build_has_never_heard_of_is_dropped() {
        let agreed = agreed_capabilities(&[Capability::CursorStream], &[9999]);

        assert!(agreed.is_empty());
    }

    #[test]
    fn file_clipboard_is_agreed_only_beside_the_clipboard() {
        let local = [Capability::Clipboard, Capability::FileClipboard];

        assert_eq!(
            agreed_capabilities(
                &local,
                &[
                    i32::from(Capability::Clipboard),
                    i32::from(Capability::FileClipboard),
                ],
            ),
            local
        );
        assert!(
            agreed_capabilities(
                &[Capability::FileClipboard],
                &[i32::from(Capability::FileClipboard)],
            )
            .is_empty()
        );
    }

    #[test]
    fn file_clipboard_does_not_exist_at_protocol_one_two() {
        assert_eq!(
            capabilities_at(
                version(1, 2),
                vec![Capability::Clipboard, Capability::FileClipboard],
            ),
            vec![Capability::Clipboard]
        );
        assert_eq!(
            capabilities_at(
                version(1, 3),
                vec![Capability::Clipboard, Capability::FileClipboard],
            ),
            vec![Capability::Clipboard, Capability::FileClipboard]
        );
    }

    #[test]
    fn a_peer_that_agreed_on_something_never_offered_is_refused() {
        let error = confirm_capabilities(
            &[Capability::CursorStream],
            &[i32::from(Capability::DynamicResolution)],
        )
        .expect_err("a capability this side did not offer");

        assert_eq!(error.value, i32::from(Capability::DynamicResolution));
    }

    #[test]
    fn confirming_accepts_what_was_offered() {
        assert_eq!(
            confirm_capabilities(
                &[Capability::CursorStream, Capability::DynamicResolution],
                &[i32::from(Capability::CursorStream)]
            ),
            Ok(vec![Capability::CursorStream])
        );
    }
}
