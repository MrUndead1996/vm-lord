//! Linux AF_VSOCK transport for the guest agent.

use std::{
    io::{self, Read, Write},
    os::fd::RawFd,
};

/// The well-known CID for a guest's host.
pub const VMADDR_CID_HOST: u32 = libc::VMADDR_CID_HOST;

/// The port where VMLord accepts its guest agent connection.
pub const AGENT_VSOCK_PORT: u32 = 0x564D_4C41;

/// An owned, connected AF_VSOCK stream.
pub struct VsockStream {
    descriptor: RawFd,
}

/// Connects to `cid:port` and makes idle reads time out for heartbeats.
pub fn connect(cid: u32, port: u32) -> io::Result<VsockStream> {
    // SAFETY: the constants describe a Linux stream socket and `socket` has
    // no pointer arguments. Its return value is checked before it is owned.
    let descriptor = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    let stream = VsockStream { descriptor };

    let timeout = libc::timeval {
        tv_sec: 30,
        tv_usec: 0,
    };
    // SAFETY: `timeout` is a valid initialized `timeval`, and its pointer and
    // length describe that value for the duration of this call.
    let timeout_result = unsafe {
        libc::setsockopt(
            stream.descriptor,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            (&raw const timeout).cast(),
            std::mem::size_of_val(&timeout) as libc::socklen_t,
        )
    };
    if timeout_result < 0 {
        return Err(io::Error::last_os_error());
    }

    let address = libc::sockaddr_vm {
        svm_family: libc::AF_VSOCK as libc::sa_family_t,
        svm_reserved1: 0,
        svm_port: port,
        svm_cid: cid,
        svm_zero: [0; 4],
    };
    // SAFETY: `address` is a fully initialized Linux `sockaddr_vm`; its
    // pointer stays valid for this synchronous call and the supplied length
    // is its exact C ABI size.
    let connect_result = unsafe {
        libc::connect(
            stream.descriptor,
            (&raw const address).cast(),
            std::mem::size_of_val(&address) as libc::socklen_t,
        )
    };
    if connect_result < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(stream)
}

impl VsockStream {
    /// The descriptor, for a caller that hands it to something else.
    ///
    /// Borrowed rather than given away: the 9p mount takes its own reference
    /// to the file, so the stream still closes its copy when it is dropped.
    #[must_use]
    pub const fn as_raw_fd(&self) -> RawFd {
        self.descriptor
    }
}

impl Read for VsockStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        // SAFETY: `buffer` is a valid mutable byte slice for this call.
        let result =
            unsafe { libc::read(self.descriptor, buffer.as_mut_ptr().cast(), buffer.len()) };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(result as usize)
        }
    }
}

impl Write for VsockStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        // SAFETY: `buffer` is a valid immutable byte slice for this call.
        let result = unsafe { libc::write(self.descriptor, buffer.as_ptr().cast(), buffer.len()) };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(result as usize)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for VsockStream {
    fn drop(&mut self) {
        // SAFETY: `descriptor` is owned by this stream and is closed exactly
        // once, when its owner is dropped.
        unsafe {
            libc::close(self.descriptor);
        }
    }
}
