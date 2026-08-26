//! Which display protocol revisions a payload's services can speak.

use serde::{Deserialize, Serialize};

/// The revisions of the display protocol a payload understands.
///
/// A major and a closed range of minors, mirroring how the display protocol
/// itself negotiates: a differing major cannot be negotiated at all, and
/// minors negotiate down to the lower of the two. A payload therefore declares
/// the one major its services implement and the span of minors they have been
/// built against.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolRange {
    pub major: u32,
    pub min_minor: u32,
    pub max_minor: u32,
}

/// The revision one side actually implements.
///
/// Passed in rather than read from `vmlord-display-protocol`, so this crate
/// stays free of that dependency and a test can ask what a catalog does at a
/// revision this build does not have.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProtocolVersionParts {
    pub major: u32,
    pub minor: u32,
}

impl ProtocolRange {
    /// Whether a peer at this revision can talk to the payload's services.
    #[must_use]
    pub const fn covers(&self, major: u32, minor: u32) -> bool {
        self.major == major && minor >= self.min_minor && minor <= self.max_minor
    }

    /// Whether the range is a range at all.
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        self.min_minor <= self.max_minor
    }
}

/// The releases this repository builds, as their specs are checked in.
///
/// Held here so that a protocol revision cannot be added without the payloads
/// that have to speak it being looked at in the same change.
#[cfg(test)]
const SPECS: [(&str, &str); 3] = [
    (
        "ubuntu-22.04-amd64",
        include_str!("../../../payloads/display/ubuntu-22.04-amd64/payload.spec.json"),
    ),
    (
        "ubuntu-24.04-amd64",
        include_str!("../../../payloads/display/ubuntu-24.04-amd64/payload.spec.json"),
    ),
    (
        "ubuntu-26.04-amd64",
        include_str!("../../../payloads/display/ubuntu-26.04-amd64/payload.spec.json"),
    ),
];

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::{ProtocolRange, ProtocolVersionParts, SPECS};

    /// The revision the guest services in these payloads implement.
    ///
    /// Written out rather than depended on, which is what keeps this crate
    /// free of `vmlord-display-protocol`; `vmlord-platform`, which has both,
    /// holds this against what the protocol actually negotiates.
    const CURRENT: ProtocolVersionParts = ProtocolVersionParts { major: 1, minor: 3 };

    #[derive(Deserialize)]
    struct Spec {
        version: String,
        protocol: ProtocolRange,
    }

    #[test]
    fn every_release_speaks_the_revision_this_build_negotiates() {
        for (name, document) in SPECS {
            let spec: Spec =
                serde_json::from_str(document).unwrap_or_else(|error| panic!("{name}: {error}"));

            assert!(spec.protocol.is_valid(), "{name} has no protocol range");
            assert!(
                spec.protocol.covers(CURRENT.major, CURRENT.minor),
                "{name} at {} does not cover {}.{}",
                spec.version,
                CURRENT.major,
                CURRENT.minor
            );
        }
    }

    #[test]
    fn a_payload_from_before_a_revision_is_not_promoted_into_covering_it() {
        // What 0.1.5 declared, which is every release before the file
        // clipboard: it may not be read as covering what it never spoke.
        let historical = ProtocolRange {
            major: 1,
            min_minor: 0,
            max_minor: 0,
        };

        assert!(historical.covers(1, 0));
        assert!(!historical.covers(CURRENT.major, CURRENT.minor));
    }

    #[test]
    fn a_range_covers_only_its_own_major() {
        let range = ProtocolRange {
            major: 1,
            min_minor: 0,
            max_minor: 2,
        };

        assert!(range.covers(1, 0) && range.covers(1, 2));
        assert!(
            !range.covers(1, 3),
            "a payload cannot promise a minor it has never seen"
        );
        assert!(!range.covers(2, 0), "a differing major is not negotiable");
        assert!(!range.covers(0, 9));
    }

    #[test]
    fn a_range_whose_bounds_are_inverted_is_invalid() {
        assert!(
            !ProtocolRange {
                major: 1,
                min_minor: 3,
                max_minor: 1,
            }
            .is_valid()
        );
        assert!(
            ProtocolRange {
                major: 1,
                min_minor: 1,
                max_minor: 1,
            }
            .is_valid(),
            "one revision is a range of one"
        );
    }
}
