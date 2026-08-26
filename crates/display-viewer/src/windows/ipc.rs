//! One window per VM, and the two things a later VMLord can ask it to do.
//!
//! A named mutex answers the question "is there already a viewer for this VM?"
//! before anything else happens, so a second Connect finds a window rather than
//! opening one. A named pipe answers everything after that: `Focus` brings the
//! window forward, `Close` shuts the session down.
//!
//! The pipe's authentication is the default DACL of the launching user. That is
//! the right amount for these two operations -- a same-user process could
//! foreground a window or close it without asking us -- and it is deliberately
//! not enough for anything else: asking for a **new session** goes over the
//! launch pipes with the token, so only the VMLord instance that spawned this
//! viewer can be the one to run a handshake for it.
//!
//! The pipe server belongs to the viewer, so it outlives the VMLord that
//! started it and is found by a later one. The later viewer closes that orphan
//! and takes its mutex: only a fresh process can receive fresh launch pipes.

use std::{
    error::Error,
    fmt,
    io::{self, Read, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use windows::{
    Win32::{
        Foundation::{
            CloseHandle, ERROR_ALREADY_EXISTS, ERROR_NO_DATA, ERROR_PIPE_BUSY,
            ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE,
        },
        Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_MODE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
            ReadFile, WriteFile,
        },
        System::{
            Pipes::{
                ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
                PIPE_TYPE_BYTE, PIPE_WAIT,
            },
            Threading::CreateMutexW,
        },
    },
    core::{HSTRING, PCWSTR},
};

use crate::launch::{Command, Link, Message};

/// The most a command message may be. A command is a dozen bytes.
const MAX_COMMAND: usize = 4096;

/// How many bytes the pipe's buffers hold.
const PIPE_BUFFER: u32 = 4096;

/// How long a client waits for the one pipe instance to come free.
///
/// The server serves one connection at a time, so a caller that arrives while
/// the previous message is still being read is told the pipe is busy. Waiting
/// is what that means, not a failure.
const BUSY_WAIT: Duration = Duration::from_secs(2);

/// How long a busy client sleeps between attempts.
const BUSY_POLL: Duration = Duration::from_millis(10);

/// How long a replacement waits for the old viewer to release its claim.
const REPLACE_WAIT: Duration = Duration::from_secs(5);

/// The mutex one viewer holds for the life of its process.
#[must_use]
pub fn mutex_name(runtime_id: &[u8; 16]) -> String {
    format!("Local\\VMLord.Display.{}", hyphenated(runtime_id))
}

/// The pipe one viewer listens on.
#[must_use]
pub fn pipe_name(runtime_id: &[u8; 16]) -> String {
    format!("\\\\.\\pipe\\vmlord-display.{}", hyphenated(runtime_id))
}

/// The runtime id as the text a name carries.
fn hyphenated(runtime_id: &[u8; 16]) -> String {
    let hex: String = runtime_id
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();

    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// The claim on one VM's window, held for the life of the process.
pub struct SingleInstance {
    handle: HANDLE,
}

impl SingleInstance {
    /// Takes the claim, or reports that another viewer has it.
    ///
    /// `Ok(None)` means a viewer for this VM is already running. A caller from
    /// a new VMLord replaces it; repeated Connect inside one VMLord is handled
    /// before another viewer is launched.
    ///
    /// # Errors
    ///
    /// [`IpcError::Win32`] if the mutex could not be created at all.
    pub fn take(runtime_id: &[u8; 16]) -> Result<Option<Self>, IpcError> {
        let name = HSTRING::from(mutex_name(runtime_id));
        // SAFETY: `name` is a NUL-terminated wide string living across the call.
        // The returned handle is owned by the `SingleInstance` below.
        let handle = unsafe { CreateMutexW(None, true, PCWSTR(name.as_ptr())) }
            .map_err(|error| IpcError::Win32(error.to_string()))?;

        // SAFETY: A thread-local read of the last error, which `CreateMutexW`
        // sets to `ERROR_ALREADY_EXISTS` when it opened rather than created.
        let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        if existed {
            // SAFETY: The handle is owned here and closed exactly once.
            unsafe {
                let _ = CloseHandle(handle);
            }
            return Ok(None);
        }

        Ok(Some(Self { handle }))
    }
}

/// Closes the viewer left by an earlier VMLord and takes its claim.
///
/// The old viewer cannot reuse launch pipes whose owning process has exited.
/// Replacing it is what binds the surviving window request to the new
/// process's pipes and session driver.
pub fn replace_instance(runtime_id: &[u8; 16]) -> Result<SingleInstance, IpcError> {
    send_command(runtime_id, Command::Close)?;
    let deadline = Instant::now() + REPLACE_WAIT;
    loop {
        if let Some(claim) = SingleInstance::take(runtime_id)? {
            return Ok(claim);
        }
        if Instant::now() >= deadline {
            return Err(IpcError::Unreachable(
                "the old display viewer did not close".to_owned(),
            ));
        }
        thread::sleep(BUSY_POLL);
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        // SAFETY: This instance owns the handle and closes it once.
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// A handle on its way to the thread that will own it.
///
/// `HANDLE` is a raw pointer and so is not `Send`. Moving one to a thread that
/// then owns it exclusively is sound, and this is where that is said out loud.
struct SendHandle(HANDLE);

// SAFETY: the handle is moved, not shared: the thread it reaches is the only
// one that uses it, and the only one that closes it.
unsafe impl Send for SendHandle {}

/// The pipe a later VMLord asks this window to focus or close through.
pub struct CommandServer {
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    name: String,
}

impl CommandServer {
    /// Starts serving, on a thread of its own.
    ///
    /// # Errors
    ///
    /// [`IpcError::Win32`] if the pipe could not be created, which is what a
    /// second viewer for the same VM would see -- and cannot happen, because
    /// [`SingleInstance`] has already answered that question.
    pub fn start(runtime_id: &[u8; 16], sink: Sender<Command>) -> Result<Self, IpcError> {
        let name = pipe_name(runtime_id);
        // Created here rather than on the thread, so that a failure is the
        // caller's to see.
        let first = create_pipe(&name)?;
        let running = Arc::new(AtomicBool::new(true));

        let thread = {
            let running = Arc::clone(&running);
            let first = SendHandle(first);
            thread::spawn(move || {
                // Named so that the closure captures the `Send` wrapper rather
                // than the raw handle inside it.
                let first = first;
                serve(first.0, &sink, &running);
            })
        };

        Ok(Self {
            running,
            thread: Some(thread),
            name,
        })
    }
}

impl Drop for CommandServer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        // A connection of our own, so that the blocking `ConnectNamedPipe`
        // returns and the thread sees that it should stop. Its failure is
        // expected once the thread has already gone.
        let _ = connect_pipe(&self.name);

        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Serves one connection at a time until the server is dropped.
///
/// One pipe instance, reused: a viewer serves one VMLord, and a second
/// connection waits its turn rather than being answered out of order.
fn serve(pipe: HANDLE, sink: &Sender<Command>, running: &Arc<AtomicBool>) {
    while running.load(Ordering::Relaxed) {
        // SAFETY: `pipe` is an owned pipe instance in the listening state.
        let connected = unsafe { ConnectNamedPipe(pipe, None) };
        if connected.is_err() {
            // SAFETY: A thread-local read of the last error. A client that got
            // in before the call is `ERROR_PIPE_CONNECTED`, and one that wrote
            // its message and hung up before it is `ERROR_NO_DATA` -- what it
            // left is still in the pipe's buffer. Both are a connection to
            // read rather than a failure.
            let code = unsafe { GetLastError() };
            if code != ERROR_PIPE_CONNECTED && code != ERROR_NO_DATA {
                tracing::debug!("the command pipe stopped listening: {code:?}");
                break;
            }
        }

        if running.load(Ordering::Relaxed) {
            // Borrowed: the listening instance is closed once, below, after the
            // loop -- not at the end of every connection.
            let mut handle = PipeHandle::borrowed(pipe);
            let mut link = Link::new(&mut handle, io::sink());
            match link.read() {
                Ok(Message::Command(command)) => {
                    tracing::info!("the viewer was asked to {command:?}");
                    if sink.send(command).is_err() {
                        break;
                    }
                }
                Ok(other) => tracing::warn!(
                    "a {other:?} arrived on the command pipe, which answers commands only"
                ),
                Err(error) => tracing::debug!("a command could not be read: {error}"),
            }
        }

        // SAFETY: `pipe` is owned by this thread and is connected.
        unsafe {
            let _ = DisconnectNamedPipe(pipe);
        }
    }

    // SAFETY: The server owns the pipe and closes it once.
    unsafe {
        let _ = CloseHandle(pipe);
    }
}

/// Creates the listening end of the command pipe.
fn create_pipe(name: &str) -> Result<HANDLE, IpcError> {
    let wide = HSTRING::from(name);
    // SAFETY: `wide` is a NUL-terminated wide string living across the call.
    // `None` for the security attributes is the default DACL of the launching
    // user, which is the authentication this pipe relies on.
    let handle = unsafe {
        CreateNamedPipeW(
            PCWSTR(wide.as_ptr()),
            PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            1,
            PIPE_BUFFER,
            PIPE_BUFFER,
            0,
            None,
        )
    };
    if handle.is_invalid() {
        return Err(IpcError::Win32(format!(
            "the command pipe {name} could not be created"
        )));
    }

    Ok(handle)
}

/// Opens the client end of a viewer's command pipe.
///
/// A busy pipe is the server still reading the previous message rather than an
/// absent viewer, so it is waited out up to [`BUSY_WAIT`].
fn connect_pipe(name: &str) -> Result<PipeHandle, IpcError> {
    let wide = HSTRING::from(name);
    let deadline = Instant::now() + BUSY_WAIT;

    loop {
        // SAFETY: `wide` is a NUL-terminated wide string living across the call,
        // and the returned handle is owned by the `PipeHandle` below.
        let opened = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                GENERIC_READ.0 | GENERIC_WRITE.0,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        };

        match opened {
            Ok(handle) => return Ok(PipeHandle::owned(handle)),
            Err(error) if error.code() == ERROR_PIPE_BUSY.to_hresult() => {
                if Instant::now() >= deadline {
                    return Err(IpcError::Unreachable(
                        "the viewer's command pipe stayed busy".to_owned(),
                    ));
                }
                thread::sleep(BUSY_POLL);
            }
            Err(error) => return Err(IpcError::Unreachable(error.to_string())),
        }
    }
}

/// Asks the viewer of `runtime_id` to do something.
///
/// # Errors
///
/// [`IpcError::Unreachable`] if no viewer is listening, which is how a caller
/// learns that the window it expected is gone.
pub fn send_command(runtime_id: &[u8; 16], command: Command) -> Result<(), IpcError> {
    send_message(runtime_id, &Message::Command(command))
}

/// Writes one message to a viewer's command pipe.
///
/// Public so that the tests can send something the server refuses; production
/// code sends commands.
///
/// # Errors
///
/// [`IpcError::Unreachable`] if no viewer is listening or the write failed.
pub fn send_message(runtime_id: &[u8; 16], message: &Message) -> Result<(), IpcError> {
    let mut handle = connect_pipe(&pipe_name(runtime_id))?;
    let mut link = Link::new(io::empty(), &mut handle);

    link.write(message)
        .map_err(|error| IpcError::Unreachable(error.to_string()))
}

/// A pipe handle that reads and writes like a stream.
///
/// `owned` says who closes it: the client end this module opened closes on
/// drop, and the server's listening instance does not -- it is reused for the
/// next connection and closed once, when the loop ends.
struct PipeHandle {
    handle: HANDLE,
    owned: bool,
}

impl PipeHandle {
    /// A handle this wrapper closes.
    fn owned(handle: HANDLE) -> Self {
        Self {
            handle,
            owned: true,
        }
    }

    /// A handle somebody else closes.
    fn borrowed(handle: HANDLE) -> Self {
        Self {
            handle,
            owned: false,
        }
    }
}

impl Read for PipeHandle {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let mut read = 0u32;
        let take = buffer.len().min(MAX_COMMAND);
        // SAFETY: `self.handle` is a valid pipe handle and `buffer` is valid for
        // writes for `take` bytes.
        unsafe {
            ReadFile(
                self.handle,
                Some(&mut buffer[..take]),
                Some(&raw mut read),
                None,
            )
        }
        .map_err(|error| io::Error::other(error.to_string()))?;

        Ok(read as usize)
    }
}

impl Write for PipeHandle {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let mut written = 0u32;
        // SAFETY: `self.handle` is a valid pipe handle and `buffer` is valid for
        // reads for its own length.
        unsafe { WriteFile(self.handle, Some(buffer), Some(&raw mut written), None) }
            .map_err(|error| io::Error::other(error.to_string()))?;

        Ok(written as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for PipeHandle {
    fn drop(&mut self) {
        if !self.owned {
            return;
        }

        // SAFETY: an owned handle, closed exactly once.
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// Why a viewer could not be claimed or reached.
#[derive(Debug)]
pub enum IpcError {
    /// A Win32 call refused.
    Win32(String),
    /// No viewer is listening on that VM's pipe.
    Unreachable(String),
}

impl fmt::Display for IpcError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Win32(detail) => write!(formatter, "a Windows call refused: {detail}"),
            Self::Unreachable(detail) => {
                write!(formatter, "no display viewer is listening: {detail}")
            }
        }
    }
}

impl Error for IpcError {}

#[cfg(test)]
mod tests {
    use std::{
        sync::mpsc,
        time::{Duration, Instant},
    };

    use super::{
        CommandServer, SingleInstance, mutex_name, pipe_name, replace_instance, send_command,
    };
    use crate::launch::{Command, Message};

    /// A runtime id nothing else in the test process uses.
    fn runtime_id(tag: u8) -> [u8; 16] {
        let mut id = [0xab; 16];
        id[15] = tag;
        id
    }

    #[test]
    fn the_names_are_per_vm_and_local_to_this_session() {
        let name = mutex_name(&runtime_id(1));
        assert!(name.starts_with("Local\\VMLord.Display."));
        assert_ne!(name, mutex_name(&runtime_id(2)));

        let pipe = pipe_name(&runtime_id(1));
        assert!(pipe.starts_with("\\\\.\\pipe\\vmlord-display."));
        assert_ne!(pipe, pipe_name(&runtime_id(2)));
    }

    #[test]
    fn a_second_viewer_for_the_same_vm_finds_the_mutex_taken() {
        let id = runtime_id(3);
        let first = SingleInstance::take(&id)
            .expect("the mutex can be created")
            .expect("nothing else holds it");

        assert!(
            SingleInstance::take(&id)
                .expect("the mutex can be opened")
                .is_none(),
            "a second viewer must find the first"
        );

        drop(first);
        assert!(
            SingleInstance::take(&id)
                .expect("the mutex can be created")
                .is_some(),
            "the mutex is released when the viewer exits"
        );
    }

    #[test]
    fn a_new_parent_closes_the_old_viewer_and_takes_its_claim() {
        let id = runtime_id(9);
        let first = SingleInstance::take(&id)
            .expect("the mutex can be created")
            .expect("nothing else holds it");
        let (sink, commands) = mpsc::channel();
        let server = CommandServer::start(&id, sink).expect("the pipe can be created");
        let (replacement, replaced) = mpsc::channel();
        let replacing = std::thread::spawn(move || {
            let claim = replace_instance(&id).expect("the old viewer gives way");
            replacement.send(()).unwrap();
            drop(claim);
        });

        assert_eq!(
            commands.recv_timeout(Duration::from_secs(5)).unwrap(),
            Command::Close
        );
        drop(server);
        drop(first);
        replaced.recv_timeout(Duration::from_secs(5)).unwrap();

        replacing.join().unwrap();
    }

    #[test]
    fn two_vms_get_a_window_each() {
        let _first = SingleInstance::take(&runtime_id(4))
            .expect("the mutex can be created")
            .expect("nothing else holds it");
        let second = SingleInstance::take(&runtime_id(5)).expect("the mutex can be created");

        assert!(second.is_some());
    }

    #[test]
    fn the_pipe_delivers_focus_and_close() {
        let id = runtime_id(6);
        let (sink, commands) = mpsc::channel();
        let _server = CommandServer::start(&id, sink).expect("the pipe can be created");

        send_command(&id, Command::Focus).expect("the server is listening");
        send_command(&id, Command::Close).expect("the server is listening");

        assert_eq!(
            commands
                .recv_timeout(Duration::from_secs(5))
                .expect("a focus"),
            Command::Focus
        );
        assert_eq!(
            commands
                .recv_timeout(Duration::from_secs(5))
                .expect("a close"),
            Command::Close
        );
    }

    #[test]
    fn a_request_for_a_new_session_is_not_answerable_on_this_pipe() {
        let id = runtime_id(7);
        let (sink, commands) = mpsc::channel();
        let _server = CommandServer::start(&id, sink).expect("the pipe can be created");

        super::send_message(&id, &Message::RequestRelay { token: vec![1; 32] })
            .expect("the server is listening");
        // Something the server does accept, so that the test is not waiting on
        // a message it already refused.
        send_command(&id, Command::Focus).expect("the server is listening");

        assert_eq!(
            commands
                .recv_timeout(Duration::from_secs(5))
                .expect("the focus that followed"),
            Command::Focus,
            "a refresh must not be delivered as a command"
        );
    }

    #[test]
    fn a_pipe_nobody_is_serving_reports_it_rather_than_hanging() {
        let started = Instant::now();

        assert!(send_command(&runtime_id(8), Command::Focus).is_err());
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
