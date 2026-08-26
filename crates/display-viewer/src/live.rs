//! One established session, over three sockets.
//!
//! The session machine is the protocol crate's, built from the hand-over rather
//! than from a handshake this process ran: generations, sequences and the
//! three-record binds are its arithmetic, and what is here is the order things
//! happen in and what to do when one of them fails.
//!
//! Nothing blocks. [`Live::pump`] does whatever can be done without waiting and
//! returns; the window calls it between messages, and a test calls it in a
//! loop. Every read is one a quiet socket answers `Idle` to.

use std::{
    io::{Read, Write},
    time::{Duration, Instant},
};

use prost::Message as _;
use vmlord_display_codec::{CursorPosition, Geometry, OwnedCursorImage, Rect};
use vmlord_display_protocol::{
    keys::ChannelKey,
    record::{self, Channel, Limits, Record, RecordError},
    session::{Event as SessionEvent, HandedOver, Negotiated, Session, SessionError},
    v1::{
        Capability, ControlRecord, DisplayState, DisplayTiming, Error as ErrorRecord, InputRecord,
        KeyEvent, Mode, Ping, PointerButton, PointerMotion, PointerScroll, Pong, ProtocolVersion,
        SetAvailableModes, SetDisplayMode, SetMode, SetResolution,
    },
};

use crate::{
    display_modes::DisplayMode,
    input,
    launch::Handover,
    status::Event,
    video::{Update, Video, VideoError},
};

/// How often the viewer proves the control socket is still there.
pub const PING_INTERVAL: Duration = Duration::from_secs(5);

/// How overdue a pong may be before control is treated as dead.
pub const PONG_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a failed bind waits before it is tried again.
pub const BIND_BACKOFF: Duration = Duration::from_secs(1);

/// How long a bind waits for each of the two records it is owed.
///
/// A bind is three records with nothing to compute between them, so a guest
/// that is going to answer answers at once. This is the bound on how long the
/// session thread sits inside one attempt before the backoff takes over.
const BIND_REPLY_WAIT: Duration = Duration::from_secs(2);

/// How long the bind sleeps between polls of a socket that is merely quiet.
const BIND_POLL: Duration = Duration::from_millis(1);

/// How many frame records one pass reads before it hands back what it has.
///
/// A guest capturing a desktop leaves no gap between records, so a drain that
/// ends only at a quiet socket has no end: the pass never returns, and what it
/// has already collected -- the status the window shows, the pixels it draws --
/// waits for a silence that never comes. Thirty-two records against the two
/// milliseconds the session loop sleeps is some sixteen thousand a second, far
/// more than a sixty-frame desktop asks for and still a pass that ends.
const FRAMES_PER_PASS: usize = 32;

/// What one pump produced.
#[derive(Debug)]
pub enum Signal {
    /// The stream's geometry. The window sizes its texture to it.
    Configured(Geometry),
    /// The rectangles of the frame that changed.
    Damage(Vec<Rect>),
    /// A new cursor bitmap.
    Cursor(OwnedCursorImage),
    /// Where the cursor is now.
    Moved(CursorPosition),
    /// Something the status machine has to know.
    Status(Event),
    /// The session is over, for the reason given. Fit for a log.
    Ended(String),
}

/// One session, driven by the window's message loop.
pub struct Live<S: Read + Write, C: FnMut(Channel) -> Result<S, String>> {
    session: Session,
    control: S,
    frame: Option<S>,
    input: Option<S>,
    connect: C,
    video: Video,
    /// The frame channel's caps, which the negotiated geometry sizes.
    limits: Limits,
    /// The control channel's caps, which nothing sizes.
    control_limits: Limits,
    /// Whether the window has been told the session is running.
    announced: bool,
    next_ping: Instant,
    /// The token of a ping still waiting for its pong, and when it went out.
    outstanding: Option<(u64, Instant)>,
    ping_token: u64,
    /// When a channel that failed to bind may be tried again.
    next_bind: Instant,
    payload: Vec<u8>,
}

impl<S: Read + Write, C: FnMut(Channel) -> Result<S, String>> Live<S, C> {
    /// Takes a session over from VMLord.
    ///
    /// `connect` opens one socket for a channel; the viewer calls it for the
    /// first bind and for every rebind, so that reconnecting a channel is the
    /// same code path as opening it.
    ///
    /// # Errors
    ///
    /// A message naming the field the hand-over got wrong: a session id or a
    /// key of the wrong width, or a version, mode or capability this build has
    /// no name for.
    pub fn new(handover: Handover, control: S, connect: C, now: Instant) -> Result<Self, String> {
        let session_id = handover
            .session_id
            .as_slice()
            .try_into()
            .map_err(|_| "the hand-over's session id is not sixteen bytes".to_owned())?;
        let frame_key = channel_key(&handover.frame_key, "frame")?;
        let input_key = channel_key(&handover.input_key, "input")?;
        let clipboard_key = channel_key(&handover.clipboard_key, "clipboard")?;

        let negotiated = Negotiated {
            version: ProtocolVersion {
                major: handover.version_major,
                minor: handover.version_minor,
            },
            capabilities: handover
                .capabilities
                .iter()
                .filter_map(|value| Capability::try_from(*value).ok())
                .collect(),
            mode: Mode::try_from(handover.mode)
                .map_err(|_| "the hand-over names a mode this build has no name for".to_owned())?,
            width: handover.width,
            height: handover.height,
            tile_size: handover.tile_size,
        };
        let limits = Limits::new(negotiated.width, negotiated.height);

        tracing::info!(
            "the display session is {}x{} at {}-pixel tiles, mode {:?}",
            negotiated.width,
            negotiated.height,
            negotiated.tile_size,
            negotiated.mode
        );

        Ok(Self {
            session: Session::established_host(HandedOver {
                session_id,
                negotiated,
                frame_key,
                input_key,
                clipboard_key,
                control_sequence: handover.control_sequence,
            }),
            control,
            frame: None,
            input: None,
            connect,
            video: Video::new(),
            limits,
            control_limits: Limits::new(0, 0),
            announced: false,
            next_ping: now,
            outstanding: None,
            ping_token: 0,
            next_bind: now,
            payload: Vec::new(),
        })
    }

    /// What has been decoded so far.
    #[must_use]
    pub fn video(&self) -> &Video {
        &self.video
    }

    /// Does whatever can be done without waiting.
    pub fn pump(&mut self, now: Instant, signals: &mut Vec<Signal>) {
        self.bind_channels(now, signals);
        self.read_control(now, signals);
        self.beat(now, signals);
        self.read_frames(now, signals);
    }

    /// Asks the guest for a whole frame.
    ///
    /// Recovery, not flow control: the decoder has nothing to apply a delta to.
    pub fn request_keyframe(&mut self) {
        self.write_control(ControlRecord::RequestKeyframe, Vec::new());
    }

    /// Asks the guest to put its output on a new size.
    ///
    /// A request and not a setting: what the compositor commits arrives as a
    /// `StreamConfig` on the frame channel, and it need not be this. The
    /// guest refusing a size answers with an `Error` instead, which is a log
    /// line rather than a state -- the picture that is on screen is still the
    /// one the guest has.
    pub fn set_resolution(&mut self, width: u32, height: u32) {
        tracing::info!("asking the guest for {width}x{height}");
        self.write_control(
            ControlRecord::SetResolution,
            SetResolution { width, height }.encode_to_vec(),
        );
    }

    /// Whether the guest agreed to take this host's monitor mode list.
    ///
    /// A guest on an older payload has not, and everything about host modes is
    /// then left alone: the window still resizes the output, which is the
    /// contract that build implements.
    #[must_use]
    pub fn host_modes(&self) -> bool {
        self.session.negotiated().is_some_and(|negotiated| {
            negotiated
                .capabilities
                .contains(&Capability::HostDisplayModes)
        })
    }

    /// Publishes the host monitor's modes, and the one to prefer.
    ///
    /// The list and the selection in one record because the guest has to apply
    /// them in that order, which is the guest's business rather than a
    /// sequence this side has to get right.
    pub fn set_available_modes(&mut self, modes: &[DisplayMode], preferred: Option<DisplayMode>) {
        tracing::info!("publishing {} host modes", modes.len());
        self.write_control(
            ControlRecord::SetAvailableModes,
            SetAvailableModes {
                modes: modes.iter().copied().map(timing).collect(),
                preferred: preferred.map(timing),
            }
            .encode_to_vec(),
        );
    }

    /// Asks the guest to prefer one of the modes it was published.
    pub fn set_display_mode(&mut self, mode: DisplayMode) {
        tracing::info!(
            "asking the guest for {}x{}@{}",
            mode.width,
            mode.height,
            mode.refresh_hz
        );
        self.write_control(
            ControlRecord::SetDisplayMode,
            SetDisplayMode {
                mode: Some(timing(mode)),
            }
            .encode_to_vec(),
        );
    }

    /// Asks the guest for an encoding mode.
    pub fn set_mode(&mut self, mode: Mode) {
        tracing::info!("asking the guest for {mode:?}");
        self.write_control(
            ControlRecord::SetMode,
            SetMode { mode: mode as i32 }.encode_to_vec(),
        );
    }

    /// Sends one input event to the guest.
    ///
    /// A write that fails closes the socket rather than retrying: the bind
    /// path reconnects it at the next generation, and the `ReleaseAll` that
    /// opens every bind covers whatever was held when it broke. The frame
    /// channel is untouched -- the picture does not stop because a key did.
    pub fn send_input(&mut self, event: input::Event) {
        if self.input.is_none() {
            return;
        }

        let (message_type, payload) = encode_input(event);
        let sequence = match self.session.take_channel_sequence(Channel::Input) {
            Ok(sequence) => sequence,
            Err(error) => {
                tracing::debug!("an input record could not be numbered: {error}");
                self.input = None;

                return;
            }
        };
        let record = Record::new(
            Channel::Input,
            message_type as u16,
            sequence,
            0,
            self.session.generation(Channel::Input),
            payload,
        );

        let limits = self.control_limits;
        let Some(socket) = self.input.as_mut() else {
            return;
        };
        if let Err(error) = record::write(socket, &record, &limits) {
            tracing::debug!("the input channel could not be written to: {error}");
            self.input = None;
        }
    }

    /// Tells the guest the session is over, best effort.
    ///
    /// The guest may stop capturing without waiting for the sockets to drop,
    /// and a failed write means it will find out the other way.
    pub fn end(&mut self) {
        self.write_control(ControlRecord::EndSession, Vec::new());
    }

    /// Opens and binds whichever of the two channels is not bound.
    fn bind_channels(&mut self, now: Instant, signals: &mut Vec<Signal>) {
        if now < self.next_bind {
            return;
        }

        for channel in [Channel::Frame, Channel::Input] {
            if self.socket(channel).is_some() {
                continue;
            }

            match self.bind(channel) {
                Ok(()) => {
                    tracing::info!(
                        "the {channel} channel bound at generation {}",
                        self.session.generation(channel)
                    );
                    if channel == Channel::Frame && !self.announced {
                        self.announced = true;
                        signals.push(Signal::Status(Event::Established));
                    }
                }
                Err(reason) => {
                    tracing::debug!("the {channel} channel could not bind: {reason}");
                    self.next_bind = now + BIND_BACKOFF;
                    return;
                }
            }
        }
    }

    /// Opens one socket and runs the three-record bind on it.
    fn bind(&mut self, channel: Channel) -> Result<(), String> {
        let mut socket = (self.connect)(channel)?;

        let hello = self
            .session
            .open_channel(channel)
            .map_err(|error: SessionError| error.to_string())?;
        record::write(&mut socket, &hello, &self.control_limits)
            .map_err(|error| error.to_string())?;

        // The ack, then this side's proof. Both are small and both are owed
        // straight away, so a socket that stays quiet past `BIND_REPLY_WAIT` is
        // one that is not going to bind.
        let mut payload = Vec::new();
        let header = read_awaited(&mut socket, &self.control_limits, &mut payload)?;
        let outcome = self
            .session
            .handle(&header, &payload)
            .map_err(|error| error.to_string())?;
        if let Some(reply) = outcome.reply {
            record::write(&mut socket, &reply, &self.control_limits)
                .map_err(|error| error.to_string())?;
        }
        if outcome.event != SessionEvent::ChannelBound(channel) {
            return Err(format!("the {channel} channel did not bind"));
        }

        if channel == Channel::Input {
            // What a freshly bound input channel owes, per the protocol's
            // recovery rule: the guest has just released everything it held, so
            // the first record says so. Harmless on a first bind, and the one
            // thing that keeps a key held across a reconnect from staying down.
            let sequence = self
                .session
                .take_channel_sequence(channel)
                .map_err(|error| error.to_string())?;
            let release = Record::new(
                Channel::Input,
                InputRecord::ReleaseAll as u16,
                sequence,
                0,
                self.session.generation(channel),
                Vec::new(),
            );
            record::write(&mut socket, &release, &self.control_limits)
                .map_err(|error| error.to_string())?;
        }

        match channel {
            Channel::Frame => self.frame = Some(socket),
            Channel::Input => self.input = Some(socket),
            // Neither is this session's to bind: control established it, and
            // the clipboard is bound by the thread that owns that socket.
            Channel::Control | Channel::Clipboard => {
                unreachable!("this session binds frame and input only")
            }
        }

        Ok(())
    }

    /// Reads whatever the control channel has to say.
    fn read_control(&mut self, _now: Instant, signals: &mut Vec<Signal>) {
        loop {
            let mut payload = std::mem::take(&mut self.payload);
            let header = match record::read(&mut self.control, &self.control_limits, &mut payload) {
                Ok(header) => header,
                Err(RecordError::Idle) => {
                    self.payload = payload;
                    return;
                }
                Err(error) => {
                    self.payload = payload;
                    signals.push(Signal::Status(Event::ControlLost));
                    signals.push(Signal::Ended(format!("control was lost: {error}")));
                    return;
                }
            };

            match ControlRecord::try_from(i32::from(header.message_type)) {
                Ok(ControlRecord::Pong) => {
                    let token = Pong::decode(payload.as_slice()).map(|pong| pong.token).ok();
                    if self.outstanding.map(|(sent, _)| sent) == token {
                        self.outstanding = None;
                    }
                }
                Ok(ControlRecord::Ping) => {
                    let token = Ping::decode(payload.as_slice())
                        .map(|ping| ping.token)
                        .unwrap_or_default();
                    self.write_control(ControlRecord::Pong, Pong { token }.encode_to_vec());
                }
                Ok(ControlRecord::DisplayState) => {
                    if let Ok(state) = DisplayState::decode(payload.as_slice()) {
                        tracing::info!(
                            "the guest reports {}x{} at {}-pixel tiles",
                            state.width,
                            state.height,
                            state.tile_size
                        );
                    }
                }
                Ok(ControlRecord::Error) => {
                    if let Ok(error) = ErrorRecord::decode(payload.as_slice()) {
                        tracing::warn!(
                            "the guest reported display error {}: {}",
                            error.code,
                            error.detail
                        );
                    }
                }
                Ok(ControlRecord::EndSession) => {
                    signals.push(Signal::Ended("the guest ended the session".to_owned()));
                    self.payload = payload;
                    return;
                }
                _ => tracing::debug!(
                    "a control record of type {} is one this build does not read",
                    header.message_type
                ),
            }

            self.payload = payload;
        }
    }

    /// Sends a ping when one is due, and gives up when one goes unanswered.
    fn beat(&mut self, now: Instant, signals: &mut Vec<Signal>) {
        if let Some((token, sent)) = self.outstanding
            && now.duration_since(sent) >= PONG_TIMEOUT
        {
            tracing::warn!(
                "ping {token} went unanswered for {}s",
                PONG_TIMEOUT.as_secs()
            );
            signals.push(Signal::Status(Event::ControlLost));
            signals.push(Signal::Ended(
                "the guest stopped answering pings".to_owned(),
            ));
            self.outstanding = None;
            return;
        }

        if now < self.next_ping {
            return;
        }

        self.ping_token = self.ping_token.wrapping_add(1);
        let token = self.ping_token;
        self.write_control(ControlRecord::Ping, Ping { token }.encode_to_vec());
        self.outstanding.get_or_insert((token, now));
        self.next_ping = now + PING_INTERVAL;
    }

    /// Reads whatever the frame channel has, and decodes it.
    fn read_frames(&mut self, now: Instant, signals: &mut Vec<Signal>) {
        for _ in 0..FRAMES_PER_PASS {
            let Some(socket) = self.frame.as_mut() else {
                return;
            };

            let mut payload = std::mem::take(&mut self.payload);
            let header = match record::read(socket, &self.limits, &mut payload) {
                Ok(header) => header,
                Err(RecordError::Idle) => {
                    self.payload = payload;
                    return;
                }
                Err(error) => {
                    self.payload = payload;
                    self.rebind(now, signals, &error.to_string());
                    return;
                }
            };

            if let Err(error) = self.session.accept(&header) {
                self.payload = payload;
                self.rebind(now, signals, &error.to_string());
                return;
            }

            match self.video.apply(&header, &payload) {
                Ok(Update::Nothing) => {}
                Ok(Update::Configured(geometry)) => {
                    self.limits
                        .set_geometry(geometry.width(), geometry.height());
                    signals.push(Signal::Configured(geometry));
                }
                Ok(Update::Damage(damage)) => signals.push(Signal::Damage(damage)),
                Ok(Update::Cursor(image)) => signals.push(Signal::Cursor(image)),
                Ok(Update::Moved(position)) => signals.push(Signal::Moved(position)),
                Err(VideoError::Rebind(reason)) => {
                    self.payload = payload;
                    self.rebind(now, signals, &reason);
                    return;
                }
                Err(VideoError::Fatal(reason)) => {
                    self.payload = payload;
                    signals.push(Signal::Ended(reason));
                    return;
                }
            }

            self.payload = payload;
        }
    }

    /// Drops the frame socket and asks for a replacement at the next generation.
    ///
    /// What the reconnected channel owes -- a `StreamConfig` and a keyframe --
    /// is the guest's obligation, which is why nothing is requested here.
    fn rebind(&mut self, now: Instant, signals: &mut Vec<Signal>, reason: &str) {
        tracing::warn!("the frame channel is being replaced: {reason}");
        self.frame = None;
        self.video = Video::new();
        self.next_bind = now;

        if let Err(error) = self.session.reconnect_channel(Channel::Frame) {
            signals.push(Signal::Ended(format!(
                "the frame channel cannot be replaced: {error}"
            )));
            return;
        }

        signals.push(Signal::Status(Event::ChannelLost));
    }

    /// Which socket a channel is on.
    fn socket(&self, channel: Channel) -> Option<&S> {
        match channel {
            Channel::Frame => self.frame.as_ref(),
            Channel::Input => self.input.as_ref(),
            Channel::Control => Some(&self.control),
            Channel::Clipboard => None,
        }
    }

    /// One control record of this side's own.
    ///
    /// A failed write is logged and nothing else: a control socket that cannot
    /// be written to is one the next read reports as lost.
    fn write_control(&mut self, message_type: ControlRecord, payload: Vec<u8>) {
        let sequence = self.session.take_control_sequence();
        let record = Record::new(
            Channel::Control,
            message_type as u16,
            sequence,
            0,
            0,
            payload,
        );

        if let Err(error) = record::write(&mut self.control, &record, &self.control_limits) {
            tracing::debug!("a {message_type:?} record could not be written: {error}");
        }
    }
}

/// One event as the record type and payload the input channel carries.
fn encode_input(event: input::Event) -> (InputRecord, Vec<u8>) {
    match event {
        input::Event::Key { keycode, pressed } => (
            InputRecord::KeyEvent,
            KeyEvent {
                keycode: u32::from(keycode),
                pressed,
            }
            .encode_to_vec(),
        ),
        input::Event::Motion { x, y } => (
            InputRecord::PointerMotion,
            PointerMotion { x, y }.encode_to_vec(),
        ),
        input::Event::Button { button, pressed } => (
            InputRecord::PointerButton,
            PointerButton {
                button: u32::from(button),
                pressed,
            }
            .encode_to_vec(),
        ),
        input::Event::Scroll {
            horizontal,
            vertical,
        } => (
            InputRecord::PointerScroll,
            PointerScroll {
                horizontal,
                vertical,
            }
            .encode_to_vec(),
        ),
        input::Event::ReleaseAll => (InputRecord::ReleaseAll, Vec::new()),
    }
}

/// Reads one record, waiting out a socket that has simply not answered yet.
///
/// The one place the session thread waits on a socket rather than leaving it
/// for the next pump: a bind is a three-record exchange, and half of one is no
/// use to anybody.
pub(crate) fn read_awaited<S: Read + Write>(
    socket: &mut S,
    limits: &Limits,
    payload: &mut Vec<u8>,
) -> Result<record::Header, String> {
    let deadline = Instant::now() + BIND_REPLY_WAIT;
    loop {
        match record::read(socket, limits, payload) {
            Ok(header) => return Ok(header),
            Err(RecordError::Idle) if Instant::now() < deadline => std::thread::sleep(BIND_POLL),
            Err(RecordError::Idle) => {
                return Err("the guest did not answer a channel hello in time".to_owned());
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

/// One mode as the wire carries it.
fn timing(mode: DisplayMode) -> DisplayTiming {
    DisplayTiming {
        width: mode.width,
        height: mode.height,
        refresh_hz: mode.refresh_hz,
    }
}

/// Reads a channel key out of a hand-over.
pub(crate) fn channel_key(bytes: &[u8], what: &str) -> Result<ChannelKey, String> {
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| format!("the hand-over's {what} key is not thirty-two bytes"))?;

    Ok(ChannelKey::from_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use prost::Message as _;
    use vmlord_display_codec::{Encoder, EncoderConfig, Frame, Geometry, PixelFormat, TileSize};
    use vmlord_display_protocol::{
        keys::{self, ChannelKey, Role, Tag},
        record::{self, Channel, Limits, Record},
        v1::{
            ChannelAck, ChannelAuth, ChannelHello, ControlRecord, FrameRecord, InputRecord, Mode,
            Ping, PixelFormat as WireFormat, Pong, SetAvailableModes, SetDisplayMode, SetMode,
            SetResolution, StreamConfig,
        },
    };

    use super::{Live, PONG_TIMEOUT, Signal};
    use crate::{
        duplex::{self, Duplex},
        launch::Handover,
    };

    const SESSION_ID: [u8; 16] = [7; 16];
    const FRAME_KEY: [u8; 32] = [1; 32];
    const INPUT_KEY: [u8; 32] = [2; 32];
    const CLIPBOARD_KEY: [u8; 32] = [3; 32];

    fn handover() -> Handover {
        Handover {
            session_id: SESSION_ID.to_vec(),
            frame_key: FRAME_KEY.to_vec(),
            input_key: INPUT_KEY.to_vec(),
            clipboard_key: CLIPBOARD_KEY.to_vec(),
            version_major: 1,
            version_minor: 0,
            capabilities: vec![1],
            mode: 2,
            width: 320,
            height: 200,
            tile_size: 32,
            control_sequence: 2,
        }
    }

    fn geometry() -> Geometry {
        Geometry::new(320, 200, TileSize::ThirtyTwo, PixelFormat::Bgra8888)
            .expect("a geometry the codec allows")
    }

    /// The guest half of a bind, done with a channel key and nothing else --
    /// which is all the guest's capture process has.
    fn accept_bind(socket: &mut Duplex, channel: Channel, key: &ChannelKey) -> u32 {
        let limits = Limits::new(0, 0);
        let mut payload = Vec::new();
        let header = wait_for_record(socket, &limits, &mut payload);
        assert_eq!(header.message_type, FrameRecord::ChannelHello as u16);

        let hello = ChannelHello::decode(payload.as_slice()).expect("a channel hello");
        assert_eq!(hello.session_id, SESSION_ID);
        let host_nonce: [u8; 32] = hello.nonce.as_slice().try_into().expect("a 32-byte nonce");
        let guest_nonce = [9u8; 32];

        let ack = ChannelAck {
            nonce: guest_nonce.to_vec(),
            tag: keys::channel_tag(key, Role::Guest, channel, &host_nonce, &guest_nonce)
                .as_bytes()
                .to_vec(),
        };
        record::write(
            socket,
            &Record::new(
                channel,
                FrameRecord::ChannelAck as u16,
                1,
                0,
                hello.generation,
                ack.encode_to_vec(),
            ),
            &limits,
        )
        .expect("an in-memory socket");

        let header = wait_for_record(socket, &limits, &mut payload);
        assert_eq!(header.message_type, FrameRecord::ChannelAuth as u16);
        let auth = ChannelAuth::decode(payload.as_slice()).expect("a channel auth");
        let expected = keys::channel_tag(key, Role::Host, channel, &host_nonce, &guest_nonce);
        assert!(keys::verify(
            &expected,
            &Tag::from_wire(&auth.tag).expect("a 32-byte tag")
        ));

        hello.generation
    }

    /// Reads one record, spinning while the socket is merely quiet.
    fn wait_for_record(
        socket: &mut Duplex,
        limits: &Limits,
        payload: &mut Vec<u8>,
    ) -> record::Header {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match record::read(socket, limits, payload) {
                Ok(header) => return header,
                Err(record::RecordError::Idle) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("the socket failed: {error}"),
            }
        }
    }

    fn stream_config_record(sequence: u32, generation: u32) -> Record {
        let config = StreamConfig {
            width: 320,
            height: 200,
            tile_size: 32,
            pixel_format: WireFormat::Bgra8888 as i32,
        };

        Record::new(
            Channel::Frame,
            FrameRecord::StreamConfig as u16,
            sequence,
            0,
            generation,
            config.encode_to_vec(),
        )
    }

    fn keyframe_record(sequence: u32, generation: u32) -> Record {
        let mut encoder = Encoder::new(EncoderConfig::new(geometry()));
        let pixels = vec![0x40; geometry().frame_bytes()];
        encoder
            .submit(
                Frame {
                    pixels: &pixels,
                    stride: geometry().width() as usize * 4,
                },
                None,
            )
            .expect("a frame of this geometry");
        let payload = match encoder.next_payload().expect("the first payload") {
            vmlord_display_codec::Payload::Keyframe(bytes) => bytes.to_vec(),
            other => panic!("the first payload is a keyframe, not {other:?}"),
        };

        Record::new(
            Channel::Frame,
            FrameRecord::Keyframe as u16,
            sequence,
            0,
            generation,
            payload,
        )
    }

    /// A live session with all three sockets connected and bound.
    struct Harness {
        control: Duplex,
        frame: Duplex,
        input: Duplex,
    }

    /// One live session and the guest ends of its three sockets.
    type Started = (
        Live<Duplex, Box<dyn FnMut(Channel) -> Result<Duplex, String>>>,
        Harness,
    );

    fn start(now: Instant) -> Started {
        let (host_control, control) = duplex::pair();
        let (host_frame, frame) = duplex::pair();
        let (host_input, input) = duplex::pair();

        let mut sockets = vec![(Channel::Input, host_input), (Channel::Frame, host_frame)];
        let connect = move |channel: Channel| {
            let index = sockets
                .iter()
                .position(|(kind, _)| *kind == channel)
                .ok_or_else(|| format!("no more {channel} sockets"))?;
            Ok(sockets.remove(index).1)
        };

        let connect: Box<dyn FnMut(Channel) -> Result<Duplex, String>> = Box::new(connect);
        let live = Live::new(handover(), host_control, connect, now).expect("a hand-over");

        (
            live,
            Harness {
                control,
                frame,
                input,
            },
        )
    }

    /// A live session with both channels bound, and the guest ends back.
    fn established() -> (
        Live<Duplex, Box<dyn FnMut(Channel) -> Result<Duplex, String>>>,
        Harness,
    ) {
        let (mut live, mut harness) = start(Instant::now());
        let mut signals = Vec::new();

        let guest = std::thread::spawn(move || {
            accept_bind(
                &mut harness.frame,
                Channel::Frame,
                &ChannelKey::from_bytes(FRAME_KEY),
            );
            accept_bind(
                &mut harness.input,
                Channel::Input,
                &ChannelKey::from_bytes(INPUT_KEY),
            );

            // The `ReleaseAll` every bind opens with, read here so that the
            // records a test sends afterwards are the only ones it sees.
            let limits = Limits::new(0, 0);
            let mut payload = Vec::new();
            let header = wait_for_record(&mut harness.input, &limits, &mut payload);
            assert_eq!(header.message_type, InputRecord::ReleaseAll as u16);

            harness
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline
            && !signals
                .iter()
                .any(|signal| matches!(signal, Signal::Status(crate::status::Event::Established)))
        {
            live.pump(Instant::now(), &mut signals);
            std::thread::sleep(Duration::from_millis(1));
        }

        (live, guest.join().expect("the guest thread"))
    }

    #[test]
    fn an_input_event_reaches_the_guest_as_its_record() {
        let (mut live, mut harness) = established();

        live.send_input(crate::input::Event::Key {
            keycode: 30,
            pressed: true,
        });
        live.send_input(crate::input::Event::Motion { x: 100, y: 50 });

        let limits = Limits::new(0, 0);
        let mut payload = Vec::new();

        let header = wait_for_record(&mut harness.input, &limits, &mut payload);
        assert_eq!(header.message_type, InputRecord::KeyEvent as u16);
        let key =
            vmlord_display_protocol::v1::KeyEvent::decode(payload.as_slice()).expect("a key event");
        assert_eq!((key.keycode, key.pressed), (30, true));

        let header = wait_for_record(&mut harness.input, &limits, &mut payload);
        assert_eq!(header.message_type, InputRecord::PointerMotion as u16);
        let motion = vmlord_display_protocol::v1::PointerMotion::decode(payload.as_slice())
            .expect("a motion");
        assert_eq!((motion.x, motion.y), (100, 50));

        // Sequence numbers advance, which is what the guest's replay check
        // rests on. The bind's own `ReleaseAll` was the one before these.
        assert!(header.sequence > 0);
    }

    #[test]
    fn an_event_with_no_input_channel_is_dropped_rather_than_queued() {
        // Between a channel's loss and its rebind there is nowhere to put an
        // event, and holding one back would deliver it under a generation the
        // guest has already refused.
        let (mut live, _harness) = start(Instant::now());

        live.send_input(crate::input::Event::Key {
            keycode: 30,
            pressed: true,
        });

        assert!(live.socket(Channel::Input).is_none());
    }

    #[test]
    fn both_channels_bind_at_generation_zero() {
        let now = Instant::now();
        let (mut live, mut harness) = start(now);
        let mut signals = Vec::new();

        let guest = std::thread::spawn(move || {
            let frame = accept_bind(
                &mut harness.frame,
                Channel::Frame,
                &ChannelKey::from_bytes(FRAME_KEY),
            );
            let input = accept_bind(
                &mut harness.input,
                Channel::Input,
                &ChannelKey::from_bytes(INPUT_KEY),
            );

            // What a freshly bound input channel owes: the guest has just
            // released everything it held, and the first record says so.
            let limits = Limits::new(0, 0);
            let mut payload = Vec::new();
            let header = wait_for_record(&mut harness.input, &limits, &mut payload);
            assert_eq!(header.message_type, InputRecord::ReleaseAll as u16);

            (frame, input, harness)
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline
            && !signals
                .iter()
                .any(|signal| matches!(signal, Signal::Status(crate::status::Event::Established)))
        {
            live.pump(Instant::now(), &mut signals);
            std::thread::sleep(Duration::from_millis(1));
        }

        let (frame_generation, input_generation, _) = guest.join().expect("the guest thread");
        assert_eq!((frame_generation, input_generation), (0, 0));
    }

    #[test]
    fn a_stream_config_and_a_keyframe_put_pixels_on_the_screen() {
        let now = Instant::now();
        let (mut live, mut harness) = start(now);
        let mut signals = Vec::new();

        let guest = std::thread::spawn(move || {
            accept_bind(
                &mut harness.frame,
                Channel::Frame,
                &ChannelKey::from_bytes(FRAME_KEY),
            );
            accept_bind(
                &mut harness.input,
                Channel::Input,
                &ChannelKey::from_bytes(INPUT_KEY),
            );
            let limits = Limits::new(320, 200);
            record::write(&mut harness.frame, &stream_config_record(3, 0), &limits)
                .expect("an in-memory socket");
            record::write(&mut harness.frame, &keyframe_record(4, 0), &limits)
                .expect("an in-memory socket");
            harness
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline
            && !signals
                .iter()
                .any(|signal| matches!(signal, Signal::Damage(_)))
        {
            live.pump(Instant::now(), &mut signals);
            std::thread::sleep(Duration::from_millis(1));
        }
        guest.join().expect("the guest thread");

        assert!(
            signals
                .iter()
                .any(|signal| matches!(signal, Signal::Configured(_)))
        );
        assert!(
            signals
                .iter()
                .any(|signal| matches!(signal, Signal::Damage(_)))
        );
        assert_eq!(live.video().geometry(), Some(geometry()));
    }

    #[test]
    fn a_guest_that_never_stops_sending_does_not_keep_the_pass_from_returning() {
        // A desktop under capture leaves no gap between records, so a drain
        // that ends only at an idle socket ends never: everything the pass
        // collected -- the status the window shows, the pixels it draws --
        // waits behind a loop that has no reason to stop.
        let (mut live, mut harness) = established();
        let limits = Limits::new(320, 200);
        record::write(&mut harness.frame, &stream_config_record(3, 0), &limits)
            .expect("an in-memory socket");
        // Enough waiting frames that draining them all is unmistakable.
        let flood = 200u32;
        for offset in 0..flood {
            record::write(&mut harness.frame, &keyframe_record(4 + offset, 0), &limits)
                .expect("an in-memory socket");
        }

        let mut signals = Vec::new();
        live.pump(Instant::now(), &mut signals);

        let damage = signals
            .iter()
            .filter(|signal| matches!(signal, Signal::Damage(_)))
            .count();
        assert!(
            damage < flood as usize,
            "one pass read all {flood} waiting frames, so nothing it collected reached the \
             window until the guest fell silent"
        );
    }

    #[test]
    fn a_corrupted_frame_record_rebinds_the_channel_at_the_next_generation() {
        let now = Instant::now();
        let (mut live, mut harness) = start(now);
        let mut signals = Vec::new();

        let guest = std::thread::spawn(move || {
            accept_bind(
                &mut harness.frame,
                Channel::Frame,
                &ChannelKey::from_bytes(FRAME_KEY),
            );
            accept_bind(
                &mut harness.input,
                Channel::Input,
                &ChannelKey::from_bytes(INPUT_KEY),
            );
            let limits = Limits::new(320, 200);
            record::write(&mut harness.frame, &stream_config_record(3, 0), &limits)
                .expect("an in-memory socket");
            // A keyframe whose payload was cut: the codec refuses it, and the
            // channel cannot continue.
            let mut broken = keyframe_record(4, 0);
            broken.payload.truncate(4);
            let broken = Record::new(
                Channel::Frame,
                FrameRecord::Keyframe as u16,
                4,
                0,
                0,
                broken.payload,
            );
            record::write(&mut harness.frame, &broken, &limits).expect("an in-memory socket");
            harness
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline
            && !signals
                .iter()
                .any(|signal| matches!(signal, Signal::Status(crate::status::Event::ChannelLost)))
        {
            live.pump(Instant::now(), &mut signals);
            std::thread::sleep(Duration::from_millis(1));
        }
        guest.join().expect("the guest thread");

        assert!(
            signals
                .iter()
                .any(|signal| matches!(signal, Signal::Status(crate::status::Event::ChannelLost)))
        );
    }

    #[test]
    fn a_ping_is_answered_and_a_missing_pong_expires_control() {
        let now = Instant::now();
        let (mut live, mut harness) = start(now);
        let mut signals = Vec::new();
        let limits = Limits::new(0, 0);

        // The first pump writes the first ping.
        live.pump(now, &mut signals);
        let mut payload = Vec::new();
        let header = wait_for_record(&mut harness.control, &limits, &mut payload);
        assert_eq!(header.message_type, ControlRecord::Ping as u16);
        let token = Ping::decode(payload.as_slice()).expect("a ping").token;
        assert_eq!(header.sequence, 2, "the hand-over's control sequence");

        record::write(
            &mut harness.control,
            &Record::new(
                Channel::Control,
                ControlRecord::Pong as u16,
                0,
                0,
                0,
                Pong { token }.encode_to_vec(),
            ),
            &limits,
        )
        .expect("an in-memory socket");

        live.pump(Instant::now(), &mut signals);
        assert!(
            !signals
                .iter()
                .any(|signal| matches!(signal, Signal::Status(crate::status::Event::ControlLost)))
        );

        // The next ping goes unanswered.
        live.pump(now + super::PING_INTERVAL, &mut signals);
        live.pump(now + super::PING_INTERVAL + PONG_TIMEOUT, &mut signals);

        assert!(
            signals
                .iter()
                .any(|signal| matches!(signal, Signal::Status(crate::status::Event::ControlLost)))
        );
    }

    #[test]
    fn a_guest_that_ends_the_session_is_reported_rather_than_retried() {
        let now = Instant::now();
        let (mut live, mut harness) = start(now);
        let mut signals = Vec::new();
        let limits = Limits::new(0, 0);

        record::write(
            &mut harness.control,
            &Record::new(
                Channel::Control,
                ControlRecord::EndSession as u16,
                0,
                0,
                0,
                Vec::new(),
            ),
            &limits,
        )
        .expect("an in-memory socket");

        live.pump(Instant::now(), &mut signals);

        assert!(
            signals
                .iter()
                .any(|signal| matches!(signal, Signal::Ended(_)))
        );
    }

    #[test]
    fn an_error_record_is_logged_and_the_session_carries_on() {
        let (_, text) = crate::log::capture::capture(|| {
            let now = Instant::now();
            let (mut live, mut harness) = start(now);
            let mut signals = Vec::new();
            let limits = Limits::new(0, 0);

            record::write(
                &mut harness.control,
                &Record::new(
                    Channel::Control,
                    ControlRecord::Error as u16,
                    0,
                    0,
                    0,
                    vmlord_display_protocol::v1::Error {
                        code: vmlord_display_protocol::v1::ErrorCode::CaptureFailed as i32,
                        detail: "the compositor stopped".to_owned(),
                    }
                    .encode_to_vec(),
                ),
                &limits,
            )
            .expect("an in-memory socket");

            live.pump(Instant::now(), &mut signals);

            assert!(
                !signals
                    .iter()
                    .any(|signal| matches!(signal, Signal::Ended(_)))
            );
        });
        assert!(text.contains("the compositor stopped"));
    }

    #[test]
    fn ending_a_session_tells_the_guest_before_the_sockets_close() {
        let now = Instant::now();
        let (mut live, mut harness) = start(now);
        let limits = Limits::new(0, 0);

        live.end();

        let mut payload = Vec::new();
        let mut header = wait_for_record(&mut harness.control, &limits, &mut payload);
        while header.message_type != ControlRecord::EndSession as u16 {
            header = wait_for_record(&mut harness.control, &limits, &mut payload);
        }
        assert_eq!(header.channel, Channel::Control);
    }

    #[test]
    fn the_monitor_list_reaches_the_guest_before_the_mode_chosen_from_it() {
        // The order the guest applies them in: a mode marked preferred while
        // the connector is still offering the old list is a hotplug onto a
        // mode about to be withdrawn.
        let now = Instant::now();
        let (mut live, mut harness) = start(now);
        let limits = Limits::new(0, 0);
        let offered = [
            super::DisplayMode::new(1280, 720, 60).expect("a valid fixture"),
            super::DisplayMode::new(1920, 1080, 144).expect("a valid fixture"),
        ];

        live.set_available_modes(&offered, Some(offered[1]));
        live.set_display_mode(offered[1]);

        let mut payload = Vec::new();
        let mut header = wait_for_record(&mut harness.control, &limits, &mut payload);
        while header.message_type != ControlRecord::SetAvailableModes as u16 {
            header = wait_for_record(&mut harness.control, &limits, &mut payload);
        }
        let published = SetAvailableModes::decode(payload.as_slice()).expect("a mode list");
        assert_eq!(published.modes.len(), 2);
        assert_eq!(
            published.preferred,
            Some(super::timing(offered[1])),
            "the list carries the selection the policy made"
        );

        while header.message_type != ControlRecord::SetDisplayMode as u16 {
            header = wait_for_record(&mut harness.control, &limits, &mut payload);
        }
        let chosen = SetDisplayMode::decode(payload.as_slice()).expect("a mode");
        assert_eq!(chosen.mode, Some(super::timing(offered[1])));
    }

    #[test]
    fn a_settled_window_reaches_the_guest_as_a_resolution_and_a_mode() {
        let now = Instant::now();
        let (mut live, mut harness) = start(now);
        let limits = Limits::new(0, 0);

        live.set_resolution(1720, 970);
        live.set_mode(Mode::Auto);

        let mut payload = Vec::new();
        let mut header = wait_for_record(&mut harness.control, &limits, &mut payload);
        while header.message_type != ControlRecord::SetResolution as u16 {
            header = wait_for_record(&mut harness.control, &limits, &mut payload);
        }
        let wanted = SetResolution::decode(payload.as_slice()).expect("a resolution");
        assert_eq!((wanted.width, wanted.height), (1720, 970));

        while header.message_type != ControlRecord::SetMode as u16 {
            header = wait_for_record(&mut harness.control, &limits, &mut payload);
        }
        let mode = SetMode::decode(payload.as_slice()).expect("a mode");
        assert_eq!(
            mode.mode,
            Mode::Auto as i32,
            "Auto is the guest's to resolve: it is what knows what it can encode"
        );
    }

    #[test]
    fn a_keyframe_request_reaches_the_guest_on_the_control_channel() {
        let now = Instant::now();
        let (mut live, mut harness) = start(now);
        let limits = Limits::new(0, 0);

        live.request_keyframe();

        let mut payload = Vec::new();
        let mut header = wait_for_record(&mut harness.control, &limits, &mut payload);
        while header.message_type != ControlRecord::RequestKeyframe as u16 {
            header = wait_for_record(&mut harness.control, &limits, &mut payload);
        }
        assert_eq!(header.channel, Channel::Control);
    }
}
