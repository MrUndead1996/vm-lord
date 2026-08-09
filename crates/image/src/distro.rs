//! Which distribution to fetch, where its releases live, and what the guest
//! inside them looks like.
//!
//! A profile is a table of data, not a trait with one implementation per
//! distribution. Ubuntu and Fedora differ by a URL template, a default user, an
//! admin group and the name of a checksum file -- those are fields, not
//! behaviour, and five structs differing only in constants are exactly what
//! AGENTS.md means by unnecessary abstractions.

use crate::error::ResolveError;

/// The placeholder both templates carry.
const RELEASE_PLACEHOLDER: &str = "{release}";

/// Where a distribution publishes its cloud images, and what the guest inside
/// them looks like.
///
/// The URL is kept as two templates rather than one: the checksum file sits in
/// the same directory as the image, and a single template would have to have its
/// tail cut off to get at that directory.
pub struct DistroProfile {
    pub name: &'static str,
    pub directory_template: &'static str,
    pub file_name_template: &'static str,
    pub checksum_file: &'static str,
    /// The account cloud-init creates in the guest.
    pub default_user: &'static str,
    /// The group that account must join to hold administrative rights.
    pub admin_group: &'static str,
}

/// Ubuntu's official cloud images.
///
/// The directory is addressed by version number even though the server stores
/// it under the codename: `/releases/24.04/` answers 302 to `/releases/noble/`,
/// so a table of codenames would buy nothing and would need a line added for
/// every future release. The file name, in contrast, does carry the version
/// number rather than the codename -- verified on 24.04 and 22.04.
///
/// The architecture is baked into the template. Hyper-V here is x86_64, and a
/// field with one possible value is no better than an enum with one variant.
pub const UBUNTU: DistroProfile = DistroProfile {
    name: "Ubuntu",
    directory_template: "https://cloud-images.ubuntu.com/releases/{release}/release/",
    file_name_template: "ubuntu-{release}-server-cloudimg-amd64.img",
    checksum_file: "SHA256SUMS",
    default_user: "ubuntu",
    admin_group: "sudo",
};

impl DistroProfile {
    /// The URL of the image itself.
    pub(crate) fn image_url(&self, release: &str) -> String {
        format!("{}{}", self.directory(release), self.file_name(release))
    }

    /// The URL of the checksum file published beside it.
    pub(crate) fn checksums_url(&self, release: &str) -> String {
        format!("{}{}", self.directory(release), self.checksum_file)
    }

    /// The name the image carries inside the checksum file.
    pub(crate) fn file_name(&self, release: &str) -> String {
        self.file_name_template.replace(RELEASE_PLACEHOLDER, release)
    }

    fn directory(&self, release: &str) -> String {
        let directory = self.directory_template.replace(RELEASE_PLACEHOLDER, release);
        if directory.ends_with('/') {
            directory
        } else {
            format!("{directory}/")
        }
    }
}

/// Accepts a release version of two or three digits, a dot and two digits, and
/// refuses everything else.
///
/// The string is pasted straight into a URL, which makes it
/// attacker-influenced input in the same sense the extension taken from a URL is
/// in `cache_file_name`: unchecked, `../..` walks the request into another
/// directory of the same server. Codenames are refused on purpose -- the server
/// redirects a version number to its codename by itself, and accepting both
/// would give one release two spellings that resolve to different file names.
pub(crate) fn validated_release(release: &str) -> Result<&str, ResolveError> {
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
    use super::{DistroProfile, UBUNTU, validated_release};
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

    #[test]
    fn a_profile_builds_the_image_url_and_the_checksums_url_in_one_directory() {
        assert_eq!(
            UBUNTU.image_url("24.04"),
            "https://cloud-images.ubuntu.com/releases/24.04/release/\
             ubuntu-24.04-server-cloudimg-amd64.img"
        );
        assert_eq!(
            UBUNTU.checksums_url("24.04"),
            "https://cloud-images.ubuntu.com/releases/24.04/release/SHA256SUMS"
        );
        assert_eq!(
            UBUNTU.file_name("22.04"),
            "ubuntu-22.04-server-cloudimg-amd64.img"
        );
    }

    #[test]
    fn a_directory_template_without_a_trailing_slash_still_joins_cleanly() {
        let profile = DistroProfile {
            directory_template: "http://127.0.0.1:9/{release}",
            ..UBUNTU
        };

        assert_eq!(
            profile.checksums_url("24.04"),
            "http://127.0.0.1:9/24.04/SHA256SUMS",
            "a profile written by hand must not silently produce a glued-together URL"
        );
    }
}
