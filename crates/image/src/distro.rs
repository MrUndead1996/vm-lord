//! Which release of a distribution a request names.
//!
//! The profile itself lives in `vmlord-core`: it is a table of domain facts --
//! a default user, an admin group -- that the provisioning contract reads.
//! What stays here is the check that belongs to URL building rather than to the
//! domain.

use crate::error::ResolveError;

/// Accepts a release version of two or three digits, a dot and two digits, and
/// refuses everything else.
///
/// The string is pasted straight into a URL, which makes it
/// attacker-influenced input in the same sense the extension taken from a URL is
/// in `cache_file_name`: unchecked, `../..` walks the request into another
/// directory of the same server. Codenames are refused on purpose -- the server
/// redirects a version number to its codename by itself, and accepting both
/// would give one release two spellings that resolve to different file names.
pub fn validated_release(release: &str) -> Result<&str, ResolveError> {
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
    use super::validated_release;
    use crate::error::ResolveError;

    #[test]
    fn a_release_version_is_accepted_in_the_shape_canonical_publishes() {
        for candidate in ["24.04", "22.04", "24.10", "100.04"] {
            assert_eq!(validated_release(candidate).unwrap(), candidate);
        }
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
