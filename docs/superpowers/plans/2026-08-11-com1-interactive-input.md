# Interactive COM1 Console Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a person type into a VM's first serial port from the COM1 console window, so that a guest with no network can still be logged into.

**Architecture:** The helper opens the existing HCS named pipe for reading and writing, keeps its capture loop exactly as it is on the main thread, and adds a detached thread that pumps its standard input into the same handle. The console is put into raw mode by a guard that restores what it found. Nothing typed is written to `com1.log`.

**Tech Stack:** Rust 2024, `windows` 0.61 Win32 APIs (`Win32_Storage_FileSystem`, `Win32_System_IO`, `Win32_System_Console`), standard-library threads and `io::Read`/`io::Write`, the existing `log` facade.

## Global Constraints

- Implement all new application code in Rust; keep Win32 calls and `unsafe` inside `vmlord-platform`.
- Do not add an async runtime or a new external crate.
- Keep secrets out of process arguments, `com1.log` and `vmlord.log`; input bytes go to the pipe and nowhere else.
- Ctrl-C reaches the guest as `0x03`; the helper gets no escape sequence of its own.
- A console whose mode cannot be read or set must never prevent capture.
- Log at `DEBUG` through `ERROR`; do not introduce `TRACE`.
- Commit subjects are `TASK-68: comment`, in English, imperative mood.
- Build and test with `cargo test -p vmlord-platform --target x86_64-pc-windows-gnu`; run `cargo fmt` and `cargo clippy --target x86_64-pc-windows-gnu` before each commit.

---

## File Structure

- Modify `crates/platform/src/com1_reader.rs`: open the pipe duplex, own the handle in a shareable `Com1Pipe`, start the input thread from `capture`.
- Create `crates/platform/src/com1_input.rs`: the input pump, the overlapped pipe writer, the raw-mode guard.
- Modify `crates/platform/src/lib.rs`: register the new module.
- Modify `crates/platform/Cargo.toml`: enable `Win32_System_Console`.
- Modify `crates/platform/tests/hyperv.rs`: add the ignored console-login scenario.
- Modify `crates/ui/src/lib.rs`: extend the empty-password hint.
- Modify `ARCHITECTURE.md`: describe the console as two-way.

---

### Task 1: Open COM1 for writing and own the handle so two threads can use it

**Files:**
- Modify: `crates/platform/src/com1_reader.rs:18-51,222-228,233-279,294-366,458-467`

**Interfaces:**
- Produces: `pub(crate) struct Com1Pipe(HANDLE)` — owns the pipe handle, closes it once, `Send + Sync`, with `pub(crate) fn raw(&self) -> HANDLE`.
- Produces: `const PIPE_ACCESS: u32` — the desired access `connect` asks `CreateFileW` for.
- Changes: `fn connect(...) -> Result<Option<Com1Pipe>, RepositoryError>` (was `Option<OwnedHandle>`).
- Changes: `fn read_until_closed(pipe: &Com1Pipe, ...)`, `fn abandon_read(pipe: &Com1Pipe, ...)`.
- `OwnedHandle` stays as it is; it still owns the parent process handle.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module at the bottom of `crates/platform/src/com1_reader.rs` (extend the existing `use super::{...}` line with `PIPE_ACCESS`):

```rust
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-platform --target x86_64-pc-windows-gnu --lib com1_reader`
Expected: FAIL — `cannot find value PIPE_ACCESS` and `cannot find type Com1Pipe`.

- [ ] **Step 3: Add the type and the access mask**

In `crates/platform/src/com1_reader.rs`, add `GENERIC_WRITE` to the `windows::Win32::Foundation` import list, then add near `READ_BUFFER_BYTES`:

```rust
/// The access the helper asks the COM1 pipe for.
///
/// Both directions: HCS serves the pipe duplex, and the same handle carries the
/// guest's output out and the user's keystrokes in.
const PIPE_ACCESS: u32 = GENERIC_READ.0 | GENERIC_WRITE.0;
```

Add beside `OwnedHandle`:

```rust
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
```

- [ ] **Step 4: Use the new type in the capture path**

In `connect`, change the signature to `Result<Option<Com1Pipe>, RepositoryError>`, pass `PIPE_ACCESS` to `CreateFileW` instead of `GENERIC_READ.0`, and return `Ok(Some(Com1Pipe(handle)))`.

In `read_until_closed` and `abandon_read`, change the `pipe: &OwnedHandle` parameter to `pipe: &Com1Pipe` and every `pipe.0` to `pipe.raw()`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p vmlord-platform --target x86_64-pc-windows-gnu --lib`
Expected: PASS, including the two new tests and every existing one.

- [ ] **Step 6: Commit**

```bash
cargo fmt
cargo clippy --target x86_64-pc-windows-gnu --all-targets
git add crates/platform/src/com1_reader.rs
git commit -m "TASK-68: Open the COM1 pipe in both directions"
```

---

### Task 2: Pump the helper's standard input into the pipe

**Files:**
- Create: `crates/platform/src/com1_input.rs`
- Modify: `crates/platform/src/lib.rs` (module list)
- Modify: `crates/platform/src/com1_reader.rs:205-228` (`capture`)

**Interfaces:**
- Consumes: `Com1Pipe`, `PIPE_ACCESS` from Task 1; `WindowsEvent` from `crate::event`.
- Produces: `pub(crate) fn pump_input(input: &mut impl Read, pipe: &mut impl Write) -> io::Result<()>`
- Produces: `pub(crate) fn start_input(pipe: Arc<Com1Pipe>, vm_name: String) -> Result<(), RepositoryError>`
- Produces: `pub(crate) fn is_end_of_stream_io(error: &io::Error) -> bool`
- Produces: `pub(crate) fn win32_code(error: &windows::core::Error) -> i32`
- Produces: `pub(crate) struct PipeWriter` implementing `io::Write`.

- [ ] **Step 1: Write the failing tests**

Create `crates/platform/src/com1_input.rs` containing only the test module for now:

```rust
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
```

Register the module in `crates/platform/src/lib.rs` beside `mod com1_reader;`:

```rust
mod com1_input;
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-platform --target x86_64-pc-windows-gnu --lib com1_input`
Expected: FAIL — `unresolved imports super::is_end_of_stream_io, super::pump_input, super::win32_code`.

- [ ] **Step 3: Write the implementation**

At the top of `crates/platform/src/com1_input.rs`, above the test module:

```rust
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
        let started =
            unsafe { WriteFile(self.pipe.raw(), Some(buffer), None, Some(&raw mut overlapped)) };
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
    std::thread::Builder::new()
        .name("vmlord-com1-input".to_owned())
        .spawn(move || {
            let mut writer = PipeWriter { pipe, event };
            let stdin = io::stdin();
            match pump_input(&mut stdin.lock(), &mut writer) {
                Ok(()) => log::debug!(
                    "COM1 input for VM \"{vm_name}\" ended with its standard input"
                ),
                Err(error) if is_end_of_stream_io(&error) => log::debug!(
                    "COM1 input for VM \"{vm_name}\" ended with the pipe"
                ),
                Err(error) => {
                    log::warn!("COM1 input for VM \"{vm_name}\" stopped: {error}");
                }
            }
        })
        .map_err(|error| {
            let error = RepositoryError::new(format!(
                "cannot start the COM1 input thread for VM \"{vm_name}\": {error}"
            ));
            log::error!("{error}");
            error
        })?;
    log::debug!("COM1 input for VM \"{vm_name}\" is being forwarded to the guest");
    Ok(())
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-platform --target x86_64-pc-windows-gnu --lib com1_input`
Expected: PASS — four tests.

- [ ] **Step 5: Start the input thread from the capture**

In `crates/platform/src/com1_reader.rs`, add `use std::sync::Arc;` to the `std` import block and `use crate::com1_input::start_input;`, then change the end of `capture`:

```rust
    let Some(pipe) = connect(options, cancel, &parent)? else {
        return Ok(());
    };
    log::debug!("COM1 reader for VM \"{}\" is connected", options.vm_name);

    // The pipe is shared, not handed over: the input thread outlives this call
    // by design, and its `Arc` keeps the handle open until the process exits.
    let pipe = Arc::new(pipe);
    start_input(Arc::clone(&pipe), options.vm_name.clone())?;

    read_until_closed(&pipe, &io_event, cancel, &parent, &mut log_file, options)
```

- [ ] **Step 6: Run the whole suite**

Run: `cargo test -p vmlord-platform --target x86_64-pc-windows-gnu --lib`
Expected: PASS — every test, including the reader's existing ones.

- [ ] **Step 7: Commit**

```bash
cargo fmt
cargo clippy --target x86_64-pc-windows-gnu --all-targets
git add crates/platform/src/com1_input.rs crates/platform/src/com1_reader.rs crates/platform/src/lib.rs
git commit -m "TASK-68: Carry the console window's keyboard into the guest"
```

---

### Task 3: Put the console into raw mode and give it back as found

**Files:**
- Modify: `crates/platform/Cargo.toml:29-48`
- Modify: `crates/platform/src/com1_input.rs`
- Modify: `crates/platform/src/com1_reader.rs` (`capture`)

**Interfaces:**
- Produces: `pub(crate) fn raw_input_mode(original: CONSOLE_MODE) -> CONSOLE_MODE`
- Produces: `pub(crate) fn vt_output_mode(original: CONSOLE_MODE) -> CONSOLE_MODE`
- Produces: `pub(crate) struct ConsoleModes` with `pub(crate) fn enter_raw() -> Self` and a restoring `Drop`.

- [ ] **Step 1: Enable the console API**

In `crates/platform/Cargo.toml`, add `"Win32_System_Console",` to the `windows` feature list, keeping it alphabetical (between `Win32_System_Com` and `Win32_System_HostComputeNetwork`).

- [ ] **Step 2: Write the failing tests**

Add to the test module in `crates/platform/src/com1_input.rs` (extend the `use super::{...}` line with `raw_input_mode, vt_output_mode`):

```rust
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
            ENABLE_LINE_INPUT.0 | ENABLE_ECHO_INPUT.0 | ENABLE_PROCESSED_INPUT.0
                | ENABLE_EXTENDED_FLAGS.0,
        );

        let raw = raw_input_mode(original);

        for cooked in [ENABLE_LINE_INPUT, ENABLE_ECHO_INPUT, ENABLE_PROCESSED_INPUT] {
            assert_eq!(raw.0 & cooked.0, 0, "{cooked:?} must be off in raw mode");
        }
        assert_eq!(raw.0 & ENABLE_VIRTUAL_TERMINAL_INPUT.0, ENABLE_VIRTUAL_TERMINAL_INPUT.0);
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
        assert_eq!(mode.0 & ENABLE_PROCESSED_OUTPUT.0, ENABLE_PROCESSED_OUTPUT.0);
    }
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p vmlord-platform --target x86_64-pc-windows-gnu --lib com1_input`
Expected: FAIL — `unresolved imports super::raw_input_mode, super::vt_output_mode`.

- [ ] **Step 4: Write the implementation**

Add to the imports of `crates/platform/src/com1_input.rs`:

```rust
use windows::Win32::{
    Foundation::HANDLE,
    System::Console::{
        CONSOLE_MODE, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
        ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode,
        GetStdHandle, STD_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleMode,
    },
};
```

Add above the test module:

```rust
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
            (STD_INPUT_HANDLE, raw_input_mode as fn(CONSOLE_MODE) -> CONSOLE_MODE),
            (STD_OUTPUT_HANDLE, vt_output_mode),
        ] {
            match set_console_mode(which, mode_of) {
                Some(previous) => changed.push(previous),
                None => log::debug!(
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
                log::warn!("could not restore the COM1 console mode: {error}");
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
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p vmlord-platform --target x86_64-pc-windows-gnu --lib com1_input`
Expected: PASS — six tests.

- [ ] **Step 6: Enter raw mode for the life of the capture**

In `crates/platform/src/com1_reader.rs`, import `ConsoleModes` alongside `start_input` and insert the guard before the input thread starts:

```rust
    let pipe = Arc::new(pipe);
    // Held until the capture ends, and dropped on every path out of it: the
    // window the helper leaves behind must type and echo as it did before.
    let _console = ConsoleModes::enter_raw();
    start_input(Arc::clone(&pipe), options.vm_name.clone())?;
```

- [ ] **Step 7: Run the whole suite**

Run: `cargo test -p vmlord-platform --target x86_64-pc-windows-gnu --lib`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
cargo fmt
cargo clippy --target x86_64-pc-windows-gnu --all-targets
git add crates/platform/Cargo.toml crates/platform/src/com1_input.rs crates/platform/src/com1_reader.rs
git commit -m "TASK-68: Free the console keyboard while the capture runs"
```

---

### Task 4: Prove a live guest can be logged into over COM1

**Files:**
- Modify: `crates/platform/tests/hyperv.rs` (add at the end of the file, and extend the `use` blocks)

**Interfaces:**
- Consumes: the public `HcsVmRepository`, `MetadataStore`, `open_by_vm_name`, `HcsSystem` already imported by this test file.
- Produces: nothing other tasks use.

- [ ] **Step 1: Write the ignored test**

Add to the imports at the top of `crates/platform/tests/hyperv.rs`:

```rust
use std::{
    io::{Read, Write},
    sync::{Arc, Mutex},
};
```

Add at the end of the file:

```rust
/// The COM1 pipe the helper opens, derived the way `hcs_config` derives it.
fn com1_pipe_path(vm_id: Uuid) -> String {
    format!(r"\\.\pipe\vmlord-{}.com1", vm_id.as_simple())
}

/// Everything the guest has said so far, collected by a thread so that the test
/// can wait for a prompt while the guest is still talking.
fn read_com1_in_background(pipe: fs::File) -> Arc<Mutex<String>> {
    let transcript = Arc::new(Mutex::new(String::new()));
    let collected = Arc::clone(&transcript);
    std::thread::spawn(move || {
        let mut pipe = pipe;
        let mut buffer = [0u8; 4096];
        while let Ok(read) = pipe.read(&mut buffer) {
            if read == 0 {
                break;
            }
            collected
                .lock()
                .unwrap()
                .push_str(&String::from_utf8_lossy(&buffer[..read]));
        }
    });
    transcript
}

/// Waits until the guest has said `expected`, or gives up with what it did say.
fn wait_for_console(
    transcript: &Arc<Mutex<String>>,
    expected: &str,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        if transcript.lock().unwrap().contains(expected) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let seen = transcript.lock().unwrap().clone();
            let tail: String = seen.chars().rev().take(400).collect::<Vec<_>>().into_iter().rev().collect();
            return Err(format!("\"{expected}\" never appeared on COM1; last output was: {tail}"));
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// The reason this task exists: a guest with no network is reachable only
/// through its serial port, and only if the console can be typed into.
///
/// The VM is started through HCS directly rather than through the repository,
/// because a repository start opens its own console helper and HCS serves one
/// pipe instance -- the test has to be the only client.
#[test]
#[ignore = "requires an elevated Windows host with Hyper-V/HCS and downloads a cloud image"]
fn a_guest_can_be_logged_into_over_com1() {
    let root = std::env::temp_dir().join(format!("vmlord-com1-login-{}", std::process::id()));
    fs::create_dir_all(&root).expect("test root should be created");
    let mut repository = cloud_repository(&root);
    repository
        .initialize()
        .expect("the native backend should initialize on a Hyper-V host");
    let name = "com1-login";
    let mut request = background_cloud_request(name);
    if let VmSource::CloudImage { provisioning, .. } = &mut request.source {
        // Without a password there is no console login: cloud-init turns
        // password authentication off and the user has no password at all.
        provisioning.password = Some(vmlord_core::Password::new("vmlord-console".to_owned()));
    }

    repository
        .create_vm(request)
        .expect("the creation should be accepted");
    wait_until_build_finishes(&mut repository, name, Duration::from_secs(20 * 60))
        .expect("the VM should finish building");
    let store = MetadataStore::new(root.join("vm-mapping.json"));
    let mapping = store
        .find_by_vm_name(name)
        .expect("the mapping file should be readable")
        .expect("a built VM has a mapping");
    let system = open_by_vm_name(&store, name, HCS_ACCESS_ALL).expect("the VM should open");
    system
        .start_and_wait(Duration::from_secs(120))
        .expect("the built VM should start");

    let result = (|| -> Result<(), String> {
        let pipe = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(com1_pipe_path(mapping.vm_id))
            .map_err(|error| format!("the COM1 pipe should open duplex: {error}"))?;
        let mut input = pipe
            .try_clone()
            .map_err(|error| format!("the pipe handle should clone: {error}"))?;
        let transcript = read_com1_in_background(pipe);

        wait_for_console(&transcript, "login:", Duration::from_secs(10 * 60))?;
        // Carriage return, not newline: this is a tty, and Enter is 0x0d.
        input.write_all(b"dev\r").map_err(|error| error.to_string())?;
        wait_for_console(&transcript, "Password:", Duration::from_secs(60))?;
        input
            .write_all(b"vmlord-console\r")
            .map_err(|error| error.to_string())?;
        wait_for_console(&transcript, "dev@", Duration::from_secs(120))?;
        input
            .write_all(b"echo vmlord-console-works\r")
            .map_err(|error| error.to_string())?;
        wait_for_console(&transcript, "vmlord-console-works", Duration::from_secs(60))
    })();

    // Cleanup before the assertion: a VM left running is worse than a failed
    // test.
    let _ = system.terminate_and_wait(Duration::from_secs(120));
    drop(system);
    let _ = repository.delete_vm(VmDeleteRequest {
        name: name.into(),
        delete_disks: true,
    });
    drop(repository);
    let _ = fs::remove_dir_all(&root);

    result.expect("a guest should accept a login typed into COM1");
}
```

- [ ] **Step 2: Verify it compiles and is skipped by default**

Run: `cargo test -p vmlord-platform --target x86_64-pc-windows-gnu --test hyperv`
Expected: PASS with the new test reported as ignored. Fix any compile error against the real signatures of `Password::new`, `MetadataStore::find_by_vm_name` and `HcsSystem::start_and_wait` — the test must compile against what those actually take.

- [ ] **Step 3: Commit**

```bash
cargo fmt
cargo clippy --target x86_64-pc-windows-gnu --all-targets
git add crates/platform/tests/hyperv.rs
git commit -m "TASK-68: Cover logging into a live guest over COM1"
```

---

### Task 5: Say what the console is now, in the UI and in the architecture

**Files:**
- Modify: `crates/ui/src/lib.rs:749-754`
- Modify: `ARCHITECTURE.md` (the COM1 section)

**Interfaces:**
- Consumes: nothing. Produces: nothing.

- [ ] **Step 1: Extend the empty-password hint**

In `crates/ui/src/lib.rs`, replace the hint shown when the password field is empty:

```rust
                if form.password.is_empty() {
                    ui.small(
                        "No password: the guest is reachable by SSH key only, \
                         and password logins are turned off. The COM1 console \
                         cannot log in either, so a guest whose network fails \
                         is out of reach.",
                    );
                }
```

- [ ] **Step 2: Run the UI tests**

Run: `cargo test -p vmlord-ui --target x86_64-pc-windows-gnu`
Expected: PASS — the existing `an_untouched_password_field_means_a_key_only_login` still holds; it asserts the request, not the copy.

- [ ] **Step 3: Update ARCHITECTURE.md**

In the COM1 section, state that the console is two-way: the helper opens the pipe duplex, forwards its standard input to the guest unchanged, and puts its console into raw mode (no line buffering, no echo, Ctrl-C to the guest, virtual terminal input and output), restoring the modes it found. State that input is never written to `com1.log`, because a password is not echoed by the guest. State that the console is closed by closing its window or by stopping the VM, and that a guest created without a password cannot be logged into over it.

- [ ] **Step 4: Run the whole suite**

Run: `cargo test --workspace --target x86_64-pc-windows-gnu`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add crates/ui/src/lib.rs ARCHITECTURE.md
git commit -m "TASK-68: Describe the console as a way in, not only out"
```

---

## Manual verification (owner, on a Hyper-V host)

1. Create a VM with a password and `network_mode: None`; the COM1 window opens on start.
2. At `login:`, type the user name — characters appear once, not twice.
3. At `Password:`, type the password — nothing is echoed, and `com1.log` contains neither.
4. Run `sleep 60`, press Ctrl-C — the command is interrupted rather than the window closing.
5. Press Backspace and the arrow keys in a shell — they edit the line rather than printing escape codes.
6. Run `top`, then `q` — the full-screen display renders and clears.
7. Close the window; open a new terminal from the same shell — it echoes and buffers lines as usual.
8. Stop the VM from VMLord while the console is open — the window closes without an error in `vmlord.log`.
