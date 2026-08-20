//! Launching the COM1 reader in a terminal window and owning what it becomes.
//!
//! Windows has no one terminal every machine has, so three hosts are tried in
//! turn. What VMLord keeps afterwards is not a process handle -- the terminal
//! owns the reader, not this process -- but a [`Com1Session`]: named events
//! through which a reader can be told to stop, can say that it has, and can be
//! found to be gone without having said anything at all.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, atomic::AtomicUsize},
    time::{Duration, Instant},
};

use uuid::Uuid;
use vmlord_core::RepositoryError;

use crate::{
    Com1LogMode, event::WindowsEvent, hcs_config::com1_pipe_path, layout::com1_log_path,
    metadata::VmComputeSystemMapping,
};

/// The name of the helper executable, which ships beside `vmlord.exe`.
const HELPER_FILE_NAME: &str = "vmlord-com1.exe";

/// How long a launch waits for the reader to report that it is able to capture.
///
/// Generous: a cold terminal host and a first process start can take seconds on
/// a busy machine, and the cost of waiting too long is a slower start, while the
/// cost of waiting too little is a VM that runs without a diagnostic console.
const READINESS_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the wait for readiness looks at the reader's events.
const READINESS_POLL: Duration = Duration::from_millis(25);

/// The `CREATE_NEW_CONSOLE` process creation flag.
///
/// Spelled out rather than imported: this is the one Win32 constant this module
/// needs, and `std`'s own `creation_flags` takes it as a plain `u32`.
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;

/// One way of putting the helper on screen, as data, so that the order the
/// hosts are tried in can be read and tested without starting anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TerminalCommand {
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<OsString>,
    pub(crate) create_new_console: bool,
    /// Whether the arguments are already quoted for the program's own parser.
    ///
    /// `cmd.exe` needs a command line `std`'s quoting would mangle, so that one
    /// is handed over untouched.
    pub(crate) raw_args: bool,
}

/// A reader VMLord launched and can still speak to.
#[derive(Debug)]
pub struct Com1Session {
    pub vm_id: Uuid,
    pub vm_name: String,
    cancel: WindowsEvent,
    failed: WindowsEvent,
    finished: WindowsEvent,
    /// How VMLord tells a reader that is still there from one that is not.
    liveness: Liveness,
    /// Test-only counter of cancellations, so that a test can see the signal a
    /// dropped session sends without a helper process to observe it.
    cancellations: Option<Arc<AtomicUsize>>,
}

impl Com1Session {
    /// Tells the reader to stop, best effort: a reader that has already exited
    /// is not an error, and neither is one whose terminal a person closed.
    fn cancel(&self) {
        if let Err(error) = self.cancel.signal() {
            log::warn!(
                "could not cancel the COM1 reader for VM \"{}\": {error}",
                self.vm_name
            );
        }
        if let Some(cancellations) = &self.cancellations {
            cancellations.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Whether the reader has reported that it is over.
    fn has_finished(&self) -> bool {
        self.finished.is_signaled().unwrap_or(false)
    }

    /// Whether the reader stopped for a reason other than the pipe closing.
    fn has_failed(&self) -> bool {
        self.failed.is_signaled().unwrap_or(false)
    }

    /// Whether the reader is over, however it ended.
    ///
    /// `finished` covers every way the helper can leave on its own two feet,
    /// and nothing else: a person closing the terminal window kills the process
    /// where it stands, and a killed process signals nothing. What it cannot
    /// take with it is the named event it created and held, so an event that is
    /// gone is a reader that is gone.
    fn has_exited(&self) -> bool {
        if self.has_finished() {
            return true;
        }
        match &self.liveness {
            Liveness::Probe(name) => match WindowsEvent::exists(name) {
                Ok(exists) => !exists,
                // Unprovable rather than proven: a reader that may still be
                // reading is left alone, as it was before this could be asked.
                Err(error) => {
                    log::warn!(
                        "could not tell whether the COM1 reader for VM \"{}\" is still \
                         running: {error}",
                        self.vm_name
                    );
                    false
                }
            },
            #[cfg(test)]
            Liveness::Held { .. } => false,
        }
    }

    #[cfg(test)]
    pub(crate) fn finish_for_test(&self) {
        self.finished.signal().unwrap();
    }

    #[cfg(test)]
    pub(crate) fn fail_for_test(&self) {
        self.failed.signal().unwrap();
        self.finished.signal().unwrap();
    }
}

impl Drop for Com1Session {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// How VMLord asks whether the helper process is still running.
#[derive(Debug)]
enum Liveness {
    /// The name of the event the helper created and holds. It exists for
    /// exactly as long as that process does.
    Probe(String),
    /// A test's stand-in for a helper that is running: an event this session
    /// holds itself, so the probe can never find it gone. Held for its
    /// existence and never read.
    #[cfg(test)]
    Held { _event: WindowsEvent },
}

/// A reader that stopped for a reason worth telling a person about.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Com1Failure {
    pub(crate) vm_id: Uuid,
    pub(crate) vm_name: String,
}

/// Every COM1 reader VMLord currently owns, one per VM.
#[derive(Default)]
pub(crate) struct Com1Sessions(BTreeMap<Uuid, Com1Session>);

impl Com1Sessions {
    /// Takes ownership of `session`, cancelling whatever this VM had before.
    ///
    /// A VM that is started twice must not end up with two readers on one pipe.
    pub(crate) fn insert(&mut self, session: Com1Session) {
        self.0.insert(session.vm_id, session);
    }

    /// Cancels and forgets the reader of one VM, if it has one.
    pub(crate) fn cancel(&mut self, vm_id: Uuid) {
        self.0.remove(&vm_id);
    }

    /// Forgets every reader that has finished, reporting those that failed.
    pub(crate) fn reap(&mut self) -> Vec<Com1Failure> {
        let finished: Vec<Uuid> = self
            .0
            .values()
            .filter(|session| session.has_exited())
            .map(|session| session.vm_id)
            .collect();

        let mut failures = Vec::new();
        for vm_id in finished {
            let Some(session) = self.0.remove(&vm_id) else {
                continue;
            };
            if session.has_failed() {
                failures.push(Com1Failure {
                    vm_id: session.vm_id,
                    vm_name: session.vm_name.clone(),
                });
            } else {
                log::debug!(
                    "the COM1 reader for VM \"{}\" finished with its pipe",
                    session.vm_name
                );
            }
        }
        failures
    }

    /// Cancels every reader, for a VMLord that is going away.
    pub(crate) fn cancel_all(&mut self) {
        self.0.clear();
    }

    pub(crate) fn contains(&self, vm_id: Uuid) -> bool {
        self.0.contains_key(&vm_id)
    }

    /// Stands in for the reader of `vm_id` exiting, as it does when a person
    /// closes its window.
    #[cfg(test)]
    pub(crate) fn finish_for_test(&self, vm_id: Uuid) {
        self.0
            .get(&vm_id)
            .expect("the VM should have a session")
            .finish_for_test();
    }
}

/// What a launch does once a terminal has been asked to start.
///
/// Test-only: production always waits for the helper process itself, and a
/// launcher with no helper to wait for exists only in tests.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum Readiness {
    /// Wait for the helper process to say it is capturing, as production does.
    Await,
    /// Stand in for a helper that reports readiness.
    Immediate,
    /// Stand in for a helper that never does.
    Never,
}

/// Starts one terminal host, which is what a test replaces to keep a launch
/// off screen.
type TerminalSpawner = Arc<dyn Fn(&TerminalCommand) -> io::Result<()> + Send + Sync>;

/// Opens the diagnostic console of a VM.
#[derive(Clone)]
pub struct Com1Launcher {
    /// `None` in production: the helper is found beside this executable at
    /// launch time, which is also where the failure to find it belongs.
    helper: Option<PathBuf>,
    spawn: TerminalSpawner,
    #[cfg(test)]
    readiness: Readiness,
    readiness_timeout: Duration,
    cancellations: Option<Arc<AtomicUsize>>,
}

impl Com1Launcher {
    /// The launcher VMLord runs with.
    pub fn production() -> Self {
        Self {
            helper: None,
            spawn: Arc::new(spawn_terminal),
            #[cfg(test)]
            readiness: Readiness::Await,
            readiness_timeout: READINESS_TIMEOUT,
            cancellations: None,
        }
    }

    /// Opens the COM1 console of `mapping` and returns the session that owns it.
    pub fn launch(
        &self,
        mapping: &VmComputeSystemMapping,
        vm_directory: &Path,
        mode: Com1LogMode,
    ) -> Result<Com1Session, RepositoryError> {
        let helper = match &self.helper {
            Some(helper) => helper.clone(),
            None => helper_path(&mapping.vm_name)?,
        };
        let session_id = Uuid::new_v4();
        let events = EventNames::of(session_id);
        // Created here, before anything is spawned: a reader that could not be
        // cancelled is worse than one that never started.
        let cancel = WindowsEvent::create_named(&events.cancel, true, false)?;
        let ready = WindowsEvent::create_named(&events.ready, true, false)?;
        let failed = WindowsEvent::create_named(&events.failed, true, false)?;
        let finished = WindowsEvent::create_named(&events.finished, true, false)?;
        // The alive event is not created here: the helper creates that one, and
        // a handle held by VMLord would keep it alive past the process it
        // stands for. In tests nothing starts a helper, so the launcher stands
        // in for one and holds the event it would have created.
        #[cfg(not(test))]
        let liveness = Liveness::Probe(events.alive.clone());
        #[cfg(test)]
        let liveness = Liveness::Held {
            _event: WindowsEvent::create_named(&events.alive, true, false)?,
        };
        let session = Com1Session {
            vm_id: mapping.vm_id,
            vm_name: mapping.vm_name.clone(),
            cancel,
            failed,
            finished,
            liveness,
            cancellations: self.cancellations.clone(),
        };

        let arguments = helper_arguments(mapping, vm_directory, mode, &events);
        self.spawn_somewhere(&helper, &mapping.vm_name, &arguments)?;
        #[cfg(test)]
        if self.readiness == Readiness::Immediate {
            ready.signal()?;
        }
        // From here a failure drops `session`, whose `Drop` tells the reader
        // that the start it was opened for is not happening.
        self.await_readiness(&ready, &session)?;

        log::info!(
            "COM1 diagnostics for VM \"{}\" are being captured to {}",
            mapping.vm_name,
            com1_log_path(vm_directory).display()
        );
        Ok(session)
    }

    /// A session over unnamed events, for tests that need one to hand around
    /// without a helper process to own it.
    #[cfg(test)]
    pub(crate) fn session_for_test(mapping: &VmComputeSystemMapping) -> Com1Session {
        Com1Session {
            vm_id: mapping.vm_id,
            vm_name: mapping.vm_name.clone(),
            cancel: WindowsEvent::new(true, false).expect("an unnamed event can always be created"),
            failed: WindowsEvent::new(true, false).expect("an unnamed event can always be created"),
            finished: WindowsEvent::new(true, false)
                .expect("an unnamed event can always be created"),
            liveness: Liveness::Held {
                _event: WindowsEvent::new(true, false)
                    .expect("an unnamed event can always be created"),
            },
            cancellations: None,
        }
    }

    /// Tries every terminal host in turn, and reports them all if none works.
    fn spawn_somewhere(
        &self,
        helper: &Path,
        vm_name: &str,
        arguments: &[OsString],
    ) -> Result<(), RepositoryError> {
        let mut refusals = Vec::new();
        for command in terminal_commands(helper, vm_name, arguments) {
            match (self.spawn)(&command) {
                Ok(()) => return Ok(()),
                Err(error) => {
                    log::warn!(
                        "could not open the COM1 console of VM \"{vm_name}\" with {}: {error}",
                        command.program.display()
                    );
                    refusals.push(format!("{}: {error}", command.program.display()));
                }
            }
        }

        let error = RepositoryError::new(format!(
            "cannot open a terminal for the COM1 console of VM \"{vm_name}\" ({})",
            refusals.join("; ")
        ));
        log::error!("{error}");
        Err(error)
    }

    /// Waits until the reader reports that it is capturing.
    ///
    /// The wait is what makes a start able to fail for the right reason: a VM
    /// whose console never came up has no diagnostics to offer if it then fails
    /// to boot.
    fn await_readiness(
        &self,
        ready: &WindowsEvent,
        session: &Com1Session,
    ) -> Result<(), RepositoryError> {
        let deadline = Instant::now() + self.readiness_timeout;
        loop {
            if ready.is_signaled()? {
                return Ok(());
            }
            if session.has_finished() {
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(READINESS_POLL);
        }

        let error = RepositoryError::new(format!(
            "the COM1 console of VM \"{}\" did not start capturing",
            session.vm_name
        ));
        log::error!("{error}");
        Err(error)
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        helper: PathBuf,
        spawn: impl Fn(&TerminalCommand) -> io::Result<()> + Send + Sync + 'static,
    ) -> Self {
        Self {
            helper: Some(helper),
            spawn: Arc::new(spawn),
            readiness: Readiness::Immediate,
            readiness_timeout: READINESS_TIMEOUT,
            cancellations: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn never_ready(&mut self) {
        self.readiness = Readiness::Never;
        self.readiness_timeout = Duration::from_millis(100);
    }

    #[cfg(test)]
    pub(crate) fn observe_cancellations(&mut self, cancellations: Arc<AtomicUsize>) {
        self.cancellations = Some(cancellations);
    }
}

/// The four events one reader and its launcher share.
struct EventNames {
    cancel: String,
    ready: String,
    failed: String,
    finished: String,
    alive: String,
}

impl EventNames {
    /// Derives the names from a fresh session id: unguessable, and local to
    /// this session, so that nothing else on the machine can end a capture.
    fn of(session_id: Uuid) -> Self {
        let prefix = format!(r"Local\VMLord.Com1.{}", session_id.as_simple());
        Self {
            cancel: format!("{prefix}.cancel"),
            ready: format!("{prefix}.ready"),
            failed: format!("{prefix}.failed"),
            finished: format!("{prefix}.finished"),
            alive: format!("{prefix}.alive"),
        }
    }
}

/// Builds the helper's command line.
///
/// Nothing secret goes in: a command line is readable by every process on the
/// machine that can enumerate this one.
fn helper_arguments(
    mapping: &VmComputeSystemMapping,
    vm_directory: &Path,
    mode: Com1LogMode,
    events: &EventNames,
) -> Vec<OsString> {
    let mode = match mode {
        Com1LogMode::Truncate => "truncate",
        Com1LogMode::Append => "append",
    };
    vec![
        OsString::from("--pipe"),
        OsString::from(com1_pipe_path(mapping.vm_id)),
        OsString::from("--log"),
        com1_log_path(vm_directory).into_os_string(),
        OsString::from("--mode"),
        OsString::from(mode),
        OsString::from("--parent-pid"),
        OsString::from(std::process::id().to_string()),
        OsString::from("--cancel-event"),
        OsString::from(&events.cancel),
        OsString::from("--ready-event"),
        OsString::from(&events.ready),
        OsString::from("--failed-event"),
        OsString::from(&events.failed),
        OsString::from("--finished-event"),
        OsString::from(&events.finished),
        OsString::from("--alive-event"),
        OsString::from(&events.alive),
        OsString::from("--vm-name"),
        OsString::from(&mapping.vm_name),
    ]
}

/// The terminal hosts to try, best first.
///
/// Windows Terminal gives a titled tab beside the user's other tabs; PowerShell
/// and `cmd` are what remains on a machine that does not have it. None of them
/// reads the log or interprets the stream: they host the helper and nothing
/// else, and they close when it exits.
fn terminal_commands(helper: &Path, vm_name: &str, arguments: &[OsString]) -> Vec<TerminalCommand> {
    let title = format!("VMLord COM1 — {vm_name}");

    let mut windows_terminal = vec![
        // `new`, not `0`: `0` hands the tab to whatever window Windows Terminal
        // considers current, which is a window VMLord neither owns nor can see.
        // That delivery is not exactly-once -- it has been observed to host the
        // helper twice -- and two readers on one COM1 pipe means the second sits
        // waiting to take the stream the moment the first window is closed.
        // A window of its own is delivered by the process VMLord itself started.
        OsString::from("-w"),
        OsString::from("new"),
        OsString::from("new-tab"),
        OsString::from("--title"),
        OsString::from(title),
        OsString::from("--"),
        helper.as_os_str().to_owned(),
    ];
    windows_terminal.extend(arguments.iter().cloned());

    let quoted = |argument: &OsString| argument.to_string_lossy().into_owned();
    let powershell_line = std::iter::once(powershell_argument(&helper.to_string_lossy()))
        .chain(
            arguments
                .iter()
                .map(|argument| powershell_argument(&quoted(argument))),
        )
        .collect::<Vec<_>>()
        .join(" ");
    let cmd_line = std::iter::once(cmd_argument(&helper.to_string_lossy()))
        .chain(
            arguments
                .iter()
                .map(|argument| cmd_argument(&quoted(argument))),
        )
        .collect::<Vec<_>>()
        .join(" ");

    vec![
        TerminalCommand {
            program: PathBuf::from("wt.exe"),
            args: windows_terminal,
            // Windows Terminal opens its own window; a console of its own would
            // leave an empty black one behind.
            create_new_console: false,
            raw_args: false,
        },
        TerminalCommand {
            program: PathBuf::from("powershell.exe"),
            args: vec![
                OsString::from("-NoLogo"),
                OsString::from("-NoProfile"),
                OsString::from("-Command"),
                // `&` runs the helper; no `-NoExit`, so the window closes with
                // the reader.
                OsString::from(format!("& {powershell_line}")),
            ],
            create_new_console: true,
            raw_args: false,
        },
        TerminalCommand {
            program: PathBuf::from("cmd.exe"),
            args: vec![
                OsString::from("/D"),
                OsString::from("/S"),
                OsString::from("/C"),
                OsString::from(format!("\"{cmd_line}\"")),
            ],
            create_new_console: true,
            raw_args: true,
        },
    ]
}

/// Quotes one argument for PowerShell's parser.
///
/// A single-quoted string in PowerShell has one escape -- a doubled quote --
/// and no other character in it means anything, which is what makes it the
/// right quoting for paths full of spaces, ampersands and parentheses.
fn powershell_argument(argument: &str) -> String {
    format!("'{}'", argument.replace('\'', "''"))
}

/// Quotes one argument for `cmd.exe`'s parser.
///
/// `cmd` has no escape inside a quoted string, so a value that contains a quote
/// cannot be passed through it; none of the helper's arguments ever does -- they
/// are paths, event names and numbers this crate produces.
fn cmd_argument(argument: &str) -> String {
    format!("\"{}\"", argument.replace('"', ""))
}

/// Starts one terminal host.
///
/// The child is deliberately not waited for: the terminal owns what it hosts,
/// and VMLord speaks to a reader through events rather than through a process
/// handle -- while an SSH session it has no business speaking to at all.
///
/// Shared with [`crate::ssh_terminal`], which puts a different program on
/// screen the same way: a terminal is started, and what it hosts is not this
/// process's child to wait for.
pub(crate) fn spawn_terminal(command: &TerminalCommand) -> io::Result<()> {
    use std::os::windows::process::CommandExt;

    let mut process = Command::new(&command.program);
    for argument in &command.args {
        if command.raw_args {
            process.raw_arg(argument);
        } else {
            process.arg(argument);
        }
    }
    if command.create_new_console {
        process.creation_flags(CREATE_NEW_CONSOLE);
    }
    process.spawn().map(|_child| ())
}

/// Finds the helper beside the running executable.
fn helper_path(vm_name: &str) -> Result<PathBuf, RepositoryError> {
    let executable = std::env::current_exe().map_err(|error| {
        RepositoryError::new(format!("cannot locate the VMLord executable: {error}"))
    })?;
    let helper = executable.with_file_name(HELPER_FILE_NAME);
    if !helper.is_file() {
        let error = RepositoryError::new(format!(
            "the COM1 console of VM \"{vm_name}\" cannot be opened: {} is missing",
            helper.display()
        ));
        log::error!("{error}");
        return Err(error);
    }
    Ok(helper)
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsString,
        io,
        path::{Path, PathBuf},
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use uuid::Uuid;

    use super::{
        Com1Launcher, Com1Session, Com1Sessions, EventNames, Liveness, TerminalCommand,
        cmd_argument, helper_arguments, powershell_argument, terminal_commands,
    };
    use crate::event::WindowsEvent;
    use vmlord_core::NetworkMode;

    use crate::{Com1LogMode, metadata::VmComputeSystemMapping};

    const HELPER: &str = r"C:\VMLord\vmlord-com1.exe";

    fn helper_args() -> Vec<OsString> {
        ["--pipe", r"\\.\pipe\vmlord-test.com1", "--vm-name", "dev"]
            .map(OsString::from)
            .to_vec()
    }

    fn mapping(name: &str) -> VmComputeSystemMapping {
        // Sessions are keyed by VM id, so each name needs its own -- and "dev"
        // needs one that can be written down.
        let vm_id = match name {
            "dev" => Uuid::from_u128(0x1f2e_3d4c_5b6a_4798_8899_aabb_ccdd_eeff),
            other => Uuid::from_u128(other.bytes().fold(1_u128, |id, byte| {
                id.wrapping_mul(31).wrapping_add(u128::from(byte))
            })),
        };
        VmComputeSystemMapping {
            vm_id,
            vm_name: name.to_owned(),
            hcs_compute_system_id: format!("vmlord-{name}"),
            disk_gb: 0,
            endpoint_id: None,
            network_mode: NetworkMode::None,
            ssh: None,
            gpu_mode: vmlord_core::GpuMode::None,
            desktop_profile: vmlord_core::DesktopProfile::Headless,
            display_provisioning: vmlord_core::DisplayProvisioning::NotRequested,
            guest_target: None,
        }
    }

    /// Every attempt a launch made, and what each was answered with.
    #[derive(Clone, Default)]
    struct Attempts {
        commands: Arc<Mutex<Vec<TerminalCommand>>>,
        failures: usize,
    }

    impl Attempts {
        fn spawn(&self) -> impl Fn(&TerminalCommand) -> io::Result<()> + Clone + use<> {
            let commands = self.commands.clone();
            let failures = self.failures;
            move |command: &TerminalCommand| {
                let mut commands = commands.lock().unwrap();
                commands.push(command.clone());
                if commands.len() <= failures {
                    return Err(io::Error::other("injected terminal failure"));
                }
                Ok(())
            }
        }

        fn count(&self) -> usize {
            self.commands.lock().unwrap().len()
        }
    }

    #[test]
    fn terminal_commands_prefer_windows_terminal_then_powershell_then_cmd() {
        let commands = terminal_commands(Path::new(HELPER), "dev", &helper_args());

        assert_eq!(commands[0].program, Path::new("wt.exe"));
        assert!(
            commands[0]
                .args
                .iter()
                .any(|arg| arg == "VMLord COM1 — dev")
        );
        assert_eq!(commands[1].program, Path::new("powershell.exe"));
        assert!(commands[1].create_new_console);
        // `-NoExit` would leave a window that outlives the VM and has to be
        // closed by hand.
        assert!(!commands[1].args.iter().any(|arg| arg == "-NoExit"));
        assert_eq!(commands[2].program, Path::new("cmd.exe"));
        assert!(commands[2].create_new_console);
    }

    /// One launch must be able to produce one reader, and asking Windows
    /// Terminal for the current window is what breaks that: `-w 0` is delivered
    /// to a window VMLord does not own, and a delivery that is retried or
    /// replayed hosts the helper twice -- two tabs on one COM1 pipe, the second
    /// of them waiting to steal the stream from the first.
    #[test]
    fn windows_terminal_is_asked_for_its_own_window() {
        let commands = terminal_commands(Path::new(HELPER), "dev", &helper_args());
        let arguments: Vec<String> = commands[0]
            .args
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();

        let window = arguments
            .iter()
            .position(|argument| argument == "-w")
            .map(|flag| arguments[flag + 1].as_str());
        assert_eq!(window, Some("new"), "{arguments:?}");
    }

    #[test]
    fn a_powershell_argument_survives_quoting() {
        assert_eq!(
            powershell_argument(r"C:\Program Files\o'brien & sons (x86)\a.exe"),
            r"'C:\Program Files\o''brien & sons (x86)\a.exe'"
        );
    }

    #[test]
    fn a_cmd_argument_survives_quoting() {
        assert_eq!(
            cmd_argument(r"C:\Program Files\o'brien & sons (x86)\a.exe"),
            r#""C:\Program Files\o'brien & sons (x86)\a.exe""#
        );
    }

    #[test]
    fn falls_back_from_windows_terminal_to_powershell_to_cmd() {
        for (failures, expected) in [(0, 1), (1, 2), (2, 3)] {
            let attempts = Attempts {
                failures,
                ..Attempts::default()
            };
            let launcher = Com1Launcher::for_test(PathBuf::from(HELPER), attempts.spawn());

            launcher
                .launch(
                    &mapping("dev"),
                    Path::new(r"C:\vms\dev"),
                    Com1LogMode::Append,
                )
                .expect("a host that starts must end the fallback");

            assert_eq!(attempts.count(), expected);
        }
    }

    #[test]
    fn falls_back_no_further_than_cmd_and_names_every_host() {
        let attempts = Attempts {
            failures: 3,
            ..Attempts::default()
        };
        let launcher = Com1Launcher::for_test(PathBuf::from(HELPER), attempts.spawn());

        let message = launcher
            .launch(
                &mapping("dev"),
                Path::new(r"C:\vms\dev"),
                Com1LogMode::Append,
            )
            .expect_err("no host left means no console")
            .to_string();

        assert_eq!(attempts.count(), 3);
        for host in ["wt.exe", "powershell.exe", "cmd.exe"] {
            assert!(message.contains(host), "{message}");
        }
        assert!(message.contains("dev"), "{message}");
    }

    #[test]
    fn explicit_launch_uses_truncate_and_waits_until_ready() {
        let attempts = Attempts::default();
        let launcher = Com1Launcher::for_test(PathBuf::from(HELPER), attempts.spawn());

        let session = launcher
            .launch(
                &mapping("dev"),
                Path::new(r"C:\vms\dev"),
                Com1LogMode::Truncate,
            )
            .unwrap();

        let commands = attempts.commands.lock().unwrap();
        let arguments: Vec<String> = commands[0]
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let line = arguments.join(" ");
        assert!(line.contains("truncate"), "{line}");
        assert!(line.contains(r"C:\vms\dev"), "{line}");
        assert!(
            line.contains("1f2e3d4c5b6a47988899aabbccddeeff"),
            "the pipe must be the one this VM's COM1 is wired to: {line}"
        );
        assert_eq!(session.vm_id, mapping("dev").vm_id);
    }

    #[test]
    fn readiness_timeout_cancels_the_helper_and_fails_launch() {
        let attempts = Attempts::default();
        let mut launcher = Com1Launcher::for_test(PathBuf::from(HELPER), attempts.spawn());
        // A spawn that reports success but never signals readiness: the window
        // is open, so the reader must be told to stop before the launch fails.
        launcher.never_ready();
        let cancelled = Arc::new(AtomicUsize::new(0));
        launcher.observe_cancellations(cancelled.clone());

        let message = launcher
            .launch(
                &mapping("dev"),
                Path::new(r"C:\vms\dev"),
                Com1LogMode::Truncate,
            )
            .expect_err("a reader that never reports readiness cannot be trusted")
            .to_string();

        assert!(message.contains("dev"), "{message}");
        assert_eq!(cancelled.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn replacing_a_session_cancels_the_previous_one() {
        let attempts = Attempts::default();
        let mut launcher = Com1Launcher::for_test(PathBuf::from(HELPER), attempts.spawn());
        let cancelled = Arc::new(AtomicUsize::new(0));
        launcher.observe_cancellations(cancelled.clone());
        let mut sessions = Com1Sessions::default();
        let vm_directory = Path::new(r"C:\vms\dev");

        sessions.insert(
            launcher
                .launch(&mapping("dev"), vm_directory, Com1LogMode::Truncate)
                .unwrap(),
        );
        sessions.insert(
            launcher
                .launch(&mapping("dev"), vm_directory, Com1LogMode::Truncate)
                .unwrap(),
        );

        assert_eq!(cancelled.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn reap_reports_failed_sessions_and_removes_finished_ones() {
        let attempts = Attempts::default();
        let launcher = Com1Launcher::for_test(PathBuf::from(HELPER), attempts.spawn());
        let mut sessions = Com1Sessions::default();
        let running = launcher
            .launch(
                &mapping("run"),
                Path::new(r"C:\vms\run"),
                Com1LogMode::Append,
            )
            .unwrap();
        let done = launcher
            .launch(
                &mapping("done"),
                Path::new(r"C:\vms\done"),
                Com1LogMode::Append,
            )
            .unwrap();
        let broken = launcher
            .launch(
                &mapping("broken"),
                Path::new(r"C:\vms\broken"),
                Com1LogMode::Append,
            )
            .unwrap();
        done.finish_for_test();
        broken.fail_for_test();
        let (running_id, done_id, broken_id) = (running.vm_id, done.vm_id, broken.vm_id);
        for session in [running, done, broken] {
            sessions.insert(session);
        }

        let failures = sessions.reap();

        assert_eq!(
            failures
                .iter()
                .map(|failure| failure.vm_name.as_str())
                .collect::<Vec<_>>(),
            ["broken"]
        );
        assert!(sessions.contains(running_id));
        assert!(!sessions.contains(done_id));
        assert!(!sessions.contains(broken_id));
    }

    #[test]
    fn cancelling_everything_leaves_no_session_behind() {
        let attempts = Attempts::default();
        let mut launcher = Com1Launcher::for_test(PathBuf::from(HELPER), attempts.spawn());
        let cancelled = Arc::new(AtomicUsize::new(0));
        launcher.observe_cancellations(cancelled.clone());
        let mut sessions = Com1Sessions::default();
        for name in ["a", "b"] {
            sessions.insert(
                launcher
                    .launch(
                        &mapping(name),
                        Path::new(r"C:\vms").join(name).as_path(),
                        Com1LogMode::Append,
                    )
                    .unwrap(),
            );
        }

        sessions.cancel_all();

        assert_eq!(cancelled.load(Ordering::Relaxed), 2);
        assert!(sessions.reap().is_empty());
    }

    /// A session as production builds one: the liveness of its reader is a
    /// question about a process, asked of an object only that process holds.
    fn session_probing(name: &str, alive: &str) -> Com1Session {
        Com1Session {
            vm_id: mapping(name).vm_id,
            vm_name: name.to_owned(),
            cancel: WindowsEvent::new(true, false).unwrap(),
            failed: WindowsEvent::new(true, false).unwrap(),
            finished: WindowsEvent::new(true, false).unwrap(),
            liveness: Liveness::Probe(alive.to_owned()),
            cancellations: None,
        }
    }

    /// The bug this covers: closing the console window kills the helper where
    /// it stands, so `finished` -- which the helper signals on its way out --
    /// is never signaled at all, and the session VMLord kept looked alive
    /// forever. Nothing could reopen that VM's console afterwards.
    #[test]
    fn a_reader_whose_process_is_gone_is_reaped_though_it_said_nothing() {
        let alive = format!(r"Local\VMLord.Test.Alive.{}", Uuid::new_v4());
        let held = WindowsEvent::create_named(&alive, true, false).unwrap();
        let mut sessions = Com1Sessions::default();
        let vm_id = mapping("dev").vm_id;
        sessions.insert(session_probing("dev", &alive));

        assert!(sessions.reap().is_empty(), "the helper is still running");
        assert!(sessions.contains(vm_id));

        // What closing the window does to the helper's own event.
        drop(held);

        assert!(
            sessions.reap().is_empty(),
            "a closed window is nobody's failure"
        );
        assert!(
            !sessions.contains(vm_id),
            "a VM whose console window was closed must be able to get another"
        );
    }

    /// A reader that is still there is not reaped just because it has been
    /// quiet: the guest may print nothing for hours.
    #[test]
    fn a_reader_that_is_still_running_survives_a_reap() {
        let alive = format!(r"Local\VMLord.Test.Alive.{}", Uuid::new_v4());
        let _held = WindowsEvent::create_named(&alive, true, false).unwrap();
        let mut sessions = Com1Sessions::default();
        sessions.insert(session_probing("dev", &alive));

        for _ in 0..3 {
            assert!(sessions.reap().is_empty());
        }

        assert!(sessions.contains(mapping("dev").vm_id));
    }

    /// The helper cannot create the event VMLord probes unless it is told its
    /// name.
    #[test]
    fn the_helper_is_told_the_name_of_the_event_it_must_hold() {
        let events = EventNames::of(Uuid::from_u128(7));
        let arguments = helper_arguments(
            &mapping("dev"),
            Path::new(r"C:\vms\dev"),
            Com1LogMode::Append,
            &events,
        );
        let arguments: Vec<String> = arguments
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();

        let name = arguments
            .iter()
            .position(|argument| argument == "--alive-event")
            .map(|flag| arguments[flag + 1].as_str());
        assert_eq!(name, Some(events.alive.as_str()), "{arguments:?}");
        assert!(
            events.alive.ends_with(".alive") && events.alive.starts_with(r"Local\VMLord.Com1."),
            "{}",
            events.alive
        );
    }
}
