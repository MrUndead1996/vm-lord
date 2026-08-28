//! Talking to the image server, and reading what it agreed to.

use std::time::Duration;

use ureq::{
    Agent,
    tls::{RootCerts, TlsConfig},
};

use crate::error::DownloadError;

/// How long a connection may take to establish.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the server may take to start answering.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);

/// What the server agreed to send, given the range we asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResumeOutcome {
    /// The body continues where our partial file ends.
    Append { total: Option<u64> },
    /// The body starts at byte zero: whatever we had is of no use.
    StartOver,
    /// The server says the range lies past the end of the file.
    RangeUnsatisfiable,
}

/// Builds the client every image request goes through.
///
/// Certificate trust comes from the platform verifier, which on Windows means
/// the system certificate store: a machine behind a TLS-inspecting corporate
/// proxy trusts that proxy, and a client with its own compiled-in root list
/// would simply fail there with nothing the user can do.
///
/// Both timeouts are explicit. A server that accepts a connection and then says
/// nothing would otherwise park the worker thread forever.
///
/// `http_status_as_error(false)` hands the status table back to us. By default
/// `ureq` turns any 4xx or 5xx into an `Err` before returning a response, which
/// would make 416 -- a legitimate answer to a resume request, and one we act on
/// -- indistinguishable from a transport failure, and would reduce every
/// `UnexpectedStatus` to an opaque string.
pub(crate) fn build_agent() -> Agent {
    Agent::config_builder()
        .http_status_as_error(false)
        .user_agent(concat!("VMLord/", env!("CARGO_PKG_VERSION")))
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(RESPONSE_TIMEOUT))
        .tls_config(
            TlsConfig::builder()
                .root_certs(RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .new_agent()
}

/// Reads `Content-Range: bytes <first>-<last>/<complete>` into its parts.
///
/// A `*` for the complete length is legal and means the server does not know
/// it, which is reported as `None` rather than faked as a number.
pub(crate) fn parse_content_range(value: &str) -> Option<(u64, u64, Option<u64>)> {
    let range = value.trim().strip_prefix("bytes ")?;
    let (bounds, complete) = range.split_once('/')?;
    let (first, last) = bounds.split_once('-')?;
    let first = first.trim().parse::<u64>().ok()?;
    let last = last.trim().parse::<u64>().ok()?;
    let complete = match complete.trim() {
        "*" => None,
        digits => Some(digits.parse::<u64>().ok()?),
    };
    Some((first, last, complete))
}

/// Decides what to do with the answer to a `Range` request.
///
/// Kept a plain function over a status and a header so the whole table can be
/// tested without a socket.
pub(crate) fn resume_decision(
    status: u16,
    content_range: Option<&str>,
    requested_from: u64,
) -> Result<ResumeOutcome, DownloadError> {
    match status {
        206 => {
            let Some((first, _, complete)) = content_range.and_then(parse_content_range) else {
                return Err(DownloadError::Http(format!(
                    "the server answered 206 without a usable Content-Range: {}",
                    content_range.unwrap_or("<missing>")
                )));
            };
            if first != requested_from {
                return Err(DownloadError::Http(format!(
                    "the server resumed at byte {first} instead of the requested {requested_from}"
                )));
            }
            Ok(ResumeOutcome::Append { total: complete })
        }
        200 => Ok(ResumeOutcome::StartOver),
        416 => Ok(ResumeOutcome::RangeUnsatisfiable),
        status => Err(DownloadError::UnexpectedStatus { status }),
    }
}

#[cfg(test)]
mod tests {
    use super::{ResumeOutcome, parse_content_range, resume_decision};
    use crate::error::DownloadError;

    #[test]
    fn a_content_range_reports_its_bounds_and_total() {
        assert_eq!(
            parse_content_range("bytes 100-999/1000"),
            Some((100, 999, Some(1000)))
        );
        assert_eq!(
            parse_content_range("bytes 0-0/*"),
            Some((0, 0, None)),
            "a server may know the range without knowing the whole length"
        );
    }

    #[test]
    fn a_content_range_that_is_not_one_is_refused_rather_than_guessed() {
        for candidate in [
            "",
            "bytes",
            "items 1-2/3",
            "bytes 100/1000",
            "bytes abc-999/1000",
            "bytes 100-999",
        ] {
            assert_eq!(parse_content_range(candidate), None, "{candidate:?}");
        }
    }

    #[test]
    fn a_partial_content_answer_at_the_requested_offset_is_appended() {
        let outcome = resume_decision(206, Some("bytes 500-999/1000"), 500).unwrap();

        assert_eq!(outcome, ResumeOutcome::Append { total: Some(1000) });
    }

    #[test]
    fn a_partial_content_answer_at_another_offset_is_an_error() {
        let error = resume_decision(206, Some("bytes 0-999/1000"), 500)
            .expect_err("appending this to our 500 bytes would duplicate them");

        assert!(matches!(error, DownloadError::Http(_)), "got {error:?}");
    }

    #[test]
    fn a_partial_content_answer_without_a_usable_range_is_an_error() {
        assert!(matches!(
            resume_decision(206, None, 500),
            Err(DownloadError::Http(_))
        ));
        assert!(matches!(
            resume_decision(206, Some("bytes ???"), 500),
            Err(DownloadError::Http(_))
        ));
    }

    #[test]
    fn a_plain_ok_means_the_server_ignored_the_range() {
        assert_eq!(
            resume_decision(200, None, 500).unwrap(),
            ResumeOutcome::StartOver,
            "the body starts at byte zero, so what we already have is worthless"
        );
    }

    #[test]
    fn a_range_not_satisfiable_answer_asks_for_a_restart() {
        assert_eq!(
            resume_decision(416, None, 5_000).unwrap(),
            ResumeOutcome::RangeUnsatisfiable
        );
    }

    #[test]
    fn any_other_status_is_reported_as_it_came() {
        let error = resume_decision(404, None, 500).expect_err("404 is not a download");

        assert!(
            matches!(error, DownloadError::UnexpectedStatus { status: 404 }),
            "got {error:?}"
        );
    }
}
