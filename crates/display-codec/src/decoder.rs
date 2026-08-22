//! Turning the frame channel's payloads back into pixels.
//!
//! Everything here arrives from another machine's process. Every length, index
//! and run is checked against the geometry this decoder was built for, and
//! every failure is a returned [`CodecError`] -- a payload can be hostile, and
//! a panic in a viewer is a lost session.

use crate::{
    container::{self, Method},
    cursor::{self, CursorPosition, OwnedCursorImage},
    error::CodecError,
    geometry::{Geometry, Rect},
    varint, zrle,
};

/// The decoding half of the codec.
pub struct Decoder {
    geometry: Geometry,
    /// The frame as the viewer wants it: bytes, ready for a texture upload.
    frame: Vec<u8>,
    has_frame: bool,
    damage: Vec<Rect>,
    tile: Vec<u32>,
}

impl Decoder {
    /// A decoder for one stream.
    #[must_use]
    pub fn new(geometry: Geometry) -> Self {
        Self {
            geometry,
            frame: vec![0; geometry.frame_bytes()],
            has_frame: false,
            damage: Vec::new(),
            tile: Vec::new(),
        }
    }

    /// The geometry this decoder was built for.
    #[must_use]
    pub fn geometry(&self) -> Geometry {
        self.geometry
    }

    /// The frame as it now stands, four bytes per pixel, `width * 4` per row.
    #[must_use]
    pub fn frame(&self) -> &[u8] {
        &self.frame
    }

    /// Replaces the whole frame, and returns the rectangles that changed --
    /// all of them.
    ///
    /// # Errors
    ///
    /// Any [`CodecError`] the container or a tile can produce, and
    /// [`CodecError::WrongPayloadKind`] for a delta.
    pub fn apply_keyframe(&mut self, payload: &[u8]) -> Result<&[Rect], CodecError> {
        let (keyframe, body) = container::read_header(payload, &self.geometry)?;
        if !keyframe {
            return Err(CodecError::WrongPayloadKind);
        }

        self.damage.clear();
        let mut read = 0;
        let mut index = 0;
        while let Some(rect) = self.geometry.tile(index) {
            read += self.read_tile(&body[read..], rect, false)?;
            self.damage.push(rect);
            index += 1;
        }

        if read != body.len() {
            return Err(CodecError::TrailingBytes);
        }

        self.has_frame = true;
        Ok(&self.damage)
    }

    /// Applies the tiles a delta carries, and returns the rectangles they
    /// cover.
    ///
    /// # Errors
    ///
    /// [`CodecError::NoBase`] when no keyframe has been applied,
    /// [`CodecError::WrongPayloadKind`] for a keyframe, and any error the
    /// container or a tile can produce.
    pub fn apply_delta(&mut self, payload: &[u8]) -> Result<&[Rect], CodecError> {
        let (keyframe, body) = container::read_header(payload, &self.geometry)?;
        if keyframe {
            return Err(CodecError::WrongPayloadKind);
        }
        if !self.has_frame {
            return Err(CodecError::NoBase);
        }

        self.damage.clear();
        let mut read = 0;
        let mut previous = None;

        while read < body.len() {
            let (index, width) = varint::read(&body[read..])?;
            read += width;

            let rect = self
                .geometry
                .tile(index)
                .ok_or(CodecError::TileIndexOutOfRange { index })?;
            if previous.is_some_and(|previous| index <= previous) {
                return Err(CodecError::TileIndexNotIncreasing { index });
            }
            previous = Some(index);

            read += self.read_tile(&body[read..], rect, true)?;
            self.damage.push(rect);
        }

        Ok(&self.damage)
    }

    /// Reads a cursor bitmap.
    ///
    /// An associated function rather than a method: the cursor stream keeps no
    /// state on either side, which is what lets a viewer draw a cursor before
    /// the first keyframe arrives.
    ///
    /// # Errors
    ///
    /// Any [`CodecError`] a cursor image can carry.
    pub fn decode_cursor_image(payload: &[u8]) -> Result<OwnedCursorImage, CodecError> {
        cursor::read_image(payload)
    }

    /// Reads a cursor position.
    ///
    /// # Errors
    ///
    /// Any [`CodecError`] a cursor position can carry.
    pub fn decode_cursor_position(payload: &[u8]) -> Result<CursorPosition, CodecError> {
        cursor::read_position(payload)
    }

    /// Reads one tile record into the frame, and says how long it was.
    fn read_tile(&mut self, bytes: &[u8], rect: Rect, xor: bool) -> Result<usize, CodecError> {
        let method = Method::from_byte(*bytes.first().ok_or(CodecError::Truncated)?)?;
        if matches!(method, Method::XorZrle) && !xor {
            // A keyframe depends on nothing, so it cannot carry a tile that
            // does.
            return Err(CodecError::UnknownMethod {
                method: method.as_byte(),
            });
        }

        let pixels = rect.width as usize * rect.height as usize;
        self.tile.clear();
        self.tile.resize(pixels, 0);

        let read = match method {
            Method::Raw => {
                let raw = bytes.get(1..1 + pixels * 4).ok_or(CodecError::Truncated)?;
                for (pixel, chunk) in self.tile.iter_mut().zip(raw.chunks_exact(4)) {
                    *pixel = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                }
                1 + pixels * 4
            }
            Method::Zrle | Method::XorZrle => {
                let (length, width) = varint::read(&bytes[1..])?;
                let start = 1 + width;
                let end = start
                    .checked_add(length as usize)
                    .ok_or(CodecError::Truncated)?;
                let stream = bytes.get(start..end).ok_or(CodecError::Truncated)?;
                zrle::decode(stream, &mut self.tile)?;
                end
            }
        };

        self.scatter(rect, matches!(method, Method::XorZrle));
        Ok(read)
    }

    /// Writes the decoded tile into the frame, XORing onto what is there when
    /// the tile was encoded as a difference.
    fn scatter(&mut self, rect: Rect, xor: bool) {
        let stride = self.geometry.width() as usize * 4;

        for (row, y) in (rect.y..rect.y + rect.height).enumerate() {
            let start = y as usize * stride + rect.x as usize * 4;
            let target = &mut self.frame[start..start + rect.width as usize * 4];
            let source = &self.tile[row * rect.width as usize..(row + 1) * rect.width as usize];

            for (pixel, chunk) in source.iter().zip(target.chunks_exact_mut(4)) {
                let pixel = if xor {
                    pixel ^ u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
                } else {
                    *pixel
                };
                chunk.copy_from_slice(&pixel.to_le_bytes());
            }
        }
    }
}
