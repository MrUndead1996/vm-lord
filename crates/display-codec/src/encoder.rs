//! Turning captured frames into the frame channel's payloads.

use crate::{
    container::{self, Method},
    cursor::{self, CursorImage, CursorPosition},
    error::CodecError,
    geometry::{Geometry, Rect},
    queue::Staging,
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
    /// How many encoded frames the protective sweep takes to cover the whole
    /// tile grid, so that a viewer which somehow diverged without noticing
    /// recovers on its own. Zero turns the sweep off.
    ///
    /// This used to be a whole keyframe on the same period, which measured
    /// badly: a real desktop's keyframe is megabytes, not the couple of
    /// hundred kilobytes the synthetic scenes suggested, and an idle desktop
    /// paid all of it for nothing. See [`Encoder::next_payload`].
    pub refresh_interval: u32,
}

impl EncoderConfig {
    /// The default configuration for a geometry: a sweep that covers the grid
    /// every 300 frames, which is ten seconds at thirty frames a second.
    #[must_use]
    pub fn new(geometry: Geometry) -> Self {
        Self {
            geometry,
            refresh_interval: 300,
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
    /// How many encoded frames the sweep takes to cross the grid. Zero is no
    /// sweep at all.
    refresh_interval: u32,
    /// The tile the next protective sweep starts at. Wraps, so the grid is
    /// covered once per interval for as long as frames keep being encoded.
    sweep: u32,
    /// What has been captured and not yet encoded. Every slot is latest-wins.
    staging: Staging,
    /// What the far side is believed to hold: the last payload handed out.
    reference: Vec<u32>,
    has_reference: bool,
    output: Vec<u8>,
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
            refresh_interval: config.refresh_interval,
            sweep: 0,
            staging: Staging::new(pixels),
            reference: vec![0; pixels],
            has_reference: false,
            output: Vec::new(),
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

        let staged = self.staging.frame_mut();
        for y in 0..height {
            let source = &frame.pixels[y * frame.stride..y * frame.stride + row];
            let target = &mut staged[y * width..(y + 1) * width];
            for (pixel, bytes) in target.iter_mut().zip(source.chunks_exact(4)) {
                *pixel = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            }
        }

        self.staging.stage_frame(damage);
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
        cursor::write_image(self.staging.cursor_image_mut(), image)?;
        self.staging.stage_cursor_image();
        Ok(())
    }

    /// Replaces the cursor position the next payload will carry.
    pub fn submit_cursor_position(&mut self, position: CursorPosition) {
        cursor::write_position(self.staging.cursor_position_mut(), position);
        self.staging.stage_cursor_position();
    }

    /// Records that the viewer asked for a keyframe, which the next payload
    /// will be.
    ///
    /// Recovery for a decoder that lost synchronisation -- the protocol's only
    /// back edge -- not flow control.
    pub fn request_keyframe(&mut self) {
        self.staging.request_keyframe();
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
        if let Some(hint) = self.staging.take_frame() {
            if self.keyframe_due() {
                self.encode_keyframe();
                return Some(Payload::Keyframe(&self.output));
            }

            if self.encode_delta(hint.as_deref()) {
                return Some(Payload::TileDelta(&self.output));
            }
        } else if self.staging.keyframe_requested() && self.has_reference {
            // Nothing new was captured, so the keyframe is of the frame the
            // far side should already have: a viewer that lost
            // synchronisation must not wait for the guest to repaint.
            self.encode_keyframe();
            return Some(Payload::Keyframe(&self.output));
        }

        if self.staging.take_cursor_image() {
            return Some(Payload::CursorImage(self.staging.cursor_image()));
        }

        if self.staging.take_cursor_position() {
            return Some(Payload::CursorPosition(self.staging.cursor_position()));
        }

        None
    }

    /// Whether the frame about to be encoded must be a whole one.
    ///
    /// Two reasons, and no others: there is nothing to build on, or the viewer
    /// asked. The protective interval used to be a third, and is now the sweep
    /// in [`Encoder::mark_sweep`] instead -- a whole frame on a period costs
    /// megabytes on a real desktop even when nothing has moved, and the sweep
    /// buys the same recovery for the bytes that actually differ.
    fn keyframe_due(&self) -> bool {
        !self.has_reference || self.staging.keyframe_requested()
    }

    /// Adds this frame's share of the protective sweep to the selection.
    ///
    /// Damage is a hint, so a tile no hint ever covers is a tile that could
    /// stay wrong on the viewer forever. The sweep is the answer: every
    /// encoded frame compares a slice of the grid whatever the hint said, and
    /// the slice advances, so the whole grid is checked once per
    /// `refresh_interval` frames. A tile that matches the reference costs
    /// nothing to check -- which is every tile of a desktop nobody is touching
    /// -- and one that does not is repaired by the same delta that carries the
    /// rest of the frame.
    fn mark_sweep(&mut self) {
        let count = self.geometry.tile_count();
        if self.refresh_interval == 0 || count == 0 {
            return;
        }

        // Ceiling, so the interval is an upper bound on how long a tile can
        // go unchecked rather than a target it may overshoot.
        let slice = count.div_ceil(self.refresh_interval);
        for step in 0..slice.min(count) {
            self.selected[((self.sweep + step) % count) as usize] = true;
        }
        self.sweep = (self.sweep + slice) % count;
    }

    /// Writes every tile of the staged frame into `output`.
    fn encode_keyframe(&mut self) {
        self.output.clear();
        container::write_header(&mut self.output, true, &self.geometry);

        let mut index = 0;
        while let Some(rect) = self.geometry.tile(index) {
            gather(self.staging.frame(), self.geometry, rect, &mut self.tile);
            write_tile(&mut self.output, &self.tile, None, &mut self.scratch);
            index += 1;
        }

        self.reference.copy_from_slice(self.staging.frame());
        self.has_reference = true;
        // The whole grid just went out, so the sweep starts over rather than
        // re-checking tiles that were sent this instant.
        self.sweep = 0;
        self.staging.keyframe_sent();
    }

    /// Writes the tiles that changed, and says whether there were any.
    ///
    /// A hint says where a frame *may* differ; the comparison still decides.
    /// Tiles no hint covers are not compared, and so are not advanced in the
    /// reference either -- what the reference holds is what the far side was
    /// sent, never what was captured. The sweep is what keeps that bounded:
    /// a tile the hints keep missing is still compared within
    /// `refresh_interval` frames.
    fn encode_delta(&mut self, hint: Option<&[Rect]>) -> bool {
        select_tiles(&mut self.selected, self.geometry, hint);
        self.mark_sweep();

        self.output.clear();
        container::write_header(&mut self.output, false, &self.geometry);
        let mut written = false;

        let mut index = 0;
        while let Some(rect) = self.geometry.tile(index) {
            if !self.selected[index as usize] {
                index += 1;
                continue;
            }

            if !differs(self.staging.frame(), &self.reference, self.geometry, rect) {
                index += 1;
                continue;
            }

            gather(self.staging.frame(), self.geometry, rect, &mut self.tile);
            gather(
                &self.reference,
                self.geometry,
                rect,
                &mut self.previous_tile,
            );

            varint::write(&mut self.output, index);
            write_tile(
                &mut self.output,
                &self.tile,
                Some(&self.previous_tile),
                &mut self.scratch,
            );
            scatter(&mut self.reference, self.geometry, rect, &self.tile);
            written = true;

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

/// Whether a tile's pixels differ between a frame and the reference.
///
/// Asked before the tile is gathered, because on an ordinary desktop almost
/// every tile is unchanged and copying eight kilobytes to discover that is the
/// larger half of what a delta frame costs. The rows are compared where they
/// lie, and the first difference ends the tile.
///
/// Written as a loop over pairs rather than as a slice comparison on purpose:
/// `==` on a slice is `memcmp`, and the guest links musl, whose `memcmp` is a
/// byte at a time. This form vectorises instead.
fn differs(frame: &[u32], reference: &[u32], geometry: Geometry, rect: Rect) -> bool {
    let width = geometry.width() as usize;

    (rect.y..rect.y + rect.height).any(|y| {
        let start = y as usize * width + rect.x as usize;
        let end = start + rect.width as usize;

        frame[start..end]
            .iter()
            .zip(&reference[start..end])
            .any(|(current, previous)| current != previous)
    })
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
