//! The guest's audio daemon.
//!
//! It is a system service, not a user one: what it reads is the ALSA loopback,
//! which belongs to the machine rather than to whoever is at the screen, and a
//! stream that starts at boot needs no seat. Membership of the `audio` group is
//! the whole of its access to `/dev/snd`.
//!
//! What it owns is one vsock socket and one capture device; what it decides is
//! nothing -- the rules about which periods travel, how many may wait and where
//! a period sits in the stream are [`vmlord_display_protocol::audio`], which
//! the host runs too.
//!
//! It holds no secret. The broker does the control handshake and sends one
//! channel key over `/run/vmlord/display-audio.sock`, which is worth one
//! session's sound and nothing else -- not a picture, not a keyboard, not a
//! selection.
//!
//! Nothing it writes to the journal carries a sample. A format, a frame count,
//! a stream position and an outcome are what an audio problem is diagnosed
//! from, and every line about a period goes through [`describe_period`] so that
//! there is one place where that is true.

use std::{
    env,
    io::Write,
    path::PathBuf,
    process::ExitCode,
    time::Duration,
};

use prost::Message as _;
use vmlord_display_protocol::{
    audio::{Format, SampleFormat, Sent, Stream},
    keys::ChannelKey,
    record::{self, Channel, Limits, Record},
    v1::{self, AudioRecord},
};

use crate::{
    alsa::{self, Capture, CaptureError},
    channel::{self, BindError},
    ipc::Message,
    unix::Connection,
    vsock::{self, AUDIO_PORT},
};

/// Where the broker offers the audio channel.
const BROKER_SOCKET: &str = "/run/vmlord/display-audio.sock";

/// How long to wait before looking for the broker again.
const RETRY: Duration = Duration::from_secs(2);

/// How long a write waits before the period it carries is given up on.
///
/// Two periods. A host that has not taken a period in twenty milliseconds is
/// not going to be helped by this guest waiting longer, and waiting is the one
/// thing capture must not do.
const WRITE_PATIENCE: Duration = Duration::from_millis(20);

/// What the daemon was started with.
pub struct Options {
    /// The socket the broker offers the audio channel on.
    pub broker_socket: PathBuf,
    /// The capture device to read.
    pub device: String,
}

impl Options {
    /// The defaults, with the environment allowed to override each one.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            broker_socket: env::var("VMLORD_DISPLAY_AUDIO_SOCKET")
                .unwrap_or_else(|_| BROKER_SOCKET.to_owned())
                .into(),
            device: env::var("VMLORD_DISPLAY_AUDIO_DEVICE")
                .unwrap_or_else(|_| alsa::DEFAULT_DEVICE.to_owned()),
        }
    }
}

/// Runs the daemon until it cannot bind its socket.
///
/// Everything short of that is waited through rather than exited over: no
/// broker yet, no session open, no loopback module. A daemon that exited on any
/// of those would spend its restart budget on the ordinary state of a guest
/// that is still booting.
#[must_use]
pub fn run(options: Options) -> ExitCode {
    let listener = match wait_for_port() {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("vmlord-display-audio: this guest has no vsock: {error}");

            return ExitCode::FAILURE;
        }
    };

    let mut last_bound: Option<(Vec<u8>, u32)> = None;
    loop {
        let Some(broker) = wait_for_broker(&options.broker_socket) else {
            continue;
        };

        match serve_session(&broker, &listener, &options.device, &mut last_bound) {
            Ok(()) => {}
            Err(reason) => eprintln!("vmlord-display-audio: {reason}"),
        }
    }
}

/// Takes the audio port, waiting for whoever holds it to let go.
///
/// # Errors
///
/// The bind error, for every reason except the port already being held -- a
/// restart that overlaps its predecessor is ordinary and worth waiting out.
fn wait_for_port() -> std::io::Result<vsock::Listener> {
    loop {
        match vsock::Listener::bind(AUDIO_PORT) {
            Ok(listener) => return Ok(listener),
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                std::thread::sleep(RETRY);
            }
            Err(error) => return Err(error),
        }
    }
}

/// Connects to the broker and asks it for whatever session is open.
fn wait_for_broker(path: &std::path::Path) -> Option<Connection> {
    let connection = match Connection::connect(path) {
        Ok(connection) => connection,
        Err(_) => {
            std::thread::sleep(RETRY);

            return None;
        }
    };
    if connection.send(&Message::Attach, &[]).is_err() {
        std::thread::sleep(RETRY);

        return None;
    }

    Some(connection)
}

/// Serves one session's audio, from the broker's key to a lost socket.
///
/// The channel is bound **before** the device is opened, deliberately. A guest
/// whose `snd-aloop` is missing then answers with an `Error` record naming the
/// reason, which reaches the host's diagnostics, instead of never appearing at
/// all and leaving a viewer to guess.
fn serve_session(
    broker: &Connection,
    listener: &vsock::Listener,
    device: &str,
    last_bound: &mut Option<(Vec<u8>, u32)>,
) -> Result<(), String> {
    let (session_id, key) = loop {
        let (message, _) = broker
            .receive()
            .map_err(|error| format!("the broker went away: {error}"))?;

        match message {
            Message::AudioOpened {
                session_id,
                audio_key,
            } => {
                let key: [u8; 32] = audio_key
                    .as_slice()
                    .try_into()
                    .map_err(|_| "the broker sent a key of the wrong width".to_owned())?;

                break (session_id, ChannelKey::from_bytes(key));
            }
            // Everything else on this socket is about a session this daemon
            // has no part in.
            _ => continue,
        }
    };

    let mut stream = listener
        .accept()
        .map_err(|error| format!("the audio socket could not be accepted: {error}"))?;
    let generation = channel::bind(
        &mut stream,
        Channel::Audio,
        &key,
        &session_id,
        guard(last_bound.as_ref(), &session_id),
    )
    .map_err(|error: BindError| format!("the audio channel did not bind: {error}"))?;
    *last_bound = Some((session_id.clone(), generation));
    eprintln!("vmlord-display-audio: the audio channel bound at generation {generation}");

    stream
        .set_write_timeout(WRITE_PATIENCE)
        .map_err(|error| format!("the audio socket refused a timeout: {error}"))?;

    let mut sequence = 0u32;
    let capture = match Capture::open(device, alsa::WANTED) {
        Ok(capture) => capture,
        Err(error) => {
            let detail = error.to_string();
            report(&mut stream, &mut sequence, generation, &detail);

            return Err(detail);
        }
    };

    pump(&mut stream, capture, sequence, generation)
}

/// The generation a hello has to climb past, for this session.
///
/// `None` for a session this daemon has not bound a channel of yet. The host
/// counts generations inside one session and starts the next from zero, so
/// carrying a bare number across sessions would refuse the first channel of
/// every session but the first.
fn guard(last_bound: Option<&(Vec<u8>, u32)>, session_id: &[u8]) -> Option<u32> {
    last_bound
        .filter(|(bound, _)| bound == session_id)
        .map(|(_, generation)| *generation)
}

/// The loop: a period off the loopback, and a record if it is worth sending.
fn pump<S: Write>(
    stream: &mut S,
    mut capture: Capture,
    mut sequence: u32,
    generation: u32,
) -> Result<(), String> {
    let limits = Limits::new(0, 0);
    let format = capture.format();
    eprintln!(
        "vmlord-display-audio: capturing {} Hz, {} channels, {:?}, {} frames a period",
        format.sample_rate, format.channels, format.sample_format, format.frames_per_period
    );

    write_record(
        stream,
        &limits,
        &Record::new(
            Channel::Audio,
            AudioRecord::Format as u16,
            take(&mut sequence),
            0,
            generation,
            format_message(format).encode_to_vec(),
        ),
    )?;

    let mut stream_state = Stream::new(format);
    let mut period = vec![0u8; format.period_bytes()];
    loop {
        let frames = capture
            .read(&mut period)
            .map_err(|error: CaptureError| error.to_string())?;

        // An xrun reads nothing and re-prepares the device. The frames it lost
        // are not counted into the stream position, so the host hears a gap
        // rather than two moments spliced together.
        if frames == 0 {
            continue;
        }

        let captured = &period[..frames as usize * format.bytes_per_frame()];
        let Some(sent) = stream_state.captured(captured) else {
            continue;
        };

        stream_state.queue(sent);
        while let Some(sent) = stream_state.take() {
            let record = data_record(&sent, take(&mut sequence), generation);

            // A period the host will not take in time is dropped rather than
            // waited on: the position the next record carries is what tells it
            // how much went missing.
            if write_record(stream, &limits, &record).is_err() {
                eprintln!("vmlord-display-audio: dropped {}", describe_period(&sent));
            }
        }
    }
}

/// Writes one record, and says whether the host took it.
fn write_record<S: Write>(stream: &mut S, limits: &Limits, record: &Record) -> Result<(), String> {
    record::write(stream, record, limits).map_err(|error| error.to_string())
}

/// Tells the host why there is no stream.
fn report<S: Write>(stream: &mut S, sequence: &mut u32, generation: u32, detail: &str) {
    let limits = Limits::new(0, 0);
    let record = Record::new(
        Channel::Audio,
        AudioRecord::Error as u16,
        take(sequence),
        0,
        generation,
        v1::Error {
            code: v1::ErrorCode::CaptureFailed as i32,
            detail: detail.to_owned(),
        }
        .encode_to_vec(),
    );

    let _ = record::write(stream, &record, &limits);
    eprintln!("vmlord-display-audio: {detail}");
}

/// Takes the next record number, leaving the one after it behind.
fn take(sequence: &mut u32) -> u32 {
    let taken = *sequence;
    *sequence = sequence.wrapping_add(1);
    taken
}

/// One period as a record, with its stream position in the header.
fn data_record(sent: &Sent, sequence: u32, generation: u32) -> Record {
    Record::new(
        Channel::Audio,
        AudioRecord::Data as u16,
        sequence,
        sent.position,
        generation,
        sent.bytes.clone(),
    )
}

/// What the guest pinned, as the host is told it.
fn format_message(format: Format) -> v1::AudioFormat {
    v1::AudioFormat {
        sample_rate: format.sample_rate,
        channels: format.channels,
        sample_format: match format.sample_format {
            SampleFormat::S16Le => v1::SampleFormat::S16Le,
            SampleFormat::S32Le => v1::SampleFormat::S32Le,
            SampleFormat::FloatLe => v1::SampleFormat::FloatLe,
        } as i32,
        frames_per_period: format.frames_per_period,
    }
}

/// Everything about a period that may be written down.
///
/// The one function every log line about a period goes through, so that "no
/// sample reaches a log" is a property of one place rather than a rule every
/// call site has to remember. It is tested.
#[must_use]
pub fn describe_period(sent: &Sent) -> String {
    format!(
        "a period of {} bytes at stream position {}",
        sent.bytes.len(),
        sent.position
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_data_record_carries_the_period_and_its_position_in_the_header() {
        let record = data_record(
            &Sent {
                position: 4800,
                bytes: vec![7u8; 1920],
            },
            3,
            2,
        );

        assert_eq!(record.header.channel, Channel::Audio);
        assert_eq!(record.header.message_type, AudioRecord::Data as u16);
        assert_eq!(record.header.base, 4800);
        assert_eq!(record.header.sequence, 3);
        assert_eq!(record.header.generation, 2);
        assert_eq!(record.payload.len(), 1920);
    }

    #[test]
    fn the_format_record_says_what_was_pinned() {
        let message = format_message(Format {
            sample_rate: 48_000,
            channels: 2,
            sample_format: SampleFormat::S32Le,
            frames_per_period: 480,
        });

        assert_eq!(message.sample_rate, 48_000);
        assert_eq!(message.channels, 2);
        assert_eq!(message.sample_format, v1::SampleFormat::S32Le as i32);
        assert_eq!(message.frames_per_period, 480);
    }

    #[test]
    fn record_numbers_advance_and_wrap_rather_than_panic() {
        let mut sequence = u32::MAX;

        assert_eq!(take(&mut sequence), u32::MAX);
        assert_eq!(take(&mut sequence), 0);
        assert_eq!(sequence, 1);
    }

    #[test]
    fn no_log_line_carries_a_sample() {
        let bytes: Vec<u8> = (0..1920u32).map(|n| (n % 251) as u8 + 1).collect();
        let sent = Sent {
            position: 4800,
            bytes: bytes.clone(),
        };

        let line = describe_period(&sent);

        assert!(line.contains("4800"), "the position is worth writing down");
        assert!(line.contains("1920"), "so is the byte count");
        for window in bytes.windows(8) {
            assert!(
                !line.as_bytes().windows(8).any(|seen| seen == window),
                "no run of the payload reached a log line"
            );
        }
    }

    #[test]
    fn a_generation_guard_is_scoped_to_its_own_session() {
        let bound = (vec![1u8; 16], 3);

        assert_eq!(guard(Some(&bound), &[1u8; 16]), Some(3));
        // A different session counts generations from zero of its own, so the
        // guard must not carry across.
        assert_eq!(guard(Some(&bound), &[2u8; 16]), None);
    }
}
