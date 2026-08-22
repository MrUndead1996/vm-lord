//! The cursor stream: its own records, its own state, its own limits.
//!
//! A cursor is not a tile. It moves far more often than anything on the
//! desktop changes, it is small, and it must reach the viewer even when a
//! frame is still being written -- so it shares neither the container nor the
//! encoder's reference frame.

use crate::{error::CodecError, zrle};

/// The largest cursor edge, in pixels.
///
/// Bounds the record without consulting the frame geometry, which is what lets
/// a cursor be decoded by a viewer that has not yet seen a `StreamConfig`.
pub const MAX_CURSOR_DIMENSION: u32 = 256;

/// The format this build writes and understands for both cursor records.
const FORMAT_VERSION: u8 = 1;

/// The width of a cursor image header.
const IMAGE_HEADER_LEN: usize = 10;

/// The whole of a cursor position record.
const POSITION_LEN: usize = 6;

/// A cursor bitmap, borrowed from whatever produced it.
#[derive(Clone, Copy, Debug)]
pub struct CursorImage<'a> {
    /// The bitmap, four bytes per pixel, `width * 4` per row.
    pub pixels: &'a [u8],
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Where the pointer actually points, from the bitmap's left edge.
    pub hotspot_x: u32,
    /// Where the pointer actually points, from its top edge.
    pub hotspot_y: u32,
}

/// A decoded cursor bitmap, which owns its pixels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnedCursorImage {
    /// The bitmap, four bytes per pixel, `width * 4` per row.
    pub pixels: Vec<u8>,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Where the pointer actually points, from the bitmap's left edge.
    pub hotspot_x: u32,
    /// Where the pointer actually points, from its top edge.
    pub hotspot_y: u32,
}

/// Where the cursor is, and whether it is drawn at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorPosition {
    /// Distance from the frame's left edge, in guest pixels.
    pub x: u32,
    /// Distance from the frame's top edge, in guest pixels.
    pub y: u32,
    /// Whether the guest is showing a cursor at all.
    pub visible: bool,
}

/// Writes a cursor image, in whichever of raw and ZRLE is shorter.
///
/// # Errors
///
/// [`CodecError::CursorTooLarge`] for an edge above [`MAX_CURSOR_DIMENSION`]
/// or a hotspot outside the bitmap, and [`CodecError::FrameSize`] if the
/// pixels do not match the dimensions.
pub(crate) fn write_image(out: &mut Vec<u8>, image: CursorImage<'_>) -> Result<(), CodecError> {
    if image.width == 0
        || image.height == 0
        || image.width > MAX_CURSOR_DIMENSION
        || image.height > MAX_CURSOR_DIMENSION
        || image.hotspot_x >= image.width
        || image.hotspot_y >= image.height
    {
        return Err(CodecError::CursorTooLarge);
    }

    let expected = image.width as usize * image.height as usize * 4;
    if image.pixels.len() != expected {
        return Err(CodecError::FrameSize {
            expected,
            actual: image.pixels.len(),
        });
    }

    let pixels: Vec<u32> = image
        .pixels
        .chunks_exact(4)
        .map(|chunk| u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect();

    let mut compressed = Vec::new();
    zrle::encode(&pixels, &mut compressed);

    out.clear();
    out.push(FORMAT_VERSION);
    let raw = compressed.len() >= expected;
    out.push(u8::from(!raw));
    out.extend_from_slice(&(image.width as u16).to_le_bytes());
    out.extend_from_slice(&(image.height as u16).to_le_bytes());
    out.extend_from_slice(&(image.hotspot_x as u16).to_le_bytes());
    out.extend_from_slice(&(image.hotspot_y as u16).to_le_bytes());

    if raw {
        out.extend_from_slice(image.pixels);
    } else {
        out.extend_from_slice(&compressed);
    }

    Ok(())
}

/// Reads a cursor image.
///
/// # Errors
///
/// [`CodecError::UnknownVersion`], [`CodecError::UnknownMethod`],
/// [`CodecError::CursorTooLarge`], [`CodecError::Truncated`] and
/// [`CodecError::TrailingBytes`], each for what its name says.
pub(crate) fn read_image(payload: &[u8]) -> Result<OwnedCursorImage, CodecError> {
    let header = payload
        .get(..IMAGE_HEADER_LEN)
        .ok_or(CodecError::Truncated)?;

    if header[0] != FORMAT_VERSION {
        return Err(CodecError::UnknownVersion { version: header[0] });
    }

    let width = u32::from(u16::from_le_bytes([header[2], header[3]]));
    let height = u32::from(u16::from_le_bytes([header[4], header[5]]));
    let hotspot_x = u32::from(u16::from_le_bytes([header[6], header[7]]));
    let hotspot_y = u32::from(u16::from_le_bytes([header[8], header[9]]));

    if width == 0
        || height == 0
        || width > MAX_CURSOR_DIMENSION
        || height > MAX_CURSOR_DIMENSION
        || hotspot_x >= width
        || hotspot_y >= height
    {
        return Err(CodecError::CursorTooLarge);
    }

    let count = width as usize * height as usize;
    let body = &payload[IMAGE_HEADER_LEN..];

    let pixels = match header[1] {
        0 => {
            if body.len() < count * 4 {
                return Err(CodecError::Truncated);
            }
            if body.len() > count * 4 {
                return Err(CodecError::TrailingBytes);
            }
            body.to_vec()
        }
        1 => {
            let mut decoded = vec![0u32; count];
            zrle::decode(body, &mut decoded)?;
            decoded
                .iter()
                .flat_map(|pixel| pixel.to_le_bytes())
                .collect()
        }
        method => return Err(CodecError::UnknownMethod { method }),
    };

    Ok(OwnedCursorImage {
        pixels,
        width,
        height,
        hotspot_x,
        hotspot_y,
    })
}

/// Writes a cursor position, which is six bytes and no varints: it is the most
/// frequent record on the frame channel.
pub(crate) fn write_position(out: &mut Vec<u8>, position: CursorPosition) {
    out.clear();
    out.push(FORMAT_VERSION);
    out.push(u8::from(position.visible));
    out.extend_from_slice(&(position.x.min(u32::from(u16::MAX)) as u16).to_le_bytes());
    out.extend_from_slice(&(position.y.min(u32::from(u16::MAX)) as u16).to_le_bytes());
}

/// Reads a cursor position.
///
/// # Errors
///
/// [`CodecError::Truncated`] or [`CodecError::TrailingBytes`] for a record
/// that is not exactly [`POSITION_LEN`] bytes, and
/// [`CodecError::UnknownVersion`] for a format this build does not know.
pub(crate) fn read_position(payload: &[u8]) -> Result<CursorPosition, CodecError> {
    if payload.len() < POSITION_LEN {
        return Err(CodecError::Truncated);
    }
    if payload.len() > POSITION_LEN {
        return Err(CodecError::TrailingBytes);
    }
    if payload[0] != FORMAT_VERSION {
        return Err(CodecError::UnknownVersion {
            version: payload[0],
        });
    }

    Ok(CursorPosition {
        x: u32::from(u16::from_le_bytes([payload[2], payload[3]])),
        y: u32::from(u16::from_le_bytes([payload[4], payload[5]])),
        visible: payload[1] != 0,
    })
}
