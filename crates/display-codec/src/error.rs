//! What can be wrong with a payload, and with a frame handed to the encoder.
//!
//! Every one of these is returned, never panicked: the decoder's input arrives
//! from another machine's process over a socket, and the only acceptable
//! answer to a payload that makes no sense is an error the session can act on.

use std::{error::Error, fmt};

/// Everything this codec refuses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodecError {
    /// A geometry no session could have agreed on.
    Geometry {
        /// What is wrong with it, for a log.
        detail: &'static str,
    },
    /// A frame whose buffer does not match the geometry it was submitted
    /// against.
    FrameSize {
        /// The smallest buffer this geometry and stride could be carried in.
        expected: usize,
        /// What arrived.
        actual: usize,
    },
    /// A container written by a build whose format this one does not know.
    UnknownVersion {
        /// The version byte that arrived.
        version: u8,
    },
    /// A tile encoded by a method this build does not implement.
    UnknownMethod {
        /// The method byte that arrived.
        method: u8,
    },
    /// A container whose grid is not this session's.
    GridMismatch {
        /// The columns the payload claims.
        columns: u16,
        /// The rows the payload claims.
        rows: u16,
    },
    /// A delta naming a tile outside the grid.
    TileIndexOutOfRange {
        /// The index that arrived.
        index: u32,
    },
    /// A delta whose tiles are not in increasing order, which makes the same
    /// change expressible two ways and a repeated tile expressible at all.
    TileIndexNotIncreasing {
        /// The index that did not advance.
        index: u32,
    },
    /// A payload that ends in the middle of something.
    Truncated,
    /// A payload with bytes after the last thing it describes.
    TrailingBytes,
    /// A run longer than the tile it fills.
    RunOverflow,
    /// A delta with nothing to apply it to. The viewer's cue to ask for a
    /// keyframe.
    NoBase,
    /// A keyframe applied as a delta, or a delta applied as a keyframe.
    WrongPayloadKind,
    /// A cursor image larger than [`MAX_CURSOR_DIMENSION`], or one whose
    /// hotspot lies outside it.
    ///
    /// [`MAX_CURSOR_DIMENSION`]: crate::cursor::MAX_CURSOR_DIMENSION
    CursorTooLarge,
}

impl fmt::Display for CodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Geometry { detail } => write!(formatter, "unusable geometry: {detail}"),
            Self::FrameSize { expected, actual } => write!(
                formatter,
                "a frame of {actual} bytes where {expected} were needed"
            ),
            Self::UnknownVersion { version } => {
                write!(formatter, "payload format version {version} is not known")
            }
            Self::UnknownMethod { method } => {
                write!(formatter, "tile method {method} is not known")
            }
            Self::GridMismatch { columns, rows } => write!(
                formatter,
                "a payload for a {columns}x{rows} grid, which is not this stream's"
            ),
            Self::TileIndexOutOfRange { index } => {
                write!(formatter, "tile index {index} is outside the grid")
            }
            Self::TileIndexNotIncreasing { index } => {
                write!(
                    formatter,
                    "tile index {index} does not follow the one before"
                )
            }
            Self::Truncated => formatter.write_str("the payload ends mid-record"),
            Self::TrailingBytes => {
                formatter.write_str("the payload has bytes after its last record")
            }
            Self::RunOverflow => formatter.write_str("a run reaches past the end of its tile"),
            Self::NoBase => formatter.write_str("a delta arrived before any keyframe"),
            Self::WrongPayloadKind => {
                formatter.write_str("a keyframe was applied as a delta, or a delta as a keyframe")
            }
            Self::CursorTooLarge => formatter.write_str("the cursor image is larger than allowed"),
        }
    }
}

impl Error for CodecError {}
