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
        Capability, ControlRecord, DisplayState, DisplayTiming, ErrorCode, Mode, Ping, Pong,
        SetAvailableModes, SetDisplayMode, SetMode, SetResolution,
    },
};

use crate::{
    ipc::{Message, SessionParameters},
    output,
};

/// What handling one control record means for the unprivileged process.
#[derive(Debug)]
pub enum Outcome {
    /// A session opened: what the capture process needs to bind its two
    /// sockets, and the clipboard key its daemon needs to bind the fourth.
    ///
    /// The two travel together because they are one fact, and they are separate
    /// values because they go to separate processes: the capture process never
    /// sees the clipboard key, and the clipboard daemon never sees the others.
    Opened(SessionParameters, Vec<u8>),
    /// Something to pass straight on.
    Relay(Message),
    /// The host asked the output to change size, and the size is one this
    /// build will drive. Whether the compositor moves is not answered here.
    Resize {
        /// The width to ask the module for.
        width: u32,
        /// The height.
        height: u32,
    },
    /// The host published the modes its own monitor drives, and every one of
    /// them is a mode this output builds.
    ///
    /// The list and the selection travel together because the module has to be
    /// told them in that order: a mode marked preferred while the connector is
    /// still offering the old list is a hotplug onto a mode about to be
    /// withdrawn.
    AvailableModes {
        /// The whole list to offer, never empty.
        modes: Vec<DisplayTiming>,
        /// The one to mark preferred, when the host named one.
        preferred: Option<DisplayTiming>,
    },
    /// The host chose one of the modes it published.
    DisplayMode(DisplayTiming),
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
        capabilities: vec![
            Capability::CursorStream,
            Capability::DynamicResolution,
            // Announced by the build rather than by an attached daemon: a
            // session commonly opens at the login screen, where no user session
            // and therefore no daemon exists yet, and a capability settled in
            // the handshake cannot be renegotiated when one appears. With no
            // daemon attached the guest simply offers nothing.
            Capability::Clipboard,
            Capability::FileClipboard,
            // The connector's mode list is the host monitor's, and this build
            // is the one that replaces it. Announced here so that a host on an
            // older protocol revision never sends a record this cannot apply.
            Capability::HostDisplayModes,
        ],
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
    /// The refresh the compositor committed, zero until one has been seen.
    refresh_hz: u32,
    tile_size: u32,
    /// Whether the host negotiated the mode-list capability.
    ///
    /// A record it did not negotiate is one this build must not act on, however
    /// well formed: the two sides agreed on what this session speaks.
    host_modes: bool,
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
            refresh_hz: 0,
            // Until the handshake settles one, this is what the codec defaults
            // to and what a `DisplayState` before then would report.
            tile_size: 32,
            host_modes: false,
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

    /// What the unprivileged processes are handed once a session exists.
    ///
    /// Channel keys and a geometry. Not the secret, and not a descriptor: what
    /// a compromised capture process could take from these bytes is one
    /// session, and only while that session runs.
    fn opened(&mut self) -> Outcome {
        let (Some(frame), Some(input), Some(clipboard), Some(negotiated)) = (
            self.session.derive_channel_key(Channel::Frame),
            self.session.derive_channel_key(Channel::Input),
            self.session.derive_channel_key(Channel::Clipboard),
            self.session.negotiated(),
        ) else {
            return Outcome::Closed("an established session with no keys".to_owned());
        };

        self.width = negotiated.width;
        self.height = negotiated.height;
        self.tile_size = negotiated.tile_size;
        let cursor_stream = negotiated.capabilities.contains(&Capability::CursorStream);
        self.host_modes = negotiated
            .capabilities
            .contains(&Capability::HostDisplayModes);
        self.limits.set_geometry(self.width, self.height);

        Outcome::Opened(
            SessionParameters {
                session_id: self.session.session_id().to_vec(),
                frame_key: frame.to_bytes().to_vec(),
                input_key: input.to_bytes().to_vec(),
                width: self.width,
                height: self.height,
                tile_size: self.tile_size,
                cursor_stream,
            },
            clipboard.to_bytes().to_vec(),
        )
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
                let wanted = SetResolution::decode(payload).ok();
                let admissible = wanted
                    .as_ref()
                    .and_then(|set| output::admissible(set.width, set.height));

                match admissible {
                    // Nothing is reported back here. The geometry that is
                    // actually on is what a `DisplayState` may carry, and it
                    // is not known until the compositor has committed a mode
                    // and a framebuffer of the new size has been seen; saying
                    // anything sooner would be a size the viewer would then
                    // scale against.
                    Some((width, height)) => Outcome::Resize { width, height },
                    None => {
                        self.report(
                            stream,
                            ErrorCode::ResolutionRejected,
                            "a size outside what this output drives",
                        );

                        Outcome::Nothing
                    }
                }
            }
            Ok(ControlRecord::SetAvailableModes) if self.host_modes => {
                let Ok(set) = SetAvailableModes::decode(payload) else {
                    self.report(
                        stream,
                        ErrorCode::MalformedRecord,
                        "an unreadable mode list",
                    );

                    return Outcome::Nothing;
                };
                // Counted before anything is validated: a host that named more
                // modes than the module holds is refused on the record rather
                // than after a list has been built out of it.
                if set.modes.len() > output::MAX_MODES {
                    self.report(
                        stream,
                        ErrorCode::ResolutionRejected,
                        "more modes than this output offers",
                    );

                    return Outcome::Nothing;
                }
                // The whole update or none of it. A list with one mode this
                // output cannot build is a host that disagrees about the
                // contract, and applying the rest would hide that.
                if !set.modes.iter().all(output::drivable) {
                    self.report(
                        stream,
                        ErrorCode::ResolutionRejected,
                        "a mode outside what this output drives",
                    );

                    return Outcome::Nothing;
                }

                let modes = if set.modes.is_empty() {
                    // A host whose enumeration found nothing still has a
                    // window, and a connector with no modes is one no
                    // compositor lights.
                    vec![output::FALLBACK]
                } else {
                    set.modes
                };
                let preferred = set.preferred.filter(output::drivable);

                Outcome::AvailableModes { modes, preferred }
            }
            Ok(ControlRecord::SetDisplayMode) if self.host_modes => {
                let wanted = SetDisplayMode::decode(payload)
                    .ok()
                    .and_then(|set| set.mode)
                    .filter(output::drivable);

                match wanted {
                    Some(mode) => Outcome::DisplayMode(mode),
                    None => {
                        self.report(
                            stream,
                            ErrorCode::ResolutionRejected,
                            "a mode outside what this output drives",
                        );

                        Outcome::Nothing
                    }
                }
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

    /// Moves the geometry this session reports to what the output came up at.
    ///
    /// The record caps move with it: a taller output is a bigger keyframe, and
    /// the caps are what say how big a record may be.
    pub fn set_geometry(&mut self, width: u32, height: u32, refresh_hz: u32) {
        self.width = width;
        self.height = height;
        self.refresh_hz = refresh_hz;
        self.limits.set_geometry(width, height);
    }

    /// The geometry this session is on, and the refresh that came up with it.
    #[must_use]
    pub fn geometry(&self) -> (u32, u32, u32) {
        (self.width, self.height, self.refresh_hz)
    }

    /// Whether this session negotiated the host's mode list.
    #[must_use]
    pub fn host_modes(&self) -> bool {
        self.host_modes
    }

    /// Reports the geometry that is actually on.
    pub fn state<S: Read + Write>(&mut self, stream: &mut S) {
        let state = DisplayState {
            width: self.width,
            height: self.height,
            tile_size: self.tile_size,
            mode: Mode::Desktop as i32,
            // Zero while nothing has been committed: a refresh is only known
            // once the compositor has settled on one of the offered modes.
            refresh_hz: self.refresh_hz,
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
            Capability, ControlRecord, DisplayState, DisplayTiming, EndSession, ErrorCode, Mode,
            Ping, RequestKeyframe, SetAvailableModes, SetDisplayMode, SetMode, SetResolution,
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
            if let Outcome::Opened(opened, _) = control.pump(&mut wire) {
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

        state_from(&mut wire)
    }

    /// The `DisplayState` a duplex has been written, if there is one.
    fn state_from(wire: &mut Duplex) -> DisplayState {
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

    fn timing(width: u32, height: u32, refresh_hz: u32) -> DisplayTiming {
        DisplayTiming {
            width,
            height,
            refresh_hz,
        }
    }

    fn control_record_available_modes(
        modes: Vec<DisplayTiming>,
        preferred: Option<DisplayTiming>,
    ) -> Record {
        control_record(
            ControlRecord::SetAvailableModes,
            SetAvailableModes { modes, preferred }.encode_to_vec(),
        )
    }

    fn control_record_display_mode(mode: DisplayTiming) -> Record {
        control_record(
            ControlRecord::SetDisplayMode,
            SetDisplayMode { mode: Some(mode) }.encode_to_vec(),
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

    /// One record on a session whose host asked for the mode-list capability.
    fn drive_with_host_modes(record: Record) -> (Outcome, Option<ErrorCode>) {
        let secret = Secret::generate();
        let (mut host, client_hello) = Session::host(
            &secret,
            Offer {
                capabilities: vec![Capability::CursorStream, Capability::HostDisplayModes],
                ..offer()
            },
        );
        let mut control = Control::new(&secret, support_from(1920, 1080));
        let mut wire = Duplex::default();

        wire.offer(&client_hello);
        for _ in 0..2 {
            let _ = control.pump(&mut wire);
            for (message_type, payload) in wire.taken() {
                let header = Record::new(Channel::Control, message_type, 0, 0, 0, payload);
                if let Ok(outcome) = host.handle(&header.header, &header.payload)
                    && let Some(reply) = outcome.reply
                {
                    wire.offer(&reply);
                }
            }
        }
        assert!(control.host_modes(), "the capability was negotiated");

        wire.offer(&record);
        let outcome = control.pump(&mut wire);
        let error = wire
            .taken()
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
            });

        (outcome, error)
    }

    #[test]
    fn a_mode_list_from_a_host_that_did_not_ask_for_the_capability_is_not_answered() {
        // The two sides agreed what this session speaks. A record outside that
        // is one this build must not act on, however well formed it is.
        let error = drive_control(control_record_available_modes(
            vec![timing(1920, 1080, 60)],
            None,
        ));

        assert_eq!(error, Some(ErrorCode::MalformedRecord));
    }

    #[test]
    fn a_published_list_arrives_with_its_selection() {
        let (outcome, error) = drive_with_host_modes(control_record_available_modes(
            vec![timing(1280, 720, 60), timing(1920, 1080, 144)],
            Some(timing(1920, 1080, 144)),
        ));

        assert_eq!(error, None);
        assert!(matches!(
            outcome,
            Outcome::AvailableModes { ref modes, preferred: Some(preferred) }
                if modes.len() == 2 && preferred == timing(1920, 1080, 144)
        ));
    }

    #[test]
    fn one_mode_this_output_cannot_drive_refuses_the_whole_list() {
        // Applying the rest would be a guest quietly offering something other
        // than what the host published, which is a disagreement about the
        // contract rather than a mode to drop.
        let (outcome, error) = drive_with_host_modes(control_record_available_modes(
            vec![timing(1920, 1080, 60), timing(3840, 2160, 60)],
            None,
        ));

        assert_eq!(error, Some(ErrorCode::ResolutionRejected));
        assert!(matches!(outcome, Outcome::Nothing));
    }

    #[test]
    fn more_modes_than_the_module_holds_are_refused_before_a_list_is_built() {
        let modes = (0..=super::output::MAX_MODES)
            .map(|step| timing(640 + step as u32 * 8, 480, 60))
            .collect();
        let (outcome, error) = drive_with_host_modes(control_record_available_modes(modes, None));

        assert_eq!(error, Some(ErrorCode::ResolutionRejected));
        assert!(matches!(outcome, Outcome::Nothing));
    }

    #[test]
    fn a_host_that_enumerated_nothing_still_leaves_the_connector_a_mode() {
        // A connector with no modes is an output no compositor lights, and a
        // host whose monitor would not answer still has a window.
        let (outcome, error) = drive_with_host_modes(control_record_available_modes(vec![], None));

        assert_eq!(error, None);
        assert!(matches!(
            outcome,
            Outcome::AvailableModes { ref modes, preferred: None }
                if modes == &vec![super::output::FALLBACK]
        ));
    }

    #[test]
    fn a_selected_mode_is_taken_and_an_impossible_one_is_refused() {
        let (outcome, error) =
            drive_with_host_modes(control_record_display_mode(timing(1280, 720, 120)));
        assert_eq!(error, None);
        assert!(matches!(outcome, Outcome::DisplayMode(mode) if mode == timing(1280, 720, 120)));

        let (refused, error) =
            drive_with_host_modes(control_record_display_mode(timing(1280, 720, 240)));
        assert_eq!(error, Some(ErrorCode::ResolutionRejected));
        assert!(matches!(refused, Outcome::Nothing));
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
        assert!(
            support.capabilities.contains(&Capability::Clipboard),
            "a build that ships the clipboard daemon says so, whether or not one is attached"
        );
    }

    #[test]
    fn set_mode_motion_is_refused_without_ending_the_session() {
        let error = drive_control(control_record_set_mode(Mode::Motion));
        assert_eq!(error, Some(ErrorCode::UnsupportedMode));
    }

    #[test]
    fn set_mode_auto_resolves_to_the_one_mode_this_build_encodes() {
        let state = drive_control_for_state(control_record_set_mode(Mode::Auto));
        assert_eq!(state.mode, Mode::Desktop as i32);
    }

    #[test]
    fn set_resolution_is_a_request_to_the_output_and_not_an_answer_to_the_host() {
        // Nothing is reported back at this point: what the output came up at
        // is not known until a framebuffer of the new size has been seen, and
        // saying anything sooner would be a size the viewer would scale
        // against.
        assert!(matches!(
            drive(control_record_set_resolution(2560, 1440)),
            Outcome::Resize {
                width: 2560,
                height: 1440
            }
        ));
    }

    #[test]
    fn set_resolution_rounds_to_a_mode_the_output_can_build() {
        // `drm_cvt_mode` rounds a width to a multiple of eight, so a request
        // it cannot build would be one the guest never reports back -- and a
        // host that asked again for it on every frame.
        assert!(matches!(
            drive(control_record_set_resolution(1727, 971)),
            Outcome::Resize {
                width: 1720,
                height: 970
            }
        ));
    }

    #[test]
    fn a_resolution_this_output_cannot_drive_is_refused_without_ending_the_session() {
        let error = drive_control(control_record_set_resolution(320, 240));
        assert_eq!(error, Some(ErrorCode::ResolutionRejected));
    }

    #[test]
    fn the_geometry_a_display_state_reports_follows_the_output() {
        let (_, _, mut control, mut wire) = opened();
        control.set_geometry(1280, 720, 75);
        control.state(&mut wire);

        let state = state_from(&mut wire);
        assert_eq!((state.width, state.height), (1280, 720));
        assert_eq!(
            state.refresh_hz, 75,
            "what the compositor committed, not what was asked for"
        );
        assert_eq!(control.geometry(), (1280, 720, 75));
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
