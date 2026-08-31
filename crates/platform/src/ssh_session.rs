//! The process a terminal hosts instead of `ssh.exe`, and the report it leaves.
//!
//! A session in a window of its own is a session VMLord cannot see the end of:
//! the terminal owns what it hosts, and OpenSSH answers nearly every failure of
//! its own with the same exit code. So the terminal hosts this instead. It runs
//! the client on its own console -- the session is interactive exactly as it
//! was, and every prompt still appears in the window -- waits for it, and turns
//! the exit code plus the log OpenSSH was told to write into one small report
//! for VMLord to read.
//!
//! The helper is the client's parent and nothing more. It does not read the
//! session, does not touch the guest, and holds nothing of the user's: what it
//! knows is a path, an exit code, and the last lines of a log.

use std::{
    ffi::{OsStr, OsString},
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};

use vmlord_core::{RepositoryError, SshSessionOutcome, SshSessionReport, classify_session};

use crate::event::WindowsEvent;

/// How much of the session log travels in the report.
///
/// The same cap as the readiness transcript's, for the same reason: what
/// reaches the diagnostics panel has to be readable in it.
const SESSION_TAIL_LINES: usize = 40;

/// Everything the helper is told on its command line.
///
/// Nothing secret is here, and nothing secret may be added: a command line is
/// readable by every process on the machine that can enumerate this one.
#[derive(Debug, PartialEq, Eq)]
pub struct SshHelperOptions {
    pub report_path: PathBuf,
    pub log_path: PathBuf,
    pub finished_event_name: String,
    pub alive_event_name: String,
    pub vm_name: String,
    pub client: PathBuf,
    pub client_args: Vec<OsString>,
}

/// The flags [`parse_ssh_helper_args`] accepts, in the order the launcher
/// writes them. Everything after `--` is the client and its own arguments.
const FLAGS: [&str; 5] = [
    "--report",
    "--log",
    "--finished-event",
    "--alive-event",
    "--vm-name",
];

/// Parses the helper's command line.
///
/// Exact flag/value pairs, then `--`, then the client: an unknown or repeated
/// flag is a launcher bug, and guessing at one would let a typo turn into a
/// session nobody hears the end of. Nothing after the separator is examined --
/// those arguments are OpenSSH's, and one of them may well be spelled like a
/// flag of this program's.
pub fn parse_ssh_helper_args(
    args: impl IntoIterator<Item = OsString>,
) -> Result<SshHelperOptions, RepositoryError> {
    let mut values: [Option<OsString>; FLAGS.len()] = Default::default();
    let mut args = args.into_iter();
    let mut client_line: Vec<OsString> = Vec::new();

    while let Some(flag) = args.next() {
        if flag == OsStr::new("--") {
            client_line.extend(args.by_ref());
            break;
        }
        let index = FLAGS
            .iter()
            .position(|known| OsStr::new(known) == flag)
            .ok_or_else(|| argument_error(format!("unknown argument {}", flag.display())))?;
        if values[index].is_some() {
            return Err(argument_error(format!("{} was given twice", FLAGS[index])));
        }
        let value = args
            .next()
            .ok_or_else(|| argument_error(format!("{} has no value", FLAGS[index])))?;
        values[index] = Some(value);
    }

    let mut values = values
        .into_iter()
        .zip(FLAGS)
        .map(|(value, flag)| value.ok_or_else(|| argument_error(format!("{flag} is required"))));
    let mut next = || values.next().expect("one value per flag");

    let report_path = PathBuf::from(next()?);
    let log_path = PathBuf::from(next()?);
    let mut name = || -> Result<String, RepositoryError> { Ok(next()?.to_string_lossy().into()) };
    let finished_event_name = name()?;
    let alive_event_name = name()?;
    let vm_name = name()?;

    let mut client_line = client_line.into_iter();
    let client = client_line
        .next()
        .ok_or_else(|| argument_error("the SSH client must follow \"--\"".to_owned()))?;

    Ok(SshHelperOptions {
        report_path,
        log_path,
        finished_event_name,
        alive_event_name,
        vm_name,
        client: PathBuf::from(client),
        client_args: client_line.collect(),
    })
}

fn argument_error(detail: String) -> RepositoryError {
    RepositoryError::new(format!("VMLord SSH helper arguments are invalid: {detail}"))
}

/// Runs the session and reports how it ended.
///
/// The client inherits this process's console: the window a person types in is
/// this window, and the helper is between them only in the sense that it is the
/// parent waiting for the child.
pub fn run_ssh_helper(options: SshHelperOptions) -> Result<(), RepositoryError> {
    let finished = WindowsEvent::open(&options.finished_event_name)?;
    // Created here rather than by VMLord, and held for as long as this process
    // lives: a named object exists while a handle to it does, so VMLord probing
    // this name is asking whether this helper is still there. That is the one
    // question a report cannot answer -- a window someone closes takes the
    // helper down with no chance to write anything.
    let _alive = WindowsEvent::create_named(&options.alive_event_name, true, false)?;
    // However this leaves -- returning, failing, or a panic unwinding through
    // it -- VMLord learns that the session is over.
    let _finish = SignalOnDrop(&finished);

    // The client is told to write its log with `-E`, and `-E` does not create
    // a directory: a client that cannot open its log writes nothing anybody
    // could classify afterwards. The helper owns that file end to end, so it
    // owns the directory too, and VMLord's launcher touches no disk at all.
    if let Some(directory) = options.log_path.parent()
        && let Err(error) = fs::create_dir_all(directory)
    {
        tracing::warn!(
            "the SSH session directory {} of VM \"{}\" could not be created: {error}",
            directory.display(),
            options.vm_name
        );
    }

    tracing::debug!(
        "hosting an SSH session to VM \"{}\" with {}",
        options.vm_name,
        options.client.display()
    );
    match Command::new(&options.client)
        .args(&options.client_args)
        .status()
    {
        Ok(status) => {
            finish(
                status.code(),
                &options.log_path,
                &options.report_path,
                &options.vm_name,
            );
            Ok(())
        }
        Err(error) => {
            let detail = error.to_string();
            tracing::error!(
                "the SSH client for VM \"{}\" could not be started: {detail}",
                options.vm_name
            );
            report(
                &options.report_path,
                &options.vm_name,
                &SshSessionReport {
                    outcome: SshSessionOutcome::NotStarted,
                    detail,
                },
            );
            Err(RepositoryError::new(format!(
                "{} could not be started",
                options.client.display()
            )))
        }
    }
}

/// Classifies a client that has exited, reports it, and takes the log with it.
///
/// The log is this process's to delete: it was written to answer one question,
/// and a directory that grew a file per shell would be a slow leak nobody asked
/// for.
fn finish(exit_code: Option<i32>, log_path: &Path, report_path: &Path, vm_name: &str) {
    let detail = log_tail(log_path);
    let outcome = classify_session(exit_code, &detail);
    tracing::info!("the SSH session to VM \"{vm_name}\" ended: {outcome:?}");
    report(report_path, vm_name, &SshSessionReport { outcome, detail });

    if log_path.exists()
        && let Err(error) = fs::remove_file(log_path)
    {
        tracing::warn!(
            "the SSH session log {} of VM \"{vm_name}\" could not be removed: {error}",
            log_path.display()
        );
    }
}

fn report(path: &Path, vm_name: &str, report: &SshSessionReport) {
    if let Err(error) = write_report(path, report) {
        tracing::error!("the SSH session of VM \"{vm_name}\" could not be reported: {error}");
    }
}

/// The last lines of what OpenSSH wrote, or nothing when it wrote nothing.
///
/// A log that cannot be read is a log that says nothing: this runs after a
/// session has ended, and refusing to report an outcome because its detail is
/// unreadable would lose the outcome as well.
fn log_tail(path: &Path) -> String {
    let mut text = String::new();
    let Ok(mut file) = File::open(path) else {
        return String::new();
    };
    if file.read_to_string(&mut text).is_err() {
        return String::new();
    }
    let kept: Vec<&str> = text.trim_end().lines().collect();
    let start = kept.len().saturating_sub(SESSION_TAIL_LINES);
    kept[start..].join("\n").trim().to_owned()
}

/// Writes a session's report, creating its directory if a deletion took it.
pub(crate) fn write_report(path: &Path, report: &SshSessionReport) -> Result<(), RepositoryError> {
    let document = serde_json::to_vec(report).map_err(|error| {
        RepositoryError::new(format!("the SSH session report cannot be written: {error}"))
    })?;
    if let Some(directory) = path.parent() {
        fs::create_dir_all(directory).map_err(|error| {
            RepositoryError::new(format!(
                "the SSH session directory {} cannot be created: {error}",
                directory.display()
            ))
        })?;
    }
    fs::write(path, document).map_err(|error| {
        RepositoryError::new(format!(
            "the SSH session report {} cannot be written: {error}",
            path.display()
        ))
    })
}

/// Reads a report, treating one that is absent or torn as none.
///
/// A helper killed mid-write leaves half a document, and half a document is
/// exactly as informative as no document: both mean the window is gone and
/// nothing was said.
pub(crate) fn read_report(path: &Path) -> Option<SshSessionReport> {
    let document = fs::read(path).ok()?;
    match serde_json::from_slice(&document) {
        Ok(report) => Some(report),
        Err(error) => {
            tracing::warn!(
                "the SSH session report {} could not be read: {error}",
                path.display()
            );
            None
        }
    }
}

/// Signals an event however the scope it guards is left.
struct SignalOnDrop<'a>(&'a WindowsEvent);

impl Drop for SignalOnDrop<'_> {
    fn drop(&mut self) {
        let _ = self.0.signal();
    }
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsString,
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use vmlord_core::{SshSessionOutcome, SshSessionReport};

    use super::{finish, parse_ssh_helper_args, read_report, write_report};

    /// A directory of its own per test, removed with the test. The same shape
    /// as the deletion tests' root: `vmlord-platform` keeps no temporary-file
    /// dependency.
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(label: &str) -> Self {
            static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "vmlord-ssh-session-test-{label}-{}-{}",
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

    fn arguments() -> Vec<OsString> {
        [
            "--report",
            r"C:\VMs\dev\ssh-sessions\a.json",
            "--log",
            r"C:\VMs\dev\ssh-sessions\a.log",
            "--finished-event",
            r"Local\VMLord.Ssh.a.finished",
            "--alive-event",
            r"Local\VMLord.Ssh.a.alive",
            "--vm-name",
            "dev-linux",
            "--",
            r"C:\Windows\System32\OpenSSH\ssh.exe",
            "-p",
            "22",
            "-l",
            "machi",
            "172.22.42.7",
        ]
        .into_iter()
        .map(OsString::from)
        .collect()
    }

    #[test]
    fn the_client_and_its_arguments_are_everything_after_the_separator() {
        let options = parse_ssh_helper_args(arguments()).unwrap();

        assert_eq!(options.vm_name, "dev-linux");
        assert_eq!(
            options.client,
            Path::new(r"C:\Windows\System32\OpenSSH\ssh.exe")
        );
        assert_eq!(
            options.client_args,
            ["-p", "22", "-l", "machi", "172.22.42.7"].map(OsString::from)
        );
        assert_eq!(
            options.report_path,
            Path::new(r"C:\VMs\dev\ssh-sessions\a.json")
        );
        assert_eq!(options.alive_event_name, r"Local\VMLord.Ssh.a.alive");
    }

    #[test]
    fn a_missing_client_is_refused_rather_than_run_as_nothing() {
        let mut arguments = arguments();
        arguments.truncate(arguments.len() - 6);

        let message = parse_ssh_helper_args(arguments).unwrap_err().to_string();

        assert!(message.contains("client"), "{message}");
    }

    #[test]
    fn an_unknown_or_repeated_flag_is_a_launcher_bug_not_a_guess() {
        let mut unknown = arguments();
        unknown.insert(0, OsString::from("--colour"));
        assert!(parse_ssh_helper_args(unknown).is_err());

        let mut repeated = arguments();
        repeated.insert(0, OsString::from("dev"));
        repeated.insert(0, OsString::from("--vm-name"));
        assert!(parse_ssh_helper_args(repeated).is_err());
    }

    /// A flag after the separator is the client's, whatever it is named.
    #[test]
    fn the_clients_own_flags_are_not_read_as_the_helpers() {
        let mut arguments = arguments();
        arguments.push(OsString::from("--vm-name"));

        let options = parse_ssh_helper_args(arguments).unwrap();

        assert_eq!(options.vm_name, "dev-linux");
        assert_eq!(
            options.client_args.last().unwrap(),
            OsString::from("--vm-name").as_os_str()
        );
    }

    #[test]
    fn a_finished_client_leaves_a_report_and_no_log() {
        let root = TempRoot::new("reported");
        let log = root.path().join("session.log");
        let report_path = root.path().join("session.json");
        fs::write(&log, "machi@172.22.42.7: Permission denied (publickey).").unwrap();

        finish(Some(255), &log, &report_path, "dev-linux");

        let report = read_report(&report_path).expect("the report is readable");
        assert_eq!(report.outcome, SshSessionOutcome::AuthenticationFailed);
        assert!(
            report.detail.contains("Permission denied"),
            "{}",
            report.detail
        );
        assert!(!log.exists(), "the helper owns the log and takes it with it");
    }

    #[test]
    fn a_session_that_said_nothing_still_reports_how_it_ended() {
        let root = TempRoot::new("silent");
        let log = root.path().join("session.log");
        let report_path = root.path().join("session.json");

        finish(Some(0), &log, &report_path, "dev-linux");

        let report = read_report(&report_path).expect("the report is readable");
        assert_eq!(report.outcome, SshSessionOutcome::Ended { code: 0 });
        assert!(report.detail.is_empty(), "{}", report.detail);
    }

    #[test]
    fn a_report_that_is_not_there_is_not_an_error() {
        let root = TempRoot::new("absent");

        assert_eq!(read_report(&root.path().join("absent.json")), None);
    }

    #[test]
    fn a_report_that_cannot_be_parsed_is_reported_as_absent() {
        let root = TempRoot::new("torn");
        let path = root.path().join("torn.json");
        fs::write(&path, "{ half a docum").unwrap();

        assert_eq!(read_report(&path), None);
    }

    #[test]
    fn a_report_is_written_into_a_directory_that_is_not_there_yet() {
        let root = TempRoot::new("nested");
        let path = root.path().join("ssh-sessions").join("session.json");

        write_report(
            &path,
            &SshSessionReport {
                outcome: SshSessionOutcome::NotStarted,
                detail: "the system cannot find the file specified".to_owned(),
            },
        )
        .unwrap();

        let report = read_report(&path).unwrap();
        assert_eq!(report.outcome, SshSessionOutcome::NotStarted);
    }
}
