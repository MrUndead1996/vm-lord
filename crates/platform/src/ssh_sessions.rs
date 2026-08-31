//! The interactive SSH sessions VMLord is still waiting to hear the end of.
//!
//! A session runs in a window of its own and is nobody's child here: what
//! VMLord holds is not a process handle but an event to wait on and a name to
//! probe, the way it holds a COM1 reader. A session is over when its helper
//! signals that it has finished, or when the name only that helper holds is
//! gone -- which is what closing the window looks like from outside.
//!
//! Not keyed by VM: two shells into one guest is an ordinary thing to want, and
//! a second click while the first session is still opening is a second session
//! rather than a duplicate to refuse.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};

use uuid::Uuid;
use vmlord_core::{DiagnosticLevel, SshSessionOutcome, SshSessionReport};

use crate::{event::WindowsEvent, layout, ssh_session::read_report};

/// One session being waited for.
pub(crate) struct SshSessionHandle {
    pub(crate) id: Uuid,
    pub(crate) vm_name: String,
    pub(crate) report_path: PathBuf,
    /// Signalled by the helper however it leaves.
    pub(crate) finished: WindowsEvent,
    /// The name the helper created and holds, so a helper that was killed can
    /// be told from one that is still hosting a shell.
    pub(crate) alive_name: String,
}

/// A session that is over, and what it says.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SshSessionEnd {
    pub(crate) vm_name: String,
    pub(crate) report: SshSessionReport,
}

/// The sessions in flight.
///
/// Interior mutability because the two sides run on different threads: a launch
/// worker inserts, and the UI's refresh tick reaps.
#[derive(Default)]
pub(crate) struct SshSessions(Mutex<Vec<SshSessionHandle>>);

impl SshSessions {
    pub(crate) fn insert(&self, session: SshSessionHandle) {
        self.lock().push(session);
    }

    /// Drops a session no terminal would host, which is a session whose helper
    /// will never signal anything.
    pub(crate) fn forget(&self, id: Uuid) {
        self.lock().retain(|session| session.id != id);
    }

    /// Reports every session that is over, and forgets it.
    pub(crate) fn reap(&self) -> Vec<SshSessionEnd> {
        let mut sessions = self.lock();
        let mut ended = Vec::new();
        let mut index = 0;
        while index < sessions.len() {
            if !has_ended(&sessions[index]) {
                index += 1;
                continue;
            }
            let session = sessions.remove(index);
            ended.push(SshSessionEnd {
                vm_name: session.vm_name,
                report: take_report(&session.report_path),
            });
        }
        ended
    }

    /// Removes what earlier runs left in a VM's session directory.
    ///
    /// Called before a session is opened. The reports this VMLord is still
    /// waiting for are exactly the ones it holds, so anything else in there was
    /// written for a VMLord that is no longer running, and nobody will ever
    /// read it.
    pub(crate) fn sweep(&self, vm_directory: &Path) {
        let directory = layout::ssh_sessions_directory(vm_directory);
        let Ok(entries) = fs::read_dir(&directory) else {
            return;
        };
        let waited_for: Vec<PathBuf> = self
            .lock()
            .iter()
            .map(|session| session.report_path.clone())
            .collect();

        for entry in entries.flatten() {
            let path = entry.path();
            if waited_for.contains(&path) {
                continue;
            }
            if let Err(error) = fs::remove_file(&path) {
                tracing::debug!(
                    "an SSH session file left behind, {}, could not be removed: {error}",
                    path.display()
                );
            }
        }
    }

    /// Recovers a poisoned lock rather than propagating the panic: a launch
    /// that panicked must not take the repository down with it.
    fn lock(&self) -> MutexGuard<'_, Vec<SshSessionHandle>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Whether a session is over: its helper said so, or its helper is gone.
fn has_ended(session: &SshSessionHandle) -> bool {
    if session.finished.is_signaled().unwrap_or(false) {
        return true;
    }
    match WindowsEvent::exists(&session.alive_name) {
        Ok(exists) => !exists,
        // Unprovable rather than proven: a session that may still be running is
        // left alone, as it was before this could be asked.
        Err(error) => {
            tracing::warn!(
                "could not tell whether the SSH session of VM \"{}\" is still running: {error}",
                session.vm_name
            );
            false
        }
    }
}

/// Reads a session's report and takes the file with it.
///
/// No report means the helper never got to write one: its window was closed,
/// which is how people end shells rather than something to report as a failure.
fn take_report(path: &Path) -> SshSessionReport {
    let report = read_report(path);
    if path.exists()
        && let Err(error) = fs::remove_file(path)
    {
        tracing::debug!(
            "the SSH session report {} could not be removed: {error}",
            path.display()
        );
    }
    report.unwrap_or(SshSessionReport {
        outcome: SshSessionOutcome::WindowClosed,
        detail: String::new(),
    })
}

/// What a person is told about a session that ended.
///
/// A line per outcome rather than one line with a variable in it, because what
/// to do next differs: a changed host key is a file to look at, a refused
/// credential is a key or a password to check, and a transport failure is a
/// guest to look at. The tail of OpenSSH's log is appended where there is one:
/// it is the sentence the client itself wrote, and no paraphrase beats it.
pub(crate) fn session_diagnostic(end: &SshSessionEnd) -> (DiagnosticLevel, String) {
    let vm = &end.vm_name;
    let (level, message) = match end.report.outcome {
        // A window someone closed is how people end shells, and a shell that
        // exited zero is the same event seen from the other side.
        SshSessionOutcome::Ended { code: 0 } | SshSessionOutcome::WindowClosed => (
            DiagnosticLevel::Info,
            format!("the SSH session to VM \"{vm}\" ended"),
        ),
        SshSessionOutcome::Ended { code } => (
            DiagnosticLevel::Info,
            format!("the SSH session to VM \"{vm}\" ended with code {code}"),
        ),
        SshSessionOutcome::HostKeyMismatch => (
            DiagnosticLevel::Error,
            format!(
                "the SSH session to VM \"{vm}\" was refused: the guest's host key is not the \
                 one VMLord learned for this VM. Nothing is reset automatically; the keys are \
                 in the VM's own known_hosts file."
            ),
        ),
        SshSessionOutcome::AuthenticationFailed => (
            DiagnosticLevel::Error,
            format!(
                "the SSH session to VM \"{vm}\" was refused: the guest did not accept the \
                 credential."
            ),
        ),
        SshSessionOutcome::TransportFailure => (
            DiagnosticLevel::Warning,
            format!("the SSH session to VM \"{vm}\" never reached the guest."),
        ),
        SshSessionOutcome::Unrecognized { code } => (
            DiagnosticLevel::Warning,
            format!("the SSH client for VM \"{vm}\" exited with code {code}."),
        ),
        SshSessionOutcome::Terminated => (
            DiagnosticLevel::Warning,
            format!("the SSH client for VM \"{vm}\" was stopped without exiting."),
        ),
        SshSessionOutcome::NotStarted => (
            DiagnosticLevel::Error,
            format!("the SSH client for VM \"{vm}\" could not be started."),
        ),
    };

    let detail = end.report.detail.trim();
    if detail.is_empty() {
        (level, message)
    } else {
        (level, format!("{message} {detail}"))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use uuid::Uuid;
    use vmlord_core::{SshSessionOutcome, SshSessionReport};

    use super::{SshSessionEnd, SshSessionHandle, SshSessions};
    use crate::{event::WindowsEvent, ssh_session::write_report};

    /// A directory of its own per test, removed with the test.
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(label: &str) -> Self {
            static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "vmlord-ssh-sessions-test-{label}-{}-{}",
                std::process::id(),
                NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("test root should be created");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct Fixture {
        root: TempRoot,
        sessions: SshSessions,
        id: Uuid,
        report_path: PathBuf,
        finished_name: String,
        /// Held the way a running helper holds it: while this handle exists,
        /// the session counts as running.
        alive: Option<WindowsEvent>,
    }

    fn fixture(label: &str) -> Fixture {
        let root = TempRoot::new(label);
        let id = Uuid::new_v4();
        let vm_directory = root.path().join("dev-linux");
        let report_path = crate::layout::ssh_session_report_path(&vm_directory, id);
        let finished_name = format!(r"Local\VMLord.Test.Ssh.{}.finished", id.as_simple());
        let alive_name = format!(r"Local\VMLord.Test.Ssh.{}.alive", id.as_simple());
        let finished = WindowsEvent::create_named(&finished_name, true, false).unwrap();
        let alive = WindowsEvent::create_named(&alive_name, true, false).unwrap();

        let sessions = SshSessions::default();
        sessions.insert(SshSessionHandle {
            id,
            vm_name: "dev-linux".to_owned(),
            report_path: report_path.clone(),
            finished,
            alive_name,
        });

        Fixture {
            root,
            sessions,
            id,
            report_path,
            finished_name,
            alive: Some(alive),
        }
    }

    fn ended(outcome: SshSessionOutcome, detail: &str) -> SshSessionEnd {
        SshSessionEnd {
            vm_name: "dev-linux".to_owned(),
            report: SshSessionReport {
                outcome,
                detail: detail.to_owned(),
            },
        }
    }

    #[test]
    fn a_session_still_running_is_not_reaped() {
        let fixture = fixture("running");

        assert!(fixture.sessions.reap().is_empty());
    }

    #[test]
    fn a_finished_session_is_reported_from_its_report_and_the_file_is_taken() {
        let fixture = fixture("finished");
        write_report(
            &fixture.report_path,
            &SshSessionReport {
                outcome: SshSessionOutcome::HostKeyMismatch,
                detail: "Host key verification failed.".to_owned(),
            },
        )
        .unwrap();
        WindowsEvent::open(&fixture.finished_name)
            .unwrap()
            .signal()
            .unwrap();

        let ended = fixture.sessions.reap();

        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].vm_name, "dev-linux");
        assert_eq!(ended[0].report.outcome, SshSessionOutcome::HostKeyMismatch);
        assert!(
            !fixture.report_path.exists(),
            "a report read once is a report gone"
        );
        assert!(
            fixture.sessions.reap().is_empty(),
            "a session is reaped once"
        );
    }

    /// The helper's process ending is exactly this: the last handle to the name
    /// goes, and the name goes with it.
    #[test]
    fn a_helper_that_is_gone_without_a_report_closed_its_window() {
        let mut fixture = fixture("closed");
        fixture.alive.take();

        let ended = fixture.sessions.reap();

        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].report.outcome, SshSessionOutcome::WindowClosed);
    }

    #[test]
    fn a_forgotten_session_is_not_waited_for() {
        let fixture = fixture("forgotten");

        fixture.sessions.forget(fixture.id);

        assert!(fixture.sessions.reap().is_empty());
    }

    #[test]
    fn a_sweep_removes_what_no_session_is_waiting_for() {
        let fixture = fixture("swept");
        let vm_directory = fixture.root.path().join("dev-linux");
        let stale = crate::layout::ssh_session_report_path(&vm_directory, Uuid::new_v4());
        fs::create_dir_all(stale.parent().unwrap()).unwrap();
        fs::write(&stale, "{}").unwrap();
        write_report(
            &fixture.report_path,
            &SshSessionReport {
                outcome: SshSessionOutcome::Ended { code: 0 },
                detail: String::new(),
            },
        )
        .unwrap();

        fixture.sessions.sweep(&vm_directory);

        assert!(
            !stale.exists(),
            "a report from a VMLord that is gone is nobody's"
        );
        assert!(
            fixture.report_path.exists(),
            "a session this VMLord is waiting for keeps its report"
        );
    }

    #[test]
    fn a_vm_with_no_session_directory_is_nothing_to_sweep() {
        let fixture = fixture("no-directory");

        fixture
            .sessions
            .sweep(fixture.root.path().join("absent").as_path());
    }

    #[test]
    fn a_changed_host_key_is_an_error_that_says_where_the_keys_are() {
        let (level, message) = super::session_diagnostic(&ended(
            SshSessionOutcome::HostKeyMismatch,
            "Host key verification failed.",
        ));

        assert_eq!(level, vmlord_core::DiagnosticLevel::Error);
        assert!(message.contains("dev-linux"), "{message}");
        assert!(message.contains("known_hosts"), "{message}");
        assert!(
            message.contains("Host key verification failed."),
            "{message}"
        );
    }

    #[test]
    fn a_refused_credential_is_an_error_and_a_transport_failure_is_a_warning() {
        let (level, message) =
            super::session_diagnostic(&ended(SshSessionOutcome::AuthenticationFailed, ""));
        assert_eq!(level, vmlord_core::DiagnosticLevel::Error);
        assert!(message.contains("credential"), "{message}");

        let (level, _) = super::session_diagnostic(&ended(SshSessionOutcome::TransportFailure, ""));
        assert_eq!(level, vmlord_core::DiagnosticLevel::Warning);
    }

    #[test]
    fn a_shell_that_ended_and_a_window_that_was_closed_are_the_same_quiet_line() {
        let (level, ended_message) =
            super::session_diagnostic(&ended(SshSessionOutcome::Ended { code: 0 }, ""));
        assert_eq!(level, vmlord_core::DiagnosticLevel::Info);

        let (level, closed_message) =
            super::session_diagnostic(&ended(SshSessionOutcome::WindowClosed, ""));
        assert_eq!(level, vmlord_core::DiagnosticLevel::Info);
        assert_eq!(ended_message, closed_message);
    }

    #[test]
    fn a_nonzero_shell_status_keeps_its_code() {
        let (level, message) =
            super::session_diagnostic(&ended(SshSessionOutcome::Ended { code: 130 }, ""));

        assert_eq!(level, vmlord_core::DiagnosticLevel::Info);
        assert!(message.contains("130"), "{message}");
    }

    #[test]
    fn a_message_does_not_end_in_the_gap_an_empty_log_leaves() {
        let (_, message) =
            super::session_diagnostic(&ended(SshSessionOutcome::AuthenticationFailed, ""));

        assert_eq!(message.trim_end(), message, "{message:?}");
    }
}
