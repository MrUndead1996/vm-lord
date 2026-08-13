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
pub const CURRENT_VERSION: ProtocolVersion = ProtocolVersion { major: 1, minor: 0 };

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
}
