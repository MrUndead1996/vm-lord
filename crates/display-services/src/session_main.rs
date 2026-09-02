//! The unprivileged half: everything that runs hot and nothing worth stealing.
//!
//! One `poll` loop, because nothing acknowledges a frame: the broker descriptor
//! is always watched, and the frame descriptor is watched for writability only
//! while unwritten bytes remain. Encoding happens when the socket has drained,
//! which is what makes the encoder's reference frame equal to the last payload
//! that actually went out.
//!
//! The input channel is watched by the same loop rather than by a thread of its
//! own. Nothing here blocks, so a second thread would buy nothing and would
//! cost the one guarantee that makes this testable: after a step returns, every
//! record that had arrived has been handled.

use std::{
    collections::HashMap,
    fmt,
    fs::File,
    io::{self, ErrorKind, Read, Write},
    os::fd::{AsRawFd, OwnedFd, RawFd},
    path::PathBuf,
    time::Duration,
};

use prost::Message as _;
use vmlord_display_codec::{CodecError, Geometry, PixelFormat, TileSize};
use vmlord_display_protocol::{
    keys::ChannelKey,
    record::{self, Channel, Limits, RecordError},
    v1::{InputRecord, KeyEvent, PointerButton, PointerMotion, PointerScroll},
};

use crate::{
    capture::{Backing, CapturedFrame, MappedBuffer},
    channel::{self, BindError},
    cursor::{self, Placement},
    drm::uapi::DRM_FORMAT_ARGB8888,
    ipc::{Message, PlaneKind, PlaneLayout, SessionParameters},
    pipeline::{Pipeline, PipelineError},
    uinput::{Keyboard, Pointer},
    unix::Connection,
};

/// How long a step waits for something to happen.
///
/// Short, because the only thing this loop owes anyone is a frame, and a frame
/// that is late is a desktop that stutters.
const STEP_TIMEOUT: Duration = Duration::from_millis(50);

/// What one turn of the loop did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Nothing was ready before the timeout.
    Idle,
    /// Something moved: a message, a record, or bytes on their way out.
    Progress,
    /// The session ended, and both sockets are closed.
    SessionClosed,
    /// The broker went away, which is the one thing this process cannot
    /// continue without.
    BrokerLost,
}

/// What stopped a step.
#[derive(Debug)]
pub enum LoopError {
    /// The broker socket or a session socket failed.
    Io(io::Error),
    /// A socket would not bind to the session.
    Bind(BindError),
    /// The encoder or a record failed.
    Pipeline(PipelineError),
    /// The geometry the session agreed is not one the codec has.
    Codec(CodecError),
}

impl fmt::Display for LoopError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "a display socket failed: {error}"),
            Self::Bind(error) => write!(formatter, "a channel would not bind: {error}"),
            Self::Pipeline(error) => write!(formatter, "{error}"),
            Self::Codec(error) => write!(
                formatter,
                "the session's geometry is not one this build encodes: {error}"
            ),
        }
    }
}

impl std::error::Error for LoopError {}

impl From<io::Error> for LoopError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<BindError> for LoopError {
    fn from(error: BindError) -> Self {
        Self::Bind(error)
    }
}

impl From<PipelineError> for LoopError {
    fn from(error: PipelineError) -> Self {
        Self::Pipeline(error)
    }
}

impl From<CodecError> for LoopError {
    fn from(error: CodecError) -> Self {
        Self::Codec(error)
    }
}

/// Anything a session socket can be: a vsock in a guest, a socketpair in a test.
pub trait Socket: Read + Write + AsRawFd {}

impl<T: Read + Write + AsRawFd> Socket for T {}

/// Where a session socket comes from, without blocking for one.
pub trait Acceptor {
    /// What it produces.
    type Socket: Socket;

    /// A socket that connected since the last call, if one did.
    ///
    /// # Errors
    ///
    /// [`io::Error`] if the listener failed. Having nothing to accept is
    /// `Ok(None)` and not an error.
    fn accept(&mut self) -> io::Result<Option<Self::Socket>>;
}

/// A bound frame channel and what is still owed to it.
struct FrameChannel<S> {
    socket: S,
    pipeline: Pipeline,
    /// Bytes written into a socket that would not take them all. Nothing is
    /// encoded while this is non-empty, which is what keeps the encoder's
    /// reference equal to what the peer actually holds.
    tail: Vec<u8>,
}

/// The unprivileged process's whole state.
pub struct Loop<F: Acceptor, I: Acceptor> {
    broker: Connection,
    frames: F,
    inputs: I,
    /// What the broker last handed over. `None` between sessions.
    parameters: Option<SessionParameters>,
    frame: Option<FrameChannel<F::Socket>>,
    input: Option<I::Socket>,
    /// The generation each channel was last bound at, so a replayed hello is
    /// refused rather than allowed to displace a live socket.
    frame_generation: Option<u32>,
    input_generation: Option<u32>,
    /// The buffers the broker has sent, mapped once and kept.
    buffers: HashMap<u64, MappedBuffer>,
    /// Whether a `NextFrame` is outstanding.
    asked_for_frame: bool,
    /// Input records read, which is what proves they are consumed rather than
    /// left to stall the socket.
    input_records: u64,
    /// Where the cursor was last reported. Resending an unchanged position
    /// every vblank would put a record on the wire for a pointer that has not
    /// moved, which is bandwidth spent to say nothing.
    cursor: Option<Placement>,
    /// The guest's keyboard, once the broker has handed it over. `None` on a
    /// guest whose kernel has no uinput, where input is read and dropped.
    keyboard: Option<Keyboard<File>>,
    /// Its pointer, on the same terms.
    pointer: Option<Pointer<File>>,
    limits: Limits,
}

impl<F: Acceptor, I: Acceptor> Loop<F, I> {
    /// A loop over one broker connection and two listeners.
    pub fn new(broker: Connection, frames: F, inputs: I) -> Self {
        Self {
            broker,
            frames,
            inputs,
            parameters: None,
            frame: None,
            input: None,
            frame_generation: None,
            input_generation: None,
            buffers: HashMap::new(),
            asked_for_frame: false,
            input_records: 0,
            cursor: None,
            keyboard: None,
            pointer: None,
            limits: Limits::new(0, 0),
        }
    }

    /// How many input records have been read.
    #[must_use]
    pub const fn input_records(&self) -> u64 {
        self.input_records
    }

    /// Whether a frame channel is bound right now.
    #[must_use]
    pub const fn has_frame_channel(&self) -> bool {
        self.frame.is_some()
    }

    /// One turn: accept, wait, then handle whatever is ready.
    ///
    /// # Errors
    ///
    /// [`LoopError`] for a failure that the session cannot continue through. A
    /// socket that merely closed is not one of them: it is reported as a step.
    pub fn step(&mut self) -> Result<Step, LoopError> {
        let mut moved = self.accept_sockets()?;

        let ready = self.wait()?;
        if ready.broker {
            match self.read_broker()? {
                Some(step) => return Ok(step),
                None => moved = true,
            }
        }
        if ready.input {
            moved |= self.read_input();
        }
        moved |= ready.frame_hangup;

        moved |= self.pump_frame()?;

        Ok(if moved { Step::Progress } else { Step::Idle })
    }

    /// Binds any socket that has connected since the last turn.
    fn accept_sockets(&mut self) -> Result<bool, LoopError> {
        let Some(parameters) = self.parameters.clone() else {
            return Ok(false);
        };

        let mut moved = false;
        if self.frame.is_none()
            && let Some(mut socket) = self.frames.accept()?
        {
            let key = key(&parameters.frame_key);
            let generation = channel::bind(
                &mut socket,
                Channel::Frame,
                &key,
                &parameters.session_id,
                self.frame_generation,
            )?;
            self.frame_generation = Some(generation);
            // Writes must never block: a viewer that stopped reading would
            // otherwise stop capture, and capture is what the broker's vblank
            // thread is waiting on.
            set_nonblocking(socket.as_raw_fd())?;

            let geometry = Geometry::new(
                parameters.width,
                parameters.height,
                TileSize::from_pixels(parameters.tile_size)?,
                PixelFormat::Xrgb8888,
            )?;
            let mut pipeline = Pipeline::new(geometry, generation, parameters.cursor_stream);
            let mut tail = Vec::new();
            pipeline.write_stream_config(&mut tail, &self.limits)?;
            // A decoder that has just been built has nothing to apply a delta
            // to, so the first frame on any socket is a whole one.
            pipeline.request_keyframe();

            // A peer on a new socket holds no cursor either, so the next
            // snapshot reports one whether or not it moved.
            self.cursor = None;
            self.frame = Some(FrameChannel {
                socket,
                pipeline,
                tail,
            });
            moved = true;
        }

        if self.input.is_none()
            && let Some(mut socket) = self.inputs.accept()?
        {
            let key = key(&parameters.input_key);
            let generation = channel::bind(
                &mut socket,
                Channel::Input,
                &key,
                &parameters.session_id,
                self.input_generation,
            )?;
            self.input_generation = Some(generation);
            self.input = Some(socket);
            moved = true;
        }

        Ok(moved)
    }

    /// Waits for one of the descriptors to be ready.
    fn wait(&mut self) -> io::Result<Ready> {
        let mut watched: Vec<libc::pollfd> = Vec::with_capacity(3);
        watched.push(libc::pollfd {
            fd: self.broker.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        // The frame socket is watched for writability only while bytes are
        // owed to it -- there is nothing to read on it, since the host says
        // everything it has to say on control -- but it is watched at all
        // times, because a hangup is reported whatever was asked for. A viewer
        // that dropped its socket has to be noticed before the next frame is
        // encoded into it, not after the write that fails.
        if let Some(frame) = self.frame.as_ref() {
            watched.push(libc::pollfd {
                fd: frame.socket.as_raw_fd(),
                events: if frame.tail.is_empty() {
                    0
                } else {
                    libc::POLLOUT
                },
                revents: 0,
            });
        }
        if let Some(input) = self.input.as_ref() {
            watched.push(libc::pollfd {
                fd: input.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }

        // SAFETY: `watched` is a live, correctly sized array of `pollfd` for
        // the length passed, and every descriptor in it is owned by this
        // process for the duration of the call.
        let result = unsafe {
            libc::poll(
                watched.as_mut_ptr(),
                watched.len() as libc::nfds_t,
                STEP_TIMEOUT.as_millis() as libc::c_int,
            )
        };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == ErrorKind::Interrupted {
                return Ok(Ready::default());
            }

            return Err(error);
        }

        let mut ready = Ready::default();
        for entry in &watched {
            if entry.revents == 0 {
                continue;
            }
            if entry.fd == self.broker.as_raw_fd() {
                ready.broker = true;
            } else if self
                .input
                .as_ref()
                .is_some_and(|s| s.as_raw_fd() == entry.fd)
            {
                ready.input = true;
            } else if self
                .frame
                .as_ref()
                .is_some_and(|frame| frame.socket.as_raw_fd() == entry.fd)
                && entry.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
            {
                ready.frame_hangup = true;
            }
        }

        if ready.frame_hangup {
            // Dropped here rather than left for the next write to discover, so
            // that the frame this turn would have encoded is staged for the
            // socket that replaces it instead of lost with this one.
            if let Some(frame) = self.frame.take() {
                shutdown(frame.socket.as_raw_fd());
            }
            self.cursor = None;
        }

        Ok(ready)
    }

    /// Handles one message from the broker.
    ///
    /// `Some(step)` ends the turn: the session changed shape and nothing after
    /// it in this turn would still be true.
    fn read_broker(&mut self) -> Result<Option<Step>, LoopError> {
        let (message, descriptors) = match self.broker.receive() {
            Ok(received) => received,
            // The broker restarting is not this process's to survive: it holds
            // the device and the secret, and systemd brings both back.
            Err(_) => return Ok(Some(Step::BrokerLost)),
        };

        match message {
            Message::SessionOpened(parameters) => {
                self.limits
                    .set_geometry(parameters.width, parameters.height);
                self.parameters = Some(parameters);
                // A new session's sockets are new sockets: nothing bound to the
                // last one may carry into this one.
                self.close_session_sockets();
                self.buffers.clear();
                self.cursor = None;
                self.frame_generation = None;
                self.input_generation = None;
                self.ask_for_frame();

                Ok(None)
            }
            Message::SessionClosed { reason } => {
                eprintln!("vmlord-display-session: the session ended: {reason}");
                self.parameters = None;
                self.close_session_sockets();
                self.buffers.clear();
                self.cursor = None;
                // Nothing is asked for while there is no session: a process
                // that keeps asking for frames is one that never stopped
                // capturing.
                self.asked_for_frame = false;

                Ok(Some(Step::SessionClosed))
            }
            Message::KeyframeRequested => {
                if let Some(frame) = self.frame.as_mut() {
                    frame.pipeline.request_keyframe();
                }

                Ok(None)
            }
            // The refresh is the control thread's to report to the host: an
            // encoder is built on a geometry and paces itself on the frames it
            // is handed, so a timing means nothing here.
            Message::Geometry {
                width,
                height,
                refresh_hz: _,
            } => {
                self.resize(width, height)?;

                Ok(None)
            }
            Message::InputDevices => {
                let mut descriptors = descriptors.into_iter();
                match (descriptors.next(), descriptors.next()) {
                    (Some(keyboard), Some(pointer)) => {
                        self.keyboard = Some(Keyboard::new(File::from(keyboard)));
                        self.pointer = Some(Pointer::new(File::from(pointer)));
                    }
                    _ => eprintln!(
                        "vmlord-display-session: the broker offered input devices without their descriptors"
                    ),
                }

                Ok(None)
            }
            Message::Snapshot {
                sequence,
                planes,
                new_buffers,
            } => {
                self.asked_for_frame = false;
                // Start waiting for the next vblank before touching this
                // frame. Mapping and submitting take several milliseconds at
                // desktop resolutions; putting them ahead of NextFrame makes
                // that work part of every frame interval and produces uneven
                // pacing even when capture averages sixty frames a second.
                self.ask_for_frame();
                self.adopt(&planes, &new_buffers, descriptors);
                if let Err(error) = self.submit(sequence, &planes) {
                    eprintln!(
                        "vmlord-display-session: dropping frame {sequence}: the encoder refused it: {error}"
                    );
                }

                Ok(None)
            }
            // Everything else on this socket is this process's to send.
            other => {
                eprintln!("vmlord-display-session: ignoring {other:?} from the broker");

                Ok(None)
            }
        }
    }

    /// Rebuilds the encoder for an output that changed size.
    ///
    /// A geometry never changes inside an encoder -- a tile grid is built on
    /// one -- so a new size is a new encoder and a new stream: a `StreamConfig`
    /// and then a whole frame, which is exactly what a freshly bound socket
    /// gets. The socket itself is untouched, because nothing about the channel
    /// changed; only what travels on it did.
    ///
    /// # Errors
    ///
    /// [`LoopError`] if the codec will not build a stream of this shape, or if
    /// the config could not be written.
    fn resize(&mut self, width: u32, height: u32) -> Result<(), LoopError> {
        let Some(parameters) = self.parameters.as_mut() else {
            return Ok(());
        };
        if (parameters.width, parameters.height) == (width, height) {
            return Ok(());
        }
        parameters.width = width;
        parameters.height = height;
        let tile_size = parameters.tile_size;
        self.limits.set_geometry(width, height);

        let Some(frame) = self.frame.as_mut() else {
            return Ok(());
        };
        let geometry = Geometry::new(
            width,
            height,
            TileSize::from_pixels(tile_size)?,
            PixelFormat::Xrgb8888,
        )?;
        // The tail is whatever was owed for the old geometry, and none of it
        // describes this one. Dropping it costs the peer a frame it was going
        // to be sent; keeping it would cost the peer a frame it cannot decode.
        frame.tail.clear();
        frame.pipeline.reconfigure(geometry);
        frame
            .pipeline
            .write_stream_config(&mut frame.tail, &self.limits)?;
        // A decoder that has just been built holds no cursor either.
        self.cursor = None;

        Ok(())
    }

    /// Maps the buffers this snapshot brought with it.
    fn adopt(&mut self, planes: &[PlaneLayout], new_buffers: &[u64], descriptors: Vec<OwnedFd>) {
        for (id, descriptor) in new_buffers.iter().zip(descriptors) {
            let Some(plane) = planes.iter().find(|plane| plane.buffer == *id) else {
                continue;
            };
            let length = plane.stride as usize * plane.height as usize;
            match MappedBuffer::map(std::os::fd::AsFd::as_fd(&descriptor), length) {
                Ok(mapped) => {
                    self.buffers.insert(*id, mapped);
                }
                Err(error) => {
                    eprintln!("vmlord-display-session: a framebuffer would not map: {error}");
                }
            }
        }
    }

    /// Feeds one vblank's planes to the pipeline.
    fn submit(&mut self, sequence: u64, planes: &[PlaneLayout]) -> Result<(), LoopError> {
        let Some(frame) = self.frame.as_mut() else {
            return Ok(());
        };
        let Some(primary) = planes.iter().find(|plane| plane.kind == PlaneKind::Primary) else {
            return Ok(());
        };
        // A frame captured either side of a mode change: the encoder is built
        // on a geometry and cannot take another shape. Dropping it is right --
        // the buffer of the new size is one vblank away, and the broker's
        // `Geometry` arrives with it.
        let encoded = frame.pipeline.geometry();
        if (primary.width, primary.height) != (encoded.width(), encoded.height()) {
            return Ok(());
        }

        // The cursor goes in first: when the peer declined the cursor stream
        // the pipeline draws it into the frame, and a cursor submitted after
        // the frame would be one frame late.
        let cursor = planes.iter().find(|plane| plane.kind == PlaneKind::Cursor);
        let placement = match cursor {
            Some(plane) => cursor::place(
                plane.x,
                plane.y,
                plane.width,
                plane.height,
                primary.width,
                primary.height,
            ),
            // No cursor plane is a hidden pointer, and the peer has to be told
            // to stop drawing it rather than left with the last one.
            None => hidden(primary.width, primary.height),
        };
        // The bitmap is sent whenever there is one to send, since this build
        // has no way to tell a new bitmap from the one before it; the position
        // is sent only when it moved.
        let image = cursor.and_then(|plane| {
            self.buffers
                .get(&plane.buffer)
                .map(|mapped| (mapped.read(<[u8]>::to_vec), plane.width, plane.height))
        });
        if image.is_some() || self.cursor != Some(placement) {
            let borrowed = image
                .as_ref()
                .map(|(pixels, width, height)| (pixels.as_slice(), *width, *height));
            frame.pipeline.submit_cursor(borrowed, &placement)?;
            self.cursor = Some(placement);
        }

        let Some(mapped) = self.buffers.remove(&primary.buffer) else {
            return Ok(());
        };
        // The mapping is moved into the frame and back, because a captured
        // frame owns its backing and this one outlives the frame: it is the
        // same scanout buffer the compositor cycles back to.
        let captured = CapturedFrame {
            sequence,
            width: primary.width,
            height: primary.height,
            stride: primary.stride,
            format: pixel_format(primary.format),
            damage: primary.damage.clone(),
            backing: Backing::Cpu(mapped),
        };
        let outcome = frame.pipeline.submit_frame(&captured);
        if let Backing::Cpu(mapped) = captured.backing {
            self.buffers.insert(primary.buffer, mapped);
        }
        outcome?;

        Ok(())
    }

    /// Drains what is owed to the frame socket, then encodes if it emptied.
    fn pump_frame(&mut self) -> Result<bool, LoopError> {
        let Some(frame) = self.frame.as_mut() else {
            return Ok(false);
        };

        let mut moved = false;
        loop {
            while !frame.tail.is_empty() {
                match frame.socket.write(&frame.tail) {
                    Ok(0) => {
                        self.frame = None;

                        return Ok(true);
                    }
                    Ok(written) => {
                        frame.tail.drain(..written);
                        moved = true;
                    }
                    Err(error) if error.kind() == ErrorKind::Interrupted => {}
                    // A socket that will take no more is a viewer that is
                    // behind. What it costs is captured frames, never a queue.
                    Err(error) if would_block(&error) => return Ok(moved),
                    Err(_) => {
                        self.frame = None;

                        return Ok(true);
                    }
                }
            }

            // Only now, with nothing owed, is the next payload encoded: the
            // encoder's reference must equal what the peer actually holds.
            if !frame.pipeline.write_next(&mut frame.tail, &self.limits)? {
                return Ok(moved);
            }
            moved = true;
        }
    }

    /// Reads whatever the input channel has and puts it on the devices.
    ///
    /// A guest whose broker found no uinput still reads the channel: an unread
    /// record would stall the socket, and the host has nothing to do about a
    /// kernel that has no uinput in it.
    fn read_input(&mut self) -> bool {
        let Some(socket) = self.input.as_mut() else {
            return false;
        };

        let mut payload = Vec::new();
        match record::read(socket, &self.limits, &mut payload) {
            Ok(header) => {
                if self
                    .input_generation
                    .is_some_and(|generation| header.generation != generation)
                {
                    // A record from a connection that has been replaced must
                    // not reach an input device.
                    eprintln!(
                        "vmlord-display-session: an input record from generation {} arrived on a channel bound at {:?}",
                        header.generation, self.input_generation
                    );
                    self.close_input();

                    return true;
                }
                self.input_records += 1;
                self.apply_input(header.message_type, &payload);

                true
            }
            Err(RecordError::Idle) => false,
            Err(error) => {
                eprintln!(
                    "vmlord-display-session: the input channel closed; nothing is held to release ({error})"
                );
                self.close_input();

                true
            }
        }
    }

    /// Tells the broker this process is ready for another frame.
    fn ask_for_frame(&mut self) {
        if self.asked_for_frame || self.parameters.is_none() {
            return;
        }
        if self.broker.send(&Message::NextFrame, &[]).is_ok() {
            self.asked_for_frame = true;
        }
    }

    /// Puts one input record on the devices, if there are any.
    ///
    /// A record type this build has no name for is ignored rather than
    /// refused: the protocol's forward-compatibility rule is that an unknown
    /// message changes nothing.
    fn apply_input(&mut self, message_type: u16, payload: &[u8]) {
        let Ok(record) = InputRecord::try_from(i32::from(message_type)) else {
            return;
        };
        let Some(parameters) = self.parameters.as_ref() else {
            return;
        };
        let (width, height) = (parameters.width, parameters.height);

        let applied = match record {
            InputRecord::KeyEvent => KeyEvent::decode(payload).map(|event| {
                self.keyboard.as_mut().map_or(Ok(()), |keyboard| {
                    keyboard.key(u16::try_from(event.keycode).unwrap_or(0), event.pressed)
                })
            }),
            InputRecord::PointerMotion => PointerMotion::decode(payload).map(|motion| {
                self.pointer.as_mut().map_or(Ok(()), |pointer| {
                    pointer.motion(motion.x, motion.y, width, height)
                })
            }),
            InputRecord::PointerButton => PointerButton::decode(payload).map(|event| {
                self.pointer.as_mut().map_or(Ok(()), |pointer| {
                    pointer.button(u16::try_from(event.button).unwrap_or(0), event.pressed)
                })
            }),
            InputRecord::PointerScroll => PointerScroll::decode(payload).map(|scroll| {
                self.pointer.as_mut().map_or(Ok(()), |pointer| {
                    pointer.scroll(scroll.horizontal, scroll.vertical)
                })
            }),
            InputRecord::ReleaseAll => {
                self.release_input();

                return;
            }
            _ => return,
        };

        match applied {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                eprintln!("vmlord-display-session: an input device refused an event: {error}");
            }
            Err(error) => {
                eprintln!("vmlord-display-session: an input record would not decode: {error}");
            }
        }
    }

    /// Releases everything both devices believe is held.
    ///
    /// Called for every way the channel can end, because a key the guest is
    /// still holding is worse than a session that was lost.
    fn release_input(&mut self) {
        if let Some(keyboard) = self.keyboard.as_mut()
            && let Err(error) = keyboard.release_all()
        {
            eprintln!("vmlord-display-session: the keyboard would not release: {error}");
        }
        if let Some(pointer) = self.pointer.as_mut()
            && let Err(error) = pointer.release_all()
        {
            eprintln!("vmlord-display-session: the pointer would not release: {error}");
        }
    }

    /// Closes both session sockets, waking anything blocked on them.
    fn close_session_sockets(&mut self) {
        if let Some(frame) = self.frame.take() {
            shutdown(frame.socket.as_raw_fd());
        }
        self.close_input();
    }

    /// Closes the input socket alone, releasing whatever the guest still holds.
    fn close_input(&mut self) {
        self.release_input();
        if let Some(input) = self.input.take() {
            shutdown(input.as_raw_fd());
        }
    }
}

/// Which descriptors a `poll` reported.
#[derive(Clone, Copy, Debug, Default)]
struct Ready {
    broker: bool,
    input: bool,
    /// The host let go of the frame socket.
    frame_hangup: bool,
}

/// A cursor that is nowhere, which is how a hidden pointer is reported.
fn hidden(width: u32, height: u32) -> Placement {
    cursor::place(
        i32::from(i16::MAX),
        i32::from(i16::MAX),
        0,
        0,
        width,
        height,
    )
}

/// A channel key from the bytes the broker sent.
fn key(bytes: &[u8]) -> ChannelKey {
    let mut material = [0u8; 32];
    let taken = material.len().min(bytes.len());
    material[..taken].copy_from_slice(&bytes[..taken]);

    ChannelKey::from_bytes(material)
}

/// The codec's format for a DRM fourcc.
///
/// Anything else has already been refused by the broker, which will not export
/// a framebuffer this build cannot map.
fn pixel_format(fourcc: u32) -> PixelFormat {
    match fourcc {
        DRM_FORMAT_ARGB8888 => PixelFormat::Bgra8888,
        // `DRM_FORMAT_XRGB8888` and nothing else reaches here.
        _ => PixelFormat::Xrgb8888,
    }
}

/// Whether an error means "not now" rather than "not ever".
fn would_block(error: &io::Error) -> bool {
    matches!(error.kind(), ErrorKind::WouldBlock) || error.raw_os_error() == Some(libc::EAGAIN)
}

/// Makes writes to a descriptor return rather than wait.
fn set_nonblocking(descriptor: RawFd) -> io::Result<()> {
    // SAFETY: `fcntl` with these commands takes a descriptor and an int, and
    // the descriptor is one this process owns.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

/// Ends a socket in both directions, waking whoever is on it.
fn shutdown(descriptor: RawFd) {
    // SAFETY: `shutdown` takes a descriptor and a flag, and one that is not an
    // open socket makes it fail rather than misbehave.
    unsafe {
        libc::shutdown(descriptor, libc::SHUT_RDWR);
    }
}

/// A vsock listener that does not block waiting for a connection.
pub struct VsockAcceptor {
    listener: crate::vsock::Listener,
}

impl VsockAcceptor {
    /// Binds a port and makes its accepts return rather than wait.
    ///
    /// # Errors
    ///
    /// [`io::Error`] if the port cannot be bound.
    pub fn bind(port: u32) -> io::Result<Self> {
        let listener = crate::vsock::Listener::bind(port)?;
        set_nonblocking(listener.as_raw_fd())?;

        Ok(Self { listener })
    }
}

impl Acceptor for VsockAcceptor {
    type Socket = crate::vsock::Stream;

    fn accept(&mut self) -> io::Result<Option<Self::Socket>> {
        match self.listener.accept() {
            Ok(stream) => {
                // The bind that follows is a fixed three-record exchange, and
                // it is the one place this process is allowed to wait.
                set_blocking(stream.as_raw_fd())?;

                Ok(Some(stream))
            }
            Err(error) if would_block(&error) => Ok(None),
            Err(error) if error.kind() == ErrorKind::Interrupted => Ok(None),
            Err(error) => Err(error),
        }
    }
}

/// Makes a descriptor's calls wait again.
fn set_blocking(descriptor: RawFd) -> io::Result<()> {
    // SAFETY: as `set_nonblocking`.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

/// What the session process was told to do.
#[derive(Clone, Debug)]
pub struct Options {
    /// The broker's socket.
    pub socket: PathBuf,
}

impl Options {
    /// The defaults, with the environment allowed to override them.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            socket: std::env::var("VMLORD_DISPLAY_SOCKET")
                .unwrap_or_else(|_| "/run/vmlord/display-broker.sock".to_owned())
                .into(),
        }
    }
}

/// Runs the session process until the broker goes away.
#[must_use]
pub fn run(options: Options) -> std::process::ExitCode {
    match serve(&options) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("vmlord-display-session: {error}");

            std::process::ExitCode::FAILURE
        }
    }
}

/// The body of [`run`].
fn serve(options: &Options) -> Result<(), LoopError> {
    // Both ports are bound before any session exists: a host that connects the
    // instant control opens must not find nothing listening.
    let frames = VsockAcceptor::bind(crate::vsock::FRAME_PORT)?;
    let inputs = VsockAcceptor::bind(crate::vsock::INPUT_PORT)?;
    let broker = connect_to_broker(&options.socket)?;
    broker.send(&Message::Attach, &[])?;

    let mut running = Loop::new(broker, frames, inputs);
    loop {
        match running.step() {
            Ok(Step::BrokerLost) => {
                // The broker holds the device and the secret; systemd brings
                // both back, and this process with them.
                return Err(LoopError::Io(io::Error::new(
                    ErrorKind::ConnectionReset,
                    "the display broker went away",
                )));
            }
            Ok(_) => {}
            Err(error) => {
                // A session that failed is a session to wait for again, not a
                // process to end: the next viewer gets a fresh one.
                eprintln!("vmlord-display-session: {error}");
            }
        }
    }
}

/// Connects to the broker, waiting for it if it is not up yet.
fn connect_to_broker(path: &std::path::Path) -> io::Result<Connection> {
    let mut wait = Duration::from_millis(100);
    for _ in 0..50 {
        match Connection::connect(path) {
            Ok(connection) => return Ok(connection),
            Err(_) => {
                std::thread::sleep(wait);
                wait = (wait * 2).min(Duration::from_secs(2));
            }
        }
    }

    Err(io::Error::new(
        ErrorKind::NotConnected,
        "the display broker's socket never appeared",
    ))
}

#[cfg(test)]
mod tests {
    use std::{
        io::{self, ErrorKind, Read, Write},
        os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd},
        sync::{Arc, Mutex},
        thread::JoinHandle,
    };

    use prost::Message as _;
    use vmlord_display_protocol::{
        keys::Secret,
        record::{self, Channel, Header, Limits, Record},
        session::{Event, Offer, Session, Support},
        v1::{
            Capability, FrameRecord, InputRecord, KeyEvent, Mode, PointerButton, PointerMotion,
            StreamConfig,
        },
    };

    use super::{Acceptor, Loop, Socket, Step, set_nonblocking};
    use crate::{
        ipc::{Message, PlaneKind, PlaneLayout, SessionParameters},
        unix::{Connection, Listener},
    };

    /// The geometry the world runs at. Small, so a keyframe is small and a
    /// stalled socket is reachable without megabytes of pixels.
    const WIDTH: u32 = 64;
    const HEIGHT: u32 = 64;
    /// What the output is resized to, and the cap the host reads records at.
    const MAX_WIDTH: u32 = 128;
    const MAX_HEIGHT: u32 = 96;
    const TILE: u32 = 32;

    /// One end of a `socketpair`, which is what stands in for a vsock.
    #[derive(Debug)]
    struct Pipe {
        descriptor: OwnedFd,
    }

    impl Pipe {
        /// A connected pair.
        fn pair() -> (Self, Self) {
            let mut fds = [0 as libc::c_int; 2];
            // SAFETY: `fds` is a live array of two ints, which is what
            // `socketpair` writes.
            let result = unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
                    0,
                    fds.as_mut_ptr(),
                )
            };
            assert!(result >= 0, "a socketpair: {}", io::Error::last_os_error());

            // SAFETY: both descriptors are ones this process now owns.
            unsafe {
                (
                    Self {
                        descriptor: OwnedFd::from_raw_fd(fds[0]),
                    },
                    Self {
                        descriptor: OwnedFd::from_raw_fd(fds[1]),
                    },
                )
            }
        }

        /// Shrinks the buffers, so a peer that stops reading stalls the writer
        /// within a few small frames rather than a few megabytes of them.
        fn narrow(&self) {
            let size: libc::c_int = 2048;
            for option in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
                // SAFETY: `size` is a live `c_int` and the length is its exact
                // size; the descriptor is this pipe's own.
                unsafe {
                    libc::setsockopt(
                        self.descriptor.as_raw_fd(),
                        libc::SOL_SOCKET,
                        option,
                        (&raw const size).cast(),
                        size_of_val(&size) as libc::socklen_t,
                    );
                }
            }
        }

        /// Whether the peer has closed its end.
        fn is_closed(&self) -> bool {
            let mut watched = libc::pollfd {
                fd: self.descriptor.as_raw_fd(),
                events: 0,
                revents: 0,
            };
            // SAFETY: `watched` is one live `pollfd` and the count matches.
            unsafe {
                libc::poll(&raw mut watched, 1, 0);
            }

            watched.revents & (libc::POLLHUP | libc::POLLERR) != 0
        }

        /// Reads whatever is there without waiting for more.
        fn drain(&self) -> Vec<u8> {
            let mut all = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                // SAFETY: `buffer` is a live byte array for the length passed.
                let read = unsafe {
                    libc::recv(
                        self.descriptor.as_raw_fd(),
                        buffer.as_mut_ptr().cast(),
                        buffer.len(),
                        libc::MSG_DONTWAIT,
                    )
                };
                if read <= 0 {
                    return all;
                }
                all.extend_from_slice(&buffer[..read as usize]);
            }
        }
    }

    impl Read for Pipe {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            // SAFETY: `buffer` is a live mutable byte slice for this call.
            let result = unsafe {
                libc::read(
                    self.descriptor.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                )
            };
            if result < 0 {
                return Err(io::Error::last_os_error());
            }

            Ok(result as usize)
        }
    }

    impl Write for Pipe {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            // SAFETY: `buffer` is a live byte slice for this call.
            let result = unsafe {
                libc::write(
                    self.descriptor.as_raw_fd(),
                    buffer.as_ptr().cast(),
                    buffer.len(),
                )
            };
            if result < 0 {
                return Err(io::Error::last_os_error());
            }

            Ok(result as usize)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl AsRawFd for Pipe {
        fn as_raw_fd(&self) -> RawFd {
            self.descriptor.as_raw_fd()
        }
    }

    /// A queue of sockets the world has decided to offer.
    #[derive(Default)]
    struct Queued {
        waiting: Vec<Pipe>,
    }

    impl Acceptor for Arc<Mutex<Queued>> {
        type Socket = Pipe;

        fn accept(&mut self) -> io::Result<Option<Pipe>> {
            Ok(self
                .lock()
                .expect("the world's lock is not poisoned")
                .waiting
                .pop())
        }
    }

    /// The whole guest side, with a real host on the far end of each socket.
    struct World {
        loops: Loop<Arc<Mutex<Queued>>, Arc<Mutex<Queued>>>,
        /// The broker's end of the IPC socket.
        broker: Connection,
        host: Arc<Mutex<Session>>,
        frames: Arc<Mutex<Queued>>,
        inputs: Arc<Mutex<Queued>>,
        /// The host's end of the frame socket, once one exists.
        host_frame: Option<Arc<Pipe>>,
        host_input: Option<Arc<Pipe>>,
        /// The bind responders, joined so a panic in one is not swallowed.
        responders: Vec<JoinHandle<()>>,
        /// Which channels the host has already opened once, so a second socket
        /// reconnects -- and so advances the generation -- rather than
        /// replaying the first hello.
        opened: Vec<Channel>,
        /// Whether the host is reading its frame socket.
        reading: bool,
        /// What the host has read and not yet been asked about.
        read_frames: Vec<u8>,
        /// The messages the broker has received.
        from_session: Vec<Message>,
        /// How many messages had arrived when the session was closed.
        closed_at: Option<usize>,
        /// Where the socket lives, removed on drop.
        socket_path: std::path::PathBuf,
        /// The read ends of the two device pipes, where the broker's real
        /// uinput descriptors go.
        keyboard: Option<std::io::PipeReader>,
        pointer: Option<std::io::PipeReader>,
    }

    impl World {
        /// A session that is open, with both processes, a real host and the
        /// two input devices the broker hands over.
        fn open() -> Self {
            Self::build(true)
        }

        /// The same on a guest whose kernel has no uinput.
        fn open_without_devices() -> Self {
            Self::build(false)
        }

        fn build(with_devices: bool) -> Self {
            let secret = Secret::generate();
            let (mut host, client_hello) = Session::host(
                &secret,
                Offer {
                    capabilities: vec![Capability::CursorStream],
                    mode: Mode::Desktop,
                    width: WIDTH,
                    height: HEIGHT,
                    tile_size: TILE,
                },
            );
            let mut guest = Session::guest(
                &secret,
                Support {
                    capabilities: vec![Capability::CursorStream],
                    modes: vec![Mode::Desktop],
                    tile_sizes: vec![TILE],
                    width: WIDTH,
                    height: HEIGHT,
                },
            );

            let server_hello = guest
                .handle(&client_hello.header, &client_hello.payload)
                .expect("a client hello")
                .reply
                .expect("a server hello");
            let server_auth = guest.pending_auth().expect("the guest's proof");
            host.handle(&server_hello.header, &server_hello.payload)
                .expect("a server hello");
            let client_auth = host
                .handle(&server_auth.header, &server_auth.payload)
                .expect("a guest proof")
                .reply
                .expect("the host's proof");
            let outcome = guest
                .handle(&client_auth.header, &client_auth.payload)
                .expect("a host proof");
            assert_eq!(outcome.event, Event::ControlEstablished);

            let parameters = SessionParameters {
                session_id: guest.session_id().to_vec(),
                frame_key: guest
                    .derive_channel_key(Channel::Frame)
                    .expect("a frame key")
                    .to_bytes()
                    .to_vec(),
                input_key: guest
                    .derive_channel_key(Channel::Input)
                    .expect("an input key")
                    .to_bytes()
                    .to_vec(),
                width: WIDTH,
                height: HEIGHT,
                tile_size: TILE,
                cursor_stream: true,
            };

            let socket_path = std::env::temp_dir().join(format!(
                "vmlord-display-session-{}-{:?}.sock",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_file(&socket_path);
            // SAFETY: `getgid` takes nothing and cannot fail.
            let group = unsafe { libc::getgid() };
            let listener = Listener::bind(&socket_path, group).expect("the broker's socket");
            let session_side = Connection::connect(&socket_path).expect("the session's end");
            // SAFETY: `getuid` takes nothing and cannot fail.
            let broker = listener
                .accept(unsafe { libc::getuid() })
                .expect("the broker's end");
            // The world drives both ends from one thread, so the broker's end
            // must never wait for a message the loop has not produced yet.
            broker
                .set_nonblocking()
                .expect("the broker's end does not block");

            let frames = Arc::new(Mutex::new(Queued::default()));
            let inputs = Arc::new(Mutex::new(Queued::default()));
            let mut world = Self {
                loops: Loop::new(session_side, Arc::clone(&frames), Arc::clone(&inputs)),
                broker,
                host: Arc::new(Mutex::new(host)),
                frames,
                inputs,
                host_frame: None,
                host_input: None,
                responders: Vec::new(),
                opened: Vec::new(),
                reading: true,
                read_frames: Vec::new(),
                from_session: Vec::new(),
                closed_at: None,
                socket_path,
                keyboard: None,
                pointer: None,
            };

            if with_devices {
                // Before the parameters, as the broker sends them: a session
                // that learned its parameters first would drop the input
                // records it read before the devices arrived.
                let (keyboard_reader, keyboard_writer) = std::io::pipe().expect("a pipe");
                let (pointer_reader, pointer_writer) = std::io::pipe().expect("a pipe");
                world
                    .broker
                    .send(
                        &Message::InputDevices,
                        &[keyboard_writer.as_fd(), pointer_writer.as_fd()],
                    )
                    .expect("the devices go out");
                world.keyboard = Some(keyboard_reader);
                world.pointer = Some(pointer_reader);
                world.step();
            }

            world
                .broker
                .send(&Message::SessionOpened(parameters), &[])
                .expect("the broker opens the session");
            world.step();
            world.open_frame_socket();
            world.open_input_socket();
            world.settle();

            world
        }

        /// Offers a frame socket and runs the host's side of its bind.
        fn open_frame_socket(&mut self) {
            let (guest_end, host_end) = Pipe::pair();
            host_end.narrow();
            guest_end.narrow();
            let host_end = Arc::new(host_end);
            self.host_frame = Some(Arc::clone(&host_end));
            self.frames
                .lock()
                .expect("the world's lock is not poisoned")
                .waiting
                .push(guest_end);
            self.respond(Channel::Frame, host_end);
        }

        /// The same for the input socket.
        fn open_input_socket(&mut self) {
            let (guest_end, host_end) = Pipe::pair();
            let host_end = Arc::new(host_end);
            self.host_input = Some(Arc::clone(&host_end));
            self.inputs
                .lock()
                .expect("the world's lock is not poisoned")
                .waiting
                .push(guest_end);
            self.respond(Channel::Input, host_end);
        }

        /// Runs the host's half of one channel bind, on a thread, because the
        /// guest's half blocks waiting for it.
        fn respond(&mut self, channel: Channel, host_end: Arc<Pipe>) {
            let host = Arc::clone(&self.host);
            let again = self.opened.contains(&channel);
            if !again {
                self.opened.push(channel);
            }
            self.responders.push(std::thread::spawn(move || {
                let limits = Limits::new(WIDTH, HEIGHT);
                let hello = {
                    let mut host = host.lock().expect("the world's lock is not poisoned");
                    if again {
                        host.reconnect_channel(channel)
                    } else {
                        host.open_channel(channel)
                    }
                    .expect("a channel hello")
                };
                let mut socket = HostEnd(host_end);
                record::write(&mut socket, &hello, &limits).expect("the hello goes out");

                let mut payload = Vec::new();
                let header =
                    record::read(&mut socket, &limits, &mut payload).expect("the guest's ack");
                let reply = {
                    let mut host = host.lock().expect("the world's lock is not poisoned");
                    host.handle(&header, &payload)
                        .expect("a well-formed ack")
                        .reply
                        .expect("the host's proof")
                };
                record::write(&mut socket, &reply, &limits).expect("the proof goes out");
            }));
        }

        /// One turn of the loop, ignoring what it found.
        fn step(&mut self) -> Step {
            let step = self.loops.step().expect("a step that does not fail");
            if step == Step::SessionClosed {
                self.closed_at = Some(self.from_session.len());
            }
            self.collect();
            self.read_available();

            step
        }

        /// Steps until nothing moves, so every pending record is handled.
        fn settle(&mut self) {
            for _ in 0..64 {
                if self.step() == Step::Idle {
                    return;
                }
            }
        }

        /// Steps until the host has whole records and nothing is still moving.
        ///
        /// Both halves matter: a keyframe is larger than the narrowed socket
        /// buffers, so the first byte arrives many steps before the last one.
        fn run_until_written(&mut self) {
            for _ in 0..1024 {
                let step = self.step();
                if !self.read_frames.is_empty() && step == Step::Idle {
                    return;
                }
            }
        }

        /// Steps until nothing moves.
        fn run_until_idle(&mut self) {
            self.settle();
        }

        /// Takes whatever the session process sent the broker.
        fn collect(&mut self) {
            while let Ok((message, _)) = self.broker.receive() {
                self.from_session.push(message);
            }
        }

        /// Reads the frame socket, if the host is reading it.
        fn read_available(&mut self) {
            if !self.reading {
                return;
            }
            if let Some(frame) = self.host_frame.as_ref() {
                self.read_frames.extend(frame.drain());
            }
        }

        /// The frame records the host has received.
        fn host_reads_frame_records(&mut self) -> Vec<Header> {
            self.host_reads_frame_stream()
                .into_iter()
                .map(|(header, _)| header)
                .collect()
        }

        /// The same, with each record's payload.
        fn host_reads_frame_stream(&mut self) -> Vec<(Header, Vec<u8>)> {
            self.read_available();
            let bytes = std::mem::take(&mut self.read_frames);
            let limits = Limits::new(MAX_WIDTH, MAX_HEIGHT);
            let mut reader = bytes.as_slice();
            let mut payload = Vec::new();
            let mut headers = Vec::new();
            while !reader.is_empty() {
                match record::read(&mut reader, &limits, &mut payload) {
                    Ok(header) => headers.push((header, payload.clone())),
                    // A record that was cut in half by the read is not one to
                    // fail on: the next drain finishes it.
                    Err(_) => break,
                }
            }

            headers
        }

        /// The broker sends one vblank's planes, with a memfd behind each.
        fn broker_sends_snapshot(&mut self, sequence: u64) {
            self.broker_sends_snapshot_sized(sequence, WIDTH, HEIGHT);
        }

        /// Sends a snapshot with a cursor the codec refuses. This is what a
        /// malformed capture must cost: one frame, not the capture session.
        fn broker_sends_snapshot_with_oversized_cursor(&mut self, sequence: u64) {
            let primary = crate::unix::memfd("frame", &vec![0; (WIDTH * HEIGHT * 4) as usize])
                .expect("a framebuffer");
            let cursor = crate::unix::memfd("oversized-cursor", &vec![0; 257 * 4])
                .expect("an oversized cursor bitmap");
            let primary_buffer = sequence * 2;
            let cursor_buffer = primary_buffer + 1;
            let planes = vec![
                PlaneLayout {
                    kind: PlaneKind::Primary,
                    damage: None,
                    buffer: primary_buffer,
                    width: WIDTH,
                    height: HEIGHT,
                    stride: WIDTH * 4,
                    format: crate::drm::uapi::DRM_FORMAT_XRGB8888,
                    x: 0,
                    y: 0,
                },
                PlaneLayout {
                    kind: PlaneKind::Cursor,
                    damage: None,
                    buffer: cursor_buffer,
                    width: 257,
                    height: 1,
                    stride: 257 * 4,
                    format: crate::drm::uapi::DRM_FORMAT_ARGB8888,
                    x: 0,
                    y: 0,
                },
            ];

            self.broker
                .send(
                    &Message::Snapshot {
                        sequence,
                        planes,
                        new_buffers: vec![primary_buffer, cursor_buffer],
                    },
                    &[
                        std::os::fd::AsFd::as_fd(&primary),
                        std::os::fd::AsFd::as_fd(&cursor),
                    ],
                )
                .expect("a snapshot with an oversized cursor");
        }

        /// The same, for an output that is not the one the session opened on.
        fn broker_sends_snapshot_sized(&mut self, sequence: u64, width: u32, height: u32) {
            let (width, height) = (width, height);
            let stride = width * 4;
            // Noise rather than a flat colour: a uniform frame compresses to
            // almost nothing, and a socket that never fills would not test what
            // a slow one costs.
            let mut state = sequence.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
            let pixels: Vec<u8> = (0..(stride * height))
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state as u8
                })
                .collect();
            let buffer = crate::unix::memfd("frame", &pixels).expect("a framebuffer");
            let planes = vec![PlaneLayout {
                kind: PlaneKind::Primary,
                damage: None,
                buffer: sequence,
                width,
                height,
                stride,
                format: crate::drm::uapi::DRM_FORMAT_XRGB8888,
                x: 0,
                y: 0,
            }];

            self.broker
                .send(
                    &Message::Snapshot {
                        sequence,
                        planes,
                        new_buffers: vec![sequence],
                    },
                    &[std::os::fd::AsFd::as_fd(&buffer)],
                )
                .expect("a snapshot");
        }

        /// The broker reports that control was lost.
        fn broker_sends_session_closed(&mut self, reason: &str) {
            self.broker
                .send(
                    &Message::SessionClosed {
                        reason: reason.to_owned(),
                    },
                    &[],
                )
                .expect("a session closed");
        }

        /// The broker reports the size the output came up at.
        fn broker_sends_geometry(&mut self, width: u32, height: u32) {
            self.broker
                .send(
                    &Message::Geometry {
                        width,
                        height,
                        refresh_hz: 60,
                    },
                    &[],
                )
                .expect("a geometry");
        }

        /// The broker relays a keyframe request.
        fn broker_sends_keyframe_request(&mut self) {
            self.broker
                .send(&Message::KeyframeRequested, &[])
                .expect("a keyframe request");
        }

        /// Whether the frame socket has been closed from the guest's side.
        fn frame_socket_is_closed(&self) -> bool {
            self.host_frame
                .as_ref()
                .is_some_and(|pipe| pipe.is_closed())
        }

        /// The same for the input socket.
        fn input_socket_is_closed(&self) -> bool {
            self.host_input
                .as_ref()
                .is_some_and(|pipe| pipe.is_closed())
        }

        /// Whether a frame was asked for after the session ended.
        fn broker_saw_next_frame_after_close(&self) -> bool {
            let Some(at) = self.closed_at else {
                return false;
            };

            self.from_session[at..]
                .iter()
                .any(|message| matches!(message, Message::NextFrame))
        }

        /// How many snapshots the session has acknowledged by asking for the
        /// next one.
        fn broker_received_next_frames(&self) -> usize {
            self.from_session
                .iter()
                .filter(|message| matches!(message, Message::NextFrame))
                .count()
        }

        /// The host drops its frame socket and opens another.
        fn host_drops_and_reopens_the_frame_socket(&mut self) {
            self.host_frame = None;
            self.settle();
            // What the old socket carried is not what the new one's assertions
            // are about, and it is cleared before the new socket exists so that
            // nothing the new one is sent can be cleared with it.
            self.read_frames.clear();
            self.open_frame_socket();
            self.settle();
        }

        fn host_stops_reading(&mut self) {
            self.reading = false;
        }

        fn host_resumes_reading(&mut self) {
            self.reading = true;
        }

        /// The host sends a key event on the input channel.
        fn host_sends_key_event(&mut self, keycode: u32, pressed: bool) {
            let generation = self
                .host
                .lock()
                .expect("the world's lock is not poisoned")
                .generation(Channel::Input);
            self.send_input(generation, keycode, pressed);
        }

        /// The same, but claiming a generation the channel is not on.
        fn host_sends_input_with_generation(&mut self, generation: u32) {
            self.send_input(generation.wrapping_add(7), 30, true);
        }

        fn send_input(&mut self, generation: u32, keycode: u32, pressed: bool) {
            use prost::Message as _;

            let Some(input) = self.host_input.as_ref() else {
                return;
            };
            let record = Record::new(
                Channel::Input,
                InputRecord::KeyEvent as u16,
                1,
                0,
                generation,
                KeyEvent { keycode, pressed }.encode_to_vec(),
            );
            let mut socket = HostEnd(Arc::clone(input));
            record::write(&mut socket, &record, &Limits::new(WIDTH, HEIGHT))
                .expect("the key event goes out");
        }

        /// The host sends a pointer motion.
        fn host_sends_motion(&mut self, x: u32, y: u32) {
            self.send_record(
                InputRecord::PointerMotion,
                PointerMotion { x, y }.encode_to_vec(),
            );
        }

        /// The host presses or releases a pointer button.
        fn host_sends_button(&mut self, button: u32, pressed: bool) {
            self.send_record(
                InputRecord::PointerButton,
                PointerButton { button, pressed }.encode_to_vec(),
            );
        }

        /// The host asks the guest to release everything.
        fn host_sends_release_all(&mut self) {
            self.send_record(InputRecord::ReleaseAll, Vec::new());
        }

        /// The host drops its end of the input socket.
        fn host_closes_input_socket(&mut self) {
            // The last reference to the descriptor, so dropping it is what the
            // session sees as the socket going away.
            drop(self.host_input.take());
        }

        /// One record on the input channel, at the generation it is bound at.
        fn send_record(&mut self, kind: InputRecord, payload: Vec<u8>) {
            let generation = self
                .host
                .lock()
                .expect("the world's lock is not poisoned")
                .generation(Channel::Input);
            let Some(input) = self.host_input.as_ref() else {
                return;
            };
            let record = Record::new(Channel::Input, kind as u16, 1, 0, generation, payload);
            let mut socket = HostEnd(Arc::clone(input));
            record::write(&mut socket, &record, &Limits::new(WIDTH, HEIGHT))
                .expect("the record goes out");
        }

        /// The `(type, code, value)` triples the keyboard has been sent.
        fn keyboard_events(&mut self) -> Vec<(u16, u16, i32)> {
            Self::device_events(self.keyboard.as_mut())
        }

        /// The same for the pointer.
        fn pointer_events(&mut self) -> Vec<(u16, u16, i32)> {
            Self::device_events(self.pointer.as_mut())
        }

        /// Whatever is on a device pipe right now, and no waiting.
        ///
        /// Non-blocking, because a device with nothing on it is what half of
        /// these tests assert.
        fn device_events(reader: Option<&mut std::io::PipeReader>) -> Vec<(u16, u16, i32)> {
            let Some(reader) = reader else {
                return Vec::new();
            };
            set_nonblocking(reader.as_raw_fd()).expect("a non-blocking pipe");

            let mut bytes = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => bytes.extend_from_slice(&chunk[..read]),
                    Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                    Err(error) => panic!("a device pipe failed: {error}"),
                }
            }

            bytes
                .chunks_exact(24)
                .map(|event| {
                    (
                        u16::from_ne_bytes([event[16], event[17]]),
                        u16::from_ne_bytes([event[18], event[19]]),
                        i32::from_ne_bytes([event[20], event[21], event[22], event[23]]),
                    )
                })
                .collect()
        }

        /// How many input records the session process consumed.
        fn input_records_consumed(&self) -> u64 {
            self.loops.input_records()
        }
    }

    impl Drop for World {
        fn drop(&mut self) {
            // The responders hold a socket each; dropping the world's ends
            // first would leave one blocked on a read that never finishes.
            for responder in self.responders.drain(..) {
                let _ = responder.join();
            }
            let _ = std::fs::remove_file(&self.socket_path);
        }
    }

    /// The host's end of a socket, so a shared pipe can be written through.
    struct HostEnd(Arc<Pipe>);

    impl Read for HostEnd {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            // SAFETY: `buffer` is a live mutable byte slice for this call.
            let result =
                unsafe { libc::read(self.0.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
            if result < 0 {
                return Err(io::Error::last_os_error());
            }

            Ok(result as usize)
        }
    }

    impl Write for HostEnd {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            // SAFETY: `buffer` is a live byte slice for this call.
            let result =
                unsafe { libc::write(self.0.as_raw_fd(), buffer.as_ptr().cast(), buffer.len()) };
            if result < 0 {
                return Err(io::Error::last_os_error());
            }

            Ok(result as usize)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Keeps the `Socket` bound honest: a `Pipe` is one.
    const fn _pipe_is_a_socket<T: Socket>() {}
    const _: () = _pipe_is_a_socket::<Pipe>();

    #[test]
    fn a_session_starts_with_a_stream_config_and_a_keyframe() {
        let mut world = World::open();
        world.broker_sends_snapshot(1);
        world.run_until_written();

        let records = world.host_reads_frame_records();
        assert_eq!(records[0].message_type, FrameRecord::StreamConfig as u16);
        assert_eq!(records[1].message_type, FrameRecord::Keyframe as u16);
    }

    #[test]
    fn an_output_that_changed_size_starts_a_new_stream_on_the_same_socket() {
        let mut world = World::open();
        world.broker_sends_snapshot(1);
        world.run_until_written();
        let opening = world.host_reads_frame_records();
        let last = opening.last().expect("the opening records").sequence;

        world.broker_sends_geometry(MAX_WIDTH, MAX_HEIGHT);
        world.broker_sends_snapshot_sized(2, MAX_WIDTH, MAX_HEIGHT);
        world.run_until_written();

        let records = world.host_reads_frame_stream();
        let (header, payload) = records.first().expect("a stream config");
        assert_eq!(header.message_type, FrameRecord::StreamConfig as u16);
        let config = StreamConfig::decode(payload.as_slice()).expect("a stream config");
        assert_eq!((config.width, config.height), (MAX_WIDTH, MAX_HEIGHT));
        assert_eq!(
            records[1].0.message_type,
            FrameRecord::Keyframe as u16,
            "a decoder built on a new geometry has nothing to apply a delta to"
        );
        assert!(
            records[0].0.sequence > last,
            "the socket did not change, so a peer that saw a record must not be sent it again"
        );
    }

    #[test]
    fn a_frame_of_the_old_shape_is_dropped_rather_than_ending_the_session() {
        // Capture and the mode change are not in step: the vblank either side
        // of a commit carries whichever buffer the compositor had. The
        // encoder is built on a geometry and cannot take another shape.
        let mut world = World::open();
        world.broker_sends_snapshot(1);
        world.run_until_written();
        let _ = world.host_reads_frame_records();

        world.broker_sends_geometry(MAX_WIDTH, MAX_HEIGHT);
        world.broker_sends_snapshot(2);
        world.run_until_written();

        let records = world.host_reads_frame_stream();
        assert_eq!(records[0].0.message_type, FrameRecord::StreamConfig as u16);
        assert!(
            records
                .iter()
                .all(|(header, _)| header.message_type != FrameRecord::TileDelta as u16),
            "a frame of the old size is not one this stream can carry"
        );
    }

    #[test]
    fn a_frame_the_encoder_refuses_is_dropped_and_capture_continues() {
        let mut world = World::open();
        let asked_before = world.broker_received_next_frames();

        world.broker_sends_snapshot_with_oversized_cursor(1);
        world.step();

        assert_eq!(
            world.broker_received_next_frames(),
            asked_before + 1,
            "a refused frame still earns the broker's next-frame request"
        );

        world.broker_sends_snapshot(2);
        world.run_until_written();

        assert!(
            !world.host_reads_frame_records().is_empty(),
            "the frame after a rejected one still reaches the viewer"
        );
    }

    #[test]
    fn losing_control_stops_capture_and_closes_both_sockets() {
        let mut world = World::open();
        world.broker_sends_session_closed("control was lost");
        world.run_until_idle();

        assert!(
            world.frame_socket_is_closed(),
            "there is no session without control"
        );
        assert!(world.input_socket_is_closed());
        assert!(
            !world.broker_saw_next_frame_after_close(),
            "a process that keeps asking for frames is one that never stopped capturing"
        );
    }

    #[test]
    fn a_reconnected_frame_channel_starts_again_with_a_keyframe() {
        let mut world = World::open();
        world.broker_sends_snapshot(1);
        world.run_until_written();
        world.host_drops_and_reopens_the_frame_socket();
        world.broker_sends_snapshot(2);
        world.run_until_written();

        let records = world.host_reads_frame_records();
        assert_eq!(records[0].message_type, FrameRecord::StreamConfig as u16);
        assert_eq!(
            records[1].message_type,
            FrameRecord::Keyframe as u16,
            "a delta has nothing to apply to on a decoder that has just been built"
        );
        assert_eq!(records[0].generation, 1);
    }

    #[test]
    fn a_slow_socket_costs_captured_frames_and_never_a_backlog() {
        let mut world = World::open();
        world.host_stops_reading();
        for sequence in 1..=8 {
            world.broker_sends_snapshot(sequence);
            world.step();
        }
        world.host_resumes_reading();
        world.run_until_written();

        assert!(
            world.host_reads_frame_records().len() <= 4,
            "the queue is before the encoder, so what a slow socket drops is captured frames"
        );
    }

    #[test]
    fn a_keyframe_request_from_the_broker_produces_one() {
        let mut world = World::open();
        world.broker_sends_snapshot(1);
        world.run_until_written();
        let _ = world.host_reads_frame_records();
        world.broker_sends_keyframe_request();
        world.broker_sends_snapshot(2);
        world.run_until_written();

        // The keyframe answers the request at once rather than waiting for the
        // guest to repaint, which is the codec's own promise: a viewer that
        // lost synchronisation must not wait for a frame to change. So the
        // keyframe is among the records, ahead of the delta that frame two
        // produced -- not the last of them.
        assert!(
            world
                .host_reads_frame_records()
                .iter()
                .any(|header| header.message_type == FrameRecord::Keyframe as u16),
            "a keyframe request produces a keyframe"
        );
    }

    #[test]
    fn a_record_from_a_stale_generation_is_refused_on_the_input_socket() {
        let mut world = World::open();
        world.host_sends_input_with_generation(0);
        world.run_until_idle();

        assert!(
            world.input_socket_is_closed(),
            "a record from a connection that was replaced must not reach an input device"
        );
    }

    #[test]
    fn a_session_with_no_devices_still_consumes_its_records() {
        // A guest whose broker found no uinput reads the channel and drops
        // what it reads, rather than letting unread records stall the socket.
        let mut world = World::open_without_devices();
        world.host_sends_key_event(30, true);
        world.run_until_idle();

        assert!(!world.input_socket_is_closed());
        assert_eq!(world.input_records_consumed(), 1);
    }

    #[test]
    fn a_key_event_reaches_the_keyboard_device() {
        let mut world = World::open();
        world.host_sends_key_event(30, true);
        world.run_until_idle();

        assert_eq!(
            world.keyboard_events(),
            vec![(1, 30, 1), (0, 0, 0)],
            "a key press and the report that closes it"
        );
    }

    #[test]
    fn a_pointer_motion_is_scaled_by_the_session_geometry() {
        let mut world = World::open();
        world.host_sends_motion(WIDTH - 1, HEIGHT - 1);
        world.run_until_idle();

        // The last pixel of the screen, at its centre: libinput reads an
        // absolute axis as `value * size / 32768`, and the pixel that comes
        // back is the one the host named.
        let events = world.pointer_events();
        assert_eq!(events[0].2 * WIDTH as i32 / 32768, WIDTH as i32 - 1);
        assert_eq!(events[1].2 * HEIGHT as i32 / 32768, HEIGHT as i32 - 1);
    }

    #[test]
    fn a_record_from_a_stale_generation_never_reaches_a_device() {
        let mut world = World::open();
        world.host_sends_input_with_generation(0);
        world.run_until_idle();

        assert!(world.input_socket_is_closed());
        assert!(
            world
                .keyboard_events()
                .iter()
                .all(|event| event.0 != 1 || event.2 != 1),
            "a record from a connection that was replaced must not press a key"
        );
    }

    #[test]
    fn a_lost_input_channel_releases_what_the_guest_holds() {
        let mut world = World::open();
        world.host_sends_key_event(30, true);
        world.run_until_idle();
        let _ = world.keyboard_events();

        world.host_closes_input_socket();
        world.run_until_idle();

        assert_eq!(
            world.keyboard_events(),
            vec![(1, 30, 0), (0, 0, 0)],
            "a channel that went must leave no key down"
        );
    }

    #[test]
    fn a_release_all_record_releases_both_devices() {
        let mut world = World::open();
        world.host_sends_key_event(30, true);
        world.host_sends_button(0x110, true);
        world.run_until_idle();
        let _ = (world.keyboard_events(), world.pointer_events());

        world.host_sends_release_all();
        world.run_until_idle();

        assert_eq!(world.keyboard_events(), vec![(1, 30, 0), (0, 0, 0)]);
        assert_eq!(world.pointer_events(), vec![(1, 0x110, 0), (0, 0, 0)]);
    }
}
