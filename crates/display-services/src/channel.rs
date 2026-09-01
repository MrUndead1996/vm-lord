//! Proving a frame or input socket belongs to a session, from the side that
//! holds only a channel key.
//!
//! The broker did the control handshake and never touches these two sockets, so
//! this is the guest half of the three-record exchange: the crate's own
//! [`keys::channel_tag`] does the arithmetic, and what is written here is the
//! order and the checks -- the session id, the generation, and a tag compared
//! in constant time.

use std::{
    fmt,
    io::{Read, Write},
};

use prost::Message as _;
use vmlord_display_protocol::{
    keys::{self, ChannelKey, NONCE_LEN, Role, SESSION_ID_LEN, Tag},
    record::{self, Channel, Limits, Record, RecordError},
    v1::{ChannelAck, ChannelAuth, ChannelHello, FrameRecord},
};

/// The three records of a bind are small and fixed, and geometry has not been
/// agreed on this socket yet, so the cap that guards them is the smallest one
/// the protocol defines rather than a frame's.
fn handshake_limits() -> Limits {
    Limits::new(0, 0)
}

/// Why a socket did not bind.
#[derive(Debug)]
pub enum BindError {
    /// The hello named a session this guest is not running. A viewer that kept
    /// a socket across a reboot, or one that has the wrong VM.
    WrongSession,
    /// The hello's generation is one this channel has already been on. A
    /// replayed hello, or a socket that raced its own replacement.
    StaleGeneration,
    /// The host's proof did not check out, so it does not hold the channel key.
    BadTag,
    /// The transport failed, or a record could not be framed.
    Record(RecordError),
    /// A record arrived out of turn, or a field was the wrong shape.
    Malformed,
}

impl fmt::Display for BindError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongSession => formatter.write_str("the hello named another session"),
            Self::StaleGeneration => {
                formatter.write_str("the hello's generation has already been used")
            }
            Self::BadTag => formatter.write_str("the host does not hold this channel's key"),
            Self::Record(error) => write!(formatter, "the socket failed: {error}"),
            Self::Malformed => formatter.write_str("a record was out of turn or the wrong shape"),
        }
    }
}

impl std::error::Error for BindError {}

impl From<RecordError> for BindError {
    fn from(error: RecordError) -> Self {
        Self::Record(error)
    }
}

/// Binds one socket, returning the generation it is now on.
///
/// The three records run at sequences 0, 1 and 2: a socket's sequence counter
/// belongs to the socket, not to the session that outlives it.
///
/// `last_generation` is the generation this channel was last bound at, if it
/// ever was. A hello that does not advance past it is a replay, and is refused
/// rather than allowed to displace a live socket.
///
/// # Errors
///
/// [`BindError`] for a hello that names another session or a used generation,
/// a proof that does not check out, a record out of turn, or a failed
/// transport.
pub fn bind<S: Read + Write>(
    stream: &mut S,
    channel: Channel,
    key: &ChannelKey,
    session_id: &[u8],
    last_generation: Option<u32>,
) -> Result<u32, BindError> {
    let limits = handshake_limits();
    let mut payload = Vec::new();

    let header = record::read(stream, &limits, &mut payload)?;
    if header.channel != channel || header.message_type != FrameRecord::ChannelHello as u16 {
        return Err(BindError::Malformed);
    }
    let hello = ChannelHello::decode(payload.as_slice()).map_err(|_| BindError::Malformed)?;

    // Compared as bytes rather than as a decoded id: a wrong length is a wrong
    // session, and there is nothing here that wants to tell them apart.
    if hello.session_id.len() != SESSION_ID_LEN || hello.session_id != session_id {
        return Err(BindError::WrongSession);
    }
    if hello.channel != u32::from(channel.as_wire()) {
        return Err(BindError::Malformed);
    }
    if last_generation.is_some_and(|last| hello.generation <= last) {
        return Err(BindError::StaleGeneration);
    }

    let host_nonce = nonce(&hello.nonce)?;
    let guest_nonce: [u8; NONCE_LEN] = keys::random_bytes();
    let ack = ChannelAck {
        nonce: guest_nonce.to_vec(),
        tag: keys::channel_tag(key, Role::Guest, channel, &host_nonce, &guest_nonce)
            .as_bytes()
            .to_vec(),
    };
    record::write(
        stream,
        &Record::new(
            channel,
            FrameRecord::ChannelAck as u16,
            1,
            0,
            hello.generation,
            ack.encode_to_vec(),
        ),
        &limits,
    )?;

    let header = record::read(stream, &limits, &mut payload)?;
    if header.channel != channel || header.message_type != FrameRecord::ChannelAuth as u16 {
        return Err(BindError::Malformed);
    }
    let auth = ChannelAuth::decode(payload.as_slice()).map_err(|_| BindError::Malformed)?;
    let offered = Tag::from_wire(&auth.tag).map_err(|_| BindError::Malformed)?;
    let expected = keys::channel_tag(key, Role::Host, channel, &host_nonce, &guest_nonce);
    if !keys::verify(&expected, &offered) {
        return Err(BindError::BadTag);
    }

    Ok(hello.generation)
}

/// A nonce off the wire, which must be exactly the width the tag is keyed over.
fn nonce(bytes: &[u8]) -> Result<[u8; NONCE_LEN], BindError> {
    <[u8; NONCE_LEN]>::try_from(bytes).map_err(|_| BindError::Malformed)
}

#[cfg(test)]
mod tests {
    use std::io::{self, Read, Write};

    use prost::Message as _;
    use vmlord_display_protocol::{
        keys::Secret,
        record::{self, Channel, Limits, Record},
        session::{Event, Offer, Session, Support},
        v1::{Capability, ChannelAuth, FrameRecord, Mode},
    };

    use super::{BindError, bind};

    /// A host and a guest that have already agreed on a session, so that the
    /// channel keys on both sides are the real ones.
    fn established() -> (Session, Session) {
        let secret = Secret::generate();
        let support = Support {
            capabilities: vec![Capability::CursorStream],
            modes: vec![Mode::Desktop],
            tile_sizes: vec![16, 32, 64],
            width: 1920,
            height: 1080,
        };
        let offer = Offer {
            capabilities: vec![Capability::CursorStream],
            mode: Mode::Desktop,
            width: 1920,
            height: 1080,
            tile_size: 32,
        };

        let (mut host, client_hello) = Session::host(&secret, offer);
        let mut guest = Session::guest(&secret, support);

        let server_hello = guest
            .handle(&client_hello.header, &client_hello.payload)
            .expect("a well-formed client hello")
            .reply
            .expect("a server hello");
        let server_auth = guest.pending_auth().expect("the guest's proof");

        host.handle(&server_hello.header, &server_hello.payload)
            .expect("a well-formed server hello");
        let client_auth = host
            .handle(&server_auth.header, &server_auth.payload)
            .expect("a valid guest proof")
            .reply
            .expect("the host's proof");

        let outcome = guest
            .handle(&client_auth.header, &client_auth.payload)
            .expect("a valid host proof");
        assert_eq!(outcome.event, Event::ControlEstablished);

        (host, guest)
    }

    /// A socket with the real host state machine behind it.
    ///
    /// Whatever the guest writes is fed to the host, and whatever the host
    /// replies is queued to be read: the bind is tested against the peer it
    /// will actually meet rather than against a script.
    struct Wire {
        host: Session,
        readable: Vec<u8>,
        read_from: usize,
        /// What the guest has written and this side has not yet parsed. A
        /// record reaches a socket as a header and then a payload, so a double
        /// that parsed each `write` on its own would see half a record.
        written: Vec<u8>,
        corrupt_auth: bool,
    }

    impl Wire {
        fn new(host: Session, opening: Record) -> Self {
            let mut wire = Self {
                host,
                readable: Vec::new(),
                read_from: 0,
                written: Vec::new(),
                corrupt_auth: false,
            };
            wire.queue(&opening);

            wire
        }

        /// Flips a byte of the host's tag, so its proof is one no key produces.
        fn corrupt_next_auth(&mut self) {
            self.corrupt_auth = true;
        }

        fn host(&self) -> &Session {
            &self.host
        }

        fn queue(&mut self, record: &Record) {
            record::write(&mut self.readable, record, &Limits::new(0, 0)).expect("a small record");
        }
    }

    impl Read for Wire {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let available = &self.readable[self.read_from..];
            let taken = available.len().min(buffer.len());
            buffer[..taken].copy_from_slice(&available[..taken]);
            self.read_from += taken;

            Ok(taken)
        }
    }

    impl Write for Wire {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.written.extend_from_slice(bytes);

            let mut payload = Vec::new();
            let mut reader = self.written.as_slice();
            let Ok(header) = record::read(&mut reader, &Limits::new(0, 0), &mut payload) else {
                return Ok(bytes.len());
            };
            let consumed = self.written.len() - reader.len();
            self.written.drain(..consumed);

            if let Ok(outcome) = self.host.handle(&header, &payload)
                && let Some(mut reply) = outcome.reply
            {
                if self.corrupt_auth && reply.header.message_type == FrameRecord::ChannelAuth as u16
                {
                    let mut auth = ChannelAuth::decode(reply.payload.as_slice()).expect("an auth");
                    auth.tag[0] ^= 0xff;
                    reply = Record::new(
                        reply.header.channel,
                        reply.header.message_type,
                        reply.header.sequence,
                        reply.header.base,
                        reply.header.generation,
                        auth.encode_to_vec(),
                    );
                }
                self.queue(&reply);
            }

            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_channel_binds_against_the_host_state_machine() {
        let (mut host, guest) = established();
        let hello = host.open_channel(Channel::Frame).unwrap();
        // The key comes from the established session, not from a bound channel:
        // that is the whole point -- the broker derives it and hands it over
        // before any frame socket exists.
        let key = guest.derive_channel_key(Channel::Frame).unwrap();
        let mut wire = Wire::new(host, hello);

        let generation = bind(&mut wire, Channel::Frame, &key, guest.session_id(), None).unwrap();

        assert_eq!(generation, 0);
        assert!(
            wire.host().channel_key(Channel::Frame).is_some(),
            "the host bound the channel too"
        );
    }

    #[test]
    fn an_audio_channel_binds_against_the_host_state_machine() {
        // The positive control the test below needs to mean anything: without
        // it, "the frame key does not bind" passes just as well on a build
        // where no audio key binds either.
        let (mut host, guest) = established();
        let hello = host.open_channel(Channel::Audio).unwrap();
        let key = guest.derive_channel_key(Channel::Audio).unwrap();
        let mut wire = Wire::new(host, hello);

        let generation = bind(&mut wire, Channel::Audio, &key, guest.session_id(), None).unwrap();

        assert_eq!(generation, 0);
        assert!(
            wire.host().channel_key(Channel::Audio).is_some(),
            "the host bound the channel too"
        );
    }

    #[test]
    fn an_audio_socket_that_offers_a_frame_key_does_not_bind() {
        // What `keys::channel_key`'s domain separation is for: a daemon that
        // has one channel's key must not be able to prove itself on another's
        // socket, so a compromised audio daemon gains no picture.
        let (mut host, guest) = established();
        let hello = host.open_channel(Channel::Audio).unwrap();
        let frame = guest.derive_channel_key(Channel::Frame).unwrap();
        let audio = guest.derive_channel_key(Channel::Audio).unwrap();
        let mut wire = Wire::new(host, hello);

        assert_ne!(frame.to_bytes(), audio.to_bytes());
        // The host refuses the proof and answers nothing, so what the guest
        // sees is a socket that stopped. Which error it reports matters less
        // than the two things asserted here: the bind did not succeed, and the
        // host did not bind the channel on its side either.
        assert!(
            bind(&mut wire, Channel::Audio, &frame, guest.session_id(), None).is_err(),
            "a frame key bound the audio channel"
        );
        assert!(
            wire.host().channel_key(Channel::Audio).is_none(),
            "the host bound a channel whose proof did not check out"
        );
    }

    #[test]
    fn a_hello_for_another_session_is_refused() {
        let (mut host, guest) = established();
        let hello = host.open_channel(Channel::Frame).unwrap();
        let key = guest.derive_channel_key(Channel::Frame).unwrap();
        let mut wire = Wire::new(host, hello);

        assert!(matches!(
            bind(&mut wire, Channel::Frame, &key, &[0u8; 16], None),
            Err(BindError::WrongSession)
        ));
    }

    #[test]
    fn a_generation_that_did_not_advance_is_refused() {
        let (mut host, guest) = established();
        let hello = host.open_channel(Channel::Frame).unwrap();
        let key = guest.derive_channel_key(Channel::Frame).unwrap();
        let mut wire = Wire::new(host, hello);

        assert!(matches!(
            bind(&mut wire, Channel::Frame, &key, guest.session_id(), Some(0)),
            Err(BindError::StaleGeneration)
        ));
    }

    #[test]
    fn a_reconnect_binds_at_the_next_generation() {
        let (mut host, guest) = established();
        host.open_channel(Channel::Frame).unwrap();
        let hello = host.reconnect_channel(Channel::Frame).unwrap();
        let key = guest.derive_channel_key(Channel::Frame).unwrap();
        let mut wire = Wire::new(host, hello);

        assert_eq!(
            bind(&mut wire, Channel::Frame, &key, guest.session_id(), Some(0)).unwrap(),
            1
        );
    }

    #[test]
    fn a_forged_auth_tag_is_refused() {
        let (mut host, guest) = established();
        let hello = host.open_channel(Channel::Frame).unwrap();
        let key = guest.derive_channel_key(Channel::Frame).unwrap();
        let mut wire = Wire::new(host, hello);
        wire.corrupt_next_auth();

        assert!(matches!(
            bind(&mut wire, Channel::Frame, &key, guest.session_id(), None),
            Err(BindError::BadTag)
        ));
    }
}
