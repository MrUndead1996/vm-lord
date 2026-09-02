//! Which release of a distribution a request names.
//!
//! The profile itself lives in `vmlord-core`: it is a table of domain facts --
//! a default user, an admin group -- that the provisioning contract reads.
//! What stays here is the check that belongs to URL building rather than to the
//! domain.

use crate::error::ResolveError;

/// The one spelling a distribution that publishes no version number gets.
///
/// Arch is the case this exists for: it releases nothing of the form `NN.NN`,
/// and its `/etc/os-release` carries `BUILD_ID=rolling` with no `VERSION_ID`
/// at all. The guest agent reports that word back, so host and guest name the
/// release identically -- which is what the payload catalogs are keyed by.
pub const ROLLING_RELEASE: &str = "rolling";

/// Accepts a release version of two or three digits, a dot and two digits, or
/// [`ROLLING_RELEASE`], and refuses everything else.
///
/// The string is pasted straight into a URL, which makes it
/// attacker-influenced input in the same sense the extension taken from a URL is
/// in `cache_file_name`: unchecked, `../..` walks the request into another
/// directory of the same server. Codenames are refused on purpose -- the server
/// redirects a version number to its codename by itself, and accepting both
/// would give one release two spellings that resolve to different file names.
/// The rolling form is one exact word rather than any word for the same reason:
/// a distribution without releases has one name for what it is, not a choice of
/// them.
pub fn validated_release(release: &str) -> Result<&str, ResolveError> {
    if release == ROLLING_RELEASE {
        return Ok(release);
    }

    let (year, month) = release
        .split_once('.')
        .ok_or_else(|| ResolveError::InvalidRelease(release.to_owned()))?;

    let digits = |part: &str, longest: usize| {
        (2..=longest).contains(&part.len()) && part.bytes().all(|byte| byte.is_ascii_digit())
    };
    if digits(year, 3) && digits(month, 2) {
        Ok(release)
    } else {
        Err(ResolveError::InvalidRelease(release.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::{ROLLING_RELEASE, validated_release};
    use crate::error::ResolveError;

    #[test]
    fn a_release_version_is_accepted_in_the_shape_canonical_publishes() {
        for candidate in ["24.04", "22.04", "24.10", "100.04"] {
            assert_eq!(validated_release(candidate).unwrap(), candidate);
        }
    }

    #[test]
    fn a_distribution_without_versions_is_accepted_under_its_one_spelling() {
        assert_eq!(validated_release(ROLLING_RELEASE).unwrap(), "rolling");
    }

    #[test]
    fn anything_that_is_not_a_version_is_refused_before_it_reaches_a_url() {
        for candidate in [
            "",
            "noble",
            "24",
            "24.4",
            "24.04.1",
            "24.04 ",
            " 24.04",
            "24.04/../..",
            "../../etc",
            "2x.04",
            "latest",
            "Rolling",
            "rolling ",
            "rolling/../..",
        ] {
            assert!(
                matches!(
                    validated_release(candidate),
                    Err(ResolveError::InvalidRelease(_))
                ),
                "{candidate:?} must not be pasted into a URL"
            );
        }
    }
}
