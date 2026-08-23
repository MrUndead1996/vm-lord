//! The states a display session moves through, without a socket in sight.
//!
//! One machine with two roles rather than a set of functions each end calls in
//! its own order. The agent protocol can leave its sequence to its callers
//! because its handshake is one exchange over one socket; this one is three
//! sockets, two directions of proof, a transcript hash and a channel binding,
//! and if the guest services and the viewer each wrote their own half they
//! would drift over exactly what must not drift: what goes into the hash, and
//! in what order.
//!
//! A session need not be run by the process that opened it.
//! [`Session::established_host`] takes a [`HandedOver`] -- what the handshake
//! settled on, the session id, two channel keys and the control sequence -- and
//! produces the established host half without a secret. That is how VMLord
//! keeps the VM's secret while the viewer keeps the sockets, and it is the
//! host's mirror of what the guest's broker does for its capture process.

use std::{error::Error, fmt};

use prost::Message;

use crate::{
    handshake::{self, UnofferedCapability, VersionMismatch},
    keys::{
        self, ChannelKey, NONCE_LEN, Role, SESSION_ID_LEN, Secret, SessionKey, Tag, Transcript,
        WrongLength,
    },
    record::{Channel, Header, Record},
    v1::{
        Capability, ChannelAck, ChannelAuth, ChannelHello, ClientAuth, ClientHello, ControlRecord,
        ErrorCode, FrameRecord, Mode, ProtocolVersion, ServerAuth, ServerHello,
    },
};

/// What a host asks a guest for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    /// The optional parts of this revision the host can use.
    pub capabilities: Vec<Capability>,
    /// The mode the host wants. `Mode::Auto` leaves the choice to the guest.
    pub mode: Mode,
    /// The width the host wants, in guest pixels.
    pub width: u32,
    /// The height the host wants, in guest pixels.
    pub height: u32,
    /// The tile size the host prefers.
    pub tile_size: u32,
}

/// What a guest can actually do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Support {
    /// The optional parts of this revision the guest can use.
    pub capabilities: Vec<Capability>,
    /// The modes the guest has. The MVP guest has `Mode::Desktop` alone.
    pub modes: Vec<Mode>,
    /// The tile sizes the guest's encoder can produce.
    pub tile_sizes: Vec<u32>,
    /// The width the guest's output is at.
    pub width: u32,
    /// The height the guest's output is at.
    pub height: u32,
}

/// What a completed handshake settled on. Both ends hold the same one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Negotiated {
    /// The revision the session runs at.
    pub version: ProtocolVersion,
    /// The capabilities both peers have.
    pub capabilities: Vec<Capability>,
    /// The mode the guest resolved to, which is never `Mode::Auto`.
    pub mode: Mode,
    /// The width the session displays.
    pub width: u32,
    /// The height the session displays.
    pub height: u32,
    /// The tile size the frame stream uses for the life of the session.
    pub tile_size: u32,
}

/// What handling a record changed, beyond the reply it produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// The handshake moved on and nothing else happened yet.
    Continue,
    /// Both peers have proved themselves; the control channel is open.
    ControlEstablished,
    /// A frame or input socket proved it belongs to this session.
    ChannelBound(Channel),
}

/// What handling a record produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// The record to send back, if the state machine owes one.
    pub reply: Option<Record>,
    /// What changed.
    pub event: Event,
}

/// Where a session is in its handshake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// A guest waiting to be opened.
    AwaitingClientHello,
    /// A host that has sent its hello.
    AwaitingServerHello,
    /// A host that has heard the guest's hello and wants its proof.
    AwaitingServerAuth,
    /// A guest that has answered and wants the host's proof.
    AwaitingClientAuth,
    /// Both peers have proved themselves.
    Established,
}

/// One end of one display session.
///
/// It holds no socket. A caller reads a record off whichever of the three
/// channels produced it, hands the header and the payload here, and writes
/// back whatever comes out.
pub struct Session {
    role: Role,
    state: State,
    /// The VM's secret, which only a session that runs its own handshake has.
    /// A session built from a hand-over never sees one -- see
    /// [`Session::established_host`].
    secret: Option<Secret>,
    offer: Option<Offer>,
    support: Option<Support>,
    session_id: [u8; SESSION_ID_LEN],
    host_nonce: Option<[u8; NONCE_LEN]>,
    guest_nonce: Option<[u8; NONCE_LEN]>,
    transcript: Transcript,
    transcript_hash: Option<[u8; 32]>,
    session_key: Option<SessionKey>,
    negotiated: Option<Negotiated>,
    /// What the hellos settled on, held until both proofs check out.
    pending: Option<Negotiated>,
    pending_auth: Option<Record>,
    control_sequence: u32,
    /// Per channel, in `Channel` order: frame then input.
    channels: [ChannelState; 2],
    /// The channel keys a hand-over carried, which outlive a reconnect.
    ///
    /// A session that handshook derives these from its session key and its
    /// transcript whenever it needs them. One built from a hand-over has
    /// neither, so it keeps what it was given: a channel key depends on the
    /// session and the channel, never on the generation, so replacing a socket
    /// does not replace it.
    handover_keys: [Option<ChannelKey>; 2],
}

/// What one bound-or-binding frame or input socket holds.
#[derive(Default)]
struct ChannelState {
    generation: u32,
    host_nonce: Option<[u8; NONCE_LEN]>,
    guest_nonce: Option<[u8; NONCE_LEN]>,
    key: Option<ChannelKey>,
    sequence: u32,
}

/// An established session, as it crosses from one process to another.
///
/// Everything the receiving process needs and nothing it does not: no secret,
/// no session key, no transcript. See [`Session::established_host`].
pub struct HandedOver {
    /// The 16 bytes that name the session across its three sockets.
    pub session_id: [u8; SESSION_ID_LEN],
    /// What the control handshake settled on.
    pub negotiated: Negotiated,
    /// The key the frame socket proves itself with.
    pub frame_key: ChannelKey,
    /// The key the input socket proves itself with.
    pub input_key: ChannelKey,
    /// The sequence the control channel carries on from.
    pub control_sequence: u32,
}

impl Session {
    /// Opens a session as the host, returning the `ClientHello` to send.
    #[must_use]
    pub fn host(secret: &Secret, offer: Offer) -> (Self, Record) {
        Self::host_with_randomness(secret, offer, keys::random_bytes(), keys::random_bytes())
    }

    /// Waits for a session as the guest.
    #[must_use]
    pub fn guest(secret: &Secret, support: Support) -> Self {
        Self::guest_with_randomness(secret, support, keys::random_bytes())
    }

    /// The same as [`Session::host`], with the session id and nonce supplied.
    ///
    /// For golden vectors, which have to be reproducible, and for nothing
    /// else: a session whose nonce a caller chose is a session whose tags a
    /// caller can replay.
    #[doc(hidden)]
    #[must_use]
    pub fn host_with_randomness(
        secret: &Secret,
        offer: Offer,
        session_id: [u8; SESSION_ID_LEN],
        nonce: [u8; NONCE_LEN],
    ) -> (Self, Record) {
        let hello = ClientHello {
            version: Some(ProtocolVersion::current()),
            capabilities: offer.capabilities.iter().map(|c| i32::from(*c)).collect(),
            session_id: session_id.to_vec(),
            host_nonce: nonce.to_vec(),
            mode: i32::from(offer.mode),
            width: offer.width,
            height: offer.height,
            tile_size: offer.tile_size,
        };

        let payload = hello.encode_to_vec();
        let mut transcript = Transcript::new();
        transcript.record(&payload);

        let mut session = Self {
            role: Role::Host,
            state: State::AwaitingServerHello,
            secret: Some(secret.duplicate()),
            offer: Some(offer),
            support: None,
            session_id,
            host_nonce: Some(nonce),
            guest_nonce: None,
            transcript,
            transcript_hash: None,
            session_key: None,
            negotiated: None,
            pending: None,
            pending_auth: None,
            control_sequence: 0,
            channels: [ChannelState::default(), ChannelState::default()],
            handover_keys: [None, None],
        };

        let record = session.control_record(ControlRecord::ClientHello, payload);
        (session, record)
    }

    /// The same as [`Session::guest`], with the nonce supplied. Golden vectors
    /// only, for the reason [`Session::host_with_randomness`] gives.
    #[doc(hidden)]
    #[must_use]
    pub fn guest_with_randomness(
        secret: &Secret,
        support: Support,
        nonce: [u8; NONCE_LEN],
    ) -> Self {
        Self {
            role: Role::Guest,
            state: State::AwaitingClientHello,
            secret: Some(secret.duplicate()),
            offer: None,
            support: Some(support),
            session_id: [0u8; SESSION_ID_LEN],
            host_nonce: None,
            guest_nonce: Some(nonce),
            transcript: Transcript::new(),
            transcript_hash: None,
            session_key: None,
            negotiated: None,
            pending: None,
            pending_auth: None,
            control_sequence: 0,
            channels: [ChannelState::default(), ChannelState::default()],
            handover_keys: [None, None],
        }
    }

    /// Builds the established host half of a session another process handshook.
    ///
    /// The process that holds the VM's secret runs the four control records and
    /// hands the result here: what the handshake settled on, the session id,
    /// the two channel keys, and where the control channel's numbering had got
    /// to. This session derives nothing and holds no secret, which is the whole
    /// point -- a viewer that is compromised loses one session's channel keys
    /// and cannot open a second.
    ///
    /// The state is the one [`Session::handle`] would have reached, so
    /// everything downstream -- [`Session::open_channel`],
    /// [`Session::reconnect_channel`], [`Session::accept`] -- is this crate's
    /// arithmetic rather than a caller's.
    #[must_use]
    pub fn established_host(handed_over: HandedOver) -> Self {
        Self {
            role: Role::Host,
            state: State::Established,
            secret: None,
            offer: None,
            support: None,
            session_id: handed_over.session_id,
            host_nonce: None,
            guest_nonce: None,
            transcript: Transcript::new(),
            transcript_hash: None,
            session_key: None,
            negotiated: Some(handed_over.negotiated),
            pending: None,
            pending_auth: None,
            control_sequence: handed_over.control_sequence,
            channels: [ChannelState::default(), ChannelState::default()],
            handover_keys: [Some(handed_over.frame_key), Some(handed_over.input_key)],
        }
    }

    /// The sequence this session's next control record will carry.
    ///
    /// What a hand-over passes on, so that the process taking a session over
    /// does not restart a stream the guest has already seen part of.
    #[must_use]
    pub fn control_sequence(&self) -> u32 {
        self.control_sequence
    }

    /// Takes the next control sequence, advancing the counter.
    ///
    /// The session machine numbers the records it produces itself; this is for
    /// the records a caller produces on an established session -- pings, pongs,
    /// keyframe requests and the end of a session -- so that one counter serves
    /// the whole channel.
    pub fn take_control_sequence(&mut self) -> u32 {
        let sequence = self.control_sequence;
        self.control_sequence += 1;

        sequence
    }

    /// Takes the next sequence on a bound channel, advancing the counter.
    ///
    /// The counterpart of [`Session::take_control_sequence`] for the records a
    /// caller writes on a frame or input socket, so that the binding records
    /// and the traffic after them share one counter.
    ///
    /// # Errors
    ///
    /// [`SessionError::Unexpected`] for [`Channel::Control`], which has its own.
    pub fn take_channel_sequence(&mut self, channel: Channel) -> Result<u32, SessionError> {
        let index = self.channel_index(channel)?;
        let sequence = self.channels[index].sequence;
        self.channels[index].sequence += 1;

        Ok(sequence)
    }

    /// What the handshake settled on, once it has.
    #[must_use]
    pub fn negotiated(&self) -> Option<&Negotiated> {
        self.negotiated.as_ref()
    }

    /// The identifier that names this session across its three sockets.
    #[must_use]
    pub fn session_id(&self) -> &[u8; SESSION_ID_LEN] {
        &self.session_id
    }

    /// The guest's proof, which follows its hello on the same turn.
    ///
    /// Two records leave the guest back to back and an [`Outcome`] carries
    /// one, so the second waits here. Returned once.
    pub fn pending_auth(&mut self) -> Option<Record> {
        self.pending_auth.take()
    }

    /// Feeds one record in and gets whatever it produced back.
    ///
    /// # Errors
    ///
    /// [`SessionError`] for anything that means the session cannot continue:
    /// an unreadable payload, a record out of turn, a version or capability
    /// that cannot be agreed, a mode the guest does not have, or a proof that
    /// does not check out. Every one of them ends the session; the code to put
    /// in the `Error` record before closing is [`SessionError::code`].
    pub fn handle(&mut self, header: &Header, payload: &[u8]) -> Result<Outcome, SessionError> {
        match (self.state, header.channel, header.message_type) {
            (State::AwaitingClientHello, Channel::Control, message_type)
                if message_type == ControlRecord::ClientHello as u16 =>
            {
                self.on_client_hello(payload)
            }
            (State::AwaitingServerHello, Channel::Control, message_type)
                if message_type == ControlRecord::ServerHello as u16 =>
            {
                self.on_server_hello(payload)
            }
            (State::AwaitingServerAuth, Channel::Control, message_type)
                if message_type == ControlRecord::ServerAuth as u16 =>
            {
                self.on_server_auth(payload)
            }
            (State::AwaitingClientAuth, Channel::Control, message_type)
                if message_type == ControlRecord::ClientAuth as u16 =>
            {
                self.on_client_auth(payload)
            }
            (State::Established, Channel::Frame | Channel::Input, message_type)
                if message_type == FrameRecord::ChannelHello as u16 =>
            {
                self.on_channel_hello(header.channel, payload)
            }
            (State::Established, Channel::Frame | Channel::Input, message_type)
                if message_type == FrameRecord::ChannelAck as u16 =>
            {
                self.on_channel_ack(header.channel, payload)
            }
            (State::Established, Channel::Frame | Channel::Input, message_type)
                if message_type == FrameRecord::ChannelAuth as u16 =>
            {
                self.on_channel_auth(header.channel, payload)
            }
            (_, channel, message_type) => Err(SessionError::Unexpected {
                channel,
                message_type,
            }),
        }
    }

    /// The guest's half: agree, answer, and queue the proof.
    fn on_client_hello(&mut self, payload: &[u8]) -> Result<Outcome, SessionError> {
        let hello = ClientHello::decode(payload).map_err(SessionError::Decode)?;
        let support = self.support.clone().expect("a guest holds its support");

        let version = handshake::negotiate_version(
            ProtocolVersion::current(),
            hello.version.unwrap_or_default(),
        )
        .map_err(SessionError::Version)?;
        let capabilities =
            handshake::agreed_capabilities(&support.capabilities, &hello.capabilities);

        let wanted = Mode::try_from(hello.mode).unwrap_or_default();
        let mode = resolve_mode(wanted, &support.modes)?;
        let tile_size = resolve_tile_size(hello.tile_size, &support.tile_sizes)?;

        self.session_id = session_id_from_wire(&hello.session_id)?;
        self.host_nonce = Some(nonce_from_wire(&hello.host_nonce)?);
        self.transcript.record(payload);

        let answer = ServerHello {
            version: Some(version),
            capabilities: capabilities.iter().map(|c| i32::from(*c)).collect(),
            guest_nonce: self.guest_nonce.expect("a guest drew its nonce").to_vec(),
            modes: support.modes.iter().map(|m| i32::from(*m)).collect(),
            tile_sizes: support.tile_sizes.clone(),
            width: support.width,
            height: support.height,
        };
        let answer_payload = answer.encode_to_vec();
        self.transcript.record(&answer_payload);

        self.derive_session_key();
        let hash = self.transcript_hash.expect("the key derivation set it");
        let key = self
            .session_key
            .as_ref()
            .expect("the key derivation set it");

        let proof = ServerAuth {
            tag: keys::control_tag(key, Role::Guest, &hash)
                .as_bytes()
                .to_vec(),
        };

        self.pending = Some(Negotiated {
            version,
            capabilities,
            mode,
            width: support.width,
            height: support.height,
            tile_size,
        });

        let hello_record = self.control_record(ControlRecord::ServerHello, answer_payload);
        let proof_record = self.control_record(ControlRecord::ServerAuth, proof.encode_to_vec());
        self.pending_auth = Some(proof_record);
        self.state = State::AwaitingClientAuth;

        Ok(Outcome {
            reply: Some(hello_record),
            event: Event::Continue,
        })
    }

    /// The host's half: confirm what came back, and wait for the proof.
    fn on_server_hello(&mut self, payload: &[u8]) -> Result<Outcome, SessionError> {
        let answer = ServerHello::decode(payload).map_err(SessionError::Decode)?;
        let offer = self.offer.clone().expect("a host holds its offer");

        let version = handshake::confirm_version(
            ProtocolVersion::current(),
            answer.version.unwrap_or_default(),
        )
        .map_err(SessionError::Version)?;
        let capabilities =
            handshake::confirm_capabilities(&offer.capabilities, &answer.capabilities)
                .map_err(SessionError::Capability)?;

        let modes: Vec<Mode> = answer
            .modes
            .iter()
            .filter_map(|value| Mode::try_from(*value).ok())
            .collect();
        let mode = resolve_mode(offer.mode, &modes)?;
        let tile_size = resolve_tile_size(offer.tile_size, &answer.tile_sizes)?;

        self.guest_nonce = Some(nonce_from_wire(&answer.guest_nonce)?);
        self.transcript.record(payload);
        self.derive_session_key();

        self.pending = Some(Negotiated {
            version,
            capabilities,
            mode,
            width: answer.width,
            height: answer.height,
            tile_size,
        });
        self.state = State::AwaitingServerAuth;

        Ok(Outcome {
            reply: None,
            event: Event::Continue,
        })
    }

    /// The host's half: check the guest's proof, then send its own.
    fn on_server_auth(&mut self, payload: &[u8]) -> Result<Outcome, SessionError> {
        let proof = ServerAuth::decode(payload).map_err(SessionError::Decode)?;
        let offered = Tag::from_wire(&proof.tag).map_err(SessionError::Field)?;

        let hash = self.transcript_hash.expect("the hello set it");
        let key = self.session_key.as_ref().expect("the hello set it");
        if !keys::verify(&keys::control_tag(key, Role::Guest, &hash), &offered) {
            return Err(SessionError::BadTag);
        }

        let answer = ClientAuth {
            tag: keys::control_tag(key, Role::Host, &hash)
                .as_bytes()
                .to_vec(),
        };
        let record = self.control_record(ControlRecord::ClientAuth, answer.encode_to_vec());

        self.negotiated = self.pending.clone();
        self.state = State::Established;

        Ok(Outcome {
            reply: Some(record),
            event: Event::ControlEstablished,
        })
    }

    /// The guest's half: check the host's proof, and open the session.
    fn on_client_auth(&mut self, payload: &[u8]) -> Result<Outcome, SessionError> {
        let proof = ClientAuth::decode(payload).map_err(SessionError::Decode)?;
        let offered = Tag::from_wire(&proof.tag).map_err(SessionError::Field)?;

        let hash = self.transcript_hash.expect("the hello set it");
        let key = self.session_key.as_ref().expect("the hello set it");
        if !keys::verify(&keys::control_tag(key, Role::Host, &hash), &offered) {
            return Err(SessionError::BadTag);
        }

        self.negotiated = self.pending.clone();
        self.state = State::Established;

        Ok(Outcome {
            reply: None,
            event: Event::ControlEstablished,
        })
    }

    /// Opens a frame or input socket for this session, as the host.
    ///
    /// # Errors
    ///
    /// [`SessionError::NotEstablished`] before the control handshake has
    /// finished -- there is no transcript to key a channel off yet -- and
    /// [`SessionError::Unexpected`] for the control channel, which is the one
    /// that establishes sessions rather than binding to them.
    pub fn open_channel(&mut self, channel: Channel) -> Result<Record, SessionError> {
        self.channel_hello(channel)
    }

    /// Opens a replacement socket for a channel that dropped.
    ///
    /// The generation goes up, so records still in flight from the previous
    /// connection are rejected by [`Session::accept`] rather than reaching a
    /// decoder or an input device.
    ///
    /// What the reconnected channel owes is not something this crate can
    /// enforce, and is named here because this is where it begins: a frame
    /// channel must send `StreamConfig` and a keyframe before any delta, since
    /// a delta has nothing to apply to, and an input channel must send
    /// `ReleaseAll`, since the guest has just released everything it held.
    ///
    /// # Errors
    ///
    /// As [`Session::open_channel`].
    pub fn reconnect_channel(&mut self, channel: Channel) -> Result<Record, SessionError> {
        let index = self.channel_index(channel)?;
        self.channels[index].generation += 1;
        self.channels[index].sequence = 0;
        self.channels[index].key = None;
        self.channel_hello(channel)
    }

    /// Which generation of `channel` this session is on.
    #[must_use]
    pub fn generation(&self, channel: Channel) -> u32 {
        match self.channel_index(channel) {
            Ok(index) => self.channels[index].generation,
            Err(_) => 0,
        }
    }

    /// The key a bound channel proves itself with, for the process that owns
    /// that socket.
    #[must_use]
    pub fn channel_key(&self, channel: Channel) -> Option<&ChannelKey> {
        self.channels[self.channel_index(channel).ok()?]
            .key
            .as_ref()
    }

    /// Checks a record against the generation its channel is on.
    ///
    /// # Errors
    ///
    /// [`SessionError::StaleGeneration`] for a record from a connection that
    /// has been replaced. The control channel is exempt: losing it ends the
    /// session rather than reconnecting a channel within one.
    pub fn accept(&self, header: &Header) -> Result<(), SessionError> {
        let Ok(index) = self.channel_index(header.channel) else {
            return Ok(());
        };

        let expected = self.channels[index].generation;
        if header.generation != expected {
            return Err(SessionError::StaleGeneration {
                channel: header.channel,
                expected,
                found: header.generation,
            });
        }

        Ok(())
    }

    /// Builds the `ChannelHello` both openers send.
    ///
    /// The three binding records are numbered 1, 2 and 3 on the frame and the
    /// input channel alike, so `FrameRecord` names them for both. The schema
    /// keeps `InputRecord` in step with it, and the compatibility rules keep
    /// either from being renumbered.
    fn channel_hello(&mut self, channel: Channel) -> Result<Record, SessionError> {
        let index = self.channel_index(channel)?;
        if self.negotiated.is_none() || self.role != Role::Host {
            return Err(SessionError::NotEstablished);
        }

        let nonce: [u8; NONCE_LEN] = keys::random_bytes();
        self.channels[index].host_nonce = Some(nonce);

        let hello = ChannelHello {
            session_id: self.session_id.to_vec(),
            channel: u32::from(channel.as_wire()),
            generation: self.channels[index].generation,
            nonce: nonce.to_vec(),
        };

        Ok(self.channel_record(
            channel,
            FrameRecord::ChannelHello as u16,
            hello.encode_to_vec(),
        ))
    }

    /// The guest's half: recognise the session, and answer with a proof.
    fn on_channel_hello(
        &mut self,
        channel: Channel,
        payload: &[u8],
    ) -> Result<Outcome, SessionError> {
        let index = self.channel_index(channel)?;
        let hello = ChannelHello::decode(payload).map_err(SessionError::Decode)?;

        if session_id_from_wire(&hello.session_id)? != self.session_id {
            return Err(SessionError::UnknownSession);
        }
        if hello.channel != u32::from(channel.as_wire()) {
            return Err(SessionError::Unexpected {
                channel,
                message_type: FrameRecord::ChannelHello as u16,
            });
        }

        let host_nonce = nonce_from_wire(&hello.nonce)?;
        let guest_nonce: [u8; NONCE_LEN] = keys::random_bytes();
        let key = self.established_channel_key(channel);
        let tag = keys::channel_tag(&key, Role::Guest, channel, &host_nonce, &guest_nonce);

        self.channels[index].generation = hello.generation;
        self.channels[index].host_nonce = Some(host_nonce);
        self.channels[index].guest_nonce = Some(guest_nonce);
        self.channels[index].key = Some(key);

        let ack = ChannelAck {
            nonce: guest_nonce.to_vec(),
            tag: tag.as_bytes().to_vec(),
        };

        Ok(Outcome {
            reply: Some(self.channel_record(
                channel,
                FrameRecord::ChannelAck as u16,
                ack.encode_to_vec(),
            )),
            event: Event::Continue,
        })
    }

    /// The host's half: check the guest's proof, then send its own.
    fn on_channel_ack(
        &mut self,
        channel: Channel,
        payload: &[u8],
    ) -> Result<Outcome, SessionError> {
        let index = self.channel_index(channel)?;
        let ack = ChannelAck::decode(payload).map_err(SessionError::Decode)?;

        let host_nonce = self.channels[index]
            .host_nonce
            .ok_or(SessionError::NotEstablished)?;
        let guest_nonce = nonce_from_wire(&ack.nonce)?;
        let offered = Tag::from_wire(&ack.tag).map_err(SessionError::Field)?;

        let key = self.established_channel_key(channel);
        let expected = keys::channel_tag(&key, Role::Guest, channel, &host_nonce, &guest_nonce);
        if !keys::verify(&expected, &offered) {
            return Err(SessionError::BadTag);
        }

        let mine = keys::channel_tag(&key, Role::Host, channel, &host_nonce, &guest_nonce);
        self.channels[index].key = Some(key);

        let auth = ChannelAuth {
            tag: mine.as_bytes().to_vec(),
        };

        Ok(Outcome {
            reply: Some(self.channel_record(
                channel,
                FrameRecord::ChannelAuth as u16,
                auth.encode_to_vec(),
            )),
            event: Event::ChannelBound(channel),
        })
    }

    /// The guest's half: check the host's proof, and open the channel.
    fn on_channel_auth(
        &mut self,
        channel: Channel,
        payload: &[u8],
    ) -> Result<Outcome, SessionError> {
        let index = self.channel_index(channel)?;
        let auth = ChannelAuth::decode(payload).map_err(SessionError::Decode)?;
        let offered = Tag::from_wire(&auth.tag).map_err(SessionError::Field)?;

        let host_nonce = self.channels[index]
            .host_nonce
            .ok_or(SessionError::NotEstablished)?;
        let guest_nonce = self.channel_guest_nonce(channel)?;
        let key = self.established_channel_key(channel);

        let expected = keys::channel_tag(&key, Role::Host, channel, &host_nonce, &guest_nonce);
        if !keys::verify(&expected, &offered) {
            self.channels[index].key = None;
            return Err(SessionError::BadTag);
        }

        Ok(Outcome {
            reply: None,
            event: Event::ChannelBound(channel),
        })
    }

    /// The key `channel` will prove itself with, derived rather than remembered.
    ///
    /// [`Session::channel_key`] answers only once a socket has bound; this
    /// answers as soon as the control handshake is done, which is what lets the
    /// process that holds the secret hand a key to the process that holds the
    /// socket. Returns `None` before the handshake completes and for
    /// [`Channel::Control`], which binds nothing.
    #[must_use]
    pub fn derive_channel_key(&self, channel: Channel) -> Option<ChannelKey> {
        if self.channel_index(channel).is_err() {
            return None;
        }

        Some(keys::channel_key(
            self.session_key.as_ref()?,
            self.transcript_hash.as_ref()?,
            channel,
        ))
    }

    /// Derives this session's key for `channel`, where it must be there.
    ///
    /// The infallible half of [`Session::derive_channel_key`], for the handlers
    /// that only run once the session is established. A session built from a
    /// hand-over has no session key to derive from and answers with the key it
    /// was given.
    fn established_channel_key(&self, channel: Channel) -> ChannelKey {
        let index = self
            .channel_index(channel)
            .expect("a channel key is never asked for on control");

        if let Some(key) = self.handover_keys[index].as_ref() {
            return ChannelKey::from_bytes(*key.to_bytes());
        }

        keys::channel_key(
            self.session_key
                .as_ref()
                .expect("an established session derived one"),
            &self
                .transcript_hash
                .expect("an established session finished its transcript"),
            channel,
        )
    }

    /// The nonce the guest put in its own `ChannelAck`.
    fn channel_guest_nonce(&self, channel: Channel) -> Result<[u8; NONCE_LEN], SessionError> {
        let index = self.channel_index(channel)?;
        self.channels[index]
            .guest_nonce
            .ok_or(SessionError::NotEstablished)
    }

    /// Where `channel` lives in `channels`.
    fn channel_index(&self, channel: Channel) -> Result<usize, SessionError> {
        match channel {
            Channel::Frame => Ok(0),
            Channel::Input => Ok(1),
            Channel::Control => Err(SessionError::Unexpected {
                channel,
                message_type: 0,
            }),
        }
    }

    /// Wraps a payload as this side's next record on a bound channel.
    fn channel_record(&mut self, channel: Channel, message_type: u16, payload: Vec<u8>) -> Record {
        let index = self
            .channel_index(channel)
            .expect("a channel record is never on control");
        let sequence = self.channels[index].sequence;
        self.channels[index].sequence += 1;

        Record::new(
            channel,
            message_type,
            sequence,
            0,
            self.channels[index].generation,
            payload,
        )
    }

    /// Finishes the transcript and derives the key both proofs are made under.
    fn derive_session_key(&mut self) {
        let hash = self.transcript.finish();
        let key = keys::session_key(
            self.secret
                .as_ref()
                .expect("a session that handshakes holds the secret"),
            &self.session_id,
            &self.host_nonce.expect("the client hello carried it"),
            &self.guest_nonce.expect("the server hello carried it"),
        );

        self.transcript_hash = Some(hash);
        self.session_key = Some(key);
    }

    /// Wraps a payload as this side's next control record.
    fn control_record(&mut self, message_type: ControlRecord, payload: Vec<u8>) -> Record {
        let sequence = self.control_sequence;
        self.control_sequence += 1;

        Record::new(
            Channel::Control,
            message_type as u16,
            sequence,
            0,
            0,
            payload,
        )
    }
}

/// Settles which mode a session runs in.
///
/// `Mode::Auto` names a host-side policy, and resolving it here is what keeps
/// it from reaching an encoder that has no such mode: it becomes the first
/// mode the guest announced, which for the MVP guest is `Mode::Desktop`.
fn resolve_mode(wanted: Mode, supported: &[Mode]) -> Result<Mode, SessionError> {
    if supported.is_empty() {
        return Err(SessionError::UnsupportedMode(wanted));
    }

    match wanted {
        Mode::Auto => Ok(supported[0]),
        wanted if supported.contains(&wanted) => Ok(wanted),
        wanted => Err(SessionError::UnsupportedMode(wanted)),
    }
}

/// Settles the tile size, which is fixed for the life of a session.
///
/// The one asked for if the guest has it; otherwise the largest it has that is
/// no larger, so a host asking for more than the guest can produce gets the
/// closest thing rather than a refusal.
fn resolve_tile_size(wanted: u32, supported: &[u32]) -> Result<u32, SessionError> {
    if supported.contains(&wanted) {
        return Ok(wanted);
    }

    supported
        .iter()
        .copied()
        .filter(|size| *size < wanted)
        .max()
        .or_else(|| supported.iter().copied().min())
        .ok_or(SessionError::NoCommonTileSize)
}

/// Reads a session id off the wire.
fn session_id_from_wire(bytes: &[u8]) -> Result<[u8; SESSION_ID_LEN], SessionError> {
    bytes.try_into().map_err(|_| {
        SessionError::Field(WrongLength {
            what: "session id",
            len: bytes.len(),
        })
    })
}

/// Reads a nonce off the wire.
fn nonce_from_wire(bytes: &[u8]) -> Result<[u8; NONCE_LEN], SessionError> {
    bytes.try_into().map_err(|_| {
        SessionError::Field(WrongLength {
            what: "nonce",
            len: bytes.len(),
        })
    })
}

/// Why a session cannot continue.
#[derive(Debug)]
pub enum SessionError {
    /// The peers' majors differ, or a peer chose a revision it was not offered.
    Version(VersionMismatch),
    /// A peer agreed on a capability this side never offered.
    Capability(UnofferedCapability),
    /// A fixed-width field arrived at another width.
    Field(WrongLength),
    /// A payload is not the message its header names.
    Decode(prost::DecodeError),
    /// A record arrived that this state has no answer for.
    Unexpected {
        /// Which channel it came in on.
        channel: Channel,
        /// What its header called it.
        message_type: u16,
    },
    /// A proof did not check out against the key this side derived.
    BadTag,
    /// The guest does not have the mode the host asked for.
    UnsupportedMode(Mode),
    /// The guest announced no tile size at all.
    NoCommonTileSize,
    /// A channel was offered for a session this end does not have.
    UnknownSession,
    /// A record arrived from a connection that has been replaced.
    StaleGeneration {
        /// Which channel it came in on.
        channel: Channel,
        /// The generation that channel is on.
        expected: u32,
        /// What the record's header carried.
        found: u32,
    },
    /// A channel was offered before the control handshake finished, or by the
    /// end that does not open channels.
    NotEstablished,
}

impl SessionError {
    /// The code to put in the `Error` record this failure is reported with.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Version(_) => ErrorCode::UnsupportedVersion,
            Self::Capability(_) | Self::Field(_) | Self::Decode(_) | Self::Unexpected { .. } => {
                ErrorCode::MalformedRecord
            }
            Self::BadTag => ErrorCode::Unauthenticated,
            Self::UnsupportedMode(_) => ErrorCode::UnsupportedMode,
            Self::NoCommonTileSize => ErrorCode::ResolutionRejected,
            Self::UnknownSession => ErrorCode::UnknownSession,
            Self::StaleGeneration { .. } => ErrorCode::ChannelBindingFailed,
            Self::NotEstablished => ErrorCode::Unauthenticated,
        }
    }
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Version(error) => write!(formatter, "{error}"),
            Self::Capability(error) => write!(formatter, "{error}"),
            Self::Field(error) => write!(formatter, "{error}"),
            Self::Decode(error) => write!(formatter, "a display record is unreadable: {error}"),
            Self::Unexpected {
                channel,
                message_type,
            } => write!(
                formatter,
                "record type {message_type} arrived on the {channel} channel with nothing to answer it"
            ),
            Self::BadTag => {
                formatter.write_str("a display peer could not prove it holds the VM secret")
            }
            Self::UnsupportedMode(mode) => {
                write!(formatter, "the guest does not have display mode {mode:?}")
            }
            Self::NoCommonTileSize => {
                formatter.write_str("the guest announced no tile size this session can use")
            }
            Self::UnknownSession => {
                formatter.write_str("a display channel named a session this end does not have")
            }
            Self::StaleGeneration {
                channel,
                expected,
                found,
            } => write!(
                formatter,
                "a {channel} record from generation {found} arrived while the channel is on {expected}"
            ),
            Self::NotEstablished => {
                formatter.write_str("a display channel was offered before its session was open")
            }
        }
    }
}

impl Error for SessionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Version(error) => Some(error),
            Self::Capability(error) => Some(error),
            Self::Field(error) => Some(error),
            Self::Decode(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        keys::TAG_LEN,
        v1::{ChannelAck, FrameRecord, Ping},
    };

    fn offer() -> Offer {
        Offer {
            capabilities: vec![Capability::CursorStream, Capability::DynamicResolution],
            mode: Mode::Desktop,
            width: 1920,
            height: 1080,
            tile_size: 32,
        }
    }

    fn support() -> Support {
        Support {
            capabilities: vec![Capability::CursorStream],
            modes: vec![Mode::Desktop],
            tile_sizes: vec![16, 32, 64],
            width: 1920,
            height: 1080,
        }
    }

    #[test]
    fn a_channel_key_can_be_derived_before_its_socket_binds() {
        let secret = Secret::generate();
        let (host, guest) = handshake(&secret, offer(), support());

        // `ChannelKey` deliberately does not hand out its bytes, so the two are
        // compared by what they produce: a tag under the same inputs.
        let host_nonce = [1u8; NONCE_LEN];
        let guest_nonce = [2u8; NONCE_LEN];
        let tag = |session: &Session| {
            session.derive_channel_key(Channel::Frame).map(|key| {
                *keys::channel_tag(&key, Role::Guest, Channel::Frame, &host_nonce, &guest_nonce)
                    .as_bytes()
            })
        };

        assert_eq!(
            tag(&guest),
            tag(&host),
            "both ends derive the same key from the same transcript"
        );
        assert!(tag(&guest).is_some(), "and an established session has one");
        assert!(
            guest.derive_channel_key(Channel::Control).is_none(),
            "the control channel establishes sessions rather than binding to one"
        );
    }

    #[test]
    fn no_channel_key_exists_before_the_handshake_finishes() {
        let secret = Secret::generate();
        let guest = Session::guest(&secret, support());

        assert!(guest.derive_channel_key(Channel::Frame).is_none());
    }

    /// Runs the four-record handshake between a host and a guest that hold the
    /// same secret, returning both machines.
    fn handshake(secret: &Secret, offer: Offer, support: Support) -> (Session, Session) {
        let (mut host, client_hello) = Session::host(secret, offer);
        let mut guest = Session::guest(secret, support);

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

    #[test]
    fn a_handshake_leaves_both_ends_agreeing_on_the_session() {
        let (host, guest) = handshake(&Secret::generate(), offer(), support());

        let host_side = host.negotiated().expect("an established host session");
        let guest_side = guest.negotiated().expect("an established guest session");

        assert_eq!(host_side.version, ProtocolVersion::current());
        assert_eq!(host_side.capabilities, vec![Capability::CursorStream]);
        assert_eq!(host_side.mode, Mode::Desktop);
        assert_eq!(host_side.width, 1920);
        assert_eq!(host_side.tile_size, 32);
        assert_eq!(host_side.capabilities, guest_side.capabilities);
        assert_eq!(host_side.tile_size, guest_side.tile_size);
        assert_eq!(host.session_id(), guest.session_id());
    }

    #[test]
    fn the_host_hears_the_guests_proof_before_it_sends_its_own() {
        let secret = Secret::generate();
        let (mut host, client_hello) = Session::host(&secret, offer());
        let mut guest = Session::guest(&secret, support());

        let server_hello = guest
            .handle(&client_hello.header, &client_hello.payload)
            .expect("a well-formed client hello")
            .reply
            .expect("a server hello");
        assert_eq!(
            server_hello.header.message_type,
            ControlRecord::ServerHello as u16
        );

        let server_auth = guest
            .pending_auth()
            .expect("the guest's proof, queued behind its hello");
        assert_eq!(
            server_auth.header.message_type,
            ControlRecord::ServerAuth as u16
        );

        // The host has not established anything yet: it has heard a hello.
        host.handle(&server_hello.header, &server_hello.payload)
            .expect("a well-formed server hello");
        assert!(host.negotiated().is_none());

        let outcome = host
            .handle(&server_auth.header, &server_auth.payload)
            .expect("a valid guest proof");
        assert_eq!(outcome.event, Event::ControlEstablished);
        assert_eq!(
            outcome.reply.expect("the host's proof").header.message_type,
            ControlRecord::ClientAuth as u16
        );
    }

    #[test]
    fn a_guest_that_holds_another_vms_secret_cannot_prove_itself() {
        let (mut host, client_hello) = Session::host(&Secret::generate(), offer());
        let mut guest = Session::guest(&Secret::generate(), support());

        let server_hello = guest
            .handle(&client_hello.header, &client_hello.payload)
            .expect("a well-formed client hello")
            .reply
            .expect("a server hello");
        let server_auth = guest.pending_auth().expect("the guest's proof");

        host.handle(&server_hello.header, &server_hello.payload)
            .expect("a well-formed server hello");

        assert!(matches!(
            host.handle(&server_auth.header, &server_auth.payload),
            Err(SessionError::BadTag)
        ));
        assert!(host.negotiated().is_none());
    }

    #[test]
    fn a_host_that_cannot_prove_itself_leaves_the_guest_unestablished() {
        let secret = Secret::generate();
        let (_, client_hello) = Session::host(&secret, offer());
        let mut guest = Session::guest(&secret, support());

        guest
            .handle(&client_hello.header, &client_hello.payload)
            .expect("a well-formed client hello");
        let _ = guest.pending_auth().expect("the guest's proof");

        let forged = Record::new(
            Channel::Control,
            ControlRecord::ClientAuth as u16,
            1,
            0,
            0,
            ClientAuth {
                tag: vec![0u8; TAG_LEN],
            }
            .encode_to_vec(),
        );

        assert!(matches!(
            guest.handle(&forged.header, &forged.payload),
            Err(SessionError::BadTag)
        ));
        assert!(guest.negotiated().is_none());
    }

    #[test]
    fn a_mode_the_guest_does_not_support_is_refused_with_its_own_code() {
        let secret = Secret::generate();
        let mut wanted = offer();
        wanted.mode = Mode::Motion;

        let (_, client_hello) = Session::host(&secret, wanted);
        let mut guest = Session::guest(&secret, support());

        let error = guest
            .handle(&client_hello.header, &client_hello.payload)
            .expect_err("a mode this guest does not have");

        assert!(matches!(error, SessionError::UnsupportedMode(Mode::Motion)));
        assert_eq!(error.code(), ErrorCode::UnsupportedMode);
    }

    #[test]
    fn auto_resolves_to_desktop_because_that_is_all_the_mvp_guest_has() {
        let secret = Secret::generate();
        let mut wanted = offer();
        wanted.mode = Mode::Auto;

        let (host, guest) = handshake(&secret, wanted, support());

        assert_eq!(host.negotiated().expect("established").mode, Mode::Desktop);
        assert_eq!(guest.negotiated().expect("established").mode, Mode::Desktop);
    }

    #[test]
    fn a_tile_size_the_guest_does_not_have_falls_back_to_one_it_does() {
        let secret = Secret::generate();
        let mut wanted = offer();
        wanted.tile_size = 128;

        let (host, _) = handshake(&secret, wanted, support());

        assert_eq!(host.negotiated().expect("established").tile_size, 64);
    }

    #[test]
    fn a_record_out_of_turn_is_refused() {
        let secret = Secret::generate();
        let (mut host, _) = Session::host(&secret, offer());

        let ping = Record::new(
            Channel::Control,
            ControlRecord::Ping as u16,
            1,
            0,
            0,
            Ping { token: 1 }.encode_to_vec(),
        );

        assert!(matches!(
            host.handle(&ping.header, &ping.payload),
            Err(SessionError::Unexpected { .. })
        ));
    }

    #[test]
    fn a_payload_that_is_not_the_message_its_header_names_is_refused() {
        let secret = Secret::generate();
        let mut guest = Session::guest(&secret, support());

        let nonsense = Record::new(
            Channel::Control,
            ControlRecord::ClientHello as u16,
            0,
            0,
            0,
            vec![0xFF; 16],
        );

        assert!(matches!(
            guest.handle(&nonsense.header, &nonsense.payload),
            Err(SessionError::Decode(_))
        ));
    }

    /// Drives a frame or input channel's three-record exchange to completion.
    fn bind(host: &mut Session, guest: &mut Session, channel: Channel) {
        let hello = host.open_channel(channel).expect("an established session");

        let ack = guest
            .handle(&hello.header, &hello.payload)
            .expect("a well-formed channel hello")
            .reply
            .expect("a channel ack");

        let outcome = host
            .handle(&ack.header, &ack.payload)
            .expect("a valid guest proof");
        let auth = outcome.reply.expect("the host's channel proof");
        assert_eq!(outcome.event, Event::ChannelBound(channel));

        let outcome = guest
            .handle(&auth.header, &auth.payload)
            .expect("a valid host proof");
        assert_eq!(outcome.event, Event::ChannelBound(channel));
        assert!(outcome.reply.is_none());
    }

    #[test]
    fn a_channel_binds_to_the_session_the_control_handshake_established() {
        let (mut host, mut guest) = handshake(&Secret::generate(), offer(), support());

        bind(&mut host, &mut guest, Channel::Frame);
        bind(&mut host, &mut guest, Channel::Input);

        assert!(host.channel_key(Channel::Frame).is_some());
        assert!(guest.channel_key(Channel::Input).is_some());
    }

    #[test]
    fn a_channel_offered_before_the_control_handshake_is_refused() {
        let (mut host, _) = Session::host(&Secret::generate(), offer());

        assert!(matches!(
            host.open_channel(Channel::Frame),
            Err(SessionError::NotEstablished)
        ));
    }

    #[test]
    fn a_channel_hello_naming_another_session_is_refused() {
        let secret = Secret::generate();
        let (mut host, mut guest) = handshake(&secret, offer(), support());

        let hello = host
            .open_channel(Channel::Frame)
            .expect("an established session");
        let mut message = ChannelHello::decode(hello.payload.as_slice()).expect("what was built");
        message.session_id = vec![0xAA; SESSION_ID_LEN];
        let forged = Record::new(
            Channel::Frame,
            FrameRecord::ChannelHello as u16,
            0,
            0,
            0,
            message.encode_to_vec(),
        );

        let error = guest
            .handle(&forged.header, &forged.payload)
            .expect_err("a hello for a session this guest never opened");

        assert!(matches!(error, SessionError::UnknownSession));
        assert_eq!(error.code(), ErrorCode::UnknownSession);
    }

    #[test]
    fn a_channel_hello_from_another_session_does_not_bind() {
        let secret = Secret::generate();
        let (mut host, _) = handshake(&secret, offer(), support());
        let (_, mut other_guest) = handshake(&secret, offer(), support());

        let hello = host
            .open_channel(Channel::Frame)
            .expect("an established session");

        // Another session with another transcript, holding the same VM
        // secret: it refuses by session id before a tag is even reached.
        assert!(matches!(
            other_guest.handle(&hello.header, &hello.payload),
            Err(SessionError::UnknownSession)
        ));
    }

    #[test]
    fn a_forged_channel_ack_does_not_bind_the_channel() {
        let (mut host, _) = handshake(&Secret::generate(), offer(), support());
        let _ = host
            .open_channel(Channel::Frame)
            .expect("an established session");

        let forged = Record::new(
            Channel::Frame,
            FrameRecord::ChannelAck as u16,
            0,
            0,
            0,
            ChannelAck {
                nonce: vec![7u8; NONCE_LEN],
                tag: vec![0u8; TAG_LEN],
            }
            .encode_to_vec(),
        );

        assert!(matches!(
            host.handle(&forged.header, &forged.payload),
            Err(SessionError::BadTag)
        ));
        assert!(host.channel_key(Channel::Frame).is_none());
    }

    #[test]
    fn a_reconnected_channel_runs_at_the_next_generation() {
        let (mut host, mut guest) = handshake(&Secret::generate(), offer(), support());
        bind(&mut host, &mut guest, Channel::Frame);

        assert_eq!(host.generation(Channel::Frame), 0);

        let hello = host
            .reconnect_channel(Channel::Frame)
            .expect("an established session");

        assert_eq!(host.generation(Channel::Frame), 1);
        assert_eq!(hello.header.generation, 1);
    }

    #[test]
    fn a_record_from_a_generation_that_has_been_replaced_is_rejected() {
        let (mut host, mut guest) = handshake(&Secret::generate(), offer(), support());
        bind(&mut host, &mut guest, Channel::Frame);

        let stale = Record::new(
            Channel::Frame,
            FrameRecord::TileDelta as u16,
            9,
            8,
            0,
            vec![1, 2, 3],
        );
        assert!(host.accept(&stale.header).is_ok());

        let _ = host
            .reconnect_channel(Channel::Frame)
            .expect("an established session");

        let error = host
            .accept(&stale.header)
            .expect_err("a record from the previous connection");
        assert!(matches!(
            error,
            SessionError::StaleGeneration {
                channel: Channel::Frame,
                expected: 1,
                found: 0
            }
        ));
    }

    #[test]
    fn the_control_channel_has_no_generations() {
        let (mut host, mut guest) = handshake(&Secret::generate(), offer(), support());
        bind(&mut host, &mut guest, Channel::Frame);
        let _ = host
            .reconnect_channel(Channel::Frame)
            .expect("an established session");

        let ping = Record::new(
            Channel::Control,
            ControlRecord::Ping as u16,
            4,
            0,
            0,
            Ping { token: 1 }.encode_to_vec(),
        );

        assert!(host.accept(&ping.header).is_ok());
    }

    /// Runs a full handshake and returns the host's session, the guest's, and
    /// what a hand-over to another process would carry.
    fn handshaken() -> (Session, Session, HandedOver) {
        let secret = Secret::generate();
        let (mut host, hello) = Session::host(
            &secret,
            Offer {
                capabilities: vec![Capability::CursorStream],
                mode: Mode::Auto,
                width: 1920,
                height: 1080,
                tile_size: 32,
            },
        );
        let mut guest = Session::guest(
            &secret,
            Support {
                capabilities: vec![Capability::CursorStream],
                modes: vec![Mode::Desktop],
                tile_sizes: vec![16, 32, 64],
                width: 1920,
                height: 1080,
            },
        );

        let server_hello = guest
            .handle(&hello.header, &hello.payload)
            .expect("a hello this guest can answer")
            .reply
            .expect("the guest answers a hello");
        let server_auth = guest.pending_auth().expect("the guest queued its proof");

        host.handle(&server_hello.header, &server_hello.payload)
            .expect("an answer this host offered");
        let client_auth = host
            .handle(&server_auth.header, &server_auth.payload)
            .expect("a proof this host can check")
            .reply
            .expect("the host answers with its own proof");
        guest
            .handle(&client_auth.header, &client_auth.payload)
            .expect("a proof this guest can check");

        let handed_over = HandedOver {
            session_id: *host.session_id(),
            negotiated: host.negotiated().expect("an established host").clone(),
            frame_key: host
                .derive_channel_key(Channel::Frame)
                .expect("an established host"),
            input_key: host
                .derive_channel_key(Channel::Input)
                .expect("an established host"),
            control_sequence: host.control_sequence(),
        };

        (host, guest, handed_over)
    }

    /// Drives one channel bind between a handed-over host and a guest.
    fn bind_from_hello(host: &mut Session, guest: &mut Session, hello: Record) -> Event {
        let ack = guest
            .handle(&hello.header, &hello.payload)
            .expect("a channel hello this guest can answer")
            .reply
            .expect("the guest answers a channel hello");
        let outcome = host
            .handle(&ack.header, &ack.payload)
            .expect("an ack this host can check");
        let auth = outcome.reply.expect("the host answers with its own proof");
        guest
            .handle(&auth.header, &auth.payload)
            .expect("a proof this guest can check");

        outcome.event
    }

    #[test]
    fn a_handed_over_session_binds_its_channels_without_the_secret() {
        let (_, mut guest, handed_over) = handshaken();
        let mut viewer = Session::established_host(handed_over);

        let hello = viewer
            .open_channel(Channel::Frame)
            .expect("an established host opens channels");
        assert_eq!(
            bind_from_hello(&mut viewer, &mut guest, hello),
            Event::ChannelBound(Channel::Frame)
        );

        let hello = viewer
            .open_channel(Channel::Input)
            .expect("an established host opens channels");
        assert_eq!(
            bind_from_hello(&mut viewer, &mut guest, hello),
            Event::ChannelBound(Channel::Input)
        );
    }

    #[test]
    fn a_handed_over_session_reconnects_a_channel_at_the_next_generation() {
        let (_, mut guest, handed_over) = handshaken();
        let mut viewer = Session::established_host(handed_over);

        let hello = viewer.open_channel(Channel::Frame).expect("generation 0");
        bind_from_hello(&mut viewer, &mut guest, hello);
        assert_eq!(viewer.generation(Channel::Frame), 0);

        // The key survives the reconnect: it was handed over rather than
        // derived, and there is no session key here to derive it again from.
        let hello = viewer
            .reconnect_channel(Channel::Frame)
            .expect("a channel may be replaced");
        assert_eq!(viewer.generation(Channel::Frame), 1);
        assert_eq!(
            bind_from_hello(&mut viewer, &mut guest, hello),
            Event::ChannelBound(Channel::Frame)
        );
    }

    #[test]
    fn a_handed_over_session_refuses_a_record_from_the_generation_it_replaced() {
        let (_, mut guest, handed_over) = handshaken();
        let mut viewer = Session::established_host(handed_over);

        let hello = viewer.open_channel(Channel::Frame).expect("generation 0");
        bind_from_hello(&mut viewer, &mut guest, hello);
        let hello = viewer
            .reconnect_channel(Channel::Frame)
            .expect("a channel may be replaced");
        bind_from_hello(&mut viewer, &mut guest, hello);

        let stale = Record::new(
            Channel::Frame,
            FrameRecord::Keyframe as u16,
            0,
            0,
            0,
            vec![],
        );
        assert!(matches!(
            viewer.accept(&stale.header),
            Err(SessionError::StaleGeneration {
                expected: 1,
                found: 0,
                ..
            })
        ));
    }

    #[test]
    fn a_handed_over_session_carries_on_the_control_numbering_it_was_given() {
        let (host, _, handed_over) = handshaken();
        // The host wrote a `ClientHello` and a `ClientAuth`, so the next
        // control record it would write is sequence 2.
        assert_eq!(host.control_sequence(), 2);

        let mut viewer = Session::established_host(handed_over);
        let ping = viewer.control_record(ControlRecord::Ping, Vec::new());

        assert_eq!(ping.header.sequence, 2);
        assert_eq!(viewer.control_sequence(), 3);
    }
}
