//! Turning captured frames into the frame channel's payloads.

use crate::{
    container::{self, Method},
    error::CodecError,
    geometry::{Geometry, Rect},
    varint, zrle,
};

/// What a capture backend hands over: a whole frame, and the distance between
/// its rows.
#[derive(Clone, Copy, Debug)]
pub struct Frame<'a> {
    /// The frame's pixels, four bytes each, rows `stride` bytes apart.
    pub pixels: &'a [u8],
    /// Bytes per row, which capture backends do not promise equals
    /// `width * 4`.
    pub stride: usize,
}

/// How an encoder is set up. Geometry is fixed for its lifetime.
#[derive(Clone, Copy, Debug)]
pub struct EncoderConfig {
    /// The stream's frames and tile grid.
    pub geometry: Geometry,
    /// A protective keyframe every so many encoded frames, so that a viewer
    /// which somehow diverged without noticing recovers on its own.
    pub keyframe_interval: u32,
}

impl EncoderConfig {
    /// The default configuration for a geometry: a keyframe every 300 frames,
    /// which is ten seconds at thirty frames a second.
    #[must_use]
    pub fn new(geometry: Geometry) -> Self {
        Self {
            geometry,
            keyframe_interval: 300,
        }
    }
}

/// One record's worth of codec bytes, named by the frame record it belongs in.
///
/// The names match the protocol's `FRAME_RECORD_*` types without depending on
/// the protocol crate: the codec stays a leaf, and the guest services do the
/// mapping.
#[derive(Debug)]
pub enum Payload<'a> {
    /// A whole frame, depending on nothing.
    Keyframe(&'a [u8]),
    /// The tiles that changed since the payload before it.
    TileDelta(&'a [u8]),
    /// A new cursor bitmap.
    CursorImage(&'a [u8]),
    /// Where the cursor is now.
    CursorPosition(&'a [u8]),
}

/// The encoding half of the codec.
pub struct Encoder {
    geometry: Geometry,
    /// The most recently submitted frame, which is the only one kept: a slow
    /// socket must be served current state, not a backlog.
    staged: Vec<u32>,
    staged_pending: bool,
    /// What the far side is believed to hold: the last payload handed out.
    reference: Vec<u32>,
    has_reference: bool,
    output: Vec<u8>,
    tile: Vec<u32>,
    scratch: Scratch,
}

/// The buffers `write_tile` reuses, so that encoding a frame allocates
/// nothing once the stream is running.
#[derive(Default)]
struct Scratch {
    xored: Vec<u32>,
    zrle: Vec<u8>,
    xor_zrle: Vec<u8>,
}

impl Encoder {
    /// An encoder for one stream.
    #[must_use]
    pub fn new(config: EncoderConfig) -> Self {
        let pixels = config.geometry.pixel_count();

        Self {
            geometry: config.geometry,
            staged: vec![0; pixels],
            staged_pending: false,
            reference: vec![0; pixels],
            has_reference: false,
            output: Vec::new(),
            tile: Vec::new(),
            scratch: Scratch::default(),
        }
    }

    /// The geometry this encoder was built for.
    #[must_use]
    pub fn geometry(&self) -> Geometry {
        self.geometry
    }

    /// Stages a captured frame, displacing any frame not yet encoded.
    ///
    /// `damage` is a hint about where the frame may differ from the one
    /// before; `None` means "somewhere". Nothing is encoded here -- see
    /// [`Encoder::next_payload`].
    ///
    /// # Errors
    ///
    /// [`CodecError::FrameSize`] if the buffer cannot hold a frame of this
    /// geometry at this stride.
    pub fn submit(&mut self, frame: Frame<'_>, damage: Option<&[Rect]>) -> Result<(), CodecError> {
        let _ = damage;
        let width = self.geometry.width() as usize;
        let height = self.geometry.height() as usize;
        let row = width * 4;

        let needed = if frame.stride < row {
            usize::MAX
        } else {
            frame.stride * (height - 1) + row
        };
        if frame.pixels.len() < needed {
            return Err(CodecError::FrameSize {
                expected: needed,
                actual: frame.pixels.len(),
            });
        }

        for y in 0..height {
            let source = &frame.pixels[y * frame.stride..y * frame.stride + row];
            let target = &mut self.staged[y * width..(y + 1) * width];
            for (pixel, bytes) in target.iter_mut().zip(source.chunks_exact(4)) {
                *pixel = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            }
        }

        self.staged_pending = true;
        Ok(())
    }

    /// The next payload to write, or `None` when there is nothing to send.
    ///
    /// Encoding happens here rather than in [`Encoder::submit`], which is what
    /// keeps the reference frame equal to the last payload handed out.
    pub fn next_payload(&mut self) -> Option<Payload<'_>> {
        if !self.staged_pending {
            return None;
        }

        self.staged_pending = false;
        self.encode_keyframe();
        self.reference.copy_from_slice(&self.staged);
        self.has_reference = true;

        Some(Payload::Keyframe(&self.output))
    }

    /// Writes every tile of the staged frame into `output`.
    fn encode_keyframe(&mut self) {
        self.output.clear();
        container::write_header(&mut self.output, true, &self.geometry);

        let mut index = 0;
        while let Some(rect) = self.geometry.tile(index) {
            gather(&self.staged, self.geometry, rect, &mut self.tile);
            write_tile(&mut self.output, &self.tile, None, &mut self.scratch);
            index += 1;
        }
    }
}

/// Copies a tile's pixels out of a frame, row by row.
fn gather(frame: &[u32], geometry: Geometry, rect: Rect, out: &mut Vec<u32>) {
    let width = geometry.width() as usize;
    out.clear();
    for y in rect.y..rect.y + rect.height {
        let start = y as usize * width + rect.x as usize;
        out.extend_from_slice(&frame[start..start + rect.width as usize]);
    }
}

/// Appends one tile in the shortest encoding available to it.
///
/// Every candidate is evaluated rather than guessed at: that is what makes the
/// output deterministic, which is what the golden vectors and every
/// cross-machine comparison rest on. Ties go to the lower method number.
fn write_tile(out: &mut Vec<u8>, tile: &[u32], previous: Option<&[u32]>, scratch: &mut Scratch) {
    let raw_len = tile.len() * 4;

    scratch.zrle.clear();
    zrle::encode(tile, &mut scratch.zrle);
    let zrle_len = scratch.zrle.len() + varint_len(scratch.zrle.len() as u32);

    let mut best = Method::Raw;
    let mut best_len = raw_len;
    if zrle_len < best_len {
        best = Method::Zrle;
        best_len = zrle_len;
    }

    if let Some(previous) = previous {
        scratch.xored.clear();
        scratch.xored.extend(
            tile.iter()
                .zip(previous)
                .map(|(current, previous)| current ^ previous),
        );

        scratch.xor_zrle.clear();
        zrle::encode(&scratch.xored, &mut scratch.xor_zrle);

        let xor_len = scratch.xor_zrle.len() + varint_len(scratch.xor_zrle.len() as u32);
        if xor_len < best_len {
            best = Method::XorZrle;
        }
    }

    out.push(best.as_byte());
    match best {
        Method::Raw => {
            for pixel in tile {
                out.extend_from_slice(&pixel.to_le_bytes());
            }
        }
        Method::Zrle => {
            varint::write(out, scratch.zrle.len() as u32);
            out.extend_from_slice(&scratch.zrle);
        }
        Method::XorZrle => {
            varint::write(out, scratch.xor_zrle.len() as u32);
            out.extend_from_slice(&scratch.xor_zrle);
        }
    }
}

/// How many bytes a varint of `value` takes.
fn varint_len(value: u32) -> usize {
    match value {
        0..=0x7F => 1,
        0x80..=0x3FFF => 2,
        0x4000..=0x1F_FFFF => 3,
        0x20_0000..=0x0FFF_FFFF => 4,
        _ => 5,
    }
}
