//! Waiting until a freshly created guest is actually ready to be used.
//!
//! A VM that HCS reports as running is not a VM anyone can use: the SSH key is
//! installed at cloud-init's init stage, while `packages:` are applied later,
//! at its config stage. Probing port 22 therefore answers "ready" in the middle
//! of the work. The guest's own `cloud-init status --wait` is what actually
//! knows, so that is what this module asks -- over Windows' own OpenSSH client,
//! because every maintained Rust SSH client is async-only and this project has
//! no async runtime.

use std::fmt;

/// How many lines of a transcript or a console log are worth carrying into a
/// diagnostic: enough for the failing unit and its context, short enough to
/// read in a message box.
pub(crate) const DIAGNOSTIC_TAIL_LINES: usize = 40;

/// A guest that has finished booting, with or without complaints.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum GuestReady {
    Ready,
    /// cloud-init finished, but a module of it failed. The guest is usable; the
    /// detail says what is missing from it.
    Degraded {
        detail: String,
    },
}

/// Every way waiting for a guest can end badly, each naming its own cause.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReadinessFailure {
    NoSshClient,
    NoAddress,
    Unreachable { last_error: String },
    CloudInitFailed { detail: String },
    TimedOut,
    Cancelled,
}

impl fmt::Display for ReadinessFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSshClient => write!(
                formatter,
                "Windows has no OpenSSH client at \
                 %SystemRoot%\\System32\\OpenSSH\\ssh.exe; install the \
                 \"OpenSSH Client\" optional feature"
            ),
            Self::NoAddress => write!(
                formatter,
                "the VM started but was never given an address on the VMLord network"
            ),
            Self::Unreachable { last_error } => write!(
                formatter,
                "the guest did not accept an SSH connection: {last_error}"
            ),
            Self::CloudInitFailed { detail } => {
                write!(formatter, "cloud-init failed inside the guest: {detail}")
            }
            Self::TimedOut => write!(
                formatter,
                "cloud-init did not finish configuring the guest in time"
            ),
            Self::Cancelled => write!(formatter, "waiting for the guest was cancelled"),
        }
    }
}

/// Turns what `ssh.exe` left behind into what is known about the guest.
///
/// `cloud-init status --wait` answers `0` when it is done, `1` when it failed
/// and `2` when it finished degraded. `255` is OpenSSH's own code, meaning the
/// command never ran. No code at all means the child was killed -- which is
/// what this module does to one that outlives its deadline.
pub(crate) fn outcome(
    exit_code: Option<i32>,
    transcript_tail: &str,
) -> Result<GuestReady, ReadinessFailure> {
    let detail = || {
        let text = transcript_tail.trim();
        if text.is_empty() {
            "no output".to_owned()
        } else {
            text.to_owned()
        }
    };

    match exit_code {
        Some(0) => Ok(GuestReady::Ready),
        Some(2) => Ok(GuestReady::Degraded { detail: detail() }),
        Some(1) => Err(ReadinessFailure::CloudInitFailed { detail: detail() }),
        Some(255) => Err(ReadinessFailure::Unreachable {
            last_error: detail(),
        }),
        Some(other) => Err(ReadinessFailure::CloudInitFailed {
            detail: format!("the SSH command exited with code {other}: {}", detail()),
        }),
        None => Err(ReadinessFailure::TimedOut),
    }
}

/// The last `lines` lines of `text`, trimmed.
pub(crate) fn tail(text: &str, lines: usize) -> String {
    let kept: Vec<&str> = text.trim_end().lines().collect();
    let start = kept.len().saturating_sub(lines);
    kept[start..].join("\n").trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::{GuestReady, ReadinessFailure, outcome, tail};

    #[test]
    fn a_zero_exit_means_the_guest_is_ready() {
        assert!(matches!(
            outcome(Some(0), "status: done"),
            Ok(GuestReady::Ready)
        ));
    }

    #[test]
    fn a_two_exit_means_ready_but_degraded_and_keeps_the_detail() {
        // cloud-init answers 2 when a module of it failed while the system
        // still came up. One broken module must not turn a working VM into a
        // failed build.
        let Ok(GuestReady::Degraded { detail }) = outcome(
            Some(2),
            "status: degraded done\nerror: cc_package_update failed",
        ) else {
            panic!("exit code 2 must report a degraded but ready guest");
        };

        assert!(detail.contains("cc_package_update"), "{detail}");
    }

    #[test]
    fn a_one_exit_is_a_cloud_init_failure_and_keeps_the_detail() {
        let Err(ReadinessFailure::CloudInitFailed { detail }) =
            outcome(Some(1), "status: error\nerror: no such file")
        else {
            panic!("exit code 1 must report a cloud-init failure");
        };

        assert!(detail.contains("no such file"), "{detail}");
    }

    #[test]
    fn exit_code_255_is_ssh_itself_failing_not_cloud_init() {
        // 255 is OpenSSH's own code: the command was never run, so nothing is
        // known about cloud-init and everything about the connection.
        let Err(ReadinessFailure::Unreachable { last_error }) =
            outcome(Some(255), "Permission denied (publickey).")
        else {
            panic!("exit code 255 must report an unreachable guest");
        };

        assert!(last_error.contains("Permission denied"), "{last_error}");
    }

    #[test]
    fn no_exit_code_means_the_child_was_killed_at_its_deadline() {
        assert_eq!(outcome(None, ""), Err(ReadinessFailure::TimedOut));
    }

    #[test]
    fn an_unknown_exit_code_names_itself_rather_than_being_swallowed() {
        let Err(ReadinessFailure::CloudInitFailed { detail }) = outcome(Some(42), "") else {
            panic!("an unrecognised exit code must still fail the wait");
        };

        assert!(detail.contains("42"), "{detail}");
    }

    #[test]
    fn the_tail_keeps_the_last_lines_and_nothing_before_them() {
        let text = (1..=10)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");

        assert_eq!(tail(&text, 3), "line 8\nline 9\nline 10");
    }

    #[test]
    fn a_text_shorter_than_the_tail_is_kept_whole_and_trimmed() {
        assert_eq!(tail("only one\n", 40), "only one");
    }

    #[test]
    fn every_failure_names_its_cause_rather_than_a_code() {
        // The style `error.rs` sets: the message says what happened, not which
        // number happened.
        let messages = [
            ReadinessFailure::NoSshClient.to_string(),
            ReadinessFailure::NoAddress.to_string(),
            ReadinessFailure::Unreachable {
                last_error: "refused".into(),
            }
            .to_string(),
            ReadinessFailure::CloudInitFailed {
                detail: "boom".into(),
            }
            .to_string(),
            ReadinessFailure::TimedOut.to_string(),
            ReadinessFailure::Cancelled.to_string(),
        ];

        assert!(messages[0].contains("OpenSSH"), "{}", messages[0]);
        assert!(messages[1].contains("address"), "{}", messages[1]);
        assert!(messages[2].contains("refused"), "{}", messages[2]);
        assert!(messages[3].contains("boom"), "{}", messages[3]);
        assert!(messages[4].contains("did not finish"), "{}", messages[4]);
        assert!(messages[5].contains("cancelled"), "{}", messages[5]);
    }
}
