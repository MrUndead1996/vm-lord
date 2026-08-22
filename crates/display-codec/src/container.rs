//! The bytes a keyframe and a tile delta share.
//!
//! Eight bytes of header, then tile records. The grid is derivable from the
//! session's `StreamConfig` and is repeated here on purpose: four bytes turn a
//! `StreamConfig`/frame mismatch from a silently wrong picture into a named
//! error. The two reserved bytes keep the header a multiple of four and are
//! checked as zero, so a later version cannot quietly reuse them.

use crate::{error::CodecError, geometry::Geometry};

/// The format this build writes and understands.
pub(crate) const FORMAT_VERSION: u8 = 1;

/// The width of the container header.
pub(crate) const HEADER_LEN: usize = 8;

/// Set when the container carries every tile and depends on nothing.
pub(crate) const FLAG_KEYFRAME: u8 = 1;

/// How one tile's pixels were encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Method {
    /// The tile's pixels, little-endian, with no length: it follows from the
    /// geometry, and a length field on every tile would not fit the record cap
    /// in the one case that approaches it.
    Raw = 0,
    /// The tile through [`zrle`], preceded by a varint length.
    ///
    /// [`zrle`]: crate::zrle
    Zrle = 1,
    /// The tile XORed with the one held, then through `zrle`. Deltas only.
    XorZrle = 2,
}

impl Method {
    /// The byte that names this method.
    pub(crate) fn as_byte(self) -> u8 {
        self as u8
    }

    /// The method a byte names.
    ///
    /// # Errors
    ///
    /// [`CodecError::UnknownMethod`] for anything else. A method cannot be
    /// skipped the way an unknown capability can: without it the tile's length
    /// is unknown and the rest of the container is unreadable.
    pub(crate) fn from_byte(method: u8) -> Result<Self, CodecError> {
        match method {
            0 => Ok(Self::Raw),
            1 => Ok(Self::Zrle),
            2 => Ok(Self::XorZrle),
            method => Err(CodecError::UnknownMethod { method }),
        }
    }
}

/// Appends the header of a container for `geometry`.
pub(crate) fn write_header(out: &mut Vec<u8>, keyframe: bool, geometry: &Geometry) {
    out.push(FORMAT_VERSION);
    out.push(if keyframe { FLAG_KEYFRAME } else { 0 });
    out.extend_from_slice(&(geometry.columns() as u16).to_le_bytes());
    out.extend_from_slice(&(geometry.rows() as u16).to_le_bytes());
    out.extend_from_slice(&[0, 0]);
}

/// Reads a container header, and returns its keyframe flag and its body.
///
/// # Errors
///
/// [`CodecError::Truncated`] for a payload shorter than a header,
/// [`CodecError::UnknownVersion`] for a format this build does not know,
/// [`CodecError::TrailingBytes`] for reserved bytes a later version claimed,
/// and [`CodecError::GridMismatch`] for a container belonging to another
/// stream's geometry.
pub(crate) fn read_header<'a>(
    bytes: &'a [u8],
    geometry: &Geometry,
) -> Result<(bool, &'a [u8]), CodecError> {
    let header = bytes.get(..HEADER_LEN).ok_or(CodecError::Truncated)?;

    if header[0] != FORMAT_VERSION {
        return Err(CodecError::UnknownVersion { version: header[0] });
    }
    if header[6] != 0 || header[7] != 0 {
        return Err(CodecError::TrailingBytes);
    }

    let columns = u16::from_le_bytes([header[2], header[3]]);
    let rows = u16::from_le_bytes([header[4], header[5]]);
    if u32::from(columns) != geometry.columns() || u32::from(rows) != geometry.rows() {
        return Err(CodecError::GridMismatch { columns, rows });
    }

    Ok((header[1] & FLAG_KEYFRAME != 0, &bytes[HEADER_LEN..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{PixelFormat, TileSize};

    fn geometry() -> Geometry {
        Geometry::new(100, 70, TileSize::ThirtyTwo, PixelFormat::Bgra8888).unwrap()
    }

    #[test]
    fn a_header_round_trips_and_names_a_keyframe() {
        let mut bytes = Vec::new();
        write_header(&mut bytes, true, &geometry());
        bytes.push(0xAB);

        assert_eq!(bytes.len(), HEADER_LEN + 1);
        let (keyframe, body) = read_header(&bytes, &geometry()).unwrap();
        assert!(keyframe);
        assert_eq!(body, &[0xAB]);
    }

    #[test]
    fn a_grid_from_another_geometry_is_refused() {
        let other = Geometry::new(1280, 720, TileSize::ThirtyTwo, PixelFormat::Bgra8888).unwrap();
        let mut bytes = Vec::new();
        write_header(&mut bytes, false, &other);

        assert!(matches!(
            read_header(&bytes, &geometry()),
            Err(CodecError::GridMismatch { .. })
        ));
    }

    #[test]
    fn a_future_version_is_refused() {
        let mut bytes = Vec::new();
        write_header(&mut bytes, false, &geometry());
        bytes[0] = 2;

        assert!(matches!(
            read_header(&bytes, &geometry()),
            Err(CodecError::UnknownVersion { version: 2 })
        ));
    }

    #[test]
    fn reserved_bytes_must_be_zero() {
        let mut bytes = Vec::new();
        write_header(&mut bytes, false, &geometry());
        bytes[6] = 1;

        assert!(matches!(
            read_header(&bytes, &geometry()),
            Err(CodecError::TrailingBytes)
        ));
    }

    #[test]
    fn a_short_header_is_truncated() {
        assert!(matches!(
            read_header(&[1, 0, 4], &geometry()),
            Err(CodecError::Truncated)
        ));
    }

    #[test]
    fn methods_map_to_their_bytes() {
        assert_eq!(Method::from_byte(2).unwrap(), Method::XorZrle);
        assert!(matches!(
            Method::from_byte(3),
            Err(CodecError::UnknownMethod { method: 3 })
        ));
    }
}
