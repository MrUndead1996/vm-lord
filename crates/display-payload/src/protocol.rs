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

#[cfg(test)]
mod tests {
    use super::ProtocolRange;

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
