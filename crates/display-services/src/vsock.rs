//! The four sockets the host connects to.
//!
//! The guest listens and the host connects, which is the opposite of the agent
//! protocol: a session lives as long as a viewer window, so no viewer means no
//! connection and no capture. Nothing here is spent while nobody is looking.

use std::{
    io::{self, Read, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
};

/// Where the host opens a session and keeps it alive. `"VMLD"`.
pub const CONTROL_PORT: u32 = 0x564D_4C44;

/// Where the frames go. `"VMLF"`.
pub const FRAME_PORT: u32 = 0x564D_4C46;

/// Where keys and pointer events come back. `"VMLI"`.
pub const INPUT_PORT: u32 = 0x564D_4C49;

/// Where selections cross, in both directions. `"VMLC"`.
///
/// Bound by the clipboard daemon in the user's session rather than by either
/// system service, which is what keeps a stalled compositor call out of the
/// capture loop.
pub const CLIPBOARD_PORT: u32 = 0x564D_4C43;

/// How many connections the kernel holds while this side is busy.
///
/// Three channels and one racing reconnect. A viewer that needs more than that
/// queued is one that is already broken.
const BACKLOG: libc::c_int = 4;

/// A bound, listening vsock. Closes on drop.
pub struct Listener {
    descriptor: OwnedFd,
}

/// One accepted connection. Closes on drop.
pub struct Stream {
    descriptor: OwnedFd,
}

impl Listener {
    /// Binds `port` on every CID this guest answers to, and listens.
    ///
    /// # Errors
    ///
    /// [`io::Error`] if the socket cannot be made, bound or listened on. A
    /// kernel with no vsock transport at all fails here, which is how the
    /// tests on a machine without one skip themselves.
    pub fn bind(port: u32) -> io::Result<Self> {
        let descriptor = socket()?;

        let reuse: libc::c_int = 1;
        // SAFETY: `reuse` is a live `c_int` and the length is its exact size;
        // the descriptor is one this function owns.
        let result = unsafe {
            libc::setsockopt(
                descriptor.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_REUSEADDR,
                (&raw const reuse).cast(),
                size_of_val(&reuse) as libc::socklen_t,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }

        let address = address(libc::VMADDR_CID_ANY, port);
        // SAFETY: `address` is a fully initialized `sockaddr_vm` that lives
        // across the call, and the length given is its exact C ABI size.
        let result = unsafe {
            libc::bind(
                descriptor.as_raw_fd(),
                (&raw const address).cast(),
                size_of_val(&address) as libc::socklen_t,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: a descriptor this function owns and a plain integer.
        if unsafe { libc::listen(descriptor.as_raw_fd(), BACKLOG) } < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(Self { descriptor })
    }

    /// Waits for the host to connect.
    ///
    /// # Errors
    ///
    /// [`io::Error`] if the accept fails, including [`io::ErrorKind::Interrupted`]
    /// when a signal arrived, which a caller shutting down wants to see.
    pub fn accept(&self) -> io::Result<Stream> {
        // SAFETY: a null address and length ask the kernel not to report the
        // peer, which this side does not use: a vsock peer is the host or it
        // is nobody.
        let raw = unsafe {
            libc::accept4(
                self.descriptor.as_raw_fd(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                libc::SOCK_CLOEXEC,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: `accept4` returned a descriptor this process now owns.
        Ok(Stream {
            descriptor: unsafe { OwnedFd::from_raw_fd(raw) },
        })
    }

    /// The descriptor, for a caller that waits on several at once.
    #[must_use]
    pub fn as_raw_fd(&self) -> RawFd {
        self.descriptor.as_raw_fd()
    }
}

impl Stream {
    /// The descriptor, for a caller that waits on several at once or that ends
    /// this one from another thread.
    #[must_use]
    pub fn as_raw_fd(&self) -> RawFd {
        self.descriptor.as_raw_fd()
    }

    /// Makes a read that waits longer than `patience` give up.
    ///
    /// What this buys is not a limit on the session -- an idle desktop is the
    /// ordinary state -- but a reader that comes up for air, so a thread
    /// blocked on a quiet host still notices what the rest of the process has
    /// decided. The protocol reports it as [`RecordError::Idle`], which is not
    /// a fault.
    ///
    /// [`RecordError::Idle`]: vmlord_display_protocol::record::RecordError::Idle
    ///
    /// # Errors
    ///
    /// [`io::Error`] if the option cannot be set.
    pub fn set_read_timeout(&self, patience: std::time::Duration) -> io::Result<()> {
        let timeout = libc::timeval {
            // The two fields change width between musl releases, so they are
            // converted to whatever this target's `timeval` actually holds
            // rather than to a type alias that is on its way out.
            tv_sec: patience.as_secs() as _,
            tv_usec: patience.subsec_micros() as _,
        };
        // SAFETY: `timeout` is a live, initialized `timeval` and the length is
        // its exact size; the descriptor is this stream's own.
        let result = unsafe {
            libc::setsockopt(
                self.descriptor.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                (&raw const timeout).cast(),
                size_of_val(&timeout) as libc::socklen_t,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    }

    /// Ends the connection in both directions, waking whoever is reading it.
    ///
    /// This is how a blocked read is stopped: `shutdown` is safe to make from
    /// another thread or a signal handler, and a read waiting on the connection
    /// returns as soon as it is made. Closing the descriptor instead would race
    /// the reader into a descriptor number something else has reused.
    ///
    /// Failures are ignored: the only caller is on its way out, and a
    /// connection that is already gone needs no ending.
    pub fn shutdown(&self) {
        // SAFETY: `shutdown` takes a descriptor and a flag, and a descriptor
        // that is not an open socket makes it fail rather than misbehave.
        unsafe {
            libc::shutdown(self.descriptor.as_raw_fd(), libc::SHUT_RDWR);
        }
    }
}

impl std::os::fd::AsRawFd for Stream {
    fn as_raw_fd(&self) -> RawFd {
        self.descriptor.as_raw_fd()
    }
}

impl Read for Stream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        // SAFETY: `buffer` is a valid mutable byte slice for this call.
        let result = unsafe {
            libc::read(
                self.descriptor.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(result as usize)
    }
}

impl Write for Stream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        // SAFETY: `buffer` is a valid immutable byte slice for this call.
        let result = unsafe {
            libc::write(
                self.descriptor.as_raw_fd(),
                buffer.as_ptr().cast(),
                buffer.len(),
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(result as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        // A vsock stream has no buffer of its own between here and the kernel.
        Ok(())
    }
}

/// One close-on-exec vsock stream socket.
fn socket() -> io::Result<OwnedFd> {
    // SAFETY: the constants describe a Linux vsock stream socket and `socket`
    // has no pointer arguments. Its result is checked before it is owned.
    let raw = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: `socket` returned a descriptor this process now owns.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// A `sockaddr_vm` for a CID and a port.
fn address(cid: u32, port: u32) -> libc::sockaddr_vm {
    libc::sockaddr_vm {
        svm_family: libc::AF_VSOCK as libc::sa_family_t,
        svm_reserved1: 0,
        svm_port: port,
        svm_cid: cid,
        svm_zero: [0; 4],
    }
}

/// Connects to this machine's own listener, for the tests.
///
/// `VMADDR_CID_LOCAL` is 1 and needs the `vsock_loopback` transport, which not
/// every kernel has loaded.
#[cfg(test)]
fn connect_local(port: u32) -> io::Result<Stream> {
    /// `VMADDR_CID_LOCAL`, which `libc` does not name.
    const VMADDR_CID_LOCAL: u32 = 1;

    let descriptor = socket()?;
    let address = address(VMADDR_CID_LOCAL, port);
    // SAFETY: `address` is a fully initialized `sockaddr_vm` that lives across
    // the call, and the length given is its exact C ABI size.
    let result = unsafe {
        libc::connect(
            descriptor.as_raw_fd(),
            (&raw const address).cast(),
            size_of_val(&address) as libc::socklen_t,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(Stream { descriptor })
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};

    use super::{CONTROL_PORT, FRAME_PORT, INPUT_PORT, Listener};

    #[test]
    fn the_ports_are_the_ones_the_contract_names() {
        // Spelled out rather than computed from the letters: these four bytes
        // are a wire constant the host side also hardcodes, and a clever
        // derivation would hide a typo in either.
        assert_eq!(CONTROL_PORT, 0x564D_4C44);
        assert_eq!(FRAME_PORT, 0x564D_4C46);
        assert_eq!(INPUT_PORT, 0x564D_4C49);
    }

    /// Skipped where the kernel has no loopback transport. This development
    /// machine is one: WSL2 has `/dev/vsock`, so the bind succeeds, but
    /// `vsock_loopback` cannot be loaded without privileges, so nothing can
    /// reach `VMADDR_CID_LOCAL` and the connect fails. Binding inside a real
    /// guest is a later task's to prove; what runs here proves the socket calls
    /// are spelled correctly wherever the transport does exist.
    ///
    /// The accept is given a deadline rather than left to block, because a
    /// kernel that binds but cannot connect would otherwise hang the suite
    /// instead of skipping.
    #[test]
    fn a_listener_accepts_a_local_connection() {
        let Ok(listener) = Listener::bind(FRAME_PORT) else {
            eprintln!("no AF_VSOCK listener on this kernel; skipping");
            return;
        };
        accept_deadline(&listener, 5);

        let client = std::thread::spawn(|| match super::connect_local(FRAME_PORT) {
            Ok(mut stream) => stream.write_all(b"hello").is_ok(),
            Err(_) => false,
        });

        let accepted = listener.accept();
        let connected = client.join().unwrap();
        if !connected {
            eprintln!("no AF_VSOCK loopback transport on this kernel; skipping");
            return;
        }

        let mut accepted = accepted.expect("a connection that was made is one that is accepted");
        let mut buffer = [0u8; 5];
        accepted.read_exact(&mut buffer).unwrap();
        assert_eq!(&buffer, b"hello");
    }

    /// Puts a receive timeout on the listening socket, so a kernel that cannot
    /// carry the connection makes the accept return rather than block.
    fn accept_deadline(listener: &Listener, seconds: i64) {
        let timeout = libc::timeval {
            tv_sec: seconds,
            tv_usec: 0,
        };
        // SAFETY: `timeout` is a live, initialized `timeval` and the length is
        // its exact size; the descriptor is the listener's own.
        unsafe {
            libc::setsockopt(
                listener.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                (&raw const timeout).cast(),
                size_of_val(&timeout) as libc::socklen_t,
            );
        }
    }
}
