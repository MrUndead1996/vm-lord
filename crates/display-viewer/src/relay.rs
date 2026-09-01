//! Carrying a handshake between the control socket and VMLord.
//!
//! The viewer holds the socket and VMLord holds the secret, so neither can run
//! the handshake alone. What happens here is the smallest thing that lets them:
//! bytes off the socket go up the pipe, bytes down the pipe go onto the socket,
//! and the viewer parses none of them. It frames records -- a stream has to be
//! cut somewhere -- and reads no further into them than the length.
//!
//! Every wait is bounded by the deadline the caller chose, which is the
//! `Authenticating` state's share of the retry budget.

use std::{
    error::Error,
    fmt,
    io::{self, Read, Write},
    sync::mpsc::{Receiver, Sender, TryRecvError},
    thread,
    time::{Duration, Instant},
};

use vmlord_display_protocol::record::{CONTROL_MAX_PAYLOAD, HEADER_LEN};

use crate::launch::{Command, Handover, Message};

/// The most a relayed record may carry.
///
/// The control channel's own cap. A handshake record is a few hundred bytes;
/// this is what a peer cannot make the viewer allocate past.
pub const RECORD_CEILING: u32 = CONTROL_MAX_PAYLOAD;

/// How long the loop sleeps when neither end had anything.
const IDLE_SLEEP: Duration = Duration::from_millis(5);

/// The handshake relay for one control socket.
pub struct Relay<'a, S: Read + Write> {
    socket: &'a mut S,
    inbox: &'a Receiver<Message>,
    outbox: &'a Sender<Message>,
    bytes: Vec<u8>,
}

impl<'a, S: Read + Write> Relay<'a, S> {
    /// A relay over one socket and one pair of launch-pipe channels.
    pub fn new(
        socket: &'a mut S,
        inbox: &'a Receiver<Message>,
        outbox: &'a Sender<Message>,
    ) -> Self {
        Self {
            socket,
            inbox,
            outbox,
            bytes: Vec::new(),
        }
    }

    /// Writes `hello` and shuttles bytes until a hand-over arrives.
    ///
    /// # Errors
    ///
    /// [`RelayError::Timeout`] if `deadline` passed first,
    /// [`RelayError::NoParent`] if the launch pipes closed,
    /// [`RelayError::Cancelled`] if VMLord asked the window to close,
    /// [`RelayError::TooLarge`] for a record above [`RECORD_CEILING`], and
    /// [`RelayError::Socket`] if the control socket failed.
    pub fn run(&mut self, hello: &[u8], deadline: Instant) -> Result<Handover, RelayError> {
        self.socket
            .write_all(hello)
            .and_then(|()| self.socket.flush())
            .map_err(|error| RelayError::Socket(error.to_string()))?;

        while Instant::now() < deadline {
            let mut idle = true;

            match self.read_record() {
                Ok(Some(bytes)) => {
                    idle = false;
                    self.outbox
                        .send(Message::RelayFromViewer(bytes))
                        .map_err(|_| RelayError::NoParent)?;
                }
                Ok(None) => {}
                Err(error) => return Err(error),
            }

            loop {
                match self.inbox.try_recv() {
                    Ok(Message::RelayToViewer(bytes)) => {
                        idle = false;
                        self.socket
                            .write_all(&bytes)
                            .and_then(|()| self.socket.flush())
                            .map_err(|error| RelayError::Socket(error.to_string()))?;
                    }
                    Ok(Message::Handover(handover)) => return Ok(handover),
                    Ok(Message::Command(Command::Close)) => return Err(RelayError::Cancelled),
                    // A focus during a handshake is the window's business, and
                    // the pipe thread has already acted on it.
                    Ok(_) => {}
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return Err(RelayError::NoParent),
                }
            }

            if idle {
                thread::sleep(IDLE_SLEEP);
            }
        }

        Err(RelayError::Timeout)
    }

    /// Frames one record off the socket, or `None` if none has arrived.
    ///
    /// Nothing here reads past the header's length: what a record means is
    /// VMLord's business.
    fn read_record(&mut self) -> Result<Option<Vec<u8>>, RelayError> {
        let mut header = [0u8; HEADER_LEN];
        match self.fill(&mut header) {
            Ok(true) => {}
            Ok(false) => return Ok(None),
            Err(error) => return Err(error),
        }

        let header_len = usize::from(header[0]);
        if header_len < HEADER_LEN {
            return Err(RelayError::Socket(format!(
                "a record header of {header_len} bytes is shorter than this build reads"
            )));
        }
        let length = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
        if length > RECORD_CEILING {
            return Err(RelayError::TooLarge(length));
        }

        self.bytes.clear();
        self.bytes.extend_from_slice(&header);
        // Whatever a newer minor appended to the header, then the payload.
        self.bytes.resize(header_len + length as usize, 0);
        let rest = &mut self.bytes[HEADER_LEN..];
        self.socket
            .read_exact(rest)
            .map_err(|error| RelayError::Socket(error.to_string()))?;

        Ok(Some(self.bytes.clone()))
    }

    /// Fills `bytes`, answering `false` for a socket that is merely quiet.
    fn fill(&mut self, bytes: &mut [u8]) -> Result<bool, RelayError> {
        let mut filled = 0;
        while filled < bytes.len() {
            match self.socket.read(&mut bytes[filled..]) {
                Ok(0) if filled == 0 => {
                    return Err(RelayError::Socket(
                        "the guest closed the control connection".to_owned(),
                    ));
                }
                Ok(0) => {
                    return Err(RelayError::Socket(
                        "the control connection ended part-way through a record".to_owned(),
                    ));
                }
                Ok(read) => filled += read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error)
                    if filled == 0
                        && matches!(
                            error.kind(),
                            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                        ) =>
                {
                    return Ok(false);
                }
                Err(error) => return Err(RelayError::Socket(error.to_string())),
            }
        }

        Ok(true)
    }
}

/// Why a handshake did not complete.
#[derive(Debug)]
pub enum RelayError {
    /// The deadline passed. The state's budget answers for what happens next.
    Timeout,
    /// The launch pipes closed: VMLord is gone, and a session needs it.
    NoParent,
    /// VMLord asked the window to close while the handshake ran.
    Cancelled,
    /// A record above the control channel's cap.
    TooLarge(u32),
    /// The control socket failed, or the guest closed it.
    Socket(String),
}

impl fmt::Display for RelayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => formatter.write_str("the handshake did not finish in time"),
            Self::NoParent => formatter.write_str("VMLord is no longer there to run the handshake"),
            Self::Cancelled => formatter.write_str("the handshake was cancelled"),
            Self::TooLarge(length) => write!(
                formatter,
                "a {length}-byte handshake record exceeds the {RECORD_CEILING}-byte limit"
            ),
            Self::Socket(detail) => write!(formatter, "the control socket failed: {detail}"),
        }
    }
}

impl Error for RelayError {}

#[cfg(test)]
mod tests {
    use std::{
        sync::mpsc,
        time::{Duration, Instant},
    };

    use vmlord_display_protocol::{
        keys::Secret,
        record::{self, Channel, Limits},
        session::{Event, Session, Support},
        v1::{Capability, Mode},
    };

    use super::{Relay, RelayError};
    use crate::{duplex, launch::Message};

    fn support() -> Support {
        Support {
            capabilities: vec![Capability::CursorStream],
            modes: vec![Mode::Desktop],
            tile_sizes: vec![16, 32, 64],
            width: 1920,
            height: 1080,
        }
    }

    /// Answers a handshake as a guest would, on one in-memory socket.
    ///
    /// Returns once the guest's session is established, so a test can assert
    /// on both ends of the same handshake.
    fn guest_thread(mut socket: duplex::Duplex, secret: Secret) -> std::thread::JoinHandle<bool> {
        std::thread::spawn(move || {
            let mut session = Session::guest(&secret, support());
            let limits = Limits::new(0, 0);
            let deadline = Instant::now() + Duration::from_secs(5);

            while Instant::now() < deadline {
                let mut payload = Vec::new();
                let header = match record::read(&mut socket, &limits, &mut payload) {
                    Ok(header) => header,
                    Err(record::RecordError::Idle) => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(_) => return false,
                };

                let Ok(outcome) = session.handle(&header, &payload) else {
                    return false;
                };
                if let Some(reply) = outcome.reply {
                    let _ = record::write(&mut socket, &reply, &limits);
                }
                if let Some(auth) = session.pending_auth() {
                    let _ = record::write(&mut socket, &auth, &limits);
                }
                if outcome.event == Event::ControlEstablished {
                    return true;
                }
            }

            false
        })
    }

    #[test]
    fn a_handshake_completes_through_the_relay() {
        let secret = Secret::generate();
        let guest_secret =
            Secret::from_base64(secret.to_base64().as_str()).expect("the same secret");
        let (mut host_socket, guest_socket) = duplex::pair();
        let guest = guest_thread(guest_socket, guest_secret);

        let (to_viewer, inbox) = mpsc::channel();
        let (outbox, from_viewer) = mpsc::channel();

        // What VMLord holds: the secret, the session, and the `ClientHello`
        // whose bytes the launch parameters carry to the viewer.
        let (mut session, hello) = Session::host(
            &secret,
            vmlord_display_protocol::session::Offer {
                capabilities: vec![Capability::CursorStream],
                mode: Mode::Auto,
                width: 1920,
                height: 1080,
                tile_size: 32,
            },
        );
        let mut hello_bytes = hello.header.encode().to_vec();
        hello_bytes.extend_from_slice(&hello.payload);

        let vmlord = std::thread::spawn(move || {
            let limits = Limits::new(0, 0);

            while let Ok(message) = from_viewer.recv_timeout(Duration::from_secs(5)) {
                let Message::RelayFromViewer(bytes) = message else {
                    continue;
                };
                let mut cursor = bytes.as_slice();
                let mut payload = Vec::new();
                let header = record::read(&mut cursor, &limits, &mut payload)
                    .expect("the viewer framed a whole record");
                let outcome = session.handle(&header, &payload).expect("a valid record");

                if let Some(reply) = outcome.reply {
                    let mut out = reply.header.encode().to_vec();
                    out.extend_from_slice(&reply.payload);
                    let _ = to_viewer.send(Message::RelayToViewer(out));
                }
                if outcome.event == Event::ControlEstablished {
                    let negotiated = session.negotiated().expect("established").clone();
                    let _ = to_viewer.send(Message::Handover(crate::launch::Handover {
                        session_id: session.session_id().to_vec(),
                        frame_key: session
                            .derive_channel_key(Channel::Frame)
                            .expect("established")
                            .to_bytes()
                            .to_vec(),
                        clipboard_key: session
                            .derive_channel_key(Channel::Clipboard)
                            .expect("established")
                            .to_bytes()
                            .to_vec(),
                        audio_key: session
                            .derive_channel_key(Channel::Audio)
                            .expect("established")
                            .to_bytes()
                            .to_vec(),
                        input_key: session
                            .derive_channel_key(Channel::Input)
                            .expect("established")
                            .to_bytes()
                            .to_vec(),
                        version_major: negotiated.version.major,
                        version_minor: negotiated.version.minor,
                        capabilities: negotiated
                            .capabilities
                            .iter()
                            .map(|capability| i32::from(*capability))
                            .collect(),
                        mode: i32::from(negotiated.mode),
                        width: negotiated.width,
                        height: negotiated.height,
                        tile_size: negotiated.tile_size,
                        control_sequence: session.control_sequence(),
                    }));
                    return true;
                }
            }

            false
        });

        let mut relay = Relay::new(&mut host_socket, &inbox, &outbox);
        let handover = relay
            .run(&hello_bytes, Instant::now() + Duration::from_secs(5))
            .expect("a handshake this guest can answer");

        assert_eq!(handover.session_id.len(), 16);
        assert_eq!(handover.frame_key.len(), 32);
        assert_eq!(handover.input_key.len(), 32);
        assert_eq!(handover.width, 1920);
        assert!(guest.join().expect("the guest thread"));
        assert!(vmlord.join().expect("the VMLord thread"));
    }

    #[test]
    fn a_silent_guest_times_out_rather_than_hanging() {
        let (mut socket, _guest) = duplex::pair();
        let (_to_viewer, inbox) = mpsc::channel();
        let (outbox, _from_viewer) = mpsc::channel();

        let mut relay = Relay::new(&mut socket, &inbox, &outbox);
        let started = Instant::now();
        let outcome = relay.run(&[], started + Duration::from_millis(200));

        assert!(matches!(outcome, Err(RelayError::Timeout)));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_parent_that_dies_mid_relay_aborts_the_attempt() {
        let (mut socket, _guest) = duplex::pair();
        let (to_viewer, inbox) = mpsc::channel::<Message>();
        let (outbox, _from_viewer) = mpsc::channel();
        drop(to_viewer);

        let mut relay = Relay::new(&mut socket, &inbox, &outbox);

        assert!(matches!(
            relay.run(&[], Instant::now() + Duration::from_secs(5)),
            Err(RelayError::NoParent)
        ));
    }

    #[test]
    fn a_close_command_during_a_handshake_stops_it() {
        let (mut socket, _guest) = duplex::pair();
        let (to_viewer, inbox) = mpsc::channel();
        let (outbox, _from_viewer) = mpsc::channel();
        to_viewer
            .send(Message::Command(crate::launch::Command::Close))
            .expect("the channel is open");

        let mut relay = Relay::new(&mut socket, &inbox, &outbox);

        assert!(matches!(
            relay.run(&[], Instant::now() + Duration::from_secs(5)),
            Err(RelayError::Cancelled)
        ));
    }

    #[test]
    fn a_record_larger_than_the_ceiling_is_refused_before_it_is_read() {
        let (mut socket, mut guest) = duplex::pair();
        let (_to_viewer, inbox) = mpsc::channel();
        let (outbox, _from_viewer) = mpsc::channel();

        // A well-formed header announcing a payload no control record may have.
        let mut header = [0u8; 24];
        header[0] = 24;
        header[1] = Channel::Control.as_wire();
        header[4..8].copy_from_slice(&(super::RECORD_CEILING + 1).to_le_bytes());
        std::io::Write::write_all(&mut guest, &header).expect("an in-memory socket");

        let mut relay = Relay::new(&mut socket, &inbox, &outbox);

        assert!(matches!(
            relay.run(&[], Instant::now() + Duration::from_secs(5)),
            Err(RelayError::TooLarge(_))
        ));
    }
}
