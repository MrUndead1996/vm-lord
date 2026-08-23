//! The socket the two services talk over, and the only place a descriptor
//! changes hands.
//!
//! `SOCK_SEQPACKET` rather than `SOCK_STREAM`: `SCM_RIGHTS` is attached to a
//! datagram, and message boundaries mean neither side has to frame anything.
//! Every accepted peer is checked with `SO_PEERCRED` on every connection --
//! the file mode is a hint and the credentials are the decision.

use std::{
    io::{self, ErrorKind},
    mem,
    os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd},
    path::Path,
    ptr,
};

use crate::ipc::{self, Message};

/// The largest datagram either side sends.
///
/// A snapshot is a handful of plane descriptions, so this is generous by two
/// orders of magnitude. It is here so that a peer cannot decide how much the
/// other side allocates.
const MAX_DATAGRAM: usize = 8 * 1024;

/// The most descriptors one datagram may carry: a primary and a cursor buffer.
const MAX_DESCRIPTORS: usize = 2;

/// The socket the broker listens on.
#[derive(Debug)]
pub struct Listener {
    descriptor: OwnedFd,
}

/// One accepted or connected peer.
#[derive(Debug)]
pub struct Connection {
    descriptor: OwnedFd,
}

impl Listener {
    /// Binds the broker's socket, owned by whoever is running and readable by
    /// one group.
    ///
    /// # Errors
    ///
    /// [`io::Error`] from any of the socket calls. A path that a killed broker
    /// left behind is removed rather than refused: a restart has to win.
    pub fn bind(path: &Path, group: libc::gid_t) -> io::Result<Self> {
        let descriptor = socket(libc::SOCK_SEQPACKET)?;
        let _ = std::fs::remove_file(path);

        let address = unix_address(path)?;
        // SAFETY: `address` is a fully initialized `sockaddr_un` that outlives
        // this synchronous call, and the length is its exact C ABI size.
        checked(unsafe {
            libc::bind(
                descriptor.as_raw_fd(),
                ptr::addr_of!(address).cast(),
                mem::size_of_val(&address) as libc::socklen_t,
            )
        })?;

        let c_path = c_string(path)?;
        // SAFETY: `c_path` is a NUL-terminated path that lives across the call.
        // A uid of -1 leaves the owner alone, which is what the broker wants:
        // it is already root, and under a test it is already the tester.
        checked(unsafe { libc::chown(c_path.as_ptr(), libc::uid_t::MAX, group) })?;
        // SAFETY: as above. The mode is set explicitly because the umask of
        // whoever started the unit is not something this code controls.
        checked(unsafe { libc::chmod(c_path.as_ptr(), 0o660) })?;

        // SAFETY: `descriptor` is an owned, bound socket.
        checked(unsafe { libc::listen(descriptor.as_raw_fd(), 8) })?;

        Ok(Self { descriptor })
    }

    /// Accepts one peer and refuses it unless its uid is the one expected.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::PermissionDenied`] for a peer that is not the service user,
    /// whose connection is closed before this returns, or [`io::Error`] from
    /// the socket calls.
    pub fn accept(&self, expected_uid: libc::uid_t) -> io::Result<Connection> {
        // SAFETY: `descriptor` is an owned listening socket; passing null for
        // the address and its length asks the kernel not to report the peer's
        // address, which a Unix socket has none of worth reading.
        let accepted = unsafe {
            libc::accept4(
                self.descriptor.as_raw_fd(),
                ptr::null_mut(),
                ptr::null_mut(),
                libc::SOCK_CLOEXEC,
            )
        };
        if accepted < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `accept4` returned a descriptor this process now owns.
        let connection = Connection {
            descriptor: unsafe { OwnedFd::from_raw_fd(accepted) },
        };

        let credentials = peer_credentials(connection.descriptor.as_raw_fd())?;
        if credentials.uid != expected_uid {
            return Err(io::Error::new(
                ErrorKind::PermissionDenied,
                format!(
                    "uid {} connected to the display broker, which only serves uid {expected_uid}",
                    credentials.uid
                ),
            ));
        }

        Ok(connection)
    }

    /// The descriptor, for a caller that polls it.
    #[must_use]
    pub fn as_raw_fd(&self) -> RawFd {
        self.descriptor.as_raw_fd()
    }
}

impl Connection {
    /// Connects to the broker's socket.
    ///
    /// # Errors
    ///
    /// [`io::Error`] from the socket calls. A broker that has not created its
    /// socket yet fails with [`ErrorKind::NotFound`], which is what the caller
    /// retries on.
    pub fn connect(path: &Path) -> io::Result<Self> {
        let descriptor = socket(libc::SOCK_SEQPACKET)?;
        let address = unix_address(path)?;
        // SAFETY: `address` is a fully initialized `sockaddr_un` that outlives
        // this synchronous call, and the length is its exact C ABI size.
        checked(unsafe {
            libc::connect(
                descriptor.as_raw_fd(),
                ptr::addr_of!(address).cast(),
                mem::size_of_val(&address) as libc::socklen_t,
            )
        })?;

        Ok(Self { descriptor })
    }

    /// The descriptor, for a caller that polls it.
    #[must_use]
    pub fn as_raw_fd(&self) -> RawFd {
        self.descriptor.as_raw_fd()
    }

    /// Sends one message, with any descriptors it refers to.
    ///
    /// # Errors
    ///
    /// [`io::Error`] if the peer is gone or the datagram cannot be written.
    pub fn send(&self, message: &Message, descriptors: &[BorrowedFd<'_>]) -> io::Result<()> {
        assert!(
            descriptors.len() <= MAX_DESCRIPTORS,
            "a snapshot describes a primary and a cursor plane and nothing else"
        );

        let payload = ipc::encode(message);
        let mut iovec = libc::iovec {
            iov_base: payload.as_ptr().cast_mut().cast(),
            iov_len: payload.len(),
        };

        // Sized for the largest control message this ever sends, so that the
        // buffer is a stack array rather than an allocation.
        let mut control = [0u8; control_capacity()];
        let control_len = if descriptors.is_empty() {
            0
        } else {
            // SAFETY: `CMSG_SPACE` is arithmetic over a constant.
            let space =
                unsafe { libc::CMSG_SPACE((mem::size_of::<RawFd>() * descriptors.len()) as u32) };
            space as usize
        };

        let mut header: libc::msghdr = unsafe { mem::zeroed() };
        header.msg_iov = ptr::addr_of_mut!(iovec);
        header.msg_iovlen = 1;
        if control_len > 0 {
            header.msg_control = control.as_mut_ptr().cast();
            header.msg_controllen = control_len as _;

            // SAFETY: `header` has a control buffer of exactly the length
            // `CMSG_SPACE` asked for, so the first header fits in it, and the
            // descriptors are copied into the space that follows it.
            unsafe {
                let control_message = libc::CMSG_FIRSTHDR(&header);
                (*control_message).cmsg_level = libc::SOL_SOCKET;
                (*control_message).cmsg_type = libc::SCM_RIGHTS;
                (*control_message).cmsg_len =
                    libc::CMSG_LEN((mem::size_of::<RawFd>() * descriptors.len()) as u32) as _;

                let raw: Vec<RawFd> = descriptors.iter().map(AsRawFd::as_raw_fd).collect();
                ptr::copy_nonoverlapping(
                    raw.as_ptr().cast::<u8>(),
                    libc::CMSG_DATA(control_message),
                    mem::size_of::<RawFd>() * raw.len(),
                );
            }
        }

        loop {
            // SAFETY: `header` describes the payload and the control buffer,
            // both of which live across the call.
            let written = unsafe { libc::sendmsg(self.descriptor.as_raw_fd(), &header, 0) };
            if written >= 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() != ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    /// Receives one message and whatever descriptors came with it.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::UnexpectedEof`] when the peer closed, which is an ending
    /// and not a fault; [`ErrorKind::InvalidData`] for a datagram this build
    /// cannot read or one the kernel had to truncate; [`io::Error`] otherwise.
    pub fn receive(&self) -> io::Result<(Message, Vec<OwnedFd>)> {
        let mut payload = [0u8; MAX_DATAGRAM];
        let mut control = [0u8; control_capacity()];

        let (read, descriptors, flags) = loop {
            let mut iovec = libc::iovec {
                iov_base: payload.as_mut_ptr().cast(),
                iov_len: payload.len(),
            };
            let mut header: libc::msghdr = unsafe { mem::zeroed() };
            header.msg_iov = ptr::addr_of_mut!(iovec);
            header.msg_iovlen = 1;
            header.msg_control = control.as_mut_ptr().cast();
            header.msg_controllen = control.len() as _;

            // SAFETY: `header` points at buffers that live across the call, and
            // their lengths are their real ones.
            let read = unsafe {
                libc::recvmsg(
                    self.descriptor.as_raw_fd(),
                    &mut header,
                    libc::MSG_CMSG_CLOEXEC,
                )
            };
            if read < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }

            // Every descriptor becomes owned before anything else can fail, so
            // that no error path leaks one.
            let descriptors = take_descriptors(&header);
            break (read as usize, descriptors, header.msg_flags);
        };

        if flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "the display broker socket carried a datagram the kernel had to truncate",
            ));
        }
        if read == 0 && descriptors.is_empty() {
            return Err(io::Error::new(
                ErrorKind::UnexpectedEof,
                "the display broker socket was closed by its peer",
            ));
        }

        let message = ipc::decode(&payload[..read])
            .map_err(|error| io::Error::new(ErrorKind::InvalidData, error))?;

        Ok((message, descriptors))
    }
}

/// A descriptor over anonymous memory, which is what a test has in place of a
/// dma-buf.
///
/// The kernel does not care which kind of descriptor `SCM_RIGHTS` carries, and
/// this is the only way to exercise that on a machine with no DRM device.
///
/// # Errors
///
/// [`io::Error`] if the descriptor cannot be created or written.
pub fn memfd(name: &str, contents: &[u8]) -> io::Result<OwnedFd> {
    let name = c_string(Path::new(name))?;
    // SAFETY: `name` is a NUL-terminated string that lives across the call.
    let raw = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `memfd_create` returned a descriptor this process now owns.
    let descriptor = unsafe { OwnedFd::from_raw_fd(raw) };

    let mut written = 0;
    while written < contents.len() {
        // SAFETY: the slice is valid for the length passed, and the descriptor
        // is owned.
        let count = unsafe {
            libc::write(
                descriptor.as_raw_fd(),
                contents[written..].as_ptr().cast(),
                contents.len() - written,
            )
        };
        if count <= 0 {
            return Err(io::Error::last_os_error());
        }
        written += count as usize;
    }

    // A descriptor passed by `SCM_RIGHTS` shares its file offset with the
    // sender's, so leaving it at the end would hand the peer an empty file.
    // SAFETY: the descriptor is owned and `lseek` has no pointer arguments.
    if unsafe { libc::lseek(descriptor.as_raw_fd(), 0, libc::SEEK_SET) } < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(descriptor)
}

/// How much room the largest control message this module sends takes.
const fn control_capacity() -> usize {
    // `CMSG_SPACE` is not a const function, so this is its arithmetic: a
    // `cmsghdr` rounded up, plus the descriptors rounded up. Generous by a word
    // rather than exact, because the only cost of being generous is stack.
    mem::size_of::<libc::cmsghdr>() + 8 + mem::size_of::<RawFd>() * MAX_DESCRIPTORS + 8
}

/// Turns every `SCM_RIGHTS` descriptor in a received message into an owned one.
fn take_descriptors(header: &libc::msghdr) -> Vec<OwnedFd> {
    let mut descriptors = Vec::new();

    // SAFETY: `header` is a `msghdr` the kernel has just filled in, and the
    // walk uses the macros written for exactly this.
    unsafe {
        let mut control_message = libc::CMSG_FIRSTHDR(header);
        while !control_message.is_null() {
            if (*control_message).cmsg_level == libc::SOL_SOCKET
                && (*control_message).cmsg_type == libc::SCM_RIGHTS
            {
                let payload = (*control_message).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                let count = payload / mem::size_of::<RawFd>();
                let data = libc::CMSG_DATA(control_message);
                for index in 0..count {
                    let mut raw: RawFd = -1;
                    ptr::copy_nonoverlapping(
                        data.add(index * mem::size_of::<RawFd>()),
                        ptr::addr_of_mut!(raw).cast::<u8>(),
                        mem::size_of::<RawFd>(),
                    );
                    descriptors.push(OwnedFd::from_raw_fd(raw));
                }
            }
            control_message = libc::CMSG_NXTHDR(header, control_message);
        }
    }

    descriptors
}

/// Reads the credentials the kernel attached to a connected peer.
fn peer_credentials(descriptor: RawFd) -> io::Result<libc::ucred> {
    let mut credentials: libc::ucred = unsafe { mem::zeroed() };
    let mut length = mem::size_of::<libc::ucred>() as libc::socklen_t;

    // SAFETY: the option is `SO_PEERCRED`, whose value is a `ucred`, and both
    // the value and its length live across the call.
    checked(unsafe {
        libc::getsockopt(
            descriptor,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            ptr::addr_of_mut!(credentials).cast(),
            ptr::addr_of_mut!(length),
        )
    })?;

    Ok(credentials)
}

fn socket(kind: libc::c_int) -> io::Result<OwnedFd> {
    // SAFETY: the constants describe a Linux Unix-domain socket and `socket`
    // has no pointer arguments. Its result is checked before it is owned.
    let raw = unsafe { libc::socket(libc::AF_UNIX, kind | libc::SOCK_CLOEXEC, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `socket` returned a descriptor this process now owns.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

fn unix_address(path: &Path) -> io::Result<libc::sockaddr_un> {
    let bytes = path.as_os_str().as_encoded_bytes();
    let mut address: libc::sockaddr_un = unsafe { mem::zeroed() };
    if bytes.len() >= address.sun_path.len() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!("{} is too long for a Unix socket address", path.display()),
        ));
    }

    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (slot, byte) in address.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }

    Ok(address)
}

/// A path as a NUL-terminated string, for the calls that take one.
pub(crate) fn c_string(path: &Path) -> io::Result<std::ffi::CString> {
    std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|error| io::Error::new(ErrorKind::InvalidInput, error))
}

fn checked(result: libc::c_int) -> io::Result<()> {
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::{ErrorKind, Read},
        os::fd::AsFd,
        path::PathBuf,
    };

    use super::{Connection, Listener, memfd};
    use crate::ipc::Message;

    fn socket_path(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "vmlord-display-{label}-{}.sock",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        path
    }

    fn own_uid() -> libc::uid_t {
        // SAFETY: `getuid` takes nothing, returns a value and cannot fail.
        unsafe { libc::getuid() }
    }

    fn own_gid() -> libc::gid_t {
        // SAFETY: `getgid` takes nothing, returns a value and cannot fail.
        unsafe { libc::getgid() }
    }

    #[test]
    fn a_message_and_its_descriptors_cross_together() {
        let path = socket_path("descriptors");
        let listener = Listener::bind(&path, own_gid()).unwrap();

        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let connection = Connection::connect(&path).unwrap();
                connection.receive().unwrap()
            }
        });

        let server = listener.accept(own_uid()).unwrap();
        let buffer = memfd("frame", b"pixels").unwrap();
        server
            .send(
                &Message::Snapshot {
                    sequence: 5,
                    planes: Vec::new(),
                    new_buffers: vec![1],
                },
                &[buffer.as_fd()],
            )
            .unwrap();

        let (message, descriptors) = client.join().unwrap();
        assert!(matches!(message, Message::Snapshot { sequence: 5, .. }));
        assert_eq!(descriptors.len(), 1);

        let mut contents = Vec::new();
        let mut file = fs::File::from(descriptors.into_iter().next().unwrap());
        file.read_to_end(&mut contents).unwrap();
        assert_eq!(contents, b"pixels");

        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_peer_with_the_wrong_uid_is_refused() {
        let path = socket_path("peercred");
        let listener = Listener::bind(&path, own_gid()).unwrap();

        let client = std::thread::spawn({
            let path = path.clone();
            move || Connection::connect(&path)
        });

        // Nobody's uid but ours can connect here, so an expectation of a
        // different uid is how the check is exercised without a second account.
        let refused = listener.accept(own_uid() + 1);
        assert_eq!(refused.unwrap_err().kind(), ErrorKind::PermissionDenied);

        // The client's connect either succeeded or was reset by the refusal;
        // either way nothing is left holding the socket.
        let _ = client.join().unwrap();
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_message_with_no_descriptors_carries_none() {
        let path = socket_path("bare");
        let listener = Listener::bind(&path, own_gid()).unwrap();

        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let connection = Connection::connect(&path).unwrap();
                connection.send(&Message::NextFrame, &[]).unwrap();
            }
        });

        let server = listener.accept(own_uid()).unwrap();
        let (message, descriptors) = server.receive().unwrap();
        assert_eq!(message, Message::NextFrame);
        assert!(descriptors.is_empty());

        client.join().unwrap();
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn the_socket_is_group_readable_and_no_wider() {
        use std::os::unix::fs::MetadataExt;

        let path = socket_path("mode");
        let _listener = Listener::bind(&path, own_gid()).unwrap();
        let metadata = fs::metadata(&path).unwrap();

        assert_eq!(
            metadata.mode() & 0o777,
            0o660,
            "the umask of whoever started the unit is not something this controls"
        );
        assert_eq!(metadata.gid(), own_gid());
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_stale_socket_from_a_killed_broker_does_not_stop_the_next_one() {
        let path = socket_path("stale");
        let first = Listener::bind(&path, own_gid()).unwrap();
        drop(first);

        assert!(
            Listener::bind(&path, own_gid()).is_ok(),
            "a broker that was killed leaves its socket behind, and the restart has to win"
        );
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn descriptors_received_and_dropped_do_not_accumulate() {
        let path = socket_path("leak");
        let listener = Listener::bind(&path, own_gid()).unwrap();
        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let connection = Connection::connect(&path).unwrap();
                for _ in 0..64 {
                    let (_, descriptors) = connection.receive().unwrap();
                    assert_eq!(descriptors.len(), 1);
                }
                open_descriptors()
            }
        });

        let server = listener.accept(own_uid()).unwrap();
        let buffer = memfd("frame", b"pixels").unwrap();
        for sequence in 0..64 {
            server
                .send(
                    &Message::Snapshot {
                        sequence,
                        planes: Vec::new(),
                        new_buffers: vec![1],
                    },
                    &[buffer.as_fd()],
                )
                .unwrap();
        }

        let after = client.join().unwrap();
        assert!(
            after < 64,
            "sixty-four descriptors were received and dropped; {after} are still open, which is a leak"
        );

        fs::remove_file(&path).unwrap();
    }

    fn open_descriptors() -> usize {
        fs::read_dir("/proc/self/fd")
            .map(|entries| entries.count())
            .unwrap_or(0)
    }

    #[test]
    fn a_closed_peer_is_an_end_and_not_a_fault_of_its_own() {
        let path = socket_path("closed");
        let listener = Listener::bind(&path, own_gid()).unwrap();
        let client = std::thread::spawn({
            let path = path.clone();
            move || drop(Connection::connect(&path).unwrap())
        });

        let server = listener.accept(own_uid()).unwrap();
        client.join().unwrap();
        assert_eq!(
            server.receive().unwrap_err().kind(),
            ErrorKind::UnexpectedEof
        );

        fs::remove_file(&path).unwrap();
    }
}
