//! Reading and writing the descriptors a selection travels on.
//!
//! Both clipboard protocols hand a selection over as a descriptor rather than
//! as bytes on a bus: Mutter's `SelectionRead` returns one, and
//! `*-data-control`'s `send` event carries one. Neither promises it will block,
//! and neither promises the whole selection fits in one call, so the reading
//! and the writing are poll loops -- and they are the same two loops for both,
//! which is why they live here rather than beside either protocol.
//!
//! Nothing here is policy. How large a selection may be is the caller's `cap`,
//! from [`vmlord_display_protocol::clipboard`].

use std::{
    io::{self, Read},
    os::fd::{AsRawFd, OwnedFd},
    time::{Duration, Instant},
};

use crate::guest_clipboard::ClipboardError;

/// Reads a descriptor that is not blocking, and may not be ready for a while.
///
/// # Errors
///
/// [`ClipboardError::TooLarge`] past `cap`, [`ClipboardError::Idle`] if nothing
/// arrives before `deadline`, and [`ClipboardError::Transfer`] if the
/// descriptor fails.
pub fn drain<R: AsRawFd>(
    source: &R,
    cap: usize,
    deadline: Duration,
) -> Result<Vec<u8>, ClipboardError> {
    let mut file = borrowed(source);
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 16 * 1024];
    let until = Instant::now() + deadline;

    loop {
        match file.read(&mut buffer) {
            Ok(0) => return Ok(bytes),
            Ok(read) => {
                if bytes.len() + read > cap {
                    return Err(ClipboardError::TooLarge);
                }
                bytes.extend_from_slice(&buffer[..read]);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= until {
                    return Err(ClipboardError::Idle);
                }
                wait(source.as_raw_fd(), libc::POLLIN);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(ClipboardError::Transfer(error)),
        }
    }
}

/// Writes a whole selection to a descriptor that may not take it all at once.
///
/// # Errors
///
/// [`ClipboardError::Transfer`] if the descriptor fails or the reader stops
/// taking bytes before the selection is finished.
pub fn fill(sink: &OwnedFd, bytes: &[u8]) -> Result<(), ClipboardError> {
    use std::io::Write;

    let mut file = borrowed(sink);
    let mut rest = bytes;

    while !rest.is_empty() {
        match file.write(rest) {
            Ok(0) => {
                return Err(ClipboardError::Transfer(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "the reader took nothing",
                )));
            }
            Ok(written) => rest = &rest[written..],
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                wait(file.as_raw_fd(), libc::POLLOUT);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(ClipboardError::Transfer(error)),
        }
    }

    Ok(())
}

/// A pipe whose ends do not block, which is the shape both protocols want.
///
/// `*-data-control` reads a selection by being handed the writing end of one of
/// these, and the tests of the two loops above need the same thing.
///
/// # Errors
///
/// [`ClipboardError::Transfer`] if the kernel refuses a pipe.
pub fn pipe() -> Result<(OwnedFd, OwnedFd), ClipboardError> {
    use std::os::fd::FromRawFd;

    let mut ends = [0 as libc::c_int; 2];
    // SAFETY: `ends` is two live ints, which is what `pipe2` fills.
    let made = unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
    if made != 0 {
        return Err(ClipboardError::Transfer(io::Error::last_os_error()));
    }

    // SAFETY: `pipe2` succeeded, so both are descriptors this owns.
    Ok(unsafe { (OwnedFd::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1])) })
}

/// Waits until a descriptor is ready, or a second passes.
fn wait(descriptor: libc::c_int, events: libc::c_short) {
    let mut poll = libc::pollfd {
        fd: descriptor,
        events,
        revents: 0,
    };

    // SAFETY: one live `pollfd` describing a descriptor the caller owns, and a
    // count that matches. A timeout means the loop above checks its deadline.
    unsafe {
        libc::poll(&raw mut poll, 1, 1000);
    }
}

/// A `File` over a descriptor this function does not own.
///
/// The descriptor belongs to the caller, which closes it when it drops; the
/// file is wrapped in `ManuallyDrop` so that reading through it does not close
/// something twice.
fn borrowed<F: AsRawFd>(descriptor: &F) -> std::mem::ManuallyDrop<std::fs::File> {
    use std::os::fd::FromRawFd;

    // SAFETY: the descriptor is live for as long as the caller holds it, and
    // the file this makes is never dropped, so it never closes it.
    std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(descriptor.as_raw_fd()) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_stops_at_its_cap() {
        let (reader, writer) = pipe().expect("a pipe");
        std::thread::spawn(move || {
            use std::io::Write;
            let mut sink = std::fs::File::from(writer);
            let _ = sink.write_all(&[b'x'; 40]);
        });

        assert!(matches!(
            drain(&reader, 16, Duration::from_secs(2)),
            Err(ClipboardError::TooLarge)
        ));
    }

    #[test]
    fn a_read_that_never_arrives_gives_up() {
        let (reader, writer) = pipe().expect("a pipe");

        let outcome = drain(&reader, 1024, Duration::from_millis(50));

        drop(writer);
        assert!(matches!(outcome, Err(ClipboardError::Idle)));
    }

    #[test]
    fn a_read_takes_everything_up_to_the_close() {
        let (reader, writer) = pipe().expect("a pipe");
        std::thread::spawn(move || {
            use std::io::Write;
            let mut sink = std::fs::File::from(writer);
            let _ = sink.write_all(b"a selection");
        });

        assert_eq!(
            drain(&reader, 1024, Duration::from_secs(2)).expect("a readable pipe"),
            b"a selection"
        );
    }

    #[test]
    fn a_write_reaches_the_other_end_whole() {
        let (reader, writer) = pipe().expect("a pipe");
        let reading = std::thread::spawn(move || drain(&reader, 1024, Duration::from_secs(2)));

        fill(&writer, b"a selection").expect("a writable pipe");
        drop(writer);

        assert_eq!(
            reading.join().expect("the reader").expect("a read"),
            b"a selection"
        );
    }
}
