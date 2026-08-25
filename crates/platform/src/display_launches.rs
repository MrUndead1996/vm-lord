//! The display windows VMLord has opened, and the pipes it serves them on.
//!
//! One process per Connect, and no map of open windows: the viewer answers
//! "is one already open?" itself, with a named mutex on the partition's
//! runtime id, and a second launch asks the window that is there to come
//! forward and exits. What is kept here is only the threads holding the launch
//! pipes.
//!
//! What is tracked beyond the threads is the pair of ids one window is
//! addressed by: the VM it belongs to, and the partition whose runtime id names
//! its command pipe. That is what lets a VM that stopped take its window with
//! it -- see [`DisplayLaunches::close`].
//!
//! Those threads are never joined at shutdown. A display session outliving the
//! application is the property the separate process was built for: closing
//! VMLord closes the pipes, which costs the viewer the right to ask for a
//! fresh session and nothing else, and leaves the desktop on screen. Threads
//! are collected when the viewer they served has gone, which the next launch
//! does.

use std::{
    io,
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
};

use uuid::Uuid;
use vmlord_core::{Diagnostic, DiagnosticLevel, DisplayMode, RepositoryError};
use vmlord_display_protocol::keys::Secret;
use vmlord_display_viewer::{
    launch::{self, Link, Message},
    windows::ipc,
};

use crate::display_session::Driver;

/// The viewer binary, which ships beside the application -- see `cargo dist`.
const VIEWER: &str = "vmlord-display.exe";

/// Where the diagnostics of a launch go: the buffer the UI already drains.
type Diagnostics = Arc<Mutex<Vec<Diagnostic>>>;

/// Everything one launch needs.
pub(crate) struct LaunchRequest<'a> {
    pub(crate) vm_name: &'a str,
    /// The VM this window belongs to, which is what a stopped VM is named by.
    pub(crate) vm_id: Uuid,
    /// The VM's secret, from which this session's keys are derived. It stays
    /// on this side of the pipes.
    pub(crate) secret: Secret,
    /// The partition the viewer's three sockets address.
    pub(crate) runtime_id: Uuid,
    /// The mode stored for this VM, if one has been.
    pub(crate) mode: Option<DisplayMode>,
    pub(crate) viewer: PathBuf,
    pub(crate) diagnostics: Diagnostics,
}

/// Where the viewer is, given where the application is.
///
/// # Errors
///
/// [`RepositoryError`] when this process cannot say where it is running from,
/// which is the only way the answer can be unknown.
pub(crate) fn viewer_path() -> Result<PathBuf, RepositoryError> {
    let executable = std::env::current_exe().map_err(|error| {
        RepositoryError::new(format!(
            "VMLord cannot tell where it is running from: {error}"
        ))
    })?;
    let directory = executable
        .parent()
        .ok_or_else(|| RepositoryError::new("VMLord is running from a path with no directory"))?;

    Ok(directory.join(VIEWER))
}

/// One viewer's thread.
struct Worker {
    vm_name: String,
    /// The VM whose desktop this window shows.
    vm_id: Uuid,
    /// The partition this viewer addresses, which is also what names its
    /// command pipe: it is how a window is asked to close.
    runtime_id: Uuid,
    /// Set by the thread as it leaves, by whichever exit, so that a thread
    /// that died is still joined rather than left in the list forever.
    finished: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

/// How a window is asked to close: its command pipe, addressed by runtime id.
///
/// Behind a function so that the tests can watch what would be sent without a
/// viewer process to send it to.
type ViewerCloser = Arc<dyn Fn(&[u8; 16]) -> Result<(), String> + Send + Sync>;

/// The display windows this process has opened.
pub(crate) struct DisplayLaunches {
    workers: Mutex<Vec<Worker>>,
    closer: ViewerCloser,
}

impl Default for DisplayLaunches {
    fn default() -> Self {
        Self {
            workers: Mutex::new(Vec::new()),
            closer: Arc::new(|runtime_id: &[u8; 16]| {
                ipc::send_command(runtime_id, launch::Command::Close)
                    .map_err(|error| error.to_string())
            }),
        }
    }
}

impl DisplayLaunches {
    /// Starts a viewer and the thread that serves its pipes.
    ///
    /// Returns once the process is running and its launch parameters are
    /// written. Everything after that -- the handshake, the hand-over, a
    /// session that had to be opened again, a window that closed -- happens on
    /// the thread and is reported through the diagnostics buffer, because none
    /// of it is quick enough to keep the caller's thread, which draws the
    /// window.
    ///
    /// # Errors
    ///
    /// [`RepositoryError`] when the viewer is not beside the application,
    /// cannot be started, or cannot be told what to open. All three are the
    /// launch failing, which is a different thing from a session that failed
    /// after one.
    pub(crate) fn start(&self, request: LaunchRequest<'_>) -> Result<(), RepositoryError> {
        let mut workers = self.lock();
        collect_finished(&mut workers);

        if !request.viewer.is_file() {
            return Err(RepositoryError::new(format!(
                "{} is not beside VMLord, so no display window can be opened",
                request.viewer.display()
            )));
        }

        let (mut driver, parameters) = Driver::open(
            request.vm_name,
            request.secret,
            request.runtime_id,
            request.mode,
        );
        let mut child = spawn(&request.viewer, request.vm_name)?;
        let (reader, writer) = pipes(&mut child, request.vm_name)?;

        let mut to_viewer = Link::new(io::empty(), writer);
        to_viewer
            .write(&Message::Launch(parameters))
            .map_err(|error| {
                RepositoryError::new(format!(
                    "the display window of VM \"{}\" could not be told what to open: {error}",
                    request.vm_name
                ))
            })?;

        let vm_name = request.vm_name.to_owned();
        let diagnostics = Arc::clone(&request.diagnostics);
        let finished = Arc::new(AtomicBool::new(false));
        let handle = std::thread::Builder::new()
            .name(format!("vmlord-display-{vm_name}"))
            .spawn({
                let finished = Arc::clone(&finished);
                let vm_name = vm_name.clone();
                move || {
                    let _finish = Finish(finished);
                    serve(&mut driver, reader, to_viewer, &vm_name, &diagnostics);
                    wait_for(child, &vm_name, &diagnostics);
                }
            })
            .map_err(|error| {
                RepositoryError::new(format!(
                    "the thread serving the display window of VM \"{vm_name}\" could not be \
                     started: {error}"
                ))
            })?;

        report(
            &request.diagnostics,
            DiagnosticLevel::Info,
            format!("Opening the display of VM \"{vm_name}\""),
        );
        workers.push(Worker {
            vm_name,
            vm_id: request.vm_id,
            runtime_id: request.runtime_id,
            finished,
            handle: Some(handle),
        });
        Ok(())
    }

    /// Asks the display window of `vm_id`, if this process opened one, to
    /// close.
    ///
    /// Sent over the viewer's command pipe rather than the launch pipe: that is
    /// the channel the window itself reads, and closing the launch pipe would
    /// only cost the viewer the right to ask for a fresh session -- which is
    /// what keeps a desktop on screen when VMLord exits.
    ///
    /// A viewer that cannot be reached is not an error worth a diagnostic: a
    /// window the user has already closed is exactly what that looks like.
    pub(crate) fn close(&self, vm_id: Uuid) {
        let mut workers = self.lock();
        collect_finished(&mut workers);

        for worker in workers.iter().filter(|worker| worker.vm_id == vm_id) {
            tracing::info!(
                "closing the display window of VM \"{}\": the VM has stopped",
                worker.vm_name
            );
            if let Err(error) = (self.closer)(worker.runtime_id.as_bytes()) {
                tracing::info!(
                    "the display window of VM \"{}\" could not be asked to close: {error}",
                    worker.vm_name
                );
            }
        }
    }

    /// Recovers a poisoned lock rather than propagating the panic: a launch
    /// that panicked must not take the repository down with it.
    fn lock(&self) -> MutexGuard<'_, Vec<Worker>> {
        self.workers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Reads what the viewer says and writes back what the driver answers.
///
/// Ends when the pipe does, which is what a viewer that exited looks like from
/// here. A pipe that cannot be written is the same thing a moment earlier.
fn serve(
    driver: &mut Driver,
    reader: ChildStdout,
    mut to_viewer: Link<io::Empty, ChildStdin>,
    vm_name: &str,
    diagnostics: &Diagnostics,
) {
    let mut from_viewer = Link::new(reader, io::sink());
    loop {
        let message = match from_viewer.read() {
            Ok(message) => message,
            Err(error) => {
                tracing::info!("the launch pipe of VM \"{vm_name}\" ended: {error}");
                return;
            }
        };

        let answer = driver.handle(message);
        for diagnostic in answer.diagnostics {
            push(diagnostics, diagnostic);
        }
        for message in answer.to_viewer {
            if let Err(error) = to_viewer.write(&message) {
                tracing::info!("the launch pipe of VM \"{vm_name}\" could not be written: {error}");
                return;
            }
        }
    }
}

/// Waits for the viewer, and reports an exit nobody asked for.
///
/// A window that was closed exits cleanly and is worth a log line and nothing
/// more: closing it is what a person does with a window.
fn wait_for(mut child: Child, vm_name: &str, diagnostics: &Diagnostics) {
    match child.wait() {
        Ok(status) if status.success() => {
            tracing::info!("the display window of VM \"{vm_name}\" was closed");
        }
        Ok(status) => report(
            diagnostics,
            DiagnosticLevel::Error,
            format!("The display window of VM \"{vm_name}\" stopped unexpectedly ({status})"),
        ),
        Err(error) => {
            tracing::warn!("VMLord lost track of the display window of VM \"{vm_name}\": {error}");
        }
    }
}

/// Starts the viewer with both pipes and no arguments.
///
/// Nothing structural and nothing sensitive is on the command line or in the
/// environment, which is what keeps a channel key out of a process listing.
fn spawn(viewer: &Path, vm_name: &str) -> Result<Child, RepositoryError> {
    Command::new(viewer)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            let error = RepositoryError::new(format!(
                "the display window of VM \"{vm_name}\" could not be started: {error}"
            ));
            tracing::error!("{error}");
            error
        })
}

/// Takes both pipes off the child, which is what makes them this side's.
fn pipes(child: &mut Child, vm_name: &str) -> Result<(ChildStdout, ChildStdin), RepositoryError> {
    let reader = child.stdout.take().ok_or_else(|| {
        RepositoryError::new(format!(
            "the display window of VM \"{vm_name}\" was started without a pipe to read"
        ))
    })?;
    let writer = child.stdin.take().ok_or_else(|| {
        RepositoryError::new(format!(
            "the display window of VM \"{vm_name}\" was started without a pipe to write"
        ))
    })?;

    Ok((reader, writer))
}

fn report(diagnostics: &Diagnostics, level: DiagnosticLevel, message: String) {
    push(diagnostics, Diagnostic { level, message });
}

/// Puts one diagnostic where the UI will find it, and in the log.
fn push(diagnostics: &Diagnostics, diagnostic: Diagnostic) {
    match diagnostic.level {
        DiagnosticLevel::Error => tracing::error!("{}", diagnostic.message),
        _ => tracing::info!("{}", diagnostic.message),
    }
    diagnostics
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(diagnostic);
}

/// Joins and drops every worker whose viewer has gone.
fn collect_finished(workers: &mut Vec<Worker>) {
    let mut index = 0;
    while index < workers.len() {
        if workers[index].finished.load(Ordering::Relaxed) {
            let mut worker = workers.remove(index);
            if let Some(handle) = worker.handle.take()
                && handle.join().is_err()
            {
                tracing::error!(
                    "the thread serving the display window of VM \"{}\" panicked",
                    worker.vm_name
                );
            }
        } else {
            index += 1;
        }
    }
}

/// Marks a worker finished however its thread leaves.
struct Finish(Arc<AtomicBool>);

impl Drop for Finish {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::{Arc, Mutex, atomic::AtomicBool},
    };

    use uuid::Uuid;
    use vmlord_display_protocol::keys::Secret;

    use super::{DisplayLaunches, LaunchRequest, Worker};

    /// What a viewer's command pipe carried, without a viewer.
    type Closed = Arc<Mutex<Vec<[u8; 16]>>>;

    /// Launches holding `workers`, whose close requests are recorded rather
    /// than sent.
    fn launches(workers: Vec<Worker>) -> (DisplayLaunches, Closed) {
        let closed: Closed = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&closed);
        let launches = DisplayLaunches {
            workers: Mutex::new(workers),
            closer: Arc::new(move |runtime_id: &[u8; 16]| {
                recorder
                    .lock()
                    .expect("an uncontended lock")
                    .push(*runtime_id);
                Ok(())
            }),
        };

        (launches, closed)
    }

    /// A worker for a window that is still open.
    fn worker(vm_name: &str, vm_id: Uuid, runtime_id: Uuid) -> Worker {
        Worker {
            vm_name: vm_name.to_owned(),
            vm_id,
            runtime_id,
            finished: Arc::new(AtomicBool::new(false)),
            handle: Some(std::thread::spawn(|| {})),
        }
    }

    #[test]
    fn a_viewer_that_is_not_beside_the_application_is_refused_by_name() {
        let launches = DisplayLaunches::default();
        let diagnostics = Arc::new(Mutex::new(Vec::new()));

        let error = launches
            .start(LaunchRequest {
                vm_name: "dev",
                vm_id: Uuid::from_u128(3),
                secret: Secret::generate(),
                runtime_id: Uuid::from_u128(7),
                mode: None,
                viewer: PathBuf::from(r"C:\nowhere\vmlord-display.exe"),
                diagnostics: Arc::clone(&diagnostics),
            })
            .expect_err("there is no viewer at that path");

        assert!(
            error.to_string().contains("vmlord-display.exe"),
            "the message must name the file that is missing: {error}"
        );
        assert!(
            diagnostics.lock().expect("an uncontended lock").is_empty(),
            "a launch that never started reports through its error, not twice"
        );
    }

    #[test]
    fn the_viewer_is_looked_for_beside_the_running_application() {
        let path = super::viewer_path().expect("this process has a path");

        assert_eq!(
            path.file_name().and_then(std::ffi::OsStr::to_str),
            Some("vmlord-display.exe")
        );
        assert_eq!(
            path.parent(),
            std::env::current_exe()
                .expect("this process has a path")
                .parent(),
            "the viewer ships beside the application, as `cargo dist` puts it"
        );
    }

    #[test]
    fn the_window_of_the_vm_that_stopped_is_asked_to_close_by_its_runtime_id() {
        let stopped = Uuid::from_u128(1);
        let (launches, closed) = launches(vec![
            worker("dev", stopped, Uuid::from_u128(11)),
            worker("build", Uuid::from_u128(2), Uuid::from_u128(22)),
        ]);

        launches.close(stopped);

        assert_eq!(
            *closed.lock().expect("an uncontended lock"),
            vec![*Uuid::from_u128(11).as_bytes()],
            "only the window of the VM that stopped is closed, and it is \
             addressed by the partition it was opened on"
        );
    }

    #[test]
    fn a_vm_with_no_window_of_this_process_closes_nothing() {
        let (launches, closed) =
            launches(vec![worker("dev", Uuid::from_u128(1), Uuid::from_u128(11))]);

        launches.close(Uuid::from_u128(9));

        assert!(
            closed.lock().expect("an uncontended lock").is_empty(),
            "a VM VMLord never opened a display for has no window to close"
        );
    }
}
