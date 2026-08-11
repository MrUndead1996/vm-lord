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
    io::{self, Write},
    path::PathBuf,
    time::Duration,
};

use vmlord_core::RepositoryError;
use windows::{
    Win32::{
        Foundation::{
            CloseHandle, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_IO_INCOMPLETE,
            ERROR_IO_PENDING, ERROR_OPERATION_ABORTED, ERROR_PIPE_BUSY, GENERIC_READ, HANDLE,
            WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
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

use crate::{error::windows_error, event::WindowsEvent};

/// How long one connection attempt waits for the pipe to exist.
///
/// Short and repeated rather than long and single: between attempts the helper
/// has to notice cancellation and a parent that went away.
const CONNECT_POLL: Duration = Duration::from_millis(250);

/// How much guest output one read may return.
const READ_BUFFER_BYTES: usize = 4096;

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
    pub vm_name: String,
}

/// The flags [`parse_com1_helper_args`] accepts, in the order the launcher
/// writes them.
const FLAGS: [&str; 9] = [
    "--pipe",
    "--log",
    "--mode",
    "--parent-pid",
    "--cancel-event",
    "--ready-event",
    "--failed-event",
    "--finished-event",
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
    log::debug!(
        "COM1 reader for VM \"{}\" is waiting for {}",
        options.vm_name,
        options.pipe_path.display()
    );

    let Some(pipe) = connect(options, cancel, &parent)? else {
        return Ok(());
    };
    log::debug!("COM1 reader for VM \"{}\" is connected", options.vm_name);

    read_until_closed(&pipe, &io_event, cancel, &parent, &mut log_file, options)
}

/// Opens the pipe, retrying while it does not yet exist or is still busy.
///
/// `Ok(None)` means the wait ended because the parent cancelled or went away.
fn connect(
    options: &Com1HelperOptions,
    cancel: &WindowsEvent,
    parent: &OwnedHandle,
) -> Result<Option<OwnedHandle>, RepositoryError> {
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
                GENERIC_READ.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                None,
            )
        };
        match opened {
            Ok(handle) => return Ok(Some(OwnedHandle(handle))),
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
    pipe: &OwnedHandle,
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
        let started =
            unsafe { ReadFile(pipe.0, Some(&mut buffer), None, Some(&raw mut overlapped)) };
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
            GetOverlappedResult(pipe.0, &raw const overlapped, &raw mut transferred, true)
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
fn abandon_read(pipe: &OwnedHandle, overlapped: &OVERLAPPED) {
    // SAFETY: `pipe` is open and `overlapped` describes an operation issued on
    // it by this thread.
    let _ = unsafe { CancelIoEx(pipe.0, Some(overlapped)) };
    let mut transferred = 0_u32;
    // SAFETY: as above; `true` waits for the cancelled operation to finish.
    let _ = unsafe { GetOverlappedResult(pipe.0, overlapped, &raw mut transferred, true) };
}

/// The three ways a pipe stops delivering that are not failures: the guest
/// closed it, the compute system went away, or this process cancelled the read.
fn is_end_of_stream(error: &Error) -> bool {
    let code = error.code();
    code == ERROR_BROKEN_PIPE.to_hresult()
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

    use super::{Com1LogMode, mirror_chunk, parse_com1_helper_args};

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
}
