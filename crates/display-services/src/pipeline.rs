//! Turning captured frames into the frame channel's records.
//!
//! Everything above the mapping and below the socket, which is what makes it
//! the part a test can drive: it takes a [`CapturedFrame`] rather than a source
//! and writes into a [`Write`] rather than a socket.
//!
//! The bounded queue is the encoder's, and it is the reason nothing here
//! encodes on submission: the reference frame must equal the last payload
//! handed out, so a frame displaced before it was encoded costs nothing but its
//! pixels. A slow socket therefore gets one current delta rather than a backlog
//! of stale ones.

use std::{fmt, io::Write};

use prost::Message as _;
use vmlord_display_codec::{
    CodecError, CursorImage, CursorPosition, Encoder, EncoderConfig, Frame, Geometry, Payload,
    PixelFormat, Rect,
};
use vmlord_display_protocol::{
    record::{self, Channel, Header, Limits, RecordError},
    v1::{self, FrameRecord, StreamConfig},
};

use crate::{
    capture::CapturedFrame,
    cursor::{self, Placement},
};

/// What went wrong between a captured frame and a written record.
#[derive(Debug)]
pub enum PipelineError {
    /// The encoder refused the frame or the cursor.
    Codec(CodecError),
    /// The record could not be framed, or the transport failed.
    Record(RecordError),
}

impl fmt::Display for PipelineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Codec(error) => write!(formatter, "the encoder refused a frame: {error}"),
            Self::Record(error) => write!(formatter, "the frame channel failed: {error}"),
        }
    }
}

impl std::error::Error for PipelineError {}

impl From<CodecError> for PipelineError {
    fn from(error: CodecError) -> Self {
        Self::Codec(error)
    }
}

impl From<RecordError> for PipelineError {
    fn from(error: RecordError) -> Self {
        Self::Record(error)
    }
}

/// A cursor the peer will not be sent, held to be drawn into the frame.
struct DrawnCursor {
    /// The bitmap, four bytes per pixel, exactly as the plane carried it.
    /// Bytes rather than words because that is what the frame is: a bitmap
    /// converted on arrival would have to be converted back to be composited.
    pixels: Vec<u8>,
    /// Its width in pixels, which is what turns an index into a row.
    width: u32,
    /// Where it lands, already cropped to the frame.
    placement: Placement,
}

/// The part of the frame a placed cursor covers.
fn drawn_rect(placement: Placement) -> Rect {
    Rect {
        x: placement.x,
        y: placement.y,
        width: placement.crop.width,
        height: placement.crop.height,
    }
}

/// The frame channel's producer: one per bound socket.
pub struct Pipeline {
    encoder: Encoder,
    /// The generation the socket was bound at. Every record carries it, which
    /// is how the host refuses one from a socket it has already replaced.
    generation: u32,
    /// Whether the peer took the cursor-stream capability. When it did not, the
    /// cursor is composited into the frame instead of sent beside it.
    cursor_stream: bool,
    /// The next record's sequence. Counted per socket, not per session: a
    /// rebound channel starts again at zero.
    sequence: u32,
    /// The sequence of the last frame payload written, which is what a delta
    /// names as its base -- a record that was sent, never a frame that was
    /// captured.
    last_frame_sequence: u32,
    /// The cursor to draw in, when the peer declined the stream.
    drawn_cursor: Option<DrawnCursor>,
    /// The composite's scratch frame, kept so a running stream allocates
    /// nothing per frame.
    composite: Vec<u8>,
    /// The damage handed to the encoder, kept for the same reason.
    damage: Vec<Rect>,
    /// Where the pointer was drawn into the frame before this one, when it
    /// was. What the composite wrote is a change the primary plane's damage
    /// has never heard of.
    pointer: Option<Rect>,
}

impl Pipeline {
    /// A pipeline for one bound frame socket.
    #[must_use]
    pub fn new(geometry: Geometry, generation: u32, cursor_stream: bool) -> Self {
        Self {
            encoder: Encoder::new(EncoderConfig::new(geometry)),
            generation,
            cursor_stream,
            sequence: 0,
            last_frame_sequence: 0,
            drawn_cursor: None,
            composite: Vec::new(),
            damage: Vec::new(),
            pointer: None,
        }
    }

    /// Moves the pipeline to a new output geometry.
    ///
    /// A geometry never changes inside an encoder -- a tile grid is built on
    /// one -- so this replaces it, and what follows on the socket is a
    /// `StreamConfig` and a whole frame, exactly as a freshly bound socket
    /// gets. The sequence counter carries on rather than restarting: the
    /// socket did not change, and a peer that saw record 40 must not be sent
    /// another one.
    pub fn reconfigure(&mut self, geometry: Geometry) {
        self.encoder = Encoder::new(EncoderConfig::new(geometry));
        self.drawn_cursor = None;
        self.composite = Vec::new();
        // Nothing the peer holds can be a base any more: the next frame
        // payload is a keyframe, and a delta would name a record of the old
        // shape.
        self.last_frame_sequence = 0;
        self.request_keyframe();
    }

    /// The geometry this pipeline encodes.
    #[must_use]
    pub fn geometry(&self) -> Geometry {
        self.encoder.geometry()
    }

    /// Stages a captured frame, displacing any frame not yet encoded.
    ///
    /// When the peer declined the cursor stream, the cursor last given to
    /// [`Pipeline::submit_cursor`] is drawn in here, which is why that call
    /// belongs before this one: a cursor submitted afterwards is one frame late.
    ///
    /// # Errors
    ///
    /// [`CodecError`] if the frame does not match the encoder's geometry.
    pub fn submit_frame(&mut self, frame: &CapturedFrame) -> Result<(), CodecError> {
        let stride = frame.stride as usize;
        let pointer = if self.cursor_stream {
            None
        } else {
            self.drawn_cursor
                .as_ref()
                .map(|cursor| cursor.placement)
                .filter(|placement| placement.visible)
                .map(drawn_rect)
        };

        // Two rectangles the compositor's damage cannot know about: where the
        // pointer is being drawn now, and where it was drawn into the frame
        // before this one. The cursor is on its own plane, so nothing repaints
        // the desktop under it, and a trail left behind would stay until the
        // protective keyframe came round.
        let known = frame.damage.is_some();
        self.damage.clear();
        if let Some(rects) = frame.damage.as_deref() {
            self.damage.extend_from_slice(rects);
        }
        self.damage.extend(self.pointer);
        self.damage.extend(pointer);
        self.pointer = pointer;
        let hint = known.then_some(self.damage.as_slice());

        if pointer.is_none() {
            return frame.read(|pixels| self.encoder.submit(Frame { pixels, stride }, hint));
        }
        let Some(cursor) = self.drawn_cursor.as_ref() else {
            return frame.read(|pixels| self.encoder.submit(Frame { pixels, stride }, hint));
        };

        // The frame is copied because it must not be written: it is a
        // read-only mapping of the guest's own scanout. One copy of the bytes
        // as they are, and then a pointer's worth of blending over it -- the
        // composite is the size of the cursor, not of the frame.
        let length = stride * frame.height as usize;
        self.composite.clear();
        frame.read(|pixels| {
            self.composite
                .extend_from_slice(&pixels[..length.min(pixels.len())]);
        });

        cursor::composite(
            &mut self.composite,
            frame.stride,
            &cursor.pixels,
            cursor.width,
            &cursor.placement,
        );

        self.encoder.submit(
            Frame {
                pixels: &self.composite,
                stride,
            },
            hint,
        )
    }

    /// Stages the cursor: its bitmap, when it changed, and where it is.
    ///
    /// `image` is `None` when only the position moved. A placement that is not
    /// visible is a hidden cursor, and is reported as one rather than dropped:
    /// the peer has to be told to stop drawing it.
    ///
    /// # Errors
    ///
    /// [`CodecError`] if the bitmap is one the codec will not carry.
    pub fn submit_cursor(
        &mut self,
        image: Option<(&[u8], u32, u32)>,
        placement: &Placement,
    ) -> Result<(), CodecError> {
        if self.cursor_stream {
            if let Some((pixels, width, height)) = image {
                self.encoder.submit_cursor_image(CursorImage {
                    pixels,
                    width,
                    height,
                    // Zeros because nothing here knows better: task #114's
                    // module does not set DRIVER_CURSOR_HOTSPOT, so the plane
                    // carries no hotspot to read, and mutter has already
                    // subtracted it -- the position below is the bitmap's
                    // corner, not the pointer. The one end that can work the
                    // hotspot out is the host, which is what moves the
                    // pointer; #170 does it there.
                    hotspot_x: 0,
                    hotspot_y: 0,
                })?;
            }
            self.encoder.submit_cursor_position(CursorPosition {
                x: placement.x,
                y: placement.y,
                visible: placement.visible,
            });

            return Ok(());
        }

        match image {
            Some((pixels, width, _height)) => {
                // Reused rather than replaced: a cursor arrives with every
                // frame, and its bitmap is the same size every time.
                let held = self.drawn_cursor.get_or_insert_with(|| DrawnCursor {
                    pixels: Vec::new(),
                    width,
                    placement: *placement,
                });
                held.pixels.clear();
                held.pixels.extend_from_slice(pixels);
                held.width = width;
                held.placement = *placement;
            }
            None => {
                if let Some(cursor) = self.drawn_cursor.as_mut() {
                    cursor.placement = *placement;
                }
            }
        }

        if !placement.visible {
            self.drawn_cursor = None;
        }

        Ok(())
    }

    /// Asks for the next frame record to be a keyframe.
    ///
    /// Recovery, not flow control: a viewer whose decoder has nothing to apply
    /// a delta to says so, and gets a whole frame back.
    pub fn request_keyframe(&mut self) {
        self.encoder.request_keyframe();
    }

    /// Writes the record that opens the stream.
    ///
    /// Geometry travels here rather than in a payload, which is what lets the
    /// codec treat it as fixed for the encoder's life.
    ///
    /// # Errors
    ///
    /// [`PipelineError::Record`] if the record cannot be framed or written.
    pub fn write_stream_config<W: Write>(
        &mut self,
        writer: &mut W,
        limits: &Limits,
    ) -> Result<(), PipelineError> {
        let geometry = self.encoder.geometry();
        let config = StreamConfig {
            width: geometry.width(),
            height: geometry.height(),
            tile_size: geometry.tile_size().as_pixels(),
            pixel_format: wire_format(geometry.pixel_format()) as i32,
        };

        write_record(
            writer,
            limits,
            self.generation,
            self.sequence,
            FrameRecord::StreamConfig,
            0,
            &config.encode_to_vec(),
        )?;
        self.sequence = self.sequence.wrapping_add(1);

        Ok(())
    }

    /// Writes the next record, if the encoder has one.
    ///
    /// `Ok(false)` means there was nothing staged, which is the ordinary answer
    /// on a socket that has caught up with the guest.
    ///
    /// # Errors
    ///
    /// [`PipelineError`] from the encoder or the transport.
    pub fn write_next<W: Write>(
        &mut self,
        writer: &mut W,
        limits: &Limits,
    ) -> Result<bool, PipelineError> {
        let Some(payload) = self.encoder.next_payload() else {
            return Ok(false);
        };

        let (kind, is_frame, bytes) = match payload {
            Payload::Keyframe(bytes) => (FrameRecord::Keyframe, true, bytes),
            Payload::TileDelta(bytes) => (FrameRecord::TileDelta, true, bytes),
            Payload::CursorImage(bytes) => (FrameRecord::CursorImage, false, bytes),
            Payload::CursorPosition(bytes) => (FrameRecord::CursorPosition, false, bytes),
        };

        // A delta is applied to the record the peer last received, so its base
        // is the sequence of that record. A keyframe depends on nothing.
        let base = if kind == FrameRecord::TileDelta {
            self.last_frame_sequence
        } else {
            0
        };
        let sequence = self.sequence;
        // Written straight out of the encoder's own buffer. A `Vec` made here
        // would be a whole keyframe copied to reach a socket that is about to
        // be handed the same bytes.
        write_record(writer, limits, self.generation, sequence, kind, base, bytes)?;
        self.sequence = self.sequence.wrapping_add(1);
        if is_frame {
            self.last_frame_sequence = sequence;
        }

        Ok(true)
    }
}

/// Frames one payload onto the frame channel.
///
/// A free function rather than a method: the payload it writes is borrowed
/// from the encoder, and the encoder is a field of the pipeline the caller
/// would otherwise be holding mutably at the same time.
fn write_record<W: Write>(
    writer: &mut W,
    limits: &Limits,
    generation: u32,
    sequence: u32,
    kind: FrameRecord,
    base: u32,
    payload: &[u8],
) -> Result<(), PipelineError> {
    let header = Header::for_payload(
        Channel::Frame,
        kind as u16,
        sequence,
        base,
        generation,
        payload,
    );
    record::write_payload(writer, &header, payload, limits)?;

    Ok(())
}

/// The codec's format as the wire spells it.
fn wire_format(format: PixelFormat) -> v1::PixelFormat {
    match format {
        PixelFormat::Bgra8888 => v1::PixelFormat::Bgra8888,
        PixelFormat::Xrgb8888 => v1::PixelFormat::Xrgb8888,
    }
}

#[cfg(test)]
mod tests {
    use vmlord_display_codec::{Geometry, PixelFormat, TileSize};
    use vmlord_display_protocol::{
        record::{self, Channel, Limits},
        v1::FrameRecord,
    };

    use super::{Pipeline, Rect};

    fn geometry() -> Geometry {
        Geometry::new(64, 64, TileSize::ThirtyTwo, PixelFormat::Xrgb8888).unwrap()
    }

    fn read_all(bytes: &[u8], limits: &Limits) -> Vec<(u16, u32, u32)> {
        let mut reader = bytes;
        let mut payload = Vec::new();
        let mut records = Vec::new();
        while !reader.is_empty() {
            let header = record::read(&mut reader, limits, &mut payload).unwrap();
            records.push((header.message_type, header.sequence, header.base));
        }
        records
    }

    #[test]
    fn a_new_socket_gets_a_stream_config_and_then_a_keyframe() {
        let limits = Limits::new(64, 64);
        let mut pipeline = Pipeline::new(geometry(), 1, true);
        let mut bytes = Vec::new();

        pipeline.write_stream_config(&mut bytes, &limits).unwrap();
        pipeline.submit_frame(&frame(0x11)).unwrap();
        assert!(pipeline.write_next(&mut bytes, &limits).unwrap());

        let records = read_all(&bytes, &limits);
        assert_eq!(records[0].0, FrameRecord::StreamConfig as u16);
        assert_eq!(records[1].0, FrameRecord::Keyframe as u16);
        assert_eq!(
            (records[0].1, records[1].1),
            (0, 1),
            "sequences run from zero on the socket, not on the session"
        );
    }

    #[test]
    fn a_delta_names_the_record_it_was_built_on() {
        let limits = Limits::new(64, 64);
        let mut pipeline = Pipeline::new(geometry(), 1, true);
        let mut bytes = Vec::new();
        pipeline.write_stream_config(&mut bytes, &limits).unwrap();

        pipeline.submit_frame(&frame(0x11)).unwrap();
        pipeline.write_next(&mut bytes, &limits).unwrap();
        pipeline.submit_frame(&frame(0x22)).unwrap();
        pipeline.write_next(&mut bytes, &limits).unwrap();

        let records = read_all(&bytes, &limits);
        assert_eq!(records[2].0, FrameRecord::TileDelta as u16);
        assert_eq!(
            records[2].2, records[1].1,
            "the base is the sequence of the record that was sent, not of a frame that was captured"
        );
    }

    #[test]
    fn frames_captured_while_the_socket_was_busy_collapse_into_one_delta() {
        let limits = Limits::new(64, 64);
        let mut pipeline = Pipeline::new(geometry(), 1, true);
        let mut bytes = Vec::new();
        pipeline.write_stream_config(&mut bytes, &limits).unwrap();
        pipeline.submit_frame(&frame(0x11)).unwrap();
        pipeline.write_next(&mut bytes, &limits).unwrap();

        for shade in [0x22u8, 0x33, 0x44] {
            pipeline.submit_frame(&frame(shade)).unwrap();
        }
        bytes.clear();
        assert!(pipeline.write_next(&mut bytes, &limits).unwrap());
        assert!(
            !pipeline.write_next(&mut bytes, &limits).unwrap(),
            "a slow socket gets one current delta, not a backlog of stale ones"
        );
    }

    #[test]
    fn a_requested_keyframe_is_the_next_record_even_with_nothing_new_captured() {
        let limits = Limits::new(64, 64);
        let mut pipeline = Pipeline::new(geometry(), 1, true);
        let mut bytes = Vec::new();
        pipeline.write_stream_config(&mut bytes, &limits).unwrap();
        pipeline.submit_frame(&frame(0x11)).unwrap();
        pipeline.write_next(&mut bytes, &limits).unwrap();

        bytes.clear();
        pipeline.request_keyframe();
        pipeline.write_next(&mut bytes, &limits).unwrap();

        assert_eq!(read_all(&bytes, &limits)[0].0, FrameRecord::Keyframe as u16);
    }

    #[test]
    fn every_record_carries_the_generation_the_socket_was_bound_at() {
        let limits = Limits::new(64, 64);
        let mut pipeline = Pipeline::new(geometry(), 7, true);
        let mut bytes = Vec::new();
        pipeline.write_stream_config(&mut bytes, &limits).unwrap();

        let mut reader = bytes.as_slice();
        let mut payload = Vec::new();
        let header = record::read(&mut reader, &limits, &mut payload).unwrap();
        assert_eq!(header.generation, 7);
        assert_eq!(header.channel, Channel::Frame);
    }

    #[test]
    fn without_the_cursor_stream_capability_no_cursor_records_are_written() {
        let limits = Limits::new(64, 64);
        let mut pipeline = Pipeline::new(geometry(), 1, false);
        let mut bytes = Vec::new();
        pipeline.write_stream_config(&mut bytes, &limits).unwrap();
        pipeline.submit_frame(&frame(0x11)).unwrap();
        pipeline
            .submit_cursor(
                Some((&[0xff; 4 * 4 * 4], 4, 4)),
                &crate::cursor::place(1, 1, 4, 4, 64, 64),
            )
            .unwrap();

        bytes.clear();
        while pipeline.write_next(&mut bytes, &limits).unwrap() {}

        assert!(
            read_all(&bytes, &limits)
                .iter()
                .all(|(kind, _, _)| *kind != FrameRecord::CursorImage as u16),
            "a peer that declined the capability gets the cursor drawn into the frame instead"
        );
    }

    #[test]
    fn a_declined_cursor_stream_puts_the_pointer_in_the_frame_the_peer_decodes() {
        // What the capability actually costs the peer, checked where it can be
        // seen: the decoded frame. Every other test here reads record kinds,
        // which a composite that silently stopped happening would still pass.
        let limits = Limits::new(64, 64);
        let mut pipeline = Pipeline::new(geometry(), 1, false);
        let mut bytes = Vec::new();
        pipeline.write_stream_config(&mut bytes, &limits).unwrap();

        // Opaque red over a frame of one shade, with the alpha last, as a
        // cursor plane carries it.
        let cursor = [0x00u8, 0x00, 0xff, 0xff].repeat(4 * 4);
        pipeline
            .submit_cursor(
                Some((&cursor, 4, 4)),
                &crate::cursor::place(8, 8, 4, 4, 64, 64),
            )
            .unwrap();
        pipeline.submit_frame(&frame(0x11)).unwrap();

        bytes.clear();
        while pipeline.write_next(&mut bytes, &limits).unwrap() {}

        let mut reader = bytes.as_slice();
        let mut payload = Vec::new();
        let header = record::read(&mut reader, &limits, &mut payload).unwrap();
        assert_eq!(header.message_type, FrameRecord::Keyframe as u16);

        let mut decoder = vmlord_display_codec::Decoder::new(geometry());
        decoder.apply_keyframe(&payload).unwrap();
        let decoded = decoder.frame();

        let pixel = |x: usize, y: usize| &decoded[(y * 64 + x) * 4..(y * 64 + x) * 4 + 4];
        assert_eq!(
            pixel(8, 8),
            [0x00, 0x00, 0xff, 0xff],
            "the pointer is drawn where the plane put it"
        );
        assert_eq!(
            pixel(11, 11),
            [0x00, 0x00, 0xff, 0xff],
            "and through to its far corner"
        );
        assert_eq!(
            pixel(12, 8),
            [0x11, 0x11, 0x11, 0x11],
            "the pixel past its edge is the frame's own"
        );
        assert_eq!(pixel(0, 0), [0x11, 0x11, 0x11, 0x11]);
    }

    #[test]
    fn a_transparent_cursor_leaves_the_frame_exactly_as_captured() {
        // The blend's other end: a bitmap of nothing must not tint the desktop
        // it was drawn over.
        let limits = Limits::new(64, 64);
        let mut drawn = Pipeline::new(geometry(), 1, false);
        let mut streamed = Pipeline::new(geometry(), 1, true);
        let (mut with_cursor, mut without) = (Vec::new(), Vec::new());

        drawn
            .submit_cursor(
                Some((&[0u8; 4 * 4 * 4], 4, 4)),
                &crate::cursor::place(8, 8, 4, 4, 64, 64),
            )
            .unwrap();
        drawn.submit_frame(&frame(0x11)).unwrap();
        streamed.submit_frame(&frame(0x11)).unwrap();
        while drawn.write_next(&mut with_cursor, &limits).unwrap() {}
        while streamed.write_next(&mut without, &limits).unwrap() {}

        assert_eq!(
            with_cursor, without,
            "a fully transparent cursor changes no byte of the frame"
        );
    }

    #[test]
    fn a_pointer_that_moved_leaves_no_trail_when_the_compositor_reported_no_damage() {
        // The composite writes pixels the primary plane's damage has never
        // heard of: the cursor is on its own plane, so a compositor that
        // repainted nothing reports nothing, and a delta that believed it
        // would leave the old pointer on the viewer's screen until the
        // protective keyframe came round.
        let limits = Limits::new(64, 64);
        let mut pipeline = Pipeline::new(geometry(), 1, false);
        let mut bytes = Vec::new();
        pipeline.write_stream_config(&mut bytes, &limits).unwrap();

        let cursor = [0x00u8, 0x00, 0xff, 0xff].repeat(4 * 4);
        let place = |x, y| crate::cursor::place(x, y, 4, 4, 64, 64);

        pipeline
            .submit_cursor(Some((&cursor, 4, 4)), &place(8, 8))
            .unwrap();
        pipeline.submit_frame(&damaged(0x11, Some(&[]))).unwrap();
        bytes.clear();
        while pipeline.write_next(&mut bytes, &limits).unwrap() {}

        let mut decoder = vmlord_display_codec::Decoder::new(geometry());
        apply(&mut decoder, &bytes, &limits);

        pipeline
            .submit_cursor(Some((&cursor, 4, 4)), &place(40, 40))
            .unwrap();
        pipeline.submit_frame(&damaged(0x11, Some(&[]))).unwrap();
        bytes.clear();
        while pipeline.write_next(&mut bytes, &limits).unwrap() {}
        apply(&mut decoder, &bytes, &limits);

        let decoded = decoder.frame();
        let pixel = |x: usize, y: usize| &decoded[(y * 64 + x) * 4..(y * 64 + x) * 4 + 4];
        assert_eq!(
            pixel(40, 40),
            [0x00, 0x00, 0xff, 0xff],
            "the pointer is drawn where it moved to"
        );
        assert_eq!(
            pixel(8, 8),
            [0x11, 0x11, 0x11, 0x11],
            "and the desktop is back where it left"
        );
    }

    #[test]
    fn damage_the_compositor_reported_reaches_the_encoder() {
        // Damage that names one tile and a frame that changed everywhere: what
        // arrives is what was named, which is the whole point of trusting it.
        let limits = Limits::new(64, 64);
        let mut pipeline = Pipeline::new(geometry(), 1, true);
        let mut bytes = Vec::new();
        pipeline.write_stream_config(&mut bytes, &limits).unwrap();
        pipeline.submit_frame(&damaged(0x11, None)).unwrap();
        bytes.clear();
        while pipeline.write_next(&mut bytes, &limits).unwrap() {}

        let mut decoder = vmlord_display_codec::Decoder::new(geometry());
        apply(&mut decoder, &bytes, &limits);

        let corner = Rect {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        };
        pipeline
            .submit_frame(&damaged(0x22, Some(&[corner])))
            .unwrap();
        bytes.clear();
        while pipeline.write_next(&mut bytes, &limits).unwrap() {}
        apply(&mut decoder, &bytes, &limits);

        let decoded = decoder.frame();
        let pixel = |x: usize, y: usize| &decoded[(y * 64 + x) * 4..(y * 64 + x) * 4 + 4];
        assert_eq!(
            pixel(0, 0),
            [0x22, 0x22, 0x22, 0x22],
            "the named tile moved"
        );
        assert_eq!(
            pixel(63, 63),
            [0x11, 0x11, 0x11, 0x11],
            "and a tile the compositor did not name was not looked at"
        );
    }

    /// Applies every frame record in `bytes` to a decoder.
    fn apply(decoder: &mut vmlord_display_codec::Decoder, bytes: &[u8], limits: &Limits) {
        let mut reader = bytes;
        let mut payload = Vec::new();
        while let Ok(header) = record::read(&mut reader, limits, &mut payload) {
            if header.message_type == FrameRecord::Keyframe as u16 {
                decoder.apply_keyframe(&payload).unwrap();
            } else if header.message_type == FrameRecord::TileDelta as u16 {
                decoder.apply_delta(&payload).unwrap();
            }
        }
    }

    /// A frame of one shade that says what changed in it.
    fn damaged(shade: u8, damage: Option<&[Rect]>) -> crate::capture::CapturedFrame {
        let mut captured = frame(shade);
        captured.damage = damage.map(<[Rect]>::to_vec);
        captured
    }

    fn frame(shade: u8) -> crate::capture::CapturedFrame {
        // A frame that owns its pixels: the pipeline's tests have no DRM, and
        // what they exercise is everything above the mapping.
        crate::capture::CapturedFrame::from_pixels(
            0,
            64,
            64,
            64 * 4,
            vmlord_display_codec::PixelFormat::Xrgb8888,
            vec![shade; 64 * 64 * 4],
        )
    }
}
