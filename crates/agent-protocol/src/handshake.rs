//! What the two peers have to agree on before anything else is sent.
//!
//! Two separate agreements, because they answer different questions. The
//! version says whether the peers can talk at all and which revision of the
//! schema they will use; the capabilities say which optional parts of that
//! revision are worth sending. Deciding both here rather than in the host and
//! the agent separately is what keeps them from disagreeing about what was
//! agreed.

use std::{error::Error, fmt};

use crate::v1::{Capability, ProtocolVersion};

/// The revision of the schema this build implements.
///
/// `major` changes when an existing message changes meaning; `minor` changes
/// when something is added. A guest agent is upgraded on its own schedule, so
/// this is the number a session negotiates against, never the crate version.
pub const CURRENT_VERSION: ProtocolVersion = ProtocolVersion { major: 1, minor: 1 };

impl ProtocolVersion {
    /// The revision this build implements.
    #[must_use]
    pub const fn current() -> Self {
        CURRENT_VERSION
    }
}

/// Settles on the revision both peers can speak.
///
/// The result is `local`'s major with the lower of the two minors: a peer must
/// never be sent a message from a minor it does not have, and the older side
/// of a session is the one that decides how new the conversation can be.
///
/// # Errors
///
/// [`VersionMismatch`] if the majors differ. There is nothing to negotiate
/// down to in that case -- a major bump means an existing message changed
/// meaning -- and the session must be refused with
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

/// The capabilities both peers have, in `local`'s order.
///
/// `remote` is raw wire values rather than [`Capability`] because that is what
/// the generated field holds: a newer peer may announce a capability this
/// build has never heard of, and the only sane reading of an unknown number is
/// that it is not something both sides have. Unspecified is dropped for the
/// same reason -- it is proto3's "absent", not a capability.
#[must_use]
pub fn agreed_capabilities(local: &[Capability], remote: &[i32]) -> Vec<Capability> {
    local
        .iter()
        .copied()
        .filter(|capability| *capability != Capability::Unspecified)
        .filter(|capability| remote.contains(&i32::from(*capability)))
        .collect()
}

/// Checks the revision a peer answered a hello with.
///
/// The side that sent the hello does not choose the session's revision, but it
/// does have to recognise what came back: a revision it never claimed to speak
/// is one it cannot be held to. `chosen` is accepted when it has `local`'s
/// major and a minor no higher than `local`'s.
///
/// # Errors
///
/// [`VersionMismatch`] if the majors differ or `chosen` is newer than `local`.
/// Neither is negotiable -- there is no third round in this handshake -- so the
/// connection is dropped rather than answered.
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

/// Checks the capabilities a peer answered a hello with.
///
/// The agreed set is the intersection of what the two peers announced, so
/// everything in it must be something this side offered. A capability that is
/// not -- including a number this build has never heard of -- is a peer
/// claiming the session may carry messages nothing here answers, which is worse
/// than a session without that capability.
///
/// `chosen` is raw wire values for the reason
/// [`agreed_capabilities`] takes them: that is what the generated field holds,
/// and an unknown number has to be judged rather than dropped on this side.
///
/// # Errors
///
/// [`UnofferedCapability`] carrying the first value that was not offered.
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
    pub local: ProtocolVersion,
    pub remote: ProtocolVersion,
}

impl fmt::Display for VersionMismatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "this build speaks agent protocol {}.{} and the peer speaks {}.{}",
            self.local.major, self.local.minor, self.remote.major, self.remote.minor
        )
    }
}

impl Error for VersionMismatch {}

#[cfg(test)]
mod tests {
    use super::*;

    const fn version(major: u32, minor: u32) -> ProtocolVersion {
        ProtocolVersion { major, minor }
    }

    #[test]
    fn a_session_speaks_the_older_peers_minor() {
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
    fn differing_majors_are_not_negotiable() {
        assert_eq!(
            negotiate_version(version(1, 0), version(2, 0)),
            Err(VersionMismatch {
                local: version(1, 0),
                remote: version(2, 0),
            })
        );
    }

    #[test]
    fn only_capabilities_both_peers_have_are_agreed() {
        assert_eq!(
            agreed_capabilities(&[Capability::Gpu], &[i32::from(Capability::Gpu)]),
            vec![Capability::Gpu]
        );
        assert!(agreed_capabilities(&[Capability::Gpu], &[]).is_empty());
    }

    #[test]
    fn a_capability_this_build_does_not_know_is_not_agreed() {
        // What a peer from a future minor announces.
        let remote = [i32::from(Capability::Gpu), 4242];

        assert_eq!(
            agreed_capabilities(&[Capability::Gpu], &remote),
            vec![Capability::Gpu]
        );
    }

    #[test]
    fn unspecified_is_never_a_capability() {
        let unspecified = i32::from(Capability::Unspecified);

        assert!(agreed_capabilities(&[Capability::Unspecified], &[unspecified]).is_empty());
    }

    #[test]
    fn a_session_may_run_at_the_confirming_peers_revision_or_older() {
        assert_eq!(
            confirm_version(version(1, 4), version(1, 4)),
            Ok(version(1, 4))
        );
        assert_eq!(
            confirm_version(version(1, 4), version(1, 0)),
            Ok(version(1, 0))
        );
    }

    #[test]
    fn a_revision_this_peer_never_claimed_is_not_confirmed() {
        // A minor above this build's is a peer answering with messages this
        // side has no arm for; a differing major is not a session at all.
        assert_eq!(
            confirm_version(version(1, 2), version(1, 3)),
            Err(VersionMismatch {
                local: version(1, 2),
                remote: version(1, 3),
            })
        );
        assert_eq!(
            confirm_version(version(1, 2), version(2, 0)),
            Err(VersionMismatch {
                local: version(1, 2),
                remote: version(2, 0),
            })
        );
    }

    #[test]
    fn a_subset_of_what_this_peer_offered_is_confirmed() {
        assert_eq!(
            confirm_capabilities(&[Capability::Gpu], &[i32::from(Capability::Gpu)]),
            Ok(vec![Capability::Gpu])
        );
        assert_eq!(
            confirm_capabilities(&[Capability::Gpu], &[]),
            Ok(Vec::new())
        );
        assert_eq!(confirm_capabilities(&[], &[]), Ok(Vec::new()));
    }

    #[test]
    fn a_capability_this_peer_never_offered_is_refused() {
        assert_eq!(
            confirm_capabilities(&[], &[i32::from(Capability::Gpu)]),
            Err(UnofferedCapability {
                value: i32::from(Capability::Gpu),
            })
        );
    }

    #[test]
    fn a_capability_number_this_build_cannot_name_is_refused() {
        // Unlike an announcement, an agreed set is what both sides may use, so
        // an unknown number here cannot be dropped and read as agreement.
        assert_eq!(
            confirm_capabilities(&[Capability::Gpu], &[4242]),
            Err(UnofferedCapability { value: 4242 })
        );
        assert_eq!(
            confirm_capabilities(
                &[Capability::Unspecified],
                &[i32::from(Capability::Unspecified)]
            ),
            Err(UnofferedCapability {
                value: i32::from(Capability::Unspecified),
            })
        );
    }
}
