//! The host end of the sockets a guest's display services listen on.
//!
//! The mirror of `vmlord-platform`'s agent socket: there the guest connects and
//! the host listens, here the host connects and the guest listens, which is
//! #118's decision -- a display session is opened by the person who pressed
//! Connect, and the guest's services are already running when they do.
//!
//! An address is a pair of GUIDs: which partition, and which service. The
//! service half is derived from a vsock port, because the guest is Linux and
//! spells an HvSocket address as `AF_VSOCK` with a port number.
//!
//! Every wait is bounded. A connect that cannot succeed says so inside its
//! timeout, and a read on a quiet socket answers `WouldBlock` rather than
//! parking the thread that owns the session.

use std::{
    error::Error,
    fmt,
    io::{self, Read, Write},
    mem,
    time::Duration,
};

use windows::{
    Win32::Networking::WinSock::{
        AF_HYPERV, FD_SET, FIONBIO, SEND_RECV_FLAGS, SOCK_STREAM, SOCKADDR, SOCKET, SOCKET_ERROR,
        TIMEVAL, WSADATA, WSAEBADF, WSAECONNREFUSED, WSAENETDOWN, WSAENETUNREACH, WSAEWOULDBLOCK,
        WSAGetLastError, WSAStartup, closesocket, connect, ioctlsocket, recv, select, send, socket,
    },
    core::GUID,
};

/// Where the host opens a session and keeps it alive. `"VMLD"`.
pub const CONTROL_PORT: u32 = 0x564D_4C44;

/// Where the frames arrive. `"VMLF"`.
pub const FRAME_PORT: u32 = 0x564D_4C46;

/// Where keys and pointer events go back. `"VMLI"`.
pub const INPUT_PORT: u32 = 0x564D_4C49;

/// The protocol number an HvSocket stream is opened with.
///
/// `HV_PROTOCOL_RAW` from `hvsocket.h`, which the Windows metadata does not
/// carry, so it is spelled here.
const HV_PROTOCOL_RAW: i32 = 1;

/// How long a connect attempt waits before it is a failure to report.
///
/// A guest whose services are up answers immediately; this is what bounds the
/// case where the partition is there and nothing is listening on it.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// How long an ordinary read waits for a quiet socket.
///
/// The session loop checks control before frames. Waiting here would therefore
/// delay every frame by the quiet control channel's timeout. A small non-zero
/// bound is still required: a stream record can arrive across more than one
/// `recv`, and `WouldBlock` in the middle of it means "not all bytes yet", not
/// a broken channel.
pub const READ_POLL: Duration = Duration::from_millis(5);

/// The service GUID a Linux guest's vsock `port` arrives on.
///
/// Hyper-V maps `AF_VSOCK` ports onto HvSocket services through a fixed
/// template: the port becomes the first field of the GUID and the rest is the
/// constant Linux integration uses. Derived rather than invented, which is what
/// lets the guest keep speaking plain vsock.
#[must_use]
pub fn vsock_service_id(port: u32) -> GUID {
    GUID::from_values(
        port,
        0xfacb,
        0x11e6,
        [0xbd, 0x58, 0x64, 0x00, 0x6a, 0x79, 0x86, 0xd3],
    )
}

/// An HvSocket address: which partition, and which service on it.
///
/// `SOCKADDR_HV` from `hvsocket.h`. The Windows metadata does not describe it,
/// so the layout is spelled out; it is stable, and Winsock reads it by size.
#[repr(C)]
#[derive(Clone, Copy)]
struct SockaddrHv {
    family: u16,
    reserved: u16,
    vm_id: GUID,
    service_id: GUID,
}

/// One connection to one service of one VM.
pub struct HvSocket {
    socket: SOCKET,
}

impl HvSocket {
    /// Connects to `port` on the partition `runtime_id` names.
    ///
    /// The socket is put into non-blocking mode for the connect and left there:
    /// reads poll with `select`, and a caller that has nothing to read gets
    /// `WouldBlock` rather than a parked thread.
    ///
    /// # Errors
    ///
    /// [`ConnectError::PartitionGone`] when the compute system is not there --
    /// a stopped VM, which is not a failure -- [`ConnectError::Refused`] when
    /// the partition is there and nothing is listening, which is a guest whose
    /// services are still starting, and [`ConnectError::Failed`] for anything
    /// else.
    pub fn connect(
        runtime_id: &[u8; 16],
        port: u32,
        timeout: Duration,
    ) -> Result<Self, ConnectError> {
        initialize_winsock()?;

        // SAFETY: A plain socket creation; the returned handle is owned by the
        // `HvSocket` built below, which closes it exactly once.
        let handle = unsafe { socket(AF_HYPERV.into(), SOCK_STREAM, HV_PROTOCOL_RAW) }
            .map_err(|error| ConnectError::Failed(error.to_string()))?;
        let stream = Self { socket: handle };
        stream.set_non_blocking()?;

        let address = SockaddrHv {
            family: AF_HYPERV,
            reserved: 0,
            vm_id: GUID::from_u128(u128::from_be_bytes(*runtime_id)),
            service_id: vsock_service_id(port),
        };

        // SAFETY: `address` is a valid `SOCKADDR_HV` living across the call, and
        // its length is what Winsock expects for an `AF_HYPERV` address.
        let started = unsafe {
            connect(
                stream.socket,
                (&raw const address).cast::<SOCKADDR>(),
                i32::try_from(mem::size_of::<SockaddrHv>()).expect("an address is 36 bytes"),
            )
        };
        if started == SOCKET_ERROR {
            let code = last_error_code();
            if code != WSAEWOULDBLOCK.0 {
                return Err(ConnectError::classify(code));
            }
        }

        stream.wait_writable(timeout)?;

        tracing::debug!(
            "connected to vsock port {port:#x} of partition {partition:?}",
            partition = address.vm_id
        );
        Ok(stream)
    }

    /// Puts the socket into non-blocking mode.
    fn set_non_blocking(&self) -> Result<(), ConnectError> {
        let mut enabled: u32 = 1;
        // SAFETY: `self.socket` is owned and `enabled` outlives the call.
        let set = unsafe { ioctlsocket(self.socket, FIONBIO, &raw mut enabled) };
        if set == SOCKET_ERROR {
            return Err(ConnectError::Failed(format!(
                "the socket could not be made non-blocking: Winsock error {}",
                last_error_code()
            )));
        }

        Ok(())
    }

    /// Waits for a non-blocking connect to finish, or for `timeout` to pass.
    fn wait_writable(&self, timeout: Duration) -> Result<(), ConnectError> {
        let mut writable = FD_SET {
            fd_count: 1,
            ..Default::default()
        };
        writable.fd_array[0] = self.socket;
        let mut failed = writable;
        let timeout = timeval(timeout);

        // SAFETY: both sets name this owned socket and outlive the call, as does
        // `timeout`. Windows ignores the first argument to `select`.
        let ready = unsafe {
            select(
                0,
                None,
                Some(&mut writable),
                Some(&mut failed),
                Some(&raw const timeout),
            )
        };
        match ready {
            0 => Err(ConnectError::Refused(
                "nothing answered on the guest's display service".to_owned(),
            )),
            SOCKET_ERROR => Err(ConnectError::classify(last_error_code())),
            _ if failed.fd_count > 0 => Err(ConnectError::classify(last_error_code())),
            _ => Ok(()),
        }
    }
}

impl Read for HvSocket {
    /// Reads what has arrived, polling no longer than [`READ_POLL`].
    ///
    /// The wait is `select` rather than `SO_RCVTIMEO`: HvSocket can signal a
    /// receive timeout as a clean read, which is indistinguishable from the
    /// guest closing the connection. A poll that expires becomes `WouldBlock`,
    /// which the record reader reports as an idle connection.
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let mut readable = FD_SET {
            fd_count: 1,
            ..Default::default()
        };
        readable.fd_array[0] = self.socket;
        let timeout = timeval(READ_POLL);

        // SAFETY: `readable` names this owned socket and outlives the call, as
        // does `timeout`.
        let ready = unsafe { select(0, Some(&mut readable), None, None, Some(&raw const timeout)) };
        match ready {
            0 => return Err(io::Error::from(io::ErrorKind::WouldBlock)),
            SOCKET_ERROR => return Err(io::Error::from_raw_os_error(last_error_code())),
            _ => {}
        }

        // SAFETY: `self.socket` is owned and `buffer` is valid for writes for
        // its own length. `select` just reported it readable.
        let read = unsafe { recv(self.socket, buffer, SEND_RECV_FLAGS(0)) };
        if read >= 0 {
            return Ok(read as usize);
        }

        let code = last_error_code();
        if code == WSAEWOULDBLOCK.0 {
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        Err(io::Error::from_raw_os_error(code))
    }
}

impl Write for HvSocket {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        // SAFETY: `self.socket` is owned and `buffer` is valid for reads for
        // its own length.
        let written = unsafe { send(self.socket, buffer, SEND_RECV_FLAGS(0)) };
        if written < 0 {
            let code = last_error_code();
            if code == WSAEWOULDBLOCK.0 {
                return Err(io::Error::from(io::ErrorKind::WouldBlock));
            }
            return Err(io::Error::from_raw_os_error(code));
        }

        Ok(written as usize)
    }

    /// Nothing is buffered on this side: `send` hands the bytes to the
    /// transport, and there is no user-space buffer left to push.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for HvSocket {
    fn drop(&mut self) {
        // SAFETY: This stream exclusively owns the socket and closes it once.
        unsafe { closesocket(self.socket) };
    }
}

/// Why a socket could not be opened.
#[derive(Debug)]
pub enum ConnectError {
    /// The compute system is not there. A stopped VM, not a fault.
    PartitionGone,
    /// The partition is there and nothing is listening.
    Refused(String),
    /// Winsock refused the socket or the address.
    Failed(String),
}

impl ConnectError {
    /// Sorts a Winsock error into the three answers that matter.
    ///
    /// The distinction the viewer acts on is "the VM is gone" against "the
    /// guest is not ready", because the first closes the window quietly and the
    /// second is retried. Verified against a live partition in #121; if a
    /// stopped VM reports something else, this is the one place to change.
    fn classify(code: i32) -> Self {
        if code == WSAENETUNREACH.0 || code == WSAENETDOWN.0 || code == WSAEBADF.0 {
            return Self::PartitionGone;
        }
        if code == WSAECONNREFUSED.0 {
            return Self::Refused(format!("Winsock error {code}"));
        }

        Self::Failed(format!("Winsock error {code}"))
    }
}

impl fmt::Display for ConnectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PartitionGone => formatter.write_str("the VM is not running"),
            Self::Refused(detail) => {
                write!(formatter, "the guest's display service is not up: {detail}")
            }
            Self::Failed(detail) => write!(formatter, "the socket could not be opened: {detail}"),
        }
    }
}

impl Error for ConnectError {}

/// Brings Winsock up once for the process.
fn initialize_winsock() -> Result<(), ConnectError> {
    use std::sync::OnceLock;
    static WINSOCK: OnceLock<Result<(), String>> = OnceLock::new();

    WINSOCK
        .get_or_init(|| {
            let mut data = WSADATA::default();
            // SAFETY: `data` is a valid `WSADATA` for the duration of the call.
            let result = unsafe { WSAStartup(0x0202, &raw mut data) };
            if result == 0 {
                Ok(())
            } else {
                Err(format!("WSAStartup failed with {result}"))
            }
        })
        .clone()
        .map_err(ConnectError::Failed)
}

/// The Winsock error the last call left behind.
fn last_error_code() -> i32 {
    // SAFETY: A thread-local read of the last Winsock error.
    unsafe { WSAGetLastError() }.0
}

/// Splits a duration the way `select` wants it.
fn timeval(duration: Duration) -> TIMEVAL {
    TIMEVAL {
        tv_sec: i32::try_from(duration.as_secs()).unwrap_or(i32::MAX),
        tv_usec: i32::try_from(duration.subsec_micros()).expect("under a million microseconds"),
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        CONTROL_PORT, ConnectError, FRAME_PORT, HvSocket, INPUT_PORT, READ_POLL, vsock_service_id,
    };

    #[test]
    fn ordinary_reads_never_stall_a_frame_for_a_refresh_interval() {
        assert!(READ_POLL <= Duration::from_millis(5));
        assert!(!READ_POLL.is_zero());
    }

    #[test]
    fn the_three_ports_are_the_ones_the_guest_listens_on() {
        assert_eq!(CONTROL_PORT, 0x564D_4C44);
        assert_eq!(FRAME_PORT, 0x564D_4C46);
        assert_eq!(INPUT_PORT, 0x564D_4C49);
    }

    #[test]
    fn a_service_guid_is_the_template_hyper_v_maps_a_vsock_port_through() {
        // The same template `vmlord-platform` derives the agent's service from:
        // the port becomes the first field, and the rest is the constant Linux
        // integration uses.
        assert_eq!(
            format!("{:?}", vsock_service_id(CONTROL_PORT)),
            "564D4C44-FACB-11E6-BD58-64006A7986D3"
        );
        assert_ne!(vsock_service_id(FRAME_PORT), vsock_service_id(INPUT_PORT));
    }

    #[test]
    fn a_connect_to_no_partition_fails_inside_its_timeout() {
        let started = Instant::now();
        let outcome = HvSocket::connect(&[0; 16], CONTROL_PORT, Duration::from_millis(500));

        assert!(matches!(
            outcome,
            Err(ConnectError::PartitionGone | ConnectError::Refused(_) | ConnectError::Failed(_))
        ));
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "a connect that cannot succeed must not hang"
        );
    }
}
