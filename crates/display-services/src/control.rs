//! The control channel, which is the only socket the privileged process reads.
//!
//! It holds the VM's secret and [`Session::guest`], and what it passes on is a
//! channel key -- never the secret, and never a socket. The frame and input
//! descriptors are opened by the unprivileged process and are never seen here.
//!
//! The protocol crate's state machine owns the four handshake records. What is
//! owned here is everything after them: a session that is established has no
//! more state transitions, only requests to answer, and a request this build
//! cannot grant is refused without ending the session.

use std::io::{Read, Write};

use prost::Message as _;
use vmlord_display_protocol::{
    keys::Secret,
    record::{self, Channel, Limits, Record, RecordError},
    session::{Event, Session, Support},
    v1::{
        Capability, ControlRecord, DisplayState, ErrorCode, Mode, Ping, Pong, SetMode,
        SetResolution,
    },
};

use crate::ipc::{Message, SessionParameters};

/// What handling one control record means for the unprivileged process.
#[derive(Debug)]
pub enum Outcome {
    /// A session opened, and here is what the other process needs to bind its
    /// two sockets.
    Opened(SessionParameters),
    /// Something to pass straight on.
    Relay(Message),
    /// The session is over, for the reason given. Fit for a journal, not for a
    /// decision.
    Closed(String),
    /// Nothing the other process needs to know.
    Nothing,
}

/// The support this build actually has, at the geometry the output is on.
///
/// Facts rather than wishes: one mode, because motion is another task's, and
/// the three tile sizes the codec implements.
#[must_use]
pub fn support_from(width: u32, height: u32) -> Support {
    Support {
        capabilities: vec![Capability::CursorStream, Capability::DynamicResolution],
        // Motion is not a mode this build has. Announcing it and then encoding
        // a desktop would be worse than refusing it.
        modes: vec![Mode::Desktop],
        tile_sizes: vec![16, 32, 64],
        width,
        height,
    }
}

/// The control channel's state machine, plus the secret it is keyed on.
pub struct Control {
    session: Session,
    /// The geometry the output is on, which is what a `DisplayState` reports.
    width: u32,
    height: u32,
    tile_size: u32,
    /// Whether the four handshake records are behind us. After them the session
    /// takes no more records, and what arrives is a request.
    established: bool,
    /// The next sequence for a record this side writes on its own account.
    sequence: u32,
    limits: Limits,
}

impl Control {
    /// A control channel for one connection.
    #[must_use]
    pub fn new(secret: &Secret, support: Support) -> Self {
        let limits = Limits::new(support.width, support.height);
        let (width, height) = (support.width, support.height);

        Self {
            session: Session::guest(secret, support),
            width,
            height,
            // Until the handshake settles one, this is what the codec defaults
            // to and what a `DisplayState` before then would report.
            tile_size: 32,
            established: false,
            sequence: 0,
            limits,
        }
    }

    /// Reads one record, answers it, and says what it meant.
    ///
    /// Never returns an error: every failure this can meet is a session that
    /// has ended, and the caller's job is to log the reason and start listening
    /// again rather than to decide between them.
    pub fn pump<S: Read + Write>(&mut self, stream: &mut S) -> Outcome {
        let mut payload = Vec::new();
        let header = match record::read(stream, &self.limits, &mut payload) {
            Ok(header) => header,
            // A timed-out read is a host with nothing to say, which is the
            // ordinary state of an idle desktop.
            Err(RecordError::Idle) => return Outcome::Nothing,
            Err(RecordError::Closed) => {
                return Outcome::Closed("the host closed the control connection".to_owned());
            }
            Err(error) => return Outcome::Closed(error.to_string()),
        };

        if self.established && header.channel == Channel::Control {
            return self.request(stream, header.message_type, &payload);
        }

        match self.session.handle(&header, &payload) {
            Ok(outcome) => {
                if let Some(reply) = outcome.reply
                    && let Err(error) = self.send(stream, &reply)
                {
                    return Outcome::Closed(error.to_string());
                }
                // The guest's own proof is queued rather than returned, because
                // it answers the hello it just sent rather than a record.
                if let Some(auth) = self.session.pending_auth()
                    && let Err(error) = self.send(stream, &auth)
                {
                    return Outcome::Closed(error.to_string());
                }

                if outcome.event == Event::ControlEstablished {
                    self.established = true;

                    return self.opened();
                }

                Outcome::Nothing
            }
            Err(error) => {
                self.report(stream, error.code(), &error.to_string());

                Outcome::Closed(error.to_string())
            }
        }
    }

    /// What the unprivileged process is handed once a session exists.
    ///
    /// Two channel keys and a geometry. Not the secret, and not a descriptor:
    /// what a compromised capture process could take from these bytes is one
    /// session, and only while that session runs.
    fn opened(&mut self) -> Outcome {
        let (Some(frame), Some(input), Some(negotiated)) = (
            self.session.derive_channel_key(Channel::Frame),
            self.session.derive_channel_key(Channel::Input),
            self.session.negotiated(),
        ) else {
            return Outcome::Closed("an established session with no keys".to_owned());
        };

        self.width = negotiated.width;
        self.height = negotiated.height;
        self.tile_size = negotiated.tile_size;
        let cursor_stream = negotiated.capabilities.contains(&Capability::CursorStream);
        self.limits.set_geometry(self.width, self.height);

        Outcome::Opened(SessionParameters {
            session_id: self.session.session_id().to_vec(),
            frame_key: frame.to_bytes().to_vec(),
            input_key: input.to_bytes().to_vec(),
            width: self.width,
            height: self.height,
            tile_size: self.tile_size,
            cursor_stream,
        })
    }

    /// One request on an established session.
    fn request<S: Read + Write>(
        &mut self,
        stream: &mut S,
        message_type: u16,
        payload: &[u8],
    ) -> Outcome {
        match ControlRecord::try_from(i32::from(message_type)) {
            Ok(ControlRecord::Ping) => {
                let token = Ping::decode(payload).map(|ping| ping.token).unwrap_or(0);
                self.write(stream, ControlRecord::Pong, Pong { token }.encode_to_vec());

                Outcome::Nothing
            }
            Ok(ControlRecord::RequestKeyframe) => Outcome::Relay(Message::KeyframeRequested),
            Ok(ControlRecord::SetMode) => {
                let wanted = SetMode::decode(payload)
                    .ok()
                    .and_then(|set| Mode::try_from(set.mode).ok())
                    .unwrap_or_default();
                // Auto resolves to the one mode this build has; anything else
                // named explicitly is refused, and the session carries on.
                if matches!(wanted, Mode::Desktop | Mode::Auto) {
                    self.state(stream);
                } else {
                    self.report(
                        stream,
                        ErrorCode::UnsupportedMode,
                        "this build encodes desktop and nothing else",
                    );
                }

                Outcome::Nothing
            }
            Ok(ControlRecord::SetResolution) => {
                // Changing the output's mode is another task's. What is
                // answered here is the geometry that is actually on, because
                // reporting the one that was asked for would be a lie a viewer
                // would then scale against.
                let _ = SetResolution::decode(payload);
                self.state(stream);

                Outcome::Nothing
            }
            Ok(ControlRecord::EndSession) => {
                Outcome::Closed("the host ended the session".to_owned())
            }
            _ => {
                self.report(
                    stream,
                    ErrorCode::MalformedRecord,
                    "a control record this build does not answer",
                );

                Outcome::Nothing
            }
        }
    }

    /// Reports the geometry that is actually on.
    fn state<S: Read + Write>(&mut self, stream: &mut S) {
        let state = DisplayState {
            width: self.width,
            height: self.height,
            tile_size: self.tile_size,
            mode: Mode::Desktop as i32,
        };
        self.write(stream, ControlRecord::DisplayState, state.encode_to_vec());
    }

    /// Writes an `Error` record, for a request refused or a fault to report.
    ///
    /// Public because a fault the broker meets is not always one this module
    /// saw: a capture that fails is discovered by another thread, and the host
    /// is owed the reason on the one socket it is listening to.
    pub fn report<S: Read + Write>(&mut self, stream: &mut S, code: ErrorCode, detail: &str) {
        let error = vmlord_display_protocol::v1::Error {
            code: code as i32,
            detail: detail.to_owned(),
        };
        self.write(stream, ControlRecord::Error, error.encode_to_vec());
    }

    /// One record of this side's own, at this side's next sequence.
    ///
    /// A failed write is not reported: every caller is already answering
    /// something, and a control socket that cannot be written to is one the
    /// next read will report as closed.
    fn write<S: Read + Write>(
        &mut self,
        stream: &mut S,
        message_type: ControlRecord,
        payload: Vec<u8>,
    ) {
        let record = Record::new(
            Channel::Control,
            message_type as u16,
            self.sequence,
            0,
            0,
            payload,
        );
        let _ = self.send(stream, &record);
    }

    /// Writes a record and keeps this side's sequence ahead of it.
    fn send<S: Read + Write>(
        &mut self,
        stream: &mut S,
        record: &Record,
    ) -> Result<(), RecordError> {
        record::write(stream, record, &self.limits)?;
        self.sequence = self.sequence.max(record.header.sequence).wrapping_add(1);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Read, Write};

    use prost::Message as _;
    use vmlord_display_protocol::{
        keys::Secret,
        record::{self, Channel, Limits, Record},
        session::{Offer, Session},
        v1::{
            Capability, ControlRecord, DisplayState, EndSession, ErrorCode, Mode, Ping,
            RequestKeyframe, SetMode, SetResolution,
        },
    };

    use super::{Control, Outcome, support_from};

    fn limits() -> Limits {
        Limits::new(1920, 1080)
    }

    /// An in-memory duplex: what the host wrote is read, what the broker writes
    /// is kept.
    #[derive(Default)]
    struct Duplex {
        incoming: Vec<u8>,
        read_from: usize,
        outgoing: Vec<u8>,
    }

    impl Duplex {
        /// Puts a record where the broker will read it.
        fn offer(&mut self, record: &Record) {
            record::write(&mut self.incoming, record, &limits()).expect("a small record");
        }

        /// Takes everything the broker has written since the last call.
        fn taken(&mut self) -> Vec<(u16, Vec<u8>)> {
            let bytes = std::mem::take(&mut self.outgoing);
            let mut reader = bytes.as_slice();
            let mut payload = Vec::new();
            let mut records = Vec::new();
            while !reader.is_empty() {
                let header = record::read(&mut reader, &limits(), &mut payload).expect("a record");
                records.push((header.message_type, payload.clone()));
            }

            records
        }
    }

    impl Read for Duplex {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let available = &self.incoming[self.read_from..];
            let taken = available.len().min(buffer.len());
            buffer[..taken].copy_from_slice(&available[..taken]);
            self.read_from += taken;

            Ok(taken)
        }
    }

    impl Write for Duplex {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.outgoing.extend_from_slice(bytes);

            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn offer() -> Offer {
        Offer {
            capabilities: vec![Capability::CursorStream],
            mode: Mode::Desktop,
            width: 1920,
            height: 1080,
            tile_size: 32,
        }
    }

    /// Runs the four control records between a real host and a `Control`,
    /// returning what the broker would hand on and the machinery to go further.
    fn opened() -> (SessionParametersOrNothing, Session, Control, Duplex) {
        let secret = Secret::generate();
        let (mut host, client_hello) = Session::host(&secret, offer());
        let mut control = Control::new(&secret, support_from(1920, 1080));
        let mut wire = Duplex::default();

        wire.offer(&client_hello);
        // The server hello, and then the guest's own proof, which the broker
        // writes without being asked for it.
        let mut parameters = None;
        for _ in 0..2 {
            if let Outcome::Opened(opened) = control.pump(&mut wire) {
                parameters = Some(opened);
            }
            for (message_type, payload) in wire.taken() {
                let header = Record::new(Channel::Control, message_type, 0, 0, 0, payload);
                if let Ok(outcome) = host.handle(&header.header, &header.payload)
                    && let Some(reply) = outcome.reply
                {
                    wire.offer(&reply);
                }
            }
        }

        (parameters, host, control, wire)
    }

    type SessionParametersOrNothing = Option<crate::ipc::SessionParameters>;

    /// One record after the handshake, and what the broker made of it.
    fn drive(record: Record) -> Outcome {
        let (_, _, mut control, mut wire) = opened();
        wire.offer(&record);

        control.pump(&mut wire)
    }

    /// One record after the handshake, and the error code the broker answered
    /// with, if it answered with one.
    fn drive_control(record: Record) -> Option<ErrorCode> {
        let (_, _, mut control, mut wire) = opened();
        wire.offer(&record);
        let _ = control.pump(&mut wire);

        wire.taken()
            .into_iter()
            .find_map(|(message_type, payload)| {
                (message_type == ControlRecord::Error as u16).then(|| {
                    ErrorCode::try_from(
                        vmlord_display_protocol::v1::Error::decode(payload.as_slice())
                            .expect("an error record")
                            .code,
                    )
                    .expect("a known code")
                })
            })
    }

    /// One record after the handshake, and the state the broker reported.
    fn drive_control_for_state(record: Record) -> DisplayState {
        let (_, _, mut control, mut wire) = opened();
        wire.offer(&record);
        let _ = control.pump(&mut wire);

        wire.taken()
            .into_iter()
            .find_map(|(message_type, payload)| {
                (message_type == ControlRecord::DisplayState as u16)
                    .then(|| DisplayState::decode(payload.as_slice()).expect("a display state"))
            })
            .expect("a display state")
    }

    fn control_record(message_type: ControlRecord, payload: Vec<u8>) -> Record {
        Record::new(Channel::Control, message_type as u16, 4, 0, 0, payload)
    }

    fn control_record_set_mode(mode: Mode) -> Record {
        control_record(
            ControlRecord::SetMode,
            SetMode { mode: mode as i32 }.encode_to_vec(),
        )
    }

    fn control_record_set_resolution(width: u32, height: u32) -> Record {
        control_record(
            ControlRecord::SetResolution,
            SetResolution { width, height }.encode_to_vec(),
        )
    }

    fn control_record_ping(token: u64) -> Record {
        control_record(ControlRecord::Ping, Ping { token }.encode_to_vec())
    }

    fn control_record_request_keyframe() -> Record {
        control_record(
            ControlRecord::RequestKeyframe,
            RequestKeyframe {}.encode_to_vec(),
        )
    }

    fn control_record_end_session() -> Record {
        control_record(ControlRecord::EndSession, EndSession {}.encode_to_vec())
    }

    /// A host that hung up at a record boundary.
    fn drive_on_closed_stream() -> Outcome {
        let (_, _, mut control, mut wire) = opened();

        control.pump(&mut wire)
    }

    #[test]
    fn a_completed_handshake_yields_the_keys_the_other_process_needs() {
        // Host and broker over an in-memory duplex; the assertions are about
        // what the broker hands on, not about the handshake, which the protocol
        // crate already proves.
        let (parameters, host, _, _) = opened();
        let parameters = parameters.expect("the handshake finished");

        assert_eq!(parameters.session_id.len(), 16);
        assert_eq!(parameters.frame_key.len(), 32);
        assert_ne!(
            parameters.frame_key, parameters.input_key,
            "a channel key is per channel, so one socket's key never opens the other"
        );
        assert!(parameters.cursor_stream);
        assert!(host.negotiated().is_some(), "and the host agrees it opened");
    }

    #[test]
    fn the_guest_offers_one_mode_and_refuses_the_other() {
        let support = support_from(1920, 1080);
        assert_eq!(support.modes, vec![Mode::Desktop]);
        assert!(support.capabilities.contains(&Capability::CursorStream));
        assert!(
            support
                .capabilities
                .contains(&Capability::DynamicResolution)
        );
    }

    #[test]
    fn set_mode_motion_is_refused_without_ending_the_session() {
        let error = drive_control(control_record_set_mode(Mode::Motion));
        assert_eq!(error, Some(ErrorCode::UnsupportedMode));
    }

    #[test]
    fn set_resolution_answers_with_what_is_actually_applied() {
        let state = drive_control_for_state(control_record_set_resolution(2560, 1440));
        assert_eq!(
            (state.width, state.height),
            (1920, 1080),
            "applying a resolution is another task's; saying it was applied would be a lie"
        );
    }

    #[test]
    fn a_ping_is_answered_without_waking_capture() {
        let outcome = drive(control_record_ping(9));
        assert!(matches!(outcome, Outcome::Nothing));
    }

    #[test]
    fn a_keyframe_request_is_relayed() {
        assert!(matches!(
            drive(control_record_request_keyframe()),
            Outcome::Relay(crate::ipc::Message::KeyframeRequested)
        ));
    }

    #[test]
    fn end_session_closes_the_session() {
        assert!(matches!(
            drive(control_record_end_session()),
            Outcome::Closed(_)
        ));
    }

    #[test]
    fn a_hung_up_host_closes_the_session() {
        // A peer that closed at a record boundary is how a session ends and is
        // not a fault, but it is still the end.
        assert!(matches!(drive_on_closed_stream(), Outcome::Closed(_)));
    }
}
