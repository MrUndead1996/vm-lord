//! How a record is delimited on any of the three channels.
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

use std::{error::Error, fmt, io};

/// The width of the header this build writes and understands.
pub const HEADER_LEN: usize = 24;

/// Which of a session's three sockets a record belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    /// Handshake, session control, liveness and errors.
    Control = 1,
    /// Frames and cursors, from the guest only.
    Frame = 2,
    /// Keyboard and pointer, from the host only.
    Input = 3,
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
    /// Which generation of the session's frame and input channels this belongs
    /// to. Stale generations are rejected here, before a decoder or an input
    /// device sees them.
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
}
