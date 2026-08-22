//! Turning captured frames into the frame channel's payloads.

use crate::{
    container::{self, Method},
    cursor::{self, CursorImage, CursorPosition},
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
    /// Where the staged frame may differ from the one before it, accumulated
    /// across every submission since the last payload: a frame displaced
    /// before it was encoded still carried changes, and its hint is the only
    /// record of where they were. `None` means "somewhere".
    hint: Option<Vec<Rect>>,
    /// What the far side is believed to hold: the last payload handed out.
    reference: Vec<u32>,
    has_reference: bool,
    output: Vec<u8>,
    /// The cursor's two records live in buffers of their own: they must be
    /// able to overtake a frame that is still being written, and each is
    /// latest-wins on its own.
    cursor_image: Vec<u8>,
    cursor_image_pending: bool,
    cursor_position: Vec<u8>,
    cursor_position_pending: bool,
    tile: Vec<u32>,
    previous_tile: Vec<u32>,
    selected: Vec<bool>,
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
            hint: Some(Vec::new()),
            reference: vec![0; pixels],
            has_reference: false,
            output: Vec::new(),
            cursor_image: Vec::new(),
            cursor_image_pending: false,
            cursor_position: Vec::new(),
            cursor_position_pending: false,
            tile: Vec::new(),
            previous_tile: Vec::new(),
            selected: vec![false; config.geometry.tile_count() as usize],
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

        match (damage, self.hint.as_mut()) {
            (Some(rects), Some(hint)) => hint.extend_from_slice(rects),
            (Some(_), None) => {}
            (None, _) => self.hint = None,
        }

        self.staged_pending = true;
        Ok(())
    }

    /// Replaces the cursor bitmap the next payload will carry.
    ///
    /// # Errors
    ///
    /// [`CodecError::CursorTooLarge`] for a bitmap above the cursor limit or a
    /// hotspot outside it, and [`CodecError::FrameSize`] when the pixels do
    /// not match the dimensions.
    pub fn submit_cursor_image(&mut self, image: CursorImage<'_>) -> Result<(), CodecError> {
        cursor::write_image(&mut self.cursor_image, image)?;
        self.cursor_image_pending = true;
        Ok(())
    }

    /// Replaces the cursor position the next payload will carry.
    pub fn submit_cursor_position(&mut self, position: CursorPosition) {
        cursor::write_position(&mut self.cursor_position, position);
        self.cursor_position_pending = true;
    }

    /// The next payload to write, or `None` when there is nothing to send.
    ///
    /// Encoding happens here rather than in [`Encoder::submit`], which is what
    /// keeps the reference frame equal to the last payload handed out.
    ///
    /// The order is fixed: the frame first, then the cursor bitmap, then the
    /// cursor position. A viewer that lost synchronisation must not wait for a
    /// mouse move to be written first.
    pub fn next_payload(&mut self) -> Option<Payload<'_>> {
        if self.staged_pending {
            self.staged_pending = false;
            let hint = self.hint.replace(Vec::new());

            if !self.has_reference {
                self.encode_keyframe();
                self.reference.copy_from_slice(&self.staged);
                self.has_reference = true;
                return Some(Payload::Keyframe(&self.output));
            }

            if self.encode_delta(hint.as_deref()) {
                return Some(Payload::TileDelta(&self.output));
            }
        }

        if self.cursor_image_pending {
            self.cursor_image_pending = false;
            return Some(Payload::CursorImage(&self.cursor_image));
        }

        if self.cursor_position_pending {
            self.cursor_position_pending = false;
            return Some(Payload::CursorPosition(&self.cursor_position));
        }

        None
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

    /// Writes the tiles that changed, and says whether there were any.
    ///
    /// A hint says where a frame *may* differ; the comparison still decides.
    /// Tiles no hint covers are not compared, and so are not advanced in the
    /// reference either -- what the reference holds is what the far side was
    /// sent, never what was captured.
    fn encode_delta(&mut self, hint: Option<&[Rect]>) -> bool {
        select_tiles(&mut self.selected, self.geometry, hint);

        self.output.clear();
        container::write_header(&mut self.output, false, &self.geometry);
        let mut written = false;

        let mut index = 0;
        while let Some(rect) = self.geometry.tile(index) {
            if !self.selected[index as usize] {
                index += 1;
                continue;
            }

            gather(&self.staged, self.geometry, rect, &mut self.tile);
            gather(
                &self.reference,
                self.geometry,
                rect,
                &mut self.previous_tile,
            );

            if self.tile != self.previous_tile {
                varint::write(&mut self.output, index);
                write_tile(
                    &mut self.output,
                    &self.tile,
                    Some(&self.previous_tile),
                    &mut self.scratch,
                );
                scatter(&mut self.reference, self.geometry, rect, &self.tile);
                written = true;
            }

            index += 1;
        }

        written
    }
}

/// Marks the tiles a hint reaches, or every tile when there is none.
///
/// A rectangle is clipped to the frame first: a capture backend that reports
/// damage in a stale resolution should select fewer tiles, not none and not a
/// panic.
fn select_tiles(selected: &mut [bool], geometry: Geometry, hint: Option<&[Rect]>) {
    let Some(hint) = hint else {
        selected.fill(true);
        return;
    };

    selected.fill(false);
    let tile = geometry.tile_size().as_pixels();

    for rect in hint {
        if rect.width == 0
            || rect.height == 0
            || rect.x >= geometry.width()
            || rect.y >= geometry.height()
        {
            continue;
        }

        let right = rect.x.saturating_add(rect.width).min(geometry.width()) - 1;
        let bottom = rect.y.saturating_add(rect.height).min(geometry.height()) - 1;

        for row in rect.y / tile..=bottom / tile {
            for column in rect.x / tile..=right / tile {
                selected[(row * geometry.columns() + column) as usize] = true;
            }
        }
    }
}

/// Writes a tile's pixels back into a frame, row by row.
fn scatter(frame: &mut [u32], geometry: Geometry, rect: Rect, tile: &[u32]) {
    let width = geometry.width() as usize;
    for (row, y) in (rect.y..rect.y + rect.height).enumerate() {
        let start = y as usize * width + rect.x as usize;
        frame[start..start + rect.width as usize]
            .copy_from_slice(&tile[row * rect.width as usize..(row + 1) * rect.width as usize]);
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
