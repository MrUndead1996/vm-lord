//! The keyboard half of the COM1 console.
//!
//! The capture loop in [`crate::com1_reader`] carries bytes out of the guest;
//! this module carries them in. It is a separate thread because a console read
//! blocks, and a separate module because raw mode, an overlapped write and a
//! blocking pump have nothing to do with capturing.

use std::{
    io::{self, Read, Write},
    sync::Arc,
};

use vmlord_core::RepositoryError;
use windows::{
    Win32::{
        Foundation::{
            ERROR_BROKEN_PIPE, ERROR_IO_INCOMPLETE, ERROR_IO_PENDING, ERROR_NO_DATA,
            ERROR_OPERATION_ABORTED, ERROR_PIPE_NOT_CONNECTED, HANDLE,
        },
        Storage::FileSystem::WriteFile,
        System::{
            Console::{
                CONSOLE_MODE, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
                ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode,
                GetStdHandle, STD_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleMode,
            },
            IO::{GetOverlappedResult, OVERLAPPED},
        },
    },
    core::Error,
};

use crate::{com1_reader::Com1Pipe, event::WindowsEvent};

/// How much typing one read may return.
///
/// Small on purpose: this is a person at a keyboard, and a paste is delivered
/// in as many chunks as it takes.
const INPUT_BUFFER_BYTES: usize = 512;

/// Copies everything `input` produces into `pipe`, until `input` ends.
///
/// Byte for byte: what a serial console carries is not text, and a pump that
/// decoded it would turn Ctrl-C into a character and a password into a guess.
/// Nothing here writes to `com1.log` -- the guest echoes what it wants echoed,
/// and a password is deliberately not echoed.
pub(crate) fn pump_input(input: &mut impl Read, pipe: &mut impl Write) -> io::Result<()> {
    let mut buffer = [0u8; INPUT_BUFFER_BYTES];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            return Ok(());
        }
        pipe.write_all(&buffer[..read])?;
        pipe.flush()?;
    }
}

/// Writes to the COM1 pipe with the overlapped I/O the handle was opened for.
pub(crate) struct PipeWriter {
    pipe: Arc<Com1Pipe>,
    event: WindowsEvent,
}

impl Write for PipeWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let mut overlapped = OVERLAPPED {
            hEvent: self.event.raw_handle(),
            ..Default::default()
        };
        // SAFETY: `buffer` and `overlapped` outlive the operation: the
        // `GetOverlappedResult` below waits for it to finish before this frame
        // is left.
        let started = unsafe {
            WriteFile(
                self.pipe.raw(),
                Some(buffer),
                None,
                Some(&raw mut overlapped),
            )
        };
        match started {
            Ok(()) => {}
            Err(error) if error.code() == ERROR_IO_PENDING.to_hresult() => {}
            Err(error) => return Err(io::Error::from_raw_os_error(win32_code(&error))),
        }

        let mut written = 0_u32;
        // SAFETY: `overlapped` describes the write started above and is still
        // alive; `true` waits for that write to complete.
        unsafe {
            GetOverlappedResult(
                self.pipe.raw(),
                &raw const overlapped,
                &raw mut written,
                true,
            )
        }
        .map_err(|error| io::Error::from_raw_os_error(win32_code(&error)))?;
        Ok(written as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The Win32 error code inside a `windows` error, so that it survives the trip
/// through `io::Error`.
pub(crate) fn win32_code(error: &Error) -> i32 {
    let code = error.code().0 as u32;
    // HRESULT_FROM_WIN32: facility 7, severity set.
    if code & 0xFFFF_0000 == 0x8007_0000 {
        (code & 0xFFFF) as i32
    } else {
        code as i32
    }
}

/// The ways a write stops arriving that are not failures: the guest closed the
/// pipe, the VM was stopped, or the operation was cancelled.
///
/// The same list `crate::com1_reader::is_end_of_stream` applies to reads; a
/// stopped VM ends both directions at once.
pub(crate) fn is_end_of_stream_io(error: &io::Error) -> bool {
    let ends = [
        ERROR_BROKEN_PIPE.0,
        ERROR_PIPE_NOT_CONNECTED.0,
        ERROR_NO_DATA.0,
        ERROR_OPERATION_ABORTED.0,
        ERROR_IO_INCOMPLETE.0,
    ];
    matches!(error.raw_os_error(), Some(code) if ends.contains(&(code as u32)))
}

/// The input mode a console has to be in for a serial console to work.
pub(crate) fn raw_input_mode(original: CONSOLE_MODE) -> CONSOLE_MODE {
    CONSOLE_MODE(
        (original.0 & !(ENABLE_LINE_INPUT.0 | ENABLE_ECHO_INPUT.0 | ENABLE_PROCESSED_INPUT.0))
            | ENABLE_VIRTUAL_TERMINAL_INPUT.0,
    )
}

/// The output mode that lets what the guest draws arrive as drawing rather than
/// as escape codes.
pub(crate) fn vt_output_mode(original: CONSOLE_MODE) -> CONSOLE_MODE {
    CONSOLE_MODE(original.0 | ENABLE_VIRTUAL_TERMINAL_PROCESSING.0)
}

/// The console modes the helper changed, restored when it is done.
///
/// A helper that exits leaving line input off would hand the user back a
/// terminal that no longer echoes what they type, so the restore has to happen
/// on every path out -- including a panic, which is what `Drop` gives.
pub(crate) struct ConsoleModes {
    changed: Vec<(HANDLE, CONSOLE_MODE)>,
}

impl ConsoleModes {
    /// Takes the console out of cooked mode, as far as this console allows.
    ///
    /// A standard handle that is not a console -- output redirected to a file,
    /// input from a pipe -- is left alone: there is no mode to change, and the
    /// bytes still travel.
    pub(crate) fn enter_raw() -> Self {
        let mut changed = Vec::new();
        for (which, mode_of) in [
            (
                STD_INPUT_HANDLE,
                raw_input_mode as fn(CONSOLE_MODE) -> CONSOLE_MODE,
            ),
            (STD_OUTPUT_HANDLE, vt_output_mode),
        ] {
            match set_console_mode(which, mode_of) {
                Some(previous) => changed.push(previous),
                None => tracing::debug!(
                    "COM1 console standard handle {} is not a console; leaving its mode alone",
                    which.0
                ),
            }
        }
        Self { changed }
    }
}

impl Drop for ConsoleModes {
    fn drop(&mut self) {
        for (handle, original) in self.changed.drain(..) {
            // SAFETY: `handle` is a standard handle this process owns for its
            // lifetime, and `original` is the mode read from it.
            if let Err(error) = unsafe { SetConsoleMode(handle, original) } {
                tracing::warn!("could not restore the COM1 console mode: {error}");
            }
        }
    }
}

/// Applies `mode_of` to one standard handle, returning what it replaced.
fn set_console_mode(
    which: STD_HANDLE,
    mode_of: fn(CONSOLE_MODE) -> CONSOLE_MODE,
) -> Option<(HANDLE, CONSOLE_MODE)> {
    // SAFETY: a standard handle is owned by the process and is not closed here.
    let handle = unsafe { GetStdHandle(which) }.ok()?;
    let mut original = CONSOLE_MODE::default();
    // SAFETY: `handle` is valid and `original` outlives the call. A failure
    // means the handle is not a console, which is not an error here.
    unsafe { GetConsoleMode(handle, &raw mut original) }.ok()?;
    // SAFETY: as above.
    unsafe { SetConsoleMode(handle, mode_of(original)) }.ok()?;
    Some((handle, original))
}

/// Starts the thread that carries this window's keyboard into the guest.
///
/// The thread is never joined, and that is deliberate: a blocking console read
/// cannot be woken. Waiting on the console input handle instead does not help,
/// because that handle is also signaled by focus, mouse and buffer-resize
/// records, which `ReadFile` discards before blocking again. So the thread stays
/// blocked and process exit collects it -- `run_com1_helper` returns, `main`
/// returns, and `ExitProcess` takes every thread with it.
///
/// What that costs is one invariant: the pipe must not be closed while this
/// thread might still write to it. The `Arc` is what pays it.
pub(crate) fn start_input(pipe: Arc<Com1Pipe>, vm_name: String) -> Result<(), RepositoryError> {
    let event = WindowsEvent::new(true, false)?;
    tracing::debug!("COM1 input for VM \"{vm_name}\" is being forwarded to the guest");
    std::thread::Builder::new()
        .name("vmlord-com1-input".to_owned())
        .spawn(move || {
            let mut writer = PipeWriter { pipe, event };
            let stdin = io::stdin();
            match pump_input(&mut stdin.lock(), &mut writer) {
                Ok(()) => {
                    tracing::debug!(
                        "COM1 input for VM \"{vm_name}\" ended with its standard input"
                    );
                }
                Err(error) if is_end_of_stream_io(&error) => {
                    tracing::debug!("COM1 input for VM \"{vm_name}\" ended with the pipe");
                }
                Err(error) => {
                    tracing::warn!("COM1 input for VM \"{vm_name}\" stopped: {error}");
                }
            }
        })
        .map_err(|error| {
            let error =
                RepositoryError::new(format!("cannot start the COM1 input thread: {error}"));
            tracing::error!("{error}");
            error
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use windows::{
        Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_BROKEN_PIPE, ERROR_PIPE_NOT_CONNECTED},
        core::Error,
    };

    use super::{is_end_of_stream_io, pump_input, raw_input_mode, vt_output_mode, win32_code};

    /// Raw mode is what makes the console usable: line input would hold every
    /// keystroke until Enter, echo would double what the guest already echoes
    /// and would show a password, and processed input would keep Ctrl-C for the
    /// helper instead of passing it to the guest. Virtual terminal input is what
    /// turns arrows and function keys into the sequences a Linux tty expects.
    #[test]
    fn raw_mode_frees_the_keyboard_and_leaves_nothing_else_touched() {
        use windows::Win32::System::Console::{
            CONSOLE_MODE, ENABLE_ECHO_INPUT, ENABLE_EXTENDED_FLAGS, ENABLE_LINE_INPUT,
            ENABLE_PROCESSED_INPUT, ENABLE_VIRTUAL_TERMINAL_INPUT,
        };

        let original = CONSOLE_MODE(
            ENABLE_LINE_INPUT.0
                | ENABLE_ECHO_INPUT.0
                | ENABLE_PROCESSED_INPUT.0
                | ENABLE_EXTENDED_FLAGS.0,
        );

        let raw = raw_input_mode(original);

        for cooked in [ENABLE_LINE_INPUT, ENABLE_ECHO_INPUT, ENABLE_PROCESSED_INPUT] {
            assert_eq!(raw.0 & cooked.0, 0, "{cooked:?} must be off in raw mode");
        }
        assert_eq!(
            raw.0 & ENABLE_VIRTUAL_TERMINAL_INPUT.0,
            ENABLE_VIRTUAL_TERMINAL_INPUT.0
        );
        assert_eq!(
            raw.0 & ENABLE_EXTENDED_FLAGS.0,
            ENABLE_EXTENDED_FLAGS.0,
            "a flag the helper has no opinion about stays as the user set it"
        );
    }

    /// Without virtual terminal processing the colors and cursor movement of
    /// cloud-init output and of any full-screen program in the guest arrive as
    /// escape codes on screen.
    #[test]
    fn output_gains_virtual_terminal_processing_and_keeps_the_rest() {
        use windows::Win32::System::Console::{
            CONSOLE_MODE, ENABLE_PROCESSED_OUTPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
        };

        let original = CONSOLE_MODE(ENABLE_PROCESSED_OUTPUT.0);

        let mode = vt_output_mode(original);

        assert_eq!(
            mode.0 & ENABLE_VIRTUAL_TERMINAL_PROCESSING.0,
            ENABLE_VIRTUAL_TERMINAL_PROCESSING.0
        );
        assert_eq!(
            mode.0 & ENABLE_PROCESSED_OUTPUT.0,
            ENABLE_PROCESSED_OUTPUT.0
        );
    }

    /// Keystrokes are bytes on a wire. Ctrl-C is 0x03 and has to arrive as
    /// 0x03, backspace as 0x7f, and a typed character that is not ASCII as the
    /// UTF-8 a Linux tty expects -- nothing here decodes, normalizes or
    /// line-buffers anything.
    #[test]
    fn input_reaches_the_pipe_unchanged() {
        let typed = b"root\r\x03\x7f\x00\xffdate\r";
        let mut input = Cursor::new(typed.to_vec());
        let mut pipe = Vec::new();

        pump_input(&mut input, &mut pipe).unwrap();

        assert_eq!(pipe, typed);
    }

    /// Standard input ending is how a helper whose window is closing stops
    /// typing: an expected end, not a failure.
    #[test]
    fn a_closed_standard_input_ends_the_pump() {
        let mut input = Cursor::new(Vec::new());
        let mut pipe = Vec::new();

        pump_input(&mut input, &mut pipe).unwrap();

        assert!(pipe.is_empty());
    }

    /// A VM being stopped breaks the pipe under the input thread exactly as it
    /// does under the reader. Reported as a failure it would put a warning in
    /// the log every time a user stops a VM.
    #[test]
    fn a_broken_pipe_on_write_is_the_end_of_the_stream() {
        for expected in [ERROR_BROKEN_PIPE, ERROR_PIPE_NOT_CONNECTED] {
            let error = std::io::Error::from_raw_os_error(expected.0 as i32);
            assert!(is_end_of_stream_io(&error), "{expected:?}");
        }
        let denied = std::io::Error::from_raw_os_error(ERROR_ACCESS_DENIED.0 as i32);
        assert!(
            !is_end_of_stream_io(&denied),
            "a write that is refused has not reached the end of anything"
        );
    }

    /// Win32 errors arrive as HRESULTs from the `windows` crate and have to be
    /// carried through `io::Error` without losing the code the classification
    /// above matches on.
    #[test]
    fn a_win32_error_keeps_its_code_through_io_error() {
        let error = Error::from_hresult(ERROR_BROKEN_PIPE.to_hresult());

        assert_eq!(win32_code(&error), ERROR_BROKEN_PIPE.0 as i32);
    }
}
