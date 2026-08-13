//! How an [`Envelope`] is delimited on a byte stream.
//!
//! A frame is a 4-byte little-endian body length followed by that many bytes
//! of encoded `Envelope`. Little-endian because both ends of this socket are
//! x86-64 and reading the prefix is then a plain `u32::from_le_bytes`;
//! length-prefixed because Protobuf messages are not self-delimiting, and a
//! stream socket owes nobody message boundaries.
//!
//! Bodies are capped at [`MAX_BODY_LEN`]. The cap is what keeps a corrupt or
//! hostile prefix from turning into a one-gigabyte allocation, so it is
//! enforced before anything is reserved, on both the reading and the writing
//! side.

use std::{
    error::Error,
    fmt,
    io::{self, Read, Write},
};

use prost::Message;

use crate::v1::Envelope;

/// The width of the length prefix that precedes every body.
pub const LENGTH_PREFIX_LEN: usize = 4;

/// The largest body this protocol carries, in bytes.
///
/// One mebibyte is far above anything the schema can currently produce and far
/// below what a guest could use to exhaust the host: the messages here are
/// status reports and manifests, not payloads. A future message that needs
/// more should be split rather than have this raised.
pub const MAX_BODY_LEN: usize = 1024 * 1024;

/// Encodes `envelope` into `buffer` as a complete frame, prefix included.
///
/// `buffer` is cleared first and is left holding exactly the frame, so a
/// caller can keep one buffer for the life of a connection.
///
/// # Errors
///
/// [`FrameError::TooLarge`] if the encoded envelope exceeds [`MAX_BODY_LEN`].
/// Nothing is written to the wire in that case, which is the point of checking
/// before encoding: a body that cannot be framed must not become a truncated
/// one.
pub fn encode(envelope: &Envelope, buffer: &mut Vec<u8>) -> Result<(), FrameError> {
    let body_len = envelope.encoded_len();
    let prefix = u32::try_from(body_len)
        .ok()
        .filter(|_| body_len <= MAX_BODY_LEN)
        .ok_or(FrameError::TooLarge { body_len })?;

    buffer.clear();
    buffer.reserve(LENGTH_PREFIX_LEN + body_len);
    buffer.extend_from_slice(&prefix.to_le_bytes());
    envelope
        .encode(buffer)
        .expect("a Vec has as much room as `encoded_len` asked for");
    Ok(())
}

/// Reads the body length out of a frame's prefix.
///
/// Split out for callers that do their own reading -- an asynchronous
/// transport, say -- and want the cap enforced the same way [`read`] enforces
/// it.
///
/// # Errors
///
/// [`FrameError::TooLarge`] if the prefix announces more than
/// [`MAX_BODY_LEN`]. The connection is then unusable: the stream is still
/// positioned at a body of unknown length, so there is no way to resynchronise
/// and the caller must close it.
pub fn body_len(prefix: [u8; LENGTH_PREFIX_LEN]) -> Result<usize, FrameError> {
    let body_len = u32::from_le_bytes(prefix) as usize;
    if body_len > MAX_BODY_LEN {
        return Err(FrameError::TooLarge { body_len });
    }
    Ok(body_len)
}

/// Decodes a frame body that has already been read whole.
///
/// # Errors
///
/// [`FrameError::Decode`] if the bytes are not a `vmlord.agent.v1.Envelope`.
pub fn decode(body: &[u8]) -> Result<Envelope, FrameError> {
    Envelope::decode(body).map_err(FrameError::Decode)
}

/// Writes one frame and flushes it.
///
/// Flushing is part of writing here rather than left to the caller: every
/// message in this protocol is something the peer is waiting on, and a
/// buffered transport that holds a request back deadlocks the session.
///
/// # Errors
///
/// [`FrameError::TooLarge`] as [`encode`] describes, or [`FrameError::Io`] if
/// the transport fails.
pub fn write<W: Write>(
    writer: &mut W,
    envelope: &Envelope,
    buffer: &mut Vec<u8>,
) -> Result<(), FrameError> {
    encode(envelope, buffer)?;
    writer.write_all(buffer).map_err(FrameError::Io)?;
    writer.flush().map_err(FrameError::Io)
}

/// Reads one frame, reusing `buffer` for the body.
///
/// # Errors
///
/// [`FrameError::Closed`] if the peer closed the connection cleanly, which is
/// how a session ends and is not by itself a fault. [`FrameError::Idle`] means
/// a transport timed out before the next frame began, so a caller may safely
/// send its own frame. [`FrameError::TooLarge`], [`FrameError::Io`] and
/// [`FrameError::Decode`] all leave the connection unusable and must be
/// answered by closing it.
pub fn read<R: Read>(reader: &mut R, buffer: &mut Vec<u8>) -> Result<Envelope, FrameError> {
    let mut prefix = [0u8; LENGTH_PREFIX_LEN];
    read_prefix(reader, &mut prefix)?;

    let body_len = body_len(prefix)?;
    buffer.clear();
    buffer.resize(body_len, 0);
    reader.read_exact(buffer).map_err(FrameError::Io)?;

    decode(buffer)
}

/// Fills `prefix`, distinguishing a connection that ended between frames from
/// one that ended inside a prefix.
///
/// `Read::read_exact` reports both as `UnexpectedEof`, and the two mean
/// opposite things: the first is a peer that finished talking, the second is a
/// truncated stream.
fn read_prefix<R: Read>(
    reader: &mut R,
    prefix: &mut [u8; LENGTH_PREFIX_LEN],
) -> Result<(), FrameError> {
    let mut filled = 0;
    while filled < prefix.len() {
        match reader.read(&mut prefix[filled..]) {
            Ok(0) if filled == 0 => return Err(FrameError::Closed),
            Ok(0) => {
                return Err(FrameError::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the connection ended part-way through a frame's length prefix",
                )));
            }
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error)
                if filled == 0
                    && matches!(
                        error.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) =>
            {
                return Err(FrameError::Idle);
            }
            Err(error) => return Err(FrameError::Io(error)),
        }
    }
    Ok(())
}

/// A frame that could not be moved between an [`Envelope`] and a stream.
#[derive(Debug)]
pub enum FrameError {
    /// A body larger than [`MAX_BODY_LEN`] was produced or announced.
    TooLarge { body_len: usize },
    /// The peer closed the connection at a frame boundary.
    Closed,
    /// The transport timed out before the peer started another frame.
    Idle,
    /// The transport failed.
    Io(io::Error),
    /// The bytes read are not an `Envelope`.
    Decode(prost::DecodeError),
}

impl fmt::Display for FrameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { body_len } => write!(
                formatter,
                "a frame body of {body_len} bytes exceeds the {MAX_BODY_LEN}-byte limit"
            ),
            Self::Closed => formatter.write_str("the agent connection was closed"),
            Self::Idle => formatter.write_str("the agent connection is idle"),
            Self::Io(error) => write!(formatter, "the agent connection failed: {error}"),
            Self::Decode(error) => {
                write!(formatter, "the agent sent an unreadable message: {error}")
            }
        }
    }
}

impl Error for FrameError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Decode(error) => Some(error),
            Self::TooLarge { .. } | Self::Closed | Self::Idle => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v1::{HeartbeatRequest, request};

    struct IdleReader;

    impl Read for IdleReader {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::WouldBlock, "idle stream"))
        }
    }

    fn heartbeat() -> Envelope {
        Envelope::request(7, request::Kind::Heartbeat(HeartbeatRequest {}))
    }

    #[test]
    fn a_frame_is_its_body_behind_a_little_endian_length() {
        let mut buffer = Vec::new();
        encode(&heartbeat(), &mut buffer).expect("a heartbeat fits");

        let (prefix, body) = buffer.split_at(LENGTH_PREFIX_LEN);
        assert_eq!(
            u32::from_le_bytes(prefix.try_into().expect("four bytes")) as usize,
            body.len()
        );
        assert_eq!(decode(body).expect("a readable body"), heartbeat());
    }

    #[test]
    fn frames_survive_a_round_trip_through_a_stream() {
        let mut buffer = Vec::new();
        let mut stream = Vec::new();
        write(&mut stream, &heartbeat(), &mut buffer).expect("a writable stream");
        write(&mut stream, &heartbeat(), &mut buffer).expect("a writable stream");

        let mut reader = stream.as_slice();
        assert_eq!(
            read(&mut reader, &mut buffer).expect("frame one"),
            heartbeat()
        );
        assert_eq!(
            read(&mut reader, &mut buffer).expect("frame two"),
            heartbeat()
        );
        assert!(matches!(
            read(&mut reader, &mut buffer),
            Err(FrameError::Closed)
        ));
    }

    #[test]
    fn an_empty_body_is_a_default_envelope() {
        let mut buffer = Vec::new();
        let empty = Envelope::default();
        encode(&empty, &mut buffer).expect("an empty envelope fits");

        assert_eq!(buffer, 0u32.to_le_bytes());
        assert_eq!(
            read(&mut buffer.as_slice(), &mut Vec::new()).expect("a frame"),
            empty
        );
    }

    #[test]
    fn an_oversized_prefix_is_refused_before_anything_is_allocated() {
        let body_len = MAX_BODY_LEN + 1;
        let prefix = u32::try_from(body_len)
            .expect("a four-byte length")
            .to_le_bytes();

        assert!(matches!(
            super::body_len(prefix),
            Err(FrameError::TooLarge { body_len: reported }) if reported == body_len
        ));
        assert!(matches!(
            read(&mut prefix.as_slice(), &mut Vec::new()),
            Err(FrameError::TooLarge { .. })
        ));
    }

    #[test]
    fn a_prefix_cut_in_half_is_not_a_clean_close() {
        let error = read(&mut [0u8, 0].as_slice(), &mut Vec::new()).expect_err("a short prefix");

        match error {
            FrameError::Io(error) => assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof),
            other => panic!("expected an I/O error, got {other}"),
        }
    }

    #[test]
    fn an_idle_stream_is_distinct_from_a_frame_that_was_started() {
        assert!(matches!(
            read(&mut IdleReader, &mut Vec::new()),
            Err(FrameError::Idle)
        ));
    }

    #[test]
    fn a_truncated_body_is_not_a_clean_close() {
        let mut frame = Vec::new();
        encode(&heartbeat(), &mut frame).expect("a heartbeat fits");
        frame.pop();

        assert!(matches!(
            read(&mut frame.as_slice(), &mut Vec::new()),
            Err(FrameError::Io(_))
        ));
    }

    #[test]
    fn a_body_that_is_not_an_envelope_is_refused() {
        // Field 1 (`request_id`) declared as a length-delimited value, which
        // is not what a `uint32` is encoded as.
        assert!(matches!(decode(&[0x0a, 0x01]), Err(FrameError::Decode(_))));
    }
}
