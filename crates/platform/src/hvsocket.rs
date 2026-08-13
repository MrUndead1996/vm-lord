//! The host end of the socket a guest's agent connects to.
//!
//! HvSocket is Hyper-V's stream transport between a partition and its host. It
//! needs no address, no adapter and no name resolution, which is what makes it
//! the right channel for an agent: it works before the guest has an IP, it
//! works when the guest's networking is broken, and nothing on the network can
//! reach it.
//!
//! The guest opens the connection and the host listens, rather than the other
//! way round. A host that connects has to know when the agent inside is up and
//! keep trying until it is; a guest that connects knows exactly when it is
//! ready, and a reconnect after a lost connection is the same code path as the
//! first connect (#92).
//!
//! An address here is a pair of GUIDs: which partition, and which service. The
//! listener binds one VM's runtime id, so a connection accepted on it can only
//! have come from that VM -- the host never has to work out who is speaking.
//! The service half is derived from a vsock port, because the guest is Linux
//! and Linux spells an HvSocket address as `AF_VSOCK` with a port number.
//!
//! Every wait here is bounded so that the thread that owns the socket can
//! notice it has been told to stop: nothing else can interrupt a blocking
//! socket call, and a VM that is going away must not leave a thread parked on
//! one forever.

use std::{
    io::{self, Read, Write},
    mem,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use uuid::Uuid;
use vmlord_core::RepositoryError;
use windows::{
    Win32::Networking::WinSock::{
        AF_HYPERV, FD_SET, SEND_RECV_FLAGS, SO_RCVTIMEO, SO_SNDTIMEO, SOCK_STREAM, SOCKADDR,
        SOCKET, SOCKET_ERROR, SOL_SOCKET, SOMAXCONN, TIMEVAL, WSADATA, WSAETIMEDOUT,
        WSAGetLastError, WSAStartup, accept, bind, closesocket, listen, recv, select, send,
        setsockopt, socket,
    },
    core::GUID,
};

/// The vsock port the agent connects to.
///
/// A number rather than a service GUID because the guest is Linux: an agent
/// there opens `AF_VSOCK` to the host's context on a port, and Hyper-V turns
/// that port into the service GUID this side binds. The value is `VMLA` read
/// as ASCII -- high enough to be nobody's registered service, and recognisable
/// in a packet capture or an `ss` listing inside the guest.
pub(crate) const AGENT_VSOCK_PORT: u32 = 0x564D_4C41;

/// The protocol number an HvSocket stream is opened with.
///
/// `HV_PROTOCOL_RAW` from `hvsocket.h`, which the Windows metadata does not
/// carry, so it is spelled here.
const HV_PROTOCOL_RAW: i32 = 1;

/// How long an accept waits before letting its thread look at anything else.
///
/// A quarter of a second, because this is also how long stopping a VM waits
/// for its listener's thread to notice: the call that stops a VM runs on the
/// thread that draws the window, and a second spent joining would be a second
/// of a frozen application. Four wakeups a second on an idle VM is what it
/// costs.
pub(crate) const ACCEPT_POLL: Duration = Duration::from_millis(250);

/// How long a read waits before letting its thread check whether it should
/// still be reading.
///
/// The agent talks when it has something to say and is silent otherwise, so
/// nearly every read here times out. That is not an error and does not end the
/// session -- see [`AgentStream::read`]. The interval is [`ACCEPT_POLL`]'s, and
/// for the same reason: it is how long ending a session takes.
const READ_POLL: Duration = ACCEPT_POLL;

/// How long a write may block before the peer is treated as gone.
///
/// Everything this protocol sends is a few dozen bytes, so a guest that cannot
/// take them within five seconds is not reading its socket at all.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// The service GUID a Linux guest's vsock `port` arrives on.
///
/// Hyper-V maps `AF_VSOCK` ports onto HvSocket services through a fixed
/// template: the port becomes the first field of the GUID and the rest is the
/// constant Linux integration uses. Deriving it rather than inventing a GUID
/// of our own is what lets the guest keep speaking plain vsock.
#[must_use]
pub(crate) fn vsock_service_id(port: u32) -> GUID {
    GUID::from_values(
        port,
        0xfacb,
        0x11e6,
        [0xbd, 0x58, 0x64, 0x00, 0x6a, 0x79, 0x86, 0xd3],
    )
}

/// The service the agent of every VM connects to.
#[must_use]
pub(crate) fn agent_service_id() -> GUID {
    vsock_service_id(AGENT_VSOCK_PORT)
}

/// An HvSocket address: which partition, and which service on it.
///
/// `SOCKADDR_HV` from `hvsocket.h`. The Windows metadata does not describe it
/// either, so the layout is spelled out here; it is stable, and Winsock reads
/// it by size.
#[repr(C)]
#[derive(Clone, Copy)]
struct SockaddrHv {
    family: u16,
    reserved: u16,
    vm_id: GUID,
    service_id: GUID,
}

/// A socket bound to one VM's agent service, waiting for that VM to connect.
///
/// Bound to a single runtime id rather than to every partition: a wildcard
/// bind would accept connections from every VM on the host, including ones
/// VMLord did not create, and would then have to work out which VM it was
/// talking to before it could refuse anything. Here that question cannot
/// arise.
pub(crate) struct AgentListener {
    socket: SOCKET,
    vm_name: String,
}

impl AgentListener {
    /// Binds the agent service of the VM whose runtime id is `runtime_id`.
    ///
    /// The runtime id is the GUID Hyper-V gives a compute system for as long as
    /// it runs, which is what an HvSocket address names -- not the compute
    /// system's own id, which is a string VMLord chose.
    ///
    /// # Errors
    ///
    /// [`RepositoryError`] if Winsock refuses the socket, the address or the
    /// listen. The usual cause of a refused bind is a VM whose configuration
    /// has no entry for this service, which is every VM created before the
    /// agent existed: those have to be recreated.
    pub(crate) fn bind(vm_name: &str, runtime_id: Uuid) -> Result<Self, RepositoryError> {
        initialize_winsock()?;

        // SAFETY: A plain socket creation; the returned handle is owned by the
        // `AgentListener` built below, which closes it exactly once.
        let handle =
            unsafe { socket(AF_HYPERV.into(), SOCK_STREAM, HV_PROTOCOL_RAW) }.map_err(|error| {
                let error = RepositoryError::new(format!(
                    "the agent listener of VM \"{vm_name}\" could not be created: {error}"
                ));
                log::error!("{error}");
                error
            })?;
        let listener = Self {
            socket: handle,
            vm_name: vm_name.to_owned(),
        };

        let address = SockaddrHv {
            family: AF_HYPERV,
            reserved: 0,
            vm_id: GUID::from_u128(runtime_id.as_u128()),
            service_id: agent_service_id(),
        };
        // SAFETY: `address` is a valid `SOCKADDR_HV` living across the call,
        // and its length is passed as Winsock expects for an `AF_HYPERV`
        // address.
        let bound = unsafe {
            bind(
                listener.socket,
                (&raw const address).cast::<SOCKADDR>(),
                i32::try_from(mem::size_of::<SockaddrHv>()).expect("an address is 36 bytes"),
            )
        };
        if bound == SOCKET_ERROR {
            return Err(listener.failure("bound to its VM", last_error()));
        }

        // SAFETY: `listener.socket` is an owned, bound socket.
        if unsafe { listen(listener.socket, SOMAXCONN as i32) } == SOCKET_ERROR {
            return Err(listener.failure("put into listening", last_error()));
        }

        log::debug!(
            "listening for the agent of VM \"{vm_name}\" on partition {runtime_id}, \
             vsock port {AGENT_VSOCK_PORT}"
        );
        Ok(listener)
    }

    /// Waits up to `timeout` for the VM to connect.
    ///
    /// `Ok(None)` means the timeout passed with nobody connecting, which is the
    /// normal answer for a guest that has not booted its agent yet: the caller
    /// loops, and the timeout is what lets it notice it should stop first.
    ///
    /// # Errors
    ///
    /// [`RepositoryError`] if the wait or the accept itself failed, which
    /// leaves the listener unusable.
    pub(crate) fn accept(
        &self,
        timeout: Duration,
        running: &Arc<AtomicBool>,
    ) -> Result<Option<AgentStream>, RepositoryError> {
        let mut readable = FD_SET {
            fd_count: 1,
            ..Default::default()
        };
        readable.fd_array[0] = self.socket;
        let timeout = timeval(timeout);

        // SAFETY: `readable` names one owned socket and outlives the call, as
        // does `timeout`. The first argument is ignored on Windows.
        let ready = unsafe { select(0, Some(&mut readable), None, None, Some(&raw const timeout)) };
        match ready {
            0 => return Ok(None),
            SOCKET_ERROR => return Err(self.failure("waited on", last_error())),
            _ => {}
        }

        // SAFETY: The socket is ready to accept, and the returned handle is
        // owned by the `AgentStream` built below.
        let accepted = unsafe { accept(self.socket, None, None) }
            .map_err(|error| self.failure("accepted from", error.to_string()))?;
        let stream = AgentStream {
            socket: accepted,
            vm_name: self.vm_name.clone(),
            running: Arc::clone(running),
        };
        stream.set_timeout(SO_RCVTIMEO, READ_POLL)?;
        stream.set_timeout(SO_SNDTIMEO, WRITE_TIMEOUT)?;

        log::info!("the agent of VM \"{}\" connected", self.vm_name);
        Ok(Some(stream))
    }

    fn failure(&self, what: &str, detail: String) -> RepositoryError {
        let error = RepositoryError::new(format!(
            "the agent listener of VM \"{}\" could not be {what}: {detail}",
            self.vm_name
        ));
        log::error!("{error}");
        error
    }
}

impl Drop for AgentListener {
    fn drop(&mut self) {
        // SAFETY: This listener exclusively owns the socket and closes it once.
        unsafe { closesocket(self.socket) };
    }
}

/// One accepted connection from a VM's agent.
///
/// Reads and writes are bounded rather than blocking forever, so that a thread
/// serving a session can be told to stop while the guest is silent -- which is
/// almost all of the time.
pub(crate) struct AgentStream {
    socket: SOCKET,
    vm_name: String,
    running: Arc<AtomicBool>,
}

impl AgentStream {
    fn set_timeout(&self, option: i32, timeout: Duration) -> Result<(), RepositoryError> {
        let milliseconds = u32::try_from(timeout.as_millis()).expect("a timeout of a few seconds");
        // SAFETY: `self.socket` is owned, and the option value is the `DWORD`
        // of milliseconds Winsock expects for both timeouts.
        let set = unsafe {
            setsockopt(
                self.socket,
                SOL_SOCKET,
                option,
                Some(&milliseconds.to_ne_bytes()),
            )
        };
        if set == SOCKET_ERROR {
            let error = RepositoryError::new(format!(
                "the agent connection of VM \"{}\" could not be given a timeout: {}",
                self.vm_name,
                last_error()
            ));
            log::error!("{error}");
            return Err(error);
        }
        Ok(())
    }
}

impl Read for AgentStream {
    /// Reads what has arrived, waiting no longer than [`READ_POLL`].
    ///
    /// A timeout is reported as [`io::ErrorKind::Interrupted`], which every
    /// reader in the standard library retries: an idle agent is not a broken
    /// one, and a frame half-read must not be abandoned because the guest
    /// paused in the middle of it. Once the session has been told to stop, the
    /// same timeout ends it instead -- that is the only thing that can
    /// interrupt a socket nobody is writing to.
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        // SAFETY: `self.socket` is owned and `buffer` is valid for writes for
        // its own length.
        let read = unsafe { recv(self.socket, buffer, SEND_RECV_FLAGS(0)) };
        if read >= 0 {
            return Ok(read as usize);
        }

        let error = last_error_code();
        if error != WSAETIMEDOUT.0 {
            return Err(io::Error::from_raw_os_error(error));
        }
        if !self.running.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                format!(
                    "the agent session of VM \"{}\" was closed by VMLord",
                    self.vm_name
                ),
            ));
        }
        Err(io::Error::from(io::ErrorKind::Interrupted))
    }
}

impl Write for AgentStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        // SAFETY: `self.socket` is owned and `buffer` is valid for reads for
        // its own length.
        let written = unsafe { send(self.socket, buffer, SEND_RECV_FLAGS(0)) };
        if written < 0 {
            return Err(io::Error::from_raw_os_error(last_error_code()));
        }
        Ok(written as usize)
    }

    /// Nothing is buffered on this side: `send` hands the bytes to the
    /// transport, and there is no user-space buffer left to push.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for AgentStream {
    fn drop(&mut self) {
        // SAFETY: This stream exclusively owns the socket and closes it once.
        unsafe { closesocket(self.socket) };
        log::debug!("the agent connection of VM \"{}\" was closed", self.vm_name);
    }
}

/// Brings Winsock up once for the process.
///
/// `WSAStartup` is reference counted and every socket call needs it to have
/// happened. There is no matching `WSACleanup`: the standard library's own
/// sockets do the same, and tearing Winsock down under them at exit would be
/// the only thing it could achieve.
fn initialize_winsock() -> Result<(), RepositoryError> {
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
        .map_err(|detail| {
            let error = RepositoryError::new(format!("Winsock is not available: {detail}"));
            log::error!("{error}");
            error
        })
}

/// The Winsock error the last call left behind.
fn last_error_code() -> i32 {
    // SAFETY: A thread-local read of the last Winsock error.
    unsafe { WSAGetLastError() }.0
}

fn last_error() -> String {
    let code = last_error_code();
    format!("Winsock error {code}")
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
    use super::{AGENT_VSOCK_PORT, agent_service_id, vsock_service_id};

    #[test]
    fn a_vsock_port_becomes_the_first_field_of_its_service_guid() {
        // The template Hyper-V maps `AF_VSOCK` onto, spelled out: a guest that
        // connects to port 1 must arrive at exactly this service.
        assert_eq!(
            format!("{:?}", vsock_service_id(1)),
            "00000001-FACB-11E6-BD58-64006A7986D3"
        );
    }

    #[test]
    fn the_agent_service_is_the_agents_vsock_port() {
        assert_eq!(agent_service_id(), vsock_service_id(AGENT_VSOCK_PORT));
        assert_eq!(
            format!("{:?}", agent_service_id()),
            "564D4C41-FACB-11E6-BD58-64006A7986D3"
        );
    }
}
