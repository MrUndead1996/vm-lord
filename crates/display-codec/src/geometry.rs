//! The shape of a stream's frames, and the grid the codec cuts them into.
//!
//! Geometry is settled out of band, by the `StreamConfig` record the protocol
//! sends ahead of the keyframe it describes, and it is constructor input here
//! rather than something a payload may change: a resolution change is a new
//! `StreamConfig`, hence a new encoder and a new decoder.

use crate::error::CodecError;

/// The largest width or height this codec will encode.
///
/// Not a display limit -- a bound that keeps a grid inside `u16` and every
/// pixel count inside `u32` without a checked multiplication in the hot path.
pub const MAX_DIMENSION: u32 = 16_384;

/// The edge of a square tile, in pixels.
///
/// One of three, because these are what the handshake can agree on. The value
/// is constant for a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TileSize {
    /// Sixteen pixels: the finest grid, and the most per-tile overhead.
    Sixteen = 16,
    /// Thirty-two pixels, the default.
    ThirtyTwo = 32,
    /// Sixty-four pixels: the least overhead, and the most redundant pixels
    /// in a tile a cursor clipped.
    SixtyFour = 64,
}

impl TileSize {
    /// This tile's edge in pixels.
    #[must_use]
    pub fn as_pixels(self) -> u32 {
        self as u32
    }

    /// The tile size a handshake named.
    ///
    /// # Errors
    ///
    /// [`CodecError::Geometry`] for any size no session can agree on.
    pub fn from_pixels(pixels: u32) -> Result<Self, CodecError> {
        match pixels {
            16 => Ok(Self::Sixteen),
            32 => Ok(Self::ThirtyTwo),
            64 => Ok(Self::SixtyFour),
            _ => Err(CodecError::Geometry {
                detail: "a tile size other than 16, 32 or 64",
            }),
        }
    }
}

/// How a pixel's four bytes are arranged.
///
/// Carried for validation and never interpreted: a run-length coder over
/// 32-bit units cannot tell these apart, and the viewer is where the
/// difference matters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    /// Blue, green, red, alpha.
    Bgra8888,
    /// Blue, green, red, one ignored byte.
    Xrgb8888,
}

/// A rectangle of pixels, in frame coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    /// Distance from the left edge.
    pub x: u32,
    /// Distance from the top edge.
    pub y: u32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// The frames of one stream, and the tile grid over them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    width: u32,
    height: u32,
    tile_size: TileSize,
    pixel_format: PixelFormat,
    columns: u32,
    rows: u32,
}

impl Geometry {
    /// The geometry a `StreamConfig` describes.
    ///
    /// # Errors
    ///
    /// [`CodecError::Geometry`] for a zero dimension or one above
    /// [`MAX_DIMENSION`].
    pub fn new(
        width: u32,
        height: u32,
        tile_size: TileSize,
        pixel_format: PixelFormat,
    ) -> Result<Self, CodecError> {
        if width == 0 || height == 0 {
            return Err(CodecError::Geometry {
                detail: "a zero width or height",
            });
        }
        if width > MAX_DIMENSION || height > MAX_DIMENSION {
            return Err(CodecError::Geometry {
                detail: "a dimension above the codec's maximum",
            });
        }

        let tile = tile_size.as_pixels();

        Ok(Self {
            width,
            height,
            tile_size,
            pixel_format,
            columns: width.div_ceil(tile),
            rows: height.div_ceil(tile),
        })
    }

    /// The frame's width in pixels.
    #[must_use]
    pub fn width(self) -> u32 {
        self.width
    }

    /// The frame's height in pixels.
    #[must_use]
    pub fn height(self) -> u32 {
        self.height
    }

    /// The edge of this stream's tiles.
    #[must_use]
    pub fn tile_size(self) -> TileSize {
        self.tile_size
    }

    /// How this stream's pixels are arranged.
    #[must_use]
    pub fn pixel_format(self) -> PixelFormat {
        self.pixel_format
    }

    /// Tiles across.
    #[must_use]
    pub fn columns(self) -> u32 {
        self.columns
    }

    /// Tiles down.
    #[must_use]
    pub fn rows(self) -> u32 {
        self.rows
    }

    /// How many tiles a keyframe carries.
    #[must_use]
    pub fn tile_count(self) -> u32 {
        self.columns * self.rows
    }

    /// Where a tile sits, clipped to the frame.
    ///
    /// The last column and the last row are narrower or shorter whenever the
    /// frame is not a multiple of the tile size, which is the common case.
    #[must_use]
    pub fn tile(self, index: u32) -> Option<Rect> {
        if index >= self.tile_count() {
            return None;
        }

        let tile = self.tile_size.as_pixels();
        let x = (index % self.columns) * tile;
        let y = (index / self.columns) * tile;

        Some(Rect {
            x,
            y,
            width: tile.min(self.width - x),
            height: tile.min(self.height - y),
        })
    }

    /// How many bytes a frame of this geometry occupies, unpadded.
    #[must_use]
    pub fn frame_bytes(self) -> usize {
        self.pixel_count() * 4
    }

    /// How many pixels a frame of this geometry holds.
    #[must_use]
    pub fn pixel_count(self) -> usize {
        self.width as usize * self.height as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_covers_a_size_that_is_not_a_multiple_of_the_tile() {
        let geometry = Geometry::new(100, 70, TileSize::ThirtyTwo, PixelFormat::Bgra8888).unwrap();

        assert_eq!(geometry.columns(), 4);
        assert_eq!(geometry.rows(), 3);
        assert_eq!(geometry.tile_count(), 12);
    }

    #[test]
    fn edge_tiles_are_clipped() {
        let geometry = Geometry::new(100, 70, TileSize::ThirtyTwo, PixelFormat::Bgra8888).unwrap();

        assert_eq!(
            geometry.tile(0),
            Some(Rect {
                x: 0,
                y: 0,
                width: 32,
                height: 32
            })
        );
        // Last column: 100 - 96 = 4 wide. Last row: 70 - 64 = 6 high.
        assert_eq!(
            geometry.tile(3),
            Some(Rect {
                x: 96,
                y: 0,
                width: 4,
                height: 32
            })
        );
        assert_eq!(
            geometry.tile(11),
            Some(Rect {
                x: 96,
                y: 64,
                width: 4,
                height: 6
            })
        );
        assert_eq!(geometry.tile(12), None);
    }

    #[test]
    fn a_zero_or_oversized_dimension_is_refused() {
        assert!(matches!(
            Geometry::new(0, 720, TileSize::ThirtyTwo, PixelFormat::Bgra8888),
            Err(CodecError::Geometry { .. })
        ));
        assert!(matches!(
            Geometry::new(
                1280,
                MAX_DIMENSION + 1,
                TileSize::ThirtyTwo,
                PixelFormat::Bgra8888
            ),
            Err(CodecError::Geometry { .. })
        ));
    }

    #[test]
    fn a_tile_size_is_one_of_three() {
        assert_eq!(TileSize::from_pixels(64).unwrap(), TileSize::SixtyFour);
        assert!(TileSize::from_pixels(48).is_err());
    }
}
