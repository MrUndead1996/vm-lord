//! The states a display session moves through, without a socket in sight.
//!
//! One machine with two roles rather than a set of functions each end calls in
//! its own order. The agent protocol can leave its sequence to its callers
//! because its handshake is one exchange over one socket; this one is three
//! sockets, two directions of proof, a transcript hash and a channel binding,
//! and if the guest services and the viewer each wrote their own half they
//! would drift over exactly what must not drift: what goes into the hash, and
//! in what order.

use std::{error::Error, fmt};

use prost::Message;

use crate::{
    handshake::{self, UnofferedCapability, VersionMismatch},
    keys::{
        self, NONCE_LEN, Role, SESSION_ID_LEN, Secret, SessionKey, Tag, Transcript, WrongLength,
    },
    record::{Channel, Header, Record},
    v1::{
        Capability, ClientAuth, ClientHello, ControlRecord, ErrorCode, Mode, ProtocolVersion,
        ServerAuth, ServerHello,
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
    secret: Secret,
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
}

impl Session {
    /// Opens a session as the host, returning the `ClientHello` to send.
    #[must_use]
    pub fn host(secret: &Secret, offer: Offer) -> (Self, Record) {
        Self::host_with_randomness(
            secret,
            offer,
            keys::random_bytes(),
            keys::random_bytes(),
        )
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
            secret: secret.duplicate(),
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
        };

        let record = session.control_record(ControlRecord::ClientHello, payload);
        (session, record)
    }

    /// The same as [`Session::guest`], with the nonce supplied. Golden vectors
    /// only, for the reason [`Session::host_with_randomness`] gives.
    #[doc(hidden)]
    #[must_use]
    pub fn guest_with_randomness(secret: &Secret, support: Support, nonce: [u8; NONCE_LEN]) -> Self {
        Self {
            role: Role::Guest,
            state: State::AwaitingClientHello,
            secret: secret.duplicate(),
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
        }
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
        let capabilities = handshake::agreed_capabilities(&support.capabilities, &hello.capabilities);

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
        let key = self.session_key.as_ref().expect("the key derivation set it");

        let proof = ServerAuth {
            tag: keys::control_tag(key, Role::Guest, &hash).as_bytes().to_vec(),
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

        let version =
            handshake::confirm_version(ProtocolVersion::current(), answer.version.unwrap_or_default())
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
            tag: keys::control_tag(key, Role::Host, &hash).as_bytes().to_vec(),
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

    /// Finishes the transcript and derives the key both proofs are made under.
    fn derive_session_key(&mut self) {
        let hash = self.transcript.finish();
        let key = keys::session_key(
            &self.secret,
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
            Self::BadTag => formatter.write_str("a display peer could not prove it holds the VM secret"),
            Self::UnsupportedMode(mode) => {
                write!(formatter, "the guest does not have display mode {mode:?}")
            }
            Self::NoCommonTileSize => {
                formatter.write_str("the guest announced no tile size this session can use")
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
    use crate::{keys::TAG_LEN, v1::Ping};

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
}
