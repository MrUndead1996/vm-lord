//! The window's sound: one thread, one socket, one endpoint.
//!
//! A thread of its own rather than work on the session thread, for the reason
//! the clipboard has one: what it does must never hold a frame. Sound arrives
//! every ten milliseconds for as long as a desktop is making any, and a
//! renderer that has to wait for an endpoint must not make the picture wait
//! with it.
//!
//! What is here is the two edges -- a bound audio channel on one side and
//! [`crate::windows::audio::Renderer`] on the other -- and the order things
//! happen in. Everything the stream decides is
//! [`vmlord_display_protocol::audio`], which the guest runs too.
//!
//! No line it writes carries a sample.

use std::{
    sync::mpsc::{Receiver, Sender, TryRecvError, channel},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use prost::Message as _;
use vmlord_display_protocol::{
    audio::{Format, SampleFormat},
    record::{self, Channel, Header, Limits},
    session::{HandedOver, Negotiated, Session},
    v1::{
        AudioFormat, AudioRecord, Capability, Error as ErrorRecord, Mode, ProtocolVersion,
        SampleFormat as WireSampleFormat,
    },
};

use crate::{
    launch::Handover,
    live::{BIND_BACKOFF, channel_key, read_awaited},
    windows::{
        audio::{Com, Renderer},
        hvsocket::{CONNECT_TIMEOUT, HvSocket},
    },
};

/// How long the loop rests while it has no socket to read.
const UNBOUND_REST: Duration = Duration::from_millis(50);

/// What the window tells this thread to do with the sound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mute {
    /// Stop playing, but keep taking what the guest sends.
    On,
    /// Play again, from the next period.
    Off,
}

/// What one audio thread is started with.
pub struct Parameters {
    /// The compute system this session's sockets are opened on.
    pub runtime_id: [u8; 16],
    /// The vsock port the guest's audio daemon listens on.
    pub port: u32,
    /// The session VMLord handed over, which carries every channel key.
    pub handover: Handover,
    /// Whether the window starts muted, as it was left last time.
    pub muted: bool,
}

/// Starts the audio thread, and returns what mutes it.
///
/// The thread ends when the returned sender is dropped, which is when the
/// window is closing.
#[must_use]
pub fn spawn(parameters: Parameters) -> (JoinHandle<()>, Sender<Mute>) {
    let (sender, receiver) = channel();
    let handle = thread::spawn(move || {
        if let Err(reason) = serve(&parameters, &receiver) {
            // One line, and the session carries on without sound: a viewer
            // that cannot play still shows a desktop and still types.
            tracing::warn!("audio is not available: {reason}");
            // The window still has to be able to drop its sender, so the
            // thread drains rather than exits with a live channel.
            while receiver.recv().is_ok() {}
        }
    });

    (handle, sender)
}

/// The thread's body.
fn serve(parameters: &Parameters, mute: &Receiver<Mute>) -> Result<(), String> {
    // Before anything WASAPI: this thread is the only one that touches COM
    // here, and a renderer built without it reports every failure as "this
    // host has no audio output".
    let _com = Com::initialize();
    let mut session = session_of(&parameters.handover)?;
    let limits = Limits::new(0, 0);

    let mut socket: Option<HvSocket> = None;
    let mut renderer: Option<Renderer> = None;
    let mut next_bind = Instant::now();
    // Whether a hello has ever gone down this channel. The guest records the
    // generation of every hello it reads, so an attempt that failed has still
    // spent one and the next has to climb past it.
    let mut greeted = false;
    let mut muted = parameters.muted;
    let mut payload = Vec::new();

    loop {
        match mute.try_recv() {
            Ok(command) => {
                muted = command == Mute::On;
                if let Some(renderer) = renderer.as_mut() {
                    renderer.set_muted(muted);
                }
                tracing::info!(
                    "the guest's audio is {}",
                    if muted { "muted" } else { "playing" }
                );
            }
            Err(TryRecvError::Disconnected) => return Ok(()),
            Err(TryRecvError::Empty) => {}
        }

        let now = Instant::now();
        if socket.is_none() && now >= next_bind {
            match bind(&mut session, parameters, &mut greeted) {
                Ok(bound) => {
                    tracing::info!(
                        "the audio channel bound at generation {}",
                        session.generation(Channel::Audio)
                    );
                    socket = Some(bound);
                    // The renderer is built from the guest's first format
                    // record rather than guessed at here: what it plays is
                    // whatever the loopback ended up pinned to.
                    renderer = None;
                }
                Err(reason) => {
                    tracing::debug!("the audio channel could not bind: {reason}");
                    next_bind = now + BIND_BACKOFF;
                }
            }
        }

        let Some(open) = socket.as_mut() else {
            // A bound socket paces this loop by itself, because its reads poll
            // rather than return at once. An unbound one does not, and a loop
            // that spun while waiting for the backoff would cost a core to
            // wait on a guest that has no daemon.
            thread::sleep(UNBOUND_REST);
            continue;
        };

        match record::read(open, &limits, &mut payload) {
            Ok(header) => {
                if header.generation == session.generation(Channel::Audio) {
                    handle(&header, &payload, &mut renderer, muted);
                }
            }
            Err(record::RecordError::Idle) => {}
            Err(error) => {
                tracing::info!("the audio channel ended: {error}");
                socket = None;
                renderer = None;
                next_bind = Instant::now() + BIND_BACKOFF;
            }
        }
    }
}

/// What one record off the channel means.
fn handle(header: &Header, payload: &[u8], renderer: &mut Option<Renderer>, muted: bool) {
    match AudioRecord::try_from(i32::from(header.message_type)) {
        Ok(AudioRecord::Format) => {
            let Ok(message) = AudioFormat::decode(payload) else {
                return;
            };
            let Some(format) = format_of(&message) else {
                tracing::warn!("the guest pinned a format this build cannot play");

                return;
            };

            match renderer.as_mut() {
                Some(renderer) => renderer.set_format(format),
                None => match Renderer::new(format) {
                    Ok(mut built) => {
                        built.set_muted(muted);
                        *renderer = Some(built);
                    }
                    Err(error) => tracing::warn!("{error}"),
                },
            }
        }
        // The payload is PCM, and its position in the stream is the header's
        // `base`: a jump in it is the gap the guest suppressed or dropped.
        Ok(AudioRecord::Data) => {
            if let Some(renderer) = renderer.as_mut() {
                renderer.play(header.base, payload);
            }
        }
        Ok(AudioRecord::Error) => {
            if let Ok(error) = ErrorRecord::decode(payload) {
                tracing::warn!("the guest has no audio: {}", error.detail);
            }
        }
        _ => {}
    }
}

/// The format a record names, if this build can play it.
fn format_of(message: &AudioFormat) -> Option<Format> {
    let sample_format = match WireSampleFormat::try_from(message.sample_format).ok()? {
        WireSampleFormat::S16Le => SampleFormat::S16Le,
        WireSampleFormat::S32Le => SampleFormat::S32Le,
        WireSampleFormat::FloatLe => SampleFormat::FloatLe,
        WireSampleFormat::Unspecified => return None,
    };
    if message.sample_rate == 0 || message.channels == 0 || message.frames_per_period == 0 {
        return None;
    }

    Some(Format {
        sample_rate: message.sample_rate,
        channels: message.channels,
        sample_format,
        frames_per_period: message.frames_per_period,
    })
}

/// The session this thread runs its own channel on.
fn session_of(handover: &Handover) -> Result<Session, String> {
    let session_id = handover
        .session_id
        .as_slice()
        .try_into()
        .map_err(|_| "the hand-over's session id is not sixteen bytes".to_owned())?;
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
        mode: Mode::try_from(handover.mode).unwrap_or(Mode::Desktop),
        width: handover.width,
        height: handover.height,
        tile_size: handover.tile_size,
    };
    if !negotiated.capabilities.contains(&Capability::Audio) {
        return Err("this session has no audio".to_owned());
    }

    Ok(Session::established_host(HandedOver {
        session_id,
        negotiated,
        frame_key: channel_key(&handover.frame_key, "frame")?,
        input_key: channel_key(&handover.input_key, "input")?,
        clipboard_key: channel_key(&handover.clipboard_key, "clipboard")?,
        audio_key: channel_key(&handover.audio_key, "audio")?,
        control_sequence: handover.control_sequence,
    }))
}

/// Opens the audio socket and runs the three-record bind on it.
fn bind(
    session: &mut Session,
    parameters: &Parameters,
    greeted: &mut bool,
) -> Result<HvSocket, String> {
    let mut socket = HvSocket::connect(&parameters.runtime_id, parameters.port, CONNECT_TIMEOUT)
        .map_err(|error| error.to_string())?;
    let limits = Limits::new(0, 0);

    let hello = if std::mem::replace(greeted, true) {
        session.reconnect_channel(Channel::Audio)
    } else {
        session.open_channel(Channel::Audio)
    }
    .map_err(|error| error.to_string())?;
    record::write(&mut socket, &hello, &limits).map_err(|error| error.to_string())?;

    let mut payload = Vec::new();
    let header = read_awaited(&mut socket, &limits, &mut payload)?;
    let outcome = session
        .handle(&header, &payload)
        .map_err(|error| error.to_string())?;
    if let Some(reply) = outcome.reply {
        record::write(&mut socket, &reply, &limits).map_err(|error| error.to_string())?;
    }
    if outcome.event != vmlord_display_protocol::session::Event::ChannelBound(Channel::Audio) {
        return Err("the audio channel did not bind".to_owned());
    }

    Ok(socket)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_format_record_this_build_can_play_becomes_a_format() {
        let format = format_of(&AudioFormat {
            sample_rate: 48_000,
            channels: 2,
            sample_format: WireSampleFormat::S16Le as i32,
            frames_per_period: 480,
        })
        .expect("a format this build plays");

        assert_eq!(format.sample_rate, 48_000);
        assert_eq!(format.channels, 2);
        assert_eq!(format.sample_format, SampleFormat::S16Le);
        assert_eq!(format.frames_per_period, 480);
        assert_eq!(format.period_bytes(), 1920);
    }

    #[test]
    fn a_format_record_with_nothing_in_it_is_refused_rather_than_played() {
        // A guest that sent zeros would otherwise divide by them.
        for message in [
            AudioFormat {
                sample_rate: 0,
                channels: 2,
                sample_format: WireSampleFormat::S16Le as i32,
                frames_per_period: 480,
            },
            AudioFormat {
                sample_rate: 48_000,
                channels: 0,
                sample_format: WireSampleFormat::S16Le as i32,
                frames_per_period: 480,
            },
            AudioFormat {
                sample_rate: 48_000,
                channels: 2,
                sample_format: WireSampleFormat::S16Le as i32,
                frames_per_period: 0,
            },
            AudioFormat {
                sample_rate: 48_000,
                channels: 2,
                sample_format: WireSampleFormat::Unspecified as i32,
                frames_per_period: 480,
            },
        ] {
            assert!(format_of(&message).is_none());
        }
    }

    fn handover(capabilities: Vec<i32>) -> Handover {
        Handover {
            session_id: vec![1; 16],
            frame_key: vec![2; 32],
            input_key: vec![3; 32],
            clipboard_key: vec![4; 32],
            audio_key: vec![5; 32],
            version_major: 1,
            version_minor: 0,
            capabilities,
            mode: i32::from(Mode::Desktop),
            width: 1920,
            height: 1080,
            tile_size: 32,
            control_sequence: 4,
        }
    }

    #[test]
    fn a_session_that_negotiated_audio_runs_one() {
        assert!(session_of(&handover(vec![i32::from(Capability::Audio)])).is_ok());
    }

    #[test]
    fn a_session_without_the_capability_gets_no_thread() {
        // The guest either ships the daemon or it does not, and a capability
        // cannot be renegotiated: there is nothing here to retry.
        assert!(session_of(&handover(vec![i32::from(Capability::Clipboard)])).is_err());
    }
}
