//! How a record is delimited on any of the four channels.
//!
//! A record is a fixed 24-byte little-endian header followed by `length`
//! payload bytes. Little-endian because both ends of these sockets are x86-64;
//! fixed rather than length-prefixed Protobuf because the frame channel's
//! payloads are megabytes of codec output that must reach the socket without
//! being copied through an encoder.
//!
//! The first byte is the header's own length. It is what lets v1.2 append a
//! field that v1.0 skips without losing the stream, and it is why there is no
//! magic number: the version is settled in the handshake, and four bytes per
//! frame to make a packet dump readable is not a trade worth making.

use std::{
    error::Error,
    fmt, io,
    time::{Duration, Instant},
};

/// The width of the header this build writes and understands.
pub const HEADER_LEN: usize = 24;

/// Which of a session's four sockets a record belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    /// Handshake, session control, liveness and errors.
    Control = 1,
    /// Frames and cursors, from the guest only.
    Frame = 2,
    /// Keyboard and pointer, from the host only.
    Input = 3,
    /// Selections, in both directions.
    Clipboard = 4,
}

impl Channel {
    /// The byte that names this channel in a header.
    #[must_use]
    pub fn as_wire(self) -> u8 {
        self as u8
    }

    /// Reads a channel out of a header.
    ///
    /// # Errors
    ///
    /// [`RecordError::UnknownChannel`] for any other value. Unlike a
    /// capability, an unknown channel cannot be ignored: there is no way to
    /// know what the payload behind it means.
    pub fn from_wire(value: u8) -> Result<Self, RecordError> {
        match value {
            1 => Ok(Self::Control),
            2 => Ok(Self::Frame),
            3 => Ok(Self::Input),
            4 => Ok(Self::Clipboard),
            value => Err(RecordError::UnknownChannel { value }),
        }
    }
}

impl fmt::Display for Channel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Control => "control",
            Self::Frame => "frame",
            Self::Input => "input",
            Self::Clipboard => "clipboard",
        })
    }
}

/// What precedes every payload on every channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// Which socket this record belongs to.
    pub channel: Channel,
    /// The record's type within its channel: one of the `*Record` enums in the
    /// schema.
    pub message_type: u16,
    /// The payload's length in bytes.
    pub length: u32,
    /// The record's position in its channel's stream, from zero.
    pub sequence: u32,
    /// For a tile delta, the `sequence` of the frame it builds on. Zero
    /// everywhere else, including on a keyframe, which builds on nothing.
    pub base: u32,
    /// CRC32C of the payload. A corruption check, not a signature.
    pub checksum: u32,
    /// Which generation of the session's bound channels this belongs to. Stale
    /// generations are rejected here, before a decoder, an input device or a
    /// clipboard sees them.
    pub generation: u32,
}

impl Header {
    /// The bytes that precede this record's payload.
    #[must_use]
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut bytes = [0u8; HEADER_LEN];

        bytes[0] = HEADER_LEN as u8;
        bytes[1] = self.channel.as_wire();
        bytes[2..4].copy_from_slice(&self.message_type.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.length.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.sequence.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.base.to_le_bytes());
        bytes[16..20].copy_from_slice(&self.checksum.to_le_bytes());
        bytes[20..24].copy_from_slice(&self.generation.to_le_bytes());

        bytes
    }

    /// Reads a header, and says how much of a longer one is left to skip.
    ///
    /// The returned count is `header_len - HEADER_LEN`: bytes a newer minor
    /// appended, which this build does not understand and the caller must
    /// consume before the payload begins.
    ///
    /// # Errors
    ///
    /// [`RecordError::MalformedHeader`] if `header_len` is below
    /// [`HEADER_LEN`] -- a header this build cannot read at all, rather than
    /// one it can read part of -- and [`RecordError::UnknownChannel`] for a
    /// channel byte that names no socket.
    pub fn decode(bytes: &[u8; HEADER_LEN]) -> Result<(Self, usize), RecordError> {
        let header_len = bytes[0];
        if usize::from(header_len) < HEADER_LEN {
            return Err(RecordError::MalformedHeader { header_len });
        }

        let header = Self {
            channel: Channel::from_wire(bytes[1])?,
            message_type: u16::from_le_bytes([bytes[2], bytes[3]]),
            length: u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            sequence: u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            base: u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]),
            checksum: u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]),
            generation: u32::from_le_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]),
        };

        Ok((header, usize::from(header_len) - HEADER_LEN))
    }
}

/// A record that could not be moved between memory and a stream.
#[derive(Debug)]
pub enum RecordError {
    /// A header shorter than this build reads.
    MalformedHeader {
        /// What the first byte announced.
        header_len: u8,
    },
    /// A channel byte that names no socket.
    UnknownChannel {
        /// What the second byte held.
        value: u8,
    },
    /// A payload larger than its channel allows.
    TooLarge {
        /// Which channel's limit was exceeded.
        channel: Channel,
        /// What the header announced.
        length: u32,
        /// What that channel allows in this session.
        cap: u32,
    },
    /// A payload whose CRC32C is not the one its header announced.
    ChecksumMismatch {
        /// What the header announced.
        expected: u32,
        /// What the payload actually hashes to.
        found: u32,
    },
    /// The peer closed the connection at a record boundary.
    Closed,
    /// The transport timed out before the peer started another record.
    Idle,
    /// The transport failed.
    Io(io::Error),
}

impl fmt::Display for RecordError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MalformedHeader { header_len } => write!(
                formatter,
                "a record header of {header_len} bytes is shorter than the {HEADER_LEN} this build reads"
            ),
            Self::UnknownChannel { value } => {
                write!(formatter, "{value} names no display protocol channel")
            }
            Self::TooLarge {
                channel,
                length,
                cap,
            } => write!(
                formatter,
                "a {length}-byte payload on the {channel} channel exceeds its {cap}-byte limit"
            ),
            Self::ChecksumMismatch { expected, found } => write!(
                formatter,
                "a record announced checksum {expected:#010x} and carries {found:#010x}"
            ),
            Self::Closed => formatter.write_str("the display connection was closed"),
            Self::Idle => formatter.write_str("the display connection is idle"),
            Self::Io(error) => write!(formatter, "the display connection failed: {error}"),
        }
    }
}

impl Error for RecordError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

/// The most a control record may carry.
///
/// Fixed, because nothing on this channel is a payload: hellos, tags, mode
/// changes and errors.
pub const CONTROL_MAX_PAYLOAD: u32 = 64 * 1024;

/// The most an input record may carry.
pub const INPUT_MAX_PAYLOAD: u32 = 4 * 1024;

/// The most a clipboard record may carry.
///
/// A selection is chunked to fit rather than sized by the session: this is what
/// one record may hold, and `clipboard::MAX_TEXT_TRANSFER` and
/// `clipboard::MAX_IMAGE_TRANSFER` are what a whole transfer may.
pub const CLIPBOARD_MAX_PAYLOAD: u32 = 64 * 1024;

/// The most a frame record may carry whatever the geometry says.
///
/// A backstop against a geometry that is itself absurd, since the cap below is
/// computed from numbers a peer sent.
pub const FRAME_PAYLOAD_CEILING: u32 = 64 * 1024 * 1024;

/// What a frame record may carry beyond its uncompressed pixels.
///
/// A keyframe should never exceed its raw size, but a codec header, a cursor
/// and a tile map are not pixels, and refusing a frame for its metadata would
/// be a limit that fires on correct behaviour.
pub const FRAME_PAYLOAD_SLACK: u32 = 64 * 1024;

/// What a session's records may weigh, given what it agreed to display.
///
/// The frame cap is derived rather than fixed: a record larger than an
/// uncompressed frame of the agreed geometry is not a frame by definition, so
/// "oversized" says something about this session instead of naming a number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    frame: u32,
}

impl Limits {
    /// The limits for a session displaying `width` by `height`.
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        let mut limits = Self { frame: 0 };
        limits.set_geometry(width, height);
        limits
    }

    /// Moves the frame cap to a new geometry, as a `StreamConfig` does.
    pub fn set_geometry(&mut self, width: u32, height: u32) {
        self.frame = width
            .saturating_mul(height)
            .saturating_mul(4)
            .saturating_add(FRAME_PAYLOAD_SLACK)
            .min(FRAME_PAYLOAD_CEILING);
    }

    /// The largest payload `channel` may carry in this session.
    #[must_use]
    pub fn for_channel(&self, channel: Channel) -> u32 {
        match channel {
            Channel::Control => CONTROL_MAX_PAYLOAD,
            Channel::Frame => self.frame,
            Channel::Input => INPUT_MAX_PAYLOAD,
            Channel::Clipboard => CLIPBOARD_MAX_PAYLOAD,
        }
    }
}

/// A header and the payload it describes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// What precedes the payload on the wire.
    pub header: Header,
    /// A Protobuf message, or codec bytes on the frame channel's four pixel
    /// types.
    pub payload: Vec<u8>,
}

impl Record {
    /// Builds a record, filling in the length and the checksum.
    ///
    /// Those two are the header's own arithmetic rather than a caller's, which
    /// is what keeps a record from ever announcing a length it does not carry.
    #[must_use]
    pub fn new(
        channel: Channel,
        message_type: u16,
        sequence: u32,
        base: u32,
        generation: u32,
        payload: Vec<u8>,
    ) -> Self {
        let header = Header {
            channel,
            message_type,
            length: u32::try_from(payload.len()).unwrap_or(u32::MAX),
            sequence,
            base,
            checksum: crc32c::crc32c(&payload),
            generation,
        };

        Self { header, payload }
    }
}

/// Writes one record and flushes it.
///
/// Flushing belongs here rather than to the caller: a buffered transport that
/// holds a keystroke or a keyframe request back is a session that appears to
/// have frozen.
///
/// # Errors
///
/// [`RecordError::TooLarge`] if the payload exceeds its channel's cap, in
/// which case nothing is written -- a payload that cannot be framed must not
/// become a truncated one -- or [`RecordError::Io`] if the transport fails.
pub fn write<W: io::Write>(
    writer: &mut W,
    record: &Record,
    limits: &Limits,
) -> Result<(), RecordError> {
    let cap = limits.for_channel(record.header.channel);
    if record.header.length > cap {
        return Err(RecordError::TooLarge {
            channel: record.header.channel,
            length: record.header.length,
            cap,
        });
    }

    writer
        .write_all(&record.header.encode())
        .map_err(RecordError::Io)?;
    writer.write_all(&record.payload).map_err(RecordError::Io)?;
    writer.flush().map_err(RecordError::Io)
}

/// Reads one record, leaving its payload in `payload`.
///
/// The cap is enforced from the header, before `payload` is grown, and the
/// checksum after the bytes are in: the first bounds what a hostile peer can
/// make this side allocate, the second catches a transport that corrupted what
/// it carried.
///
/// # Errors
///
/// [`RecordError::Closed`] if the peer hung up at a record boundary, which is
/// how a session ends and is not by itself a fault; [`RecordError::Idle`] if
/// the transport timed out before a record began, so the caller may safely
/// send one of its own. [`RecordError::MalformedHeader`],
/// [`RecordError::UnknownChannel`], [`RecordError::TooLarge`],
/// [`RecordError::ChecksumMismatch`] and [`RecordError::Io`] all leave the
/// stream unusable and must be answered by closing it.
pub fn read<R: io::Read>(
    reader: &mut R,
    limits: &Limits,
    payload: &mut Vec<u8>,
) -> Result<Header, RecordError> {
    let mut bytes = [0u8; HEADER_LEN];
    read_header_bytes(reader, &mut bytes)?;

    let (header, extra) = Header::decode(&bytes)?;

    let cap = limits.for_channel(header.channel);
    if header.length > cap {
        return Err(RecordError::TooLarge {
            channel: header.channel,
            length: header.length,
            cap,
        });
    }

    if extra > 0 {
        // At most 231 bytes, since `header_len` is one byte wide, so the
        // buffer is a stack array rather than an allocation a peer sizes.
        let mut skipped = [0u8; 256];
        fill(
            reader,
            &mut skipped[..extra],
            "the connection ended part-way through a record header",
        )?;
    }

    payload.clear();
    payload.resize(header.length as usize, 0);
    fill(
        reader,
        payload,
        "the connection ended part-way through a record payload",
    )?;

    let found = crc32c::crc32c(payload);
    if found != header.checksum {
        return Err(RecordError::ChecksumMismatch {
            expected: header.checksum,
            found,
        });
    }

    Ok(header)
}

/// How long the rest of a record that has already begun is waited for.
///
/// A record arrives across as many reads as the transport needs, and the poll
/// a socket reads with expires between them: a 2560x1440 keyframe does not fit
/// in one. So a timeout part-way through is "not all the bytes yet" and is
/// waited out rather than reported -- but not forever, because a peer that
/// stopped mid-record must not hold the thread that reads it. Generous against
/// what a keyframe costs, short against a session nobody is serving.
const RECORD_COMPLETION: Duration = Duration::from_secs(5);

/// Whether this is a transport saying "nothing more has arrived yet".
fn is_idle(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    )
}

/// Fills `bytes` from a record that has already begun.
///
/// Not `Read::read_exact`: that retries [`io::ErrorKind::Interrupted`] and
/// nothing else, so a poll expiring in the middle of a payload comes back as a
/// fault. What it means is that the rest is still on its way, which is what
/// this waits for -- up to [`RECORD_COMPLETION`], after which a peer that
/// stopped talking mid-record is a fault after all.
fn fill<R: io::Read>(
    reader: &mut R,
    bytes: &mut [u8],
    what: &'static str,
) -> Result<(), RecordError> {
    let deadline = Instant::now() + RECORD_COMPLETION;
    let mut filled = 0;
    while filled < bytes.len() {
        match reader.read(&mut bytes[filled..]) {
            Ok(0) => {
                return Err(RecordError::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    what,
                )));
            }
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if is_idle(&error) => {
                if Instant::now() >= deadline {
                    return Err(RecordError::Io(io::Error::new(
                        io::ErrorKind::TimedOut,
                        what,
                    )));
                }
            }
            Err(error) => return Err(RecordError::Io(error)),
        }
    }

    Ok(())
}

/// Fills `bytes`, telling a connection that ended between records from one
/// that ended inside a header.
///
/// `Read::read_exact` reports both as `UnexpectedEof`, and they mean opposite
/// things: the first is a peer that finished, the second is a cut stream.
///
/// The first read is the one that decides: a poll that expires before any byte
/// of a record has arrived is an idle connection, and the caller may send a
/// record of its own. Once a header has started, it is finished like any other
/// part of a record.
fn read_header_bytes<R: io::Read>(
    reader: &mut R,
    bytes: &mut [u8; HEADER_LEN],
) -> Result<(), RecordError> {
    let mut filled = 0;
    while filled == 0 {
        match reader.read(&mut bytes[..]) {
            Ok(0) => return Err(RecordError::Closed),
            Ok(read) => filled = read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if is_idle(&error) => return Err(RecordError::Idle),
            Err(error) => return Err(RecordError::Io(error)),
        }
    }

    fill(
        reader,
        &mut bytes[filled..],
        "the connection ended part-way through a record header",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> Header {
        Header {
            channel: Channel::Frame,
            message_type: 6,
            length: 4096,
            sequence: 17,
            base: 16,
            checksum: 0xDEAD_BEEF,
            generation: 2,
        }
    }

    #[test]
    fn a_header_survives_a_round_trip() {
        let (decoded, extra) =
            Header::decode(&header().encode()).expect("a header this crate encoded");

        assert_eq!(decoded, header());
        assert_eq!(extra, 0);
    }

    #[test]
    fn a_header_is_twenty_four_little_endian_bytes() {
        let bytes = header().encode();

        assert_eq!(bytes[0], 24);
        assert_eq!(bytes[1], 2);
        assert_eq!(u16::from_le_bytes([bytes[2], bytes[3]]), 6);
        assert_eq!(
            u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            4096
        );
    }

    #[test]
    fn a_longer_header_reports_the_bytes_a_reader_has_to_skip() {
        // What a future minor's writer produces: the same 24 bytes this build
        // knows, and four it does not.
        let mut bytes = header().encode();
        bytes[0] = 28;

        let (decoded, extra) = Header::decode(&bytes).expect("a header from a newer minor");

        assert_eq!(decoded, header());
        assert_eq!(extra, 4);
    }

    #[test]
    fn a_header_shorter_than_this_build_reads_is_refused() {
        let mut bytes = header().encode();
        bytes[0] = 23;

        assert!(matches!(
            Header::decode(&bytes),
            Err(RecordError::MalformedHeader { header_len: 23 })
        ));
    }

    #[test]
    fn a_channel_this_build_does_not_know_is_refused() {
        let mut bytes = header().encode();
        bytes[1] = 9;

        assert!(matches!(
            Header::decode(&bytes),
            Err(RecordError::UnknownChannel { value: 9 })
        ));
    }

    #[test]
    fn a_clipboard_channel_survives_a_round_trip() {
        let header = Header {
            channel: Channel::Clipboard,
            message_type: 7,
            length: 3,
            sequence: 9,
            base: 0,
            checksum: 0x1234_5678,
            generation: 2,
        };

        let (decoded, extra) = Header::decode(&header.encode()).expect("a header this build wrote");

        assert_eq!(decoded, header);
        assert_eq!(extra, 0);
        assert_eq!(Channel::Clipboard.as_wire(), 4);
        assert_eq!(Channel::Clipboard.to_string(), "clipboard");
    }

    #[test]
    fn a_clipboard_record_is_capped_at_sixty_four_kibibytes() {
        let limits = Limits::new(1920, 1080);

        assert_eq!(
            limits.for_channel(Channel::Clipboard),
            CLIPBOARD_MAX_PAYLOAD
        );
        assert_eq!(CLIPBOARD_MAX_PAYLOAD, 64 * 1024);
    }

    #[test]
    fn the_frame_cap_is_a_raw_frame_of_the_agreed_geometry_plus_slack() {
        let limits = Limits::new(2560, 1440);

        assert_eq!(limits.for_channel(Channel::Frame), 2560 * 1440 * 4 + 65536);
        assert_eq!(limits.for_channel(Channel::Control), 65536);
        assert_eq!(limits.for_channel(Channel::Input), 4096);
    }

    #[test]
    fn a_geometry_that_would_overflow_the_cap_is_held_at_the_ceiling() {
        let limits = Limits::new(u32::MAX, u32::MAX);

        assert_eq!(limits.for_channel(Channel::Frame), FRAME_PAYLOAD_CEILING);
    }

    #[test]
    fn a_resolution_change_moves_the_frame_cap() {
        let mut limits = Limits::new(1920, 1080);
        limits.set_geometry(1280, 720);

        assert_eq!(limits.for_channel(Channel::Frame), 1280 * 720 * 4 + 65536);
    }

    #[test]
    fn a_record_survives_a_round_trip_through_a_stream() {
        let limits = Limits::new(64, 64);
        let record = Record::new(Channel::Control, 8, 3, 0, 0, b"payload".to_vec());

        let mut wire = Vec::new();
        write(&mut wire, &record, &limits).expect("a record within the control cap");

        let mut payload = Vec::new();
        let header = read(&mut wire.as_slice(), &limits, &mut payload).expect("what was written");

        assert_eq!(header, record.header);
        assert_eq!(payload, b"payload");
    }

    /// A socket that hands over `chunk` bytes at a time and answers
    /// `WouldBlock` between them.
    ///
    /// What an HvSocket does under a keyframe: the poll it reads with expires
    /// while the rest of the record is still on its way, which the transport
    /// documents as "not all bytes yet" rather than a broken channel.
    struct Trickle {
        bytes: Vec<u8>,
        sent: usize,
        chunk: usize,
        blocked: bool,
    }

    impl io::Read for Trickle {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.sent == self.bytes.len() {
                return Ok(0);
            }
            // Every other call, so that no read of a partial record runs to
            // the end without meeting one.
            self.blocked = !self.blocked;
            if self.blocked {
                return Err(io::Error::from(io::ErrorKind::WouldBlock));
            }

            let take = self
                .chunk
                .min(self.bytes.len() - self.sent)
                .min(buffer.len());
            buffer[..take].copy_from_slice(&self.bytes[self.sent..self.sent + take]);
            self.sent += take;

            Ok(take)
        }
    }

    #[test]
    fn a_record_that_arrives_in_pieces_is_read_rather_than_refused() {
        // A 2560x1440 keyframe does not fit in one poll, and a viewer that
        // treated the gap as a fault would drop the channel and ask for a
        // keyframe that does not fit either.
        let limits = Limits::new(2560, 1440);
        let record = Record::new(Channel::Frame, 6, 3, 0, 0, vec![7u8; 300_000]);

        let mut wire = Vec::new();
        write(&mut wire, &record, &limits).expect("a record within the frame cap");
        // Starts ready, so the first read delivers bytes: a poll that expires
        // before a record begins is a legitimate `Idle` and not what this is
        // about.
        let mut stream = Trickle {
            bytes: wire,
            sent: 0,
            chunk: 4096,
            blocked: true,
        };

        let mut payload = Vec::new();
        let header = read(&mut stream, &limits, &mut payload).expect("what was written");

        assert_eq!(header, record.header);
        assert_eq!(payload.len(), 300_000);
    }

    #[test]
    fn a_payload_over_its_channel_cap_is_never_written() {
        let limits = Limits::new(64, 64);
        let record = Record::new(Channel::Input, 5, 0, 0, 0, vec![0u8; 4097]);

        let mut wire = Vec::new();
        let error = write(&mut wire, &record, &limits).expect_err("a payload over the input cap");

        assert!(matches!(
            error,
            RecordError::TooLarge {
                channel: Channel::Input,
                length: 4097,
                cap: 4096
            }
        ));
        assert!(wire.is_empty(), "nothing may reach the wire");
    }

    #[test]
    fn a_length_over_the_cap_is_refused_before_anything_is_allocated() {
        let limits = Limits::new(64, 64);
        let mut header = Record::new(Channel::Frame, 5, 0, 0, 0, Vec::new()).header;
        header.length = limits.for_channel(Channel::Frame) + 1;

        let mut payload = Vec::new();
        let error = read(&mut header.encode().as_slice(), &limits, &mut payload)
            .expect_err("a length over the frame cap");

        assert!(matches!(error, RecordError::TooLarge { .. }));
    }

    #[test]
    fn a_payload_that_does_not_match_its_checksum_is_refused() {
        let limits = Limits::new(64, 64);
        let record = Record::new(Channel::Control, 8, 0, 0, 0, b"payload".to_vec());

        let mut wire = Vec::new();
        write(&mut wire, &record, &limits).expect("a record within the control cap");
        let last = wire.len() - 1;
        wire[last] ^= 0xFF;

        let mut payload = Vec::new();
        let error =
            read(&mut wire.as_slice(), &limits, &mut payload).expect_err("a corrupt payload");

        assert!(matches!(error, RecordError::ChecksumMismatch { .. }));
    }

    #[test]
    fn a_peer_that_hangs_up_between_records_is_not_a_fault() {
        let limits = Limits::new(64, 64);
        let mut payload = Vec::new();

        let error = read(&mut [].as_slice(), &limits, &mut payload).expect_err("an empty stream");

        assert!(matches!(error, RecordError::Closed));
    }

    #[test]
    fn a_stream_cut_inside_a_header_is_a_fault() {
        let limits = Limits::new(64, 64);
        let mut payload = Vec::new();

        let error = read(&mut [24u8, 1, 0].as_slice(), &limits, &mut payload)
            .expect_err("a truncated header");

        assert!(matches!(error, RecordError::Io(_)));
    }

    #[test]
    fn the_extra_bytes_of_a_newer_minors_header_are_skipped() {
        let limits = Limits::new(64, 64);
        let record = Record::new(Channel::Control, 8, 0, 0, 0, b"payload".to_vec());

        let mut wire = record.header.encode().to_vec();
        wire[0] = 28;
        wire.extend_from_slice(&[0xAA; 4]);
        wire.extend_from_slice(&record.payload);

        let mut payload = Vec::new();
        let header =
            read(&mut wire.as_slice(), &limits, &mut payload).expect("a newer minor's record");

        assert_eq!(header.message_type, 8);
        assert_eq!(payload, b"payload");
    }
}
