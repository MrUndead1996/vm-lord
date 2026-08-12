//! The COM1 diagnostic reader run by the `vmlord-com1` helper process.
//!
//! The helper is a second process on purpose: a terminal window hosts it, and
//! the bytes a guest writes to its first serial port have to reach that window
//! and `com1.log` unchanged. Everything it needs arrives as arguments -- none of
//! which may carry a secret -- and everything the parent has to say afterwards
//! arrives through named events.

use std::{
    ffi::{OsStr, OsString},
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use vmlord_core::RepositoryError;
use windows::{
    Win32::{
        Foundation::{
            CloseHandle, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_IO_INCOMPLETE,
            ERROR_IO_PENDING, ERROR_NO_DATA, ERROR_OPERATION_ABORTED, ERROR_PIPE_BUSY,
            ERROR_PIPE_NOT_CONNECTED, GENERIC_READ, GENERIC_WRITE, HANDLE, WAIT_FAILED,
            WAIT_OBJECT_0, WAIT_TIMEOUT,
        },
        Storage::FileSystem::{
            CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
            ReadFile,
        },
        System::{
            IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
            Pipes::WaitNamedPipeW,
            Threading::{
                OpenProcess, PROCESS_SYNCHRONIZE, WaitForMultipleObjects, WaitForSingleObject,
            },
        },
    },
    core::{Error, HSTRING},
};

use crate::{
    com1_input::{ConsoleModes, start_input},
    error::windows_error,
    event::WindowsEvent,
};

/// How long one connection attempt waits for the pipe to exist.
///
/// Short and repeated rather than long and single: between attempts the helper
/// has to notice cancellation and a parent that went away.
const CONNECT_POLL: Duration = Duration::from_millis(250);

/// How much guest output one read may return.
const READ_BUFFER_BYTES: usize = 4096;

/// How much of `com1.log` a reopened console shows before the live stream.
///
/// A window opened onto a running guest is otherwise empty until the guest
/// prints its next byte, and a guest sitting at `login:` prints nothing at all:
/// the prompt was written once, minutes ago. Enough to carry the end of the
/// boot and the prompt, and not so much that reopening a console scrolls a
/// whole boot past the reader.
const REPLAY_TAIL_BYTES: usize = 64 * 1024;

/// The access the helper asks the COM1 pipe for.
///
/// Both directions: HCS serves the pipe duplex, and the same handle carries the
/// guest's output out and the user's keystrokes in.
const PIPE_ACCESS: u32 = GENERIC_READ.0 | GENERIC_WRITE.0;

/// How the helper opens `com1.log`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Com1LogMode {
    /// An explicit start: this boot's output replaces the previous one.
    Truncate,
    /// A reconnect to a VM that is already running: its log continues.
    Append,
}

/// Everything the helper process is told on its command line.
#[derive(Debug, PartialEq, Eq)]
pub struct Com1HelperOptions {
    pub pipe_path: PathBuf,
    pub log_path: PathBuf,
    pub log_mode: Com1LogMode,
    pub parent_process_id: u32,
    pub cancel_event_name: String,
    pub ready_event_name: String,
    pub failed_event_name: String,
    pub finished_event_name: String,
    /// The event this process creates and holds, so that VMLord can tell a
    /// reader that is still running from one whose window was closed.
    pub alive_event_name: String,
    pub vm_name: String,
}

/// The flags [`parse_com1_helper_args`] accepts, in the order the launcher
/// writes them.
const FLAGS: [&str; 10] = [
    "--pipe",
    "--log",
    "--mode",
    "--parent-pid",
    "--cancel-event",
    "--ready-event",
    "--failed-event",
    "--finished-event",
    "--alive-event",
    "--vm-name",
];

/// Parses the helper's command line.
///
/// Exact flag/value pairs only: an unknown or repeated flag is a launcher bug,
/// and guessing at one would let a typo silently disable cancellation.
pub fn parse_com1_helper_args(
    args: impl IntoIterator<Item = OsString>,
) -> Result<Com1HelperOptions, RepositoryError> {
    let mut values: [Option<OsString>; FLAGS.len()] = Default::default();
    let mut args = args.into_iter();

    while let Some(flag) = args.next() {
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

    let pipe_path = PathBuf::from(next()?);
    let log_path = PathBuf::from(next()?);
    let log_mode = match next()?.to_string_lossy().as_ref() {
        "truncate" => Com1LogMode::Truncate,
        "append" => Com1LogMode::Append,
        other => {
            return Err(argument_error(format!(
                "--mode must be \"truncate\" or \"append\", not \"{other}\""
            )));
        }
    };
    let parent_process_id = match next()?.to_string_lossy().parse::<u32>() {
        // Zero is the idle process: it can neither be opened nor exit, so a
        // helper given it would never notice its parent going away.
        Ok(0) | Err(_) => {
            return Err(argument_error(
                "--parent-pid must be the process id of a running VMLord".to_owned(),
            ));
        }
        Ok(parent) => parent,
    };
    let mut name = || -> Result<String, RepositoryError> { Ok(next()?.to_string_lossy().into()) };

    Ok(Com1HelperOptions {
        pipe_path,
        log_path,
        log_mode,
        parent_process_id,
        cancel_event_name: name()?,
        ready_event_name: name()?,
        failed_event_name: name()?,
        finished_event_name: name()?,
        alive_event_name: name()?,
        vm_name: name()?,
    })
}

fn argument_error(detail: String) -> RepositoryError {
    RepositoryError::new(format!(
        "VMLord COM1 reader arguments are invalid: {detail}"
    ))
}

/// Writes one chunk of guest output to both destinations, unchanged.
fn mirror_chunk(bytes: &[u8], log: &mut impl Write, terminal: &mut impl Write) -> io::Result<()> {
    log.write_all(bytes)?;
    terminal.write_all(bytes)?;
    log.flush()?;
    terminal.flush()
}

/// Mirrors the VM's serial output into `com1.log` and this process's terminal
/// until the guest closes the pipe, the parent cancels, or the parent exits.
///
/// Returning `Ok(())` means the capture ended for one of those three expected
/// reasons; every error path signals the failure event first, so that VMLord
/// can report a reader that stopped for any other reason.
pub fn run_com1_helper(options: Com1HelperOptions) -> Result<(), RepositoryError> {
    let cancel = WindowsEvent::open(&options.cancel_event_name)?;
    let ready = WindowsEvent::open(&options.ready_event_name)?;
    let failed = WindowsEvent::open(&options.failed_event_name)?;
    let finished = WindowsEvent::open(&options.finished_event_name)?;
    // Created here rather than by VMLord, and held for as long as this process
    // lives: a named object exists while a handle to it does, so VMLord probing
    // this name is asking whether this process is still there. That is the only
    // question `finished` cannot answer -- a window a person closes takes the
    // helper down with no chance to signal anything.
    let _alive = WindowsEvent::create_named(&options.alive_event_name, true, false)?;
    // Whatever happens below -- success, error, or a panic unwinding through
    // it -- VMLord learns that this reader is over.
    let _finish = SignalOnDrop(&finished);

    match capture(&options, &cancel, &ready) {
        Ok(()) => {
            log::debug!(
                "COM1 capture for VM \"{}\" finished; output is in {}",
                options.vm_name,
                options.log_path.display()
            );
            Ok(())
        }
        Err(error) => {
            let _ = failed.signal();
            log::error!(
                "COM1 capture for VM \"{}\" failed: {error}",
                options.vm_name
            );
            Err(error)
        }
    }
}

/// Acquires everything the capture needs, announces readiness, then reads.
fn capture(
    options: &Com1HelperOptions,
    cancel: &WindowsEvent,
    ready: &WindowsEvent,
) -> Result<(), RepositoryError> {
    let parent = open_parent(options.parent_process_id)?;
    let mut log_file = open_log(options)?;
    let io_event = WindowsEvent::new(true, false)?;
    // Only now: a start that hears "ready" must know the log is open and the
    // helper is able to notice cancellation.
    ready.signal()?;
    // Before the pipe, not after: the point of the replay is that the window
    // has something in it while the connection is still being made.
    replay_tail(options);
    log::debug!(
        "COM1 reader for VM \"{}\" is waiting for {}",
        options.vm_name,
        options.pipe_path.display()
    );

    let Some(pipe) = connect(options, cancel, &parent)? else {
        return Ok(());
    };
    log::debug!("COM1 reader for VM \"{}\" is connected", options.vm_name);

    // The pipe is shared, not handed over: the input thread outlives this call
    // by design, and its `Arc` keeps the handle open until the process exits.
    let pipe = Arc::new(pipe);
    // Held until the capture ends, and dropped on every path out of it: the
    // window the helper leaves behind must type and echo as it did before.
    let _console = ConsoleModes::enter_raw();
    start_input(Arc::clone(&pipe), options.vm_name.clone())?;

    read_until_closed(&pipe, &io_event, cancel, &parent, &mut log_file, options)
}

/// Opens the pipe, retrying while it does not yet exist or is still busy.
///
/// `Ok(None)` means the wait ended because the parent cancelled or went away.
fn connect(
    options: &Com1HelperOptions,
    cancel: &WindowsEvent,
    parent: &OwnedHandle,
) -> Result<Option<Com1Pipe>, RepositoryError> {
    let wide_path = HSTRING::from(options.pipe_path.as_os_str().to_string_lossy().as_ref());
    let poll_ms = CONNECT_POLL.as_millis() as u32;

    loop {
        if cancel.is_signaled()? || has_exited(parent)? {
            return Ok(None);
        }

        // SAFETY: `wide_path` outlives the call, and the returned handle is
        // owned by `OwnedHandle` and closed exactly once.
        let opened = unsafe {
            CreateFileW(
                &wide_path,
                PIPE_ACCESS,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                None,
            )
        };
        match opened {
            Ok(handle) => return Ok(Some(Com1Pipe(handle))),
            Err(error) if is_worth_retrying(&error) => {
                // SAFETY: `wide_path` outlives the call; the result only says
                // whether an instance became free within the interval, and
                // either answer leads back to the checks above.
                let _ = unsafe { WaitNamedPipeW(&wide_path, poll_ms) };
                if !was_waited_for(&error) {
                    std::thread::sleep(CONNECT_POLL);
                }
            }
            Err(error) => {
                return Err(windows_error(
                    "open COM1 named pipe",
                    Some(&options.vm_name),
                    error,
                ));
            }
        }
    }
}

/// A pipe that does not exist yet is normal: HCS creates it when the compute
/// system starts, which is after the reader is launched.
fn is_worth_retrying(error: &Error) -> bool {
    let code = error.code();
    code == ERROR_FILE_NOT_FOUND.to_hresult() || code == ERROR_PIPE_BUSY.to_hresult()
}

/// `WaitNamedPipeW` only waits for a pipe that already exists; for one that
/// does not, the caller has to pace itself.
fn was_waited_for(error: &Error) -> bool {
    error.code() == ERROR_PIPE_BUSY.to_hresult()
}

/// Reads the pipe until it closes, cancellation arrives, or the parent exits.
fn read_until_closed(
    pipe: &Com1Pipe,
    io_event: &WindowsEvent,
    cancel: &WindowsEvent,
    parent: &OwnedHandle,
    log_file: &mut File,
    options: &Com1HelperOptions,
) -> Result<(), RepositoryError> {
    let mut buffer = [0u8; READ_BUFFER_BYTES];
    let stdout = io::stdout();

    loop {
        let mut overlapped = OVERLAPPED {
            hEvent: io_event.raw_handle(),
            ..Default::default()
        };
        // SAFETY: `buffer` and `overlapped` outlive the operation: either it
        // completes below, or `abandon_read` cancels it and waits for the
        // completion before this frame is left.
        let started = unsafe {
            ReadFile(
                pipe.raw(),
                Some(&mut buffer),
                None,
                Some(&raw mut overlapped),
            )
        };
        match started {
            Ok(()) => {}
            Err(error) if error.code() == ERROR_IO_PENDING.to_hresult() => {
                match wait_for_read(io_event, cancel, parent)? {
                    ReadOutcome::Completed => {}
                    ReadOutcome::Abandoned => {
                        abandon_read(pipe, &overlapped);
                        return Ok(());
                    }
                }
            }
            Err(error) if is_end_of_stream(&error) => return Ok(()),
            Err(error) => {
                return Err(windows_error(
                    "read COM1 named pipe",
                    Some(&options.vm_name),
                    error,
                ));
            }
        }

        let mut transferred = 0_u32;
        // SAFETY: `overlapped` describes the operation started above and is
        // still alive; the read has already completed, so this does not block.
        let completed = unsafe {
            GetOverlappedResult(
                pipe.raw(),
                &raw const overlapped,
                &raw mut transferred,
                true,
            )
        };
        match completed {
            Ok(()) => {}
            Err(error) if is_end_of_stream(&error) => return Ok(()),
            Err(error) => {
                return Err(windows_error(
                    "complete COM1 read",
                    Some(&options.vm_name),
                    error,
                ));
            }
        }

        let bytes = &buffer[..transferred as usize];
        if !bytes.is_empty() {
            let mut terminal = stdout.lock();
            mirror_chunk(bytes, log_file, &mut terminal).map_err(|error| {
                RepositoryError::new(format!(
                    "failed to write COM1 output for VM \"{}\": {error}",
                    options.vm_name
                ))
            })?;
        }
    }
}

/// Why a pending read stopped being interesting.
enum ReadOutcome {
    Completed,
    Abandoned,
}

/// Waits for the read, for cancellation, or for the parent to disappear.
fn wait_for_read(
    io_event: &WindowsEvent,
    cancel: &WindowsEvent,
    parent: &OwnedHandle,
) -> Result<ReadOutcome, RepositoryError> {
    let handles = [io_event.raw_handle(), cancel.raw_handle(), parent.0];
    // SAFETY: every handle is owned by a live wrapper for the duration of the
    // wait.
    match unsafe { WaitForMultipleObjects(&handles, false, u32::MAX) } {
        result if result == WAIT_OBJECT_0 => Ok(ReadOutcome::Completed),
        result if result.0 == WAIT_OBJECT_0.0 + 1 || result.0 == WAIT_OBJECT_0.0 + 2 => {
            Ok(ReadOutcome::Abandoned)
        }
        WAIT_FAILED => Err(windows_error(
            "wait for COM1 read",
            None,
            Error::from_win32(),
        )),
        result => Err(RepositoryError::new(format!(
            "Windows API operation \"wait for COM1 read\" returned unexpected status {}",
            result.0
        ))),
    }
}

/// Cancels a read that is still pending and waits for the kernel to give the
/// buffer back, so that leaving this frame cannot free memory the kernel still
/// writes to.
fn abandon_read(pipe: &Com1Pipe, overlapped: &OVERLAPPED) {
    // SAFETY: `pipe` is open and `overlapped` describes an operation issued on
    // it by this thread.
    let _ = unsafe { CancelIoEx(pipe.raw(), Some(overlapped)) };
    let mut transferred = 0_u32;
    // SAFETY: as above; `true` waits for the cancelled operation to finish.
    let _ = unsafe { GetOverlappedResult(pipe.raw(), overlapped, &raw mut transferred, true) };
}

/// The ways a pipe stops delivering that are not failures: the guest closed it,
/// the compute system it belonged to went away, or this process cancelled the
/// read.
///
/// `ERROR_PIPE_NOT_CONNECTED` and `ERROR_NO_DATA` are in this list because they
/// are what a VM being stopped looks like from here: HCS tears the serving end
/// down under a pending read, and that read completes with one of them rather
/// than with the broken pipe a guest-side close produces. Treating them as
/// failures made every force-stop tell the user that COM1 had stopped
/// unexpectedly.
fn is_end_of_stream(error: &Error) -> bool {
    let code = error.code();
    code == ERROR_BROKEN_PIPE.to_hresult()
        || code == ERROR_PIPE_NOT_CONNECTED.to_hresult()
        || code == ERROR_NO_DATA.to_hresult()
        || code == ERROR_OPERATION_ABORTED.to_hresult()
        || code == ERROR_IO_INCOMPLETE.to_hresult()
}

/// Opens the VMLord process the helper belongs to, so that its exit can be
/// waited on.
fn open_parent(parent_process_id: u32) -> Result<OwnedHandle, RepositoryError> {
    // SAFETY: the returned handle is owned by `OwnedHandle` and closed once.
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, parent_process_id) }
        .map_err(|error| windows_error("open VMLord process", None, error))?;
    Ok(OwnedHandle(handle))
}

/// Reports whether a process has already exited, without blocking.
fn has_exited(process: &OwnedHandle) -> Result<bool, RepositoryError> {
    // SAFETY: `process` owns a live handle for the duration of the call.
    match unsafe { WaitForSingleObject(process.0, 0) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        WAIT_FAILED => Err(windows_error(
            "wait for VMLord process",
            None,
            Error::from_win32(),
        )),
        result => Err(RepositoryError::new(format!(
            "Windows API operation \"wait for VMLord process\" returned unexpected status {}",
            result.0
        ))),
    }
}

/// The COM1 pipe, owned by this module and closed exactly once.
///
/// There is one handle and there can only be one: HCS serves a single instance,
/// so a second open would either be refused as busy or take the stream away
/// from the reader. The capture loop and the input thread therefore share this
/// one, each with its own `OVERLAPPED` and event -- which is what overlapped
/// I/O is for.
pub(crate) struct Com1Pipe(HANDLE);

// SAFETY: a file handle is valid process-wide and is not owned by the thread
// that opened it. Concurrent overlapped `ReadFile` and `WriteFile` on one handle
// are supported as long as each carries its own `OVERLAPPED`, which is how the
// capture loop and the input thread use it. The handle is closed in `Drop`, and
// the input thread holds an `Arc` clone, so it cannot be closed under a write.
unsafe impl Send for Com1Pipe {}
// SAFETY: as above; every operation this type exposes takes `&self` and passes
// the handle to a Win32 call that is safe to issue from several threads.
unsafe impl Sync for Com1Pipe {}

impl Com1Pipe {
    pub(crate) fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for Com1Pipe {
    fn drop(&mut self) {
        // SAFETY: the handle came from the successful `CreateFileW` in
        // `connect` and is closed only here.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// A kernel handle this module owns and closes exactly once.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful open in this module and is
        // closed only here.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Signals an event when dropped, on every path out of a scope.
struct SignalOnDrop<'a>(&'a WindowsEvent);

impl Drop for SignalOnDrop<'_> {
    fn drop(&mut self) {
        let _ = self.0.signal();
    }
}

/// Shows the end of `com1.log` in the window before the live stream starts.
///
/// Only for a console that is joining a boot already in progress. A truncating
/// start has no history by definition -- it is about to throw the previous
/// boot's away -- and the guest is about to print this one from its first line.
///
/// Nothing here can fail the capture: the reason this window exists is the
/// bytes the guest is about to write, not the ones it already has.
fn replay_tail(options: &Com1HelperOptions) {
    if options.log_mode != Com1LogMode::Append {
        return;
    }
    let history = match read_tail(&options.log_path, REPLAY_TAIL_BYTES) {
        Ok(history) => history,
        Err(error) => {
            log::warn!(
                "the COM1 console of VM \"{}\" opens without history: {} could not be read: \
                 {error}",
                options.vm_name,
                options.log_path.display()
            );
            return;
        }
    };
    let tail = without_terminal_queries(tail_from(&history, REPLAY_TAIL_BYTES));
    if tail.is_empty() {
        return;
    }

    let mut stdout = io::stdout().lock();
    // The banner is written to the window and never to the log: it says where
    // the record ends and the live guest begins, and it is not something the
    // guest said.
    let banner = format!(
        "--- VMLord: {} earlier byte(s) from {}; live output follows ---\r\n",
        tail.len(),
        options.log_path.display()
    );
    // Bytes unchanged, as everywhere else in this reader: what is replayed is
    // what the guest wrote, including its control characters.
    if let Err(error) = stdout
        .write_all(banner.as_bytes())
        .and_then(|()| stdout.write_all(&tail))
        .and_then(|()| stdout.flush())
    {
        log::warn!(
            "could not replay the COM1 history of VM \"{}\": {error}",
            options.vm_name
        );
        return;
    }
    log::debug!(
        "the COM1 console of VM \"{}\" opened with {} byte(s) of history",
        options.vm_name,
        tail.len()
    );
}

/// Drops the sequences that ask the terminal a question.
///
/// Replaying history is not the same as receiving it. A boot log carries the
/// probes the guest's own tools made -- `ESC[6n` to find out where the cursor
/// is, `ESC[c` to ask what the terminal is -- and a terminal answers those on
/// its *input*, which for this window is the helper's stdin, which goes
/// straight into the guest's tty. Replayed unfiltered, the history makes the
/// terminal type `^[[30;1R` at the login prompt of a guest that asked nothing.
///
/// Only the replay is filtered. A live guest that asks gets its answer: it did
/// ask, and that is what a serial terminal is for.
///
/// Everything that merely paints -- colours, cursor movement, erasures -- is
/// kept, because history that has lost its formatting is history that is hard
/// to read.
fn without_terminal_queries(history: &[u8]) -> Vec<u8> {
    const ESCAPE: u8 = 0x1b;
    const BELL: u8 = 0x07;

    let mut kept = Vec::with_capacity(history.len());
    let mut rest = history;
    while let Some(start) = rest.iter().position(|byte| *byte == ESCAPE) {
        kept.extend_from_slice(&rest[..start]);
        let sequence = &rest[start..];
        let Some((length, answers)) = measure_escape(sequence, ESCAPE, BELL) else {
            // The log ends in the middle of a sequence: dropping it is what
            // keeps the terminal from applying it to the live output that
            // follows, which arrives from a different moment entirely.
            return kept;
        };
        if !answers {
            kept.extend_from_slice(&sequence[..length]);
        }
        rest = &sequence[length..];
    }
    kept.extend_from_slice(rest);
    kept
}

/// How long the escape sequence at the start of `bytes` is, and whether a
/// terminal answers it.
///
/// `None` means the sequence is unfinished, which at the end of a log means it
/// was cut off rather than that more is coming.
fn measure_escape(bytes: &[u8], escape: u8, bell: u8) -> Option<(usize, bool)> {
    match bytes.get(1)? {
        // CSI: parameters and intermediates, then one final byte that says what
        // it was. `n` is a status report and `c` is a device attributes
        // request; both are questions.
        b'[' => {
            let final_byte = bytes
                .iter()
                .enumerate()
                .skip(2)
                .find(|(_, byte)| (0x40..=0x7e).contains(*byte))?;
            let (index, byte) = final_byte;
            Some((index + 1, matches!(byte, b'n' | b'c')))
        }
        // OSC: ends at a bell or a string terminator. A `?` in one is a query
        // -- "what is your background colour" and its like.
        b']' => {
            let end = terminated_string(bytes, escape, bell)?;
            Some((end, bytes[..end].contains(&b'?')))
        }
        // DCS: nothing in a boot log needs one, and the ones that exist are
        // requests for the terminal's settings.
        b'P' => Some((terminated_string(bytes, escape, bell)?, true)),
        // The obsolete "identify terminal", answered like a device attributes
        // request.
        b'Z' => Some((2, true)),
        // Everything else is two bytes and paints something.
        _ => Some((2, false)),
    }
}

/// Where the string-terminated sequence at the start of `bytes` ends.
fn terminated_string(bytes: &[u8], escape: u8, bell: u8) -> Option<usize> {
    let mut index = 2;
    while index < bytes.len() {
        if bytes[index] == bell {
            return Some(index + 1);
        }
        if bytes[index] == escape && bytes.get(index + 1) == Some(&b'\\') {
            return Some(index + 2);
        }
        index += 1;
    }
    None
}

/// Reads at most the last `limit` bytes of a file, and one more.
///
/// The extra byte is what tells a whole short log from a long one cut down to
/// size: with it, [`tail_from`] can see that it is holding a window rather than
/// a file, and trim the half line at its top.
fn read_tail(path: &Path, limit: usize) -> io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let from = length.saturating_sub(limit as u64 + 1);
    if from > 0 {
        file.seek(SeekFrom::Start(from))?;
    }
    let mut window = Vec::new();
    file.read_to_end(&mut window)?;
    Ok(window)
}

/// The last `limit` bytes of `history`, starting at a line boundary.
///
/// Cutting a line in half would put half a message at the top of the window and
/// leave any escape sequence it was in the middle of unterminated, which the
/// terminal would then apply to everything after it.
fn tail_from(history: &[u8], limit: usize) -> &[u8] {
    if history.len() <= limit {
        return history;
    }
    let window = &history[history.len() - limit..];
    match window.iter().position(|byte| *byte == b'\n') {
        Some(newline) => &window[newline + 1..],
        // A window with no line break at all is one long line; showing it from
        // where it happens to start is better than showing nothing.
        None => window,
    }
}

/// Opens `com1.log` the way `mode` asks for.
fn open_log(options: &Com1HelperOptions) -> Result<File, RepositoryError> {
    let mut open = OpenOptions::new();
    open.create(true).write(true);
    match options.log_mode {
        Com1LogMode::Truncate => open.truncate(true),
        Com1LogMode::Append => open.append(true),
    };
    open.open(&options.log_path).map_err(|error| {
        let error = RepositoryError::new(format!(
            "failed to open COM1 log {} for VM \"{}\": {error}",
            options.log_path.display(),
            options.vm_name
        ));
        log::error!("{error}");
        error
    })
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use windows::{
        Win32::Foundation::{
            ERROR_ACCESS_DENIED, ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_NOT_CONNECTED,
        },
        core::Error,
    };

    use super::{
        Com1LogMode, PIPE_ACCESS, REPLAY_TAIL_BYTES, is_end_of_stream, mirror_chunk,
        parse_com1_helper_args, read_tail, tail_from, without_terminal_queries,
    };

    fn arguments() -> Vec<OsString> {
        [
            "--pipe",
            r"\\.\pipe\vmlord-test.com1",
            "--log",
            r"C:\vms\dev\com1.log",
            "--mode",
            "truncate",
            "--parent-pid",
            "42",
            "--cancel-event",
            r"Local\VMLord.Com1.cancel.test",
            "--ready-event",
            r"Local\VMLord.Com1.ready.test",
            "--failed-event",
            r"Local\VMLord.Com1.failed.test",
            "--finished-event",
            r"Local\VMLord.Com1.finished.test",
            "--alive-event",
            r"Local\VMLord.Com1.alive.test",
            "--vm-name",
            "dev",
        ]
        .map(OsString::from)
        .to_vec()
    }

    #[test]
    fn parses_every_non_secret_helper_argument() {
        let options = parse_com1_helper_args(arguments()).unwrap();

        assert_eq!(options.log_mode, Com1LogMode::Truncate);
        assert_eq!(options.parent_process_id, 42);
        assert_eq!(options.vm_name, "dev");
    }

    #[test]
    fn a_missing_argument_is_named_in_the_error() {
        let mut arguments = arguments();
        arguments.drain(0..2);

        let message = parse_com1_helper_args(arguments).unwrap_err().to_string();

        assert!(message.contains("--pipe"), "{message}");
    }

    #[test]
    fn an_unknown_mode_is_named_in_the_error() {
        let mut arguments = arguments();
        arguments[5] = OsString::from("overwrite");

        let message = parse_com1_helper_args(arguments).unwrap_err().to_string();

        assert!(message.contains("--mode"), "{message}");
        assert!(message.contains("overwrite"), "{message}");
    }

    #[test]
    fn a_repeated_or_unknown_flag_is_rejected() {
        let mut repeated = arguments();
        repeated.extend(["--vm-name", "other"].map(OsString::from));
        let mut unknown = arguments();
        unknown.extend(["--password", "secret"].map(OsString::from));

        assert!(
            parse_com1_helper_args(repeated)
                .unwrap_err()
                .to_string()
                .contains("--vm-name")
        );
        assert!(
            parse_com1_helper_args(unknown)
                .unwrap_err()
                .to_string()
                .contains("--password")
        );
    }

    #[test]
    fn a_parent_that_cannot_be_waited_on_is_rejected() {
        let mut arguments = arguments();
        arguments[7] = OsString::from("0");

        assert!(parse_com1_helper_args(arguments).is_err());
    }

    #[test]
    fn a_vm_being_stopped_ends_the_capture_rather_than_failing_it() {
        // Observed on a Hyper-V host: stopping the VM completes the pending
        // read with ERROR_PIPE_NOT_CONNECTED, not with a broken pipe. Reported
        // as a failure, it told the user COM1 had stopped unexpectedly every
        // time they stopped a VM.
        for expected in [ERROR_PIPE_NOT_CONNECTED, ERROR_NO_DATA, ERROR_BROKEN_PIPE] {
            assert!(
                is_end_of_stream(&Error::from_hresult(expected.to_hresult())),
                "{expected:?} is how a stopped VM ends a capture"
            );
        }
        assert!(
            !is_end_of_stream(&Error::from_hresult(ERROR_ACCESS_DENIED.to_hresult())),
            "a reader that cannot read has not reached the end of anything"
        );
    }

    /// The console is two-way, and the only thing that makes it so is the
    /// access this open asks for. A read-only mask compiles, runs, and silently
    /// turns every keystroke into ERROR_ACCESS_DENIED.
    #[test]
    fn the_pipe_is_opened_for_writing_as_well_as_reading() {
        use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};

        assert_eq!(PIPE_ACCESS & GENERIC_WRITE.0, GENERIC_WRITE.0);
        assert_eq!(PIPE_ACCESS & GENERIC_READ.0, GENERIC_READ.0);
    }

    /// One handle is shared by the capture loop and the input thread, so the
    /// type that owns it has to be sendable and shareable.
    #[test]
    fn the_pipe_handle_can_be_shared_with_another_thread() {
        fn assert_shareable<T: Send + Sync>() {}

        assert_shareable::<super::Com1Pipe>();
    }

    #[test]
    fn mirrors_serial_bytes_without_utf8_conversion() {
        // A serial console carries whatever the guest wrote: partial UTF-8
        // sequences, control bytes, NULs. Anything that decodes and re-encodes
        // them turns a diagnostic into a guess.
        let bytes = b"cloud-init\r\n\xffkernel\0";
        let mut log = Vec::new();
        let mut terminal = Vec::new();

        mirror_chunk(bytes, &mut log, &mut terminal).unwrap();

        assert_eq!(log, bytes);
        assert_eq!(terminal, bytes);
    }

    /// What a reopened console shows: a guest sitting at `login:` printed that
    /// prompt once, minutes ago, and will not print it again unaided. Without
    /// the replay the window is empty and the VM looks dead.
    #[test]
    fn a_history_shorter_than_the_limit_is_replayed_whole() {
        let history = b"[  OK  ] Reached target Multi-User System.\r\ntest login: ";

        assert_eq!(tail_from(history, REPLAY_TAIL_BYTES), history);
    }

    #[test]
    fn a_long_history_is_cut_back_to_a_line_boundary() {
        let history = b"first line\nsecond line\nthird line\n";

        // A limit that lands inside "second line" must not put half of it at
        // the top of the window.
        assert_eq!(tail_from(history, 20), b"third line\n");
    }

    #[test]
    fn one_long_line_is_shown_rather_than_nothing() {
        let history = b"aaaaaaaaaaaaaaaaaaaa";

        assert_eq!(tail_from(history, 5), b"aaaaa");
    }

    #[test]
    fn an_empty_log_replays_nothing() {
        assert!(tail_from(b"", REPLAY_TAIL_BYTES).is_empty());
    }

    /// The read has to be a seek rather than a load: a `com1.log` is the whole
    /// output of a boot, and only its end is worth showing.
    #[test]
    fn only_the_end_of_a_long_log_is_read_and_it_starts_on_a_line() {
        let path = std::env::temp_dir().join(format!("vmlord-com1-tail-{}", std::process::id()));
        let mut written = Vec::new();
        for line in 0..500 {
            written.extend_from_slice(format!("line {line} of an old boot\r\n").as_bytes());
        }
        written.extend_from_slice(b"test login: ");
        std::fs::write(&path, &written).unwrap();

        let window = read_tail(&path, 200).unwrap();
        let tail = tail_from(&window, 200);

        let _ = std::fs::remove_file(&path);
        assert!(window.len() <= 201, "read {} bytes", window.len());
        assert!(
            tail.ends_with(b"test login: "),
            "the prompt a person reopened the console for must be there: {}",
            String::from_utf8_lossy(tail)
        );
        assert!(
            tail.starts_with(b"line "),
            "the window must open on a whole line: {}",
            String::from_utf8_lossy(tail)
        );
    }

    #[test]
    fn a_log_shorter_than_the_window_is_read_whole() {
        let path =
            std::env::temp_dir().join(format!("vmlord-com1-tail-short-{}", std::process::id()));
        std::fs::write(&path, b"test login: ").unwrap();

        let window = read_tail(&path, REPLAY_TAIL_BYTES).unwrap();

        let _ = std::fs::remove_file(&path);
        assert_eq!(tail_from(&window, REPLAY_TAIL_BYTES), b"test login: ");
    }

    /// The bug this covers, seen at a login prompt: the replayed history
    /// carried the cursor-position query some tool made during the boot, the
    /// terminal answered it on the helper's stdin, and the answer was typed
    /// into the guest as `^[[30;1R`.
    #[test]
    fn a_replayed_cursor_query_is_not_asked_again() {
        let history = b"\x1b[999;999H\x1b[6ntest login: ";

        let replayed = without_terminal_queries(history);

        assert_eq!(replayed, b"\x1b[999;999Htest login: ");
    }

    #[test]
    fn a_replayed_device_attributes_request_is_dropped_in_every_spelling() {
        for query in [
            &b"\x1b[c"[..],
            b"\x1b[>c",
            b"\x1b[=0c",
            b"\x1b[?6n",
            b"\x1bZ",
        ] {
            let mut history = b"before".to_vec();
            history.extend_from_slice(query);
            history.extend_from_slice(b"after");

            assert_eq!(
                without_terminal_queries(&history),
                b"beforeafter",
                "{query:?} is a question the terminal answers"
            );
        }
    }

    /// History that has lost its formatting is history that is hard to read:
    /// only the questions go.
    #[test]
    fn replayed_colour_and_movement_survive() {
        let history = b"\x1b[32m[  OK  ]\x1b[0m Reached target\r\n\x1b[?25htest login: ";

        assert_eq!(without_terminal_queries(history), history);
    }

    #[test]
    fn a_replayed_window_title_survives_but_a_colour_question_does_not() {
        let title = b"\x1b]0;a title\x07";
        let query = b"\x1b]11;?\x1b\\";
        let mut history = title.to_vec();
        history.extend_from_slice(query);

        assert_eq!(without_terminal_queries(&history), title);
    }

    /// A log can end anywhere, including inside a sequence. Passing half of one
    /// on would leave the terminal applying it to the live output that follows.
    #[test]
    fn an_unfinished_sequence_at_the_end_is_dropped() {
        assert_eq!(
            without_terminal_queries(b"test login: \x1b[3"),
            b"test login: "
        );
    }

    #[test]
    fn plain_history_is_replayed_unchanged() {
        let history = b"Ubuntu 26.04 LTS test ttyS0\r\n\r\ntest login: ";

        assert_eq!(without_terminal_queries(history), history);
    }
}
