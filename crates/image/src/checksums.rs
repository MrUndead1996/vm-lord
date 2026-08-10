//! Reading a distribution's published list of checksums.

use crate::error::ResolveError;

/// The number of hex characters in a SHA256.
const CHECKSUM_LENGTH: usize = 64;

/// Finds the checksum `file_name` is published with. `url` names the file only
/// so failures can say which one.
///
/// Two failures are told apart on purpose. Nothing parsed at all means the body
/// is not a checksum list -- typically an HTML error page served with status
/// 200. Lines parsed but none of them ours means the distribution does not
/// publish that image. To the user these are different pieces of news, and
/// merging them would throw away the only useful half of the diagnosis.
pub(crate) fn parse_sha256sums(
    text: &str,
    file_name: &str,
    url: &str,
) -> Result<String, ResolveError> {
    let mut parsed = 0usize;
    let mut unreadable = 0usize;
    let mut found = None;

    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        match parse_line(line) {
            Some((checksum, name)) => {
                parsed += 1;
                if name == file_name && found.is_none() {
                    found = Some(checksum.to_ascii_lowercase());
                }
            }
            None => unreadable += 1,
        }
    }

    if parsed == 0 {
        return Err(ResolveError::MalformedChecksums {
            url: url.to_owned(),
        });
    }
    if unreadable > 0 {
        log::warn!("{unreadable} lines of {url} are not checksum entries and were skipped");
    }
    log::debug!("{url} lists {parsed} checksums");

    found.ok_or_else(|| ResolveError::ImageNotListed {
        file_name: file_name.to_owned(),
        url: url.to_owned(),
    })
}

/// Splits one line into its checksum and the name it belongs to.
///
/// The name is everything after the first run of whitespace, so a name with a
/// space in it survives; a leading `*` marks binary mode and is not part of the
/// name. The checksum must be 64 hex characters, which is what tells a checksum
/// line from a line of prose.
fn parse_line(line: &str) -> Option<(&str, &str)> {
    let (checksum, rest) = line.split_once(char::is_whitespace)?;
    if checksum.len() != CHECKSUM_LENGTH || !checksum.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }

    let name = rest.trim_start();
    // `str::lines` leaves the carriage return of a CRLF file behind, and a name
    // ending in one matches nothing.
    let name = name.strip_prefix('*').unwrap_or(name).trim_end();
    (!name.is_empty()).then_some((checksum, name))
}

#[cfg(test)]
mod tests {
    use super::parse_sha256sums;
    use crate::error::ResolveError;

    /// The real thing, trimmed: the same file the resolver meets in production.
    const FIXTURE: &str = include_str!("../tests/fixtures/ubuntu-24.04-SHA256SUMS");

    const URL: &str = "http://example.test/SHA256SUMS";
    const IMAGE: &str = "ubuntu-24.04-server-cloudimg-amd64.img";

    #[test]
    fn the_published_file_gives_up_the_checksum_of_the_image_we_asked_for() {
        assert_eq!(
            parse_sha256sums(FIXTURE, IMAGE, URL).unwrap(),
            "0533b0655c32e68b31d792ecd6ccfca95abdbc536c4446874fe0513bd4140ffe"
        );
    }

    #[test]
    fn a_name_is_matched_whole_and_never_by_its_beginning() {
        let error = parse_sha256sums(FIXTURE, "ubuntu-24.04-server-cloudimg-amd64", URL)
            .expect_err("a dozen lines start with this, and none of them is an image");

        assert!(
            matches!(error, ResolveError::ImageNotListed { .. }),
            "got {error:?}"
        );
    }

    #[test]
    fn another_architecture_keeps_its_own_checksum() {
        assert_eq!(
            parse_sha256sums(FIXTURE, "ubuntu-24.04-server-cloudimg-arm64.img", URL).unwrap(),
            "aa6da05756e85ea6dde4836b841fecb10cfd1ba3bcea320189d9af945db70476"
        );
    }

    #[test]
    fn the_binary_mode_marker_is_optional() {
        let text = "\
0533b0655c32e68b31d792ecd6ccfca95abdbc536c4446874fe0513bd4140ffe  plain.img
137cdec6444a55a85c5b0ee93e77e23901f50155c55147a0a2d02025551f1b1f *starred.img
";

        assert_eq!(
            parse_sha256sums(text, "plain.img", URL).unwrap(),
            "0533b0655c32e68b31d792ecd6ccfca95abdbc536c4446874fe0513bd4140ffe",
            "coreutils writes the asterisk only for binary mode"
        );
        assert_eq!(
            parse_sha256sums(text, "starred.img", URL).unwrap(),
            "137cdec6444a55a85c5b0ee93e77e23901f50155c55147a0a2d02025551f1b1f"
        );
    }

    #[test]
    fn a_checksum_is_reported_lowercase_however_it_was_published() {
        let text = "0533B0655C32E68B31D792ECD6CCFCA95ABDBC536C4446874FE0513BD4140FFE *upper.img\n";

        assert_eq!(
            parse_sha256sums(text, "upper.img", URL).unwrap(),
            "0533b0655c32e68b31d792ecd6ccfca95abdbc536c4446874fe0513bd4140ffe",
            "the caller feeds this straight to fetch_image, which wants lowercase"
        );
    }

    #[test]
    fn carriage_returns_do_not_become_part_of_the_name() {
        let text = "0533b0655c32e68b31d792ecd6ccfca95abdbc536c4446874fe0513bd4140ffe *dos.img\r\n";

        assert!(parse_sha256sums(text, "dos.img", URL).is_ok());
    }

    #[test]
    fn a_body_that_is_not_a_checksum_list_is_told_apart_from_a_missing_image() {
        let html = "<!DOCTYPE html>\n<html><body>404 Not Found</body></html>\n";

        let error = parse_sha256sums(html, IMAGE, URL)
            .expect_err("an error page served with status 200 is not a checksum list");

        assert!(
            matches!(error, ResolveError::MalformedChecksums { .. }),
            "got {error:?}"
        );
    }

    #[test]
    fn an_empty_body_is_malformed_rather_than_an_empty_answer() {
        assert!(matches!(
            parse_sha256sums("", IMAGE, URL),
            Err(ResolveError::MalformedChecksums { .. })
        ));
    }

    #[test]
    fn a_line_whose_first_field_is_not_a_sha256_is_not_a_checksum() {
        let text = "\
deadbeef *short.img
0533b0655c32e68b31d792ecd6ccfca95abdbc536c4446874fe0513bd4140ffe *good.img
";

        assert!(
            matches!(
                parse_sha256sums(text, "short.img", URL),
                Err(ResolveError::ImageNotListed { .. })
            ),
            "a truncated sum is not a sum, so that line names nothing"
        );
        assert!(parse_sha256sums(text, "good.img", URL).is_ok());
    }
}
