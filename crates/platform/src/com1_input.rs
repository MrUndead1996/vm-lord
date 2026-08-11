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
            ERROR_OPERATION_ABORTED, ERROR_PIPE_NOT_CONNECTED,
        },
        Storage::FileSystem::WriteFile,
        System::IO::{GetOverlappedResult, OVERLAPPED},
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
    log::debug!("COM1 input for VM \"{vm_name}\" is being forwarded to the guest");
    std::thread::Builder::new()
        .name("vmlord-com1-input".to_owned())
        .spawn(move || {
            let mut writer = PipeWriter { pipe, event };
            let stdin = io::stdin();
            match pump_input(&mut stdin.lock(), &mut writer) {
                Ok(()) => {
                    log::debug!("COM1 input for VM \"{vm_name}\" ended with its standard input");
                }
                Err(error) if is_end_of_stream_io(&error) => {
                    log::debug!("COM1 input for VM \"{vm_name}\" ended with the pipe");
                }
                Err(error) => {
                    log::warn!("COM1 input for VM \"{vm_name}\" stopped: {error}");
                }
            }
        })
        .map_err(|error| {
            let error =
                RepositoryError::new(format!("cannot start the COM1 input thread: {error}"));
            log::error!("{error}");
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

    use super::{is_end_of_stream_io, pump_input, win32_code};

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
