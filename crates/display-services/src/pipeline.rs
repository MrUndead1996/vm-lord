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
    PixelFormat,
};
use vmlord_display_protocol::{
    record::{self, Channel, Limits, Record, RecordError},
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
    /// The bitmap, four bytes per pixel.
    pixels: Vec<u32>,
    /// Its width, which is what turns an index into a row.
    width: u32,
    /// Where it lands, already cropped to the frame.
    placement: Placement,
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
    composite: Vec<u32>,
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
        let drawn = if self.cursor_stream {
            None
        } else {
            self.drawn_cursor.as_ref()
        };

        let Some(cursor) = drawn else {
            return frame.read(|pixels| self.encoder.submit(Frame { pixels, stride }, None));
        };

        // The composite needs words rather than bytes, and it must not write
        // the captured buffer: it is a read-only mapping of the guest's own
        // scanout.
        let words = stride / 4 * frame.height as usize;
        self.composite.clear();
        self.composite.reserve(words);
        frame.read(|pixels| {
            self.composite.extend(
                pixels
                    .chunks_exact(4)
                    .take(words)
                    .map(|word| u32::from_ne_bytes([word[0], word[1], word[2], word[3]])),
            );
        });

        cursor::composite(
            &mut self.composite,
            frame.stride / 4,
            &cursor.pixels,
            cursor.width,
            &cursor.placement,
        );

        let bytes: Vec<u8> = self
            .composite
            .iter()
            .flat_map(|word| word.to_ne_bytes())
            .collect();

        self.encoder.submit(
            Frame {
                pixels: &bytes,
                stride,
            },
            None,
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
                    // Task #114's module does not set DRIVER_CURSOR_HOTSPOT, so
                    // there is no hotspot to read and none to send: mutter puts
                    // the plane where the image is drawn.
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
                self.drawn_cursor = Some(DrawnCursor {
                    pixels: pixels
                        .chunks_exact(4)
                        .map(|word| u32::from_ne_bytes([word[0], word[1], word[2], word[3]]))
                        .collect(),
                    width,
                    placement: *placement,
                });
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

        self.write(
            writer,
            limits,
            FrameRecord::StreamConfig,
            0,
            config.encode_to_vec(),
        )
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
            Payload::Keyframe(bytes) => (FrameRecord::Keyframe, true, bytes.to_vec()),
            Payload::TileDelta(bytes) => (FrameRecord::TileDelta, true, bytes.to_vec()),
            Payload::CursorImage(bytes) => (FrameRecord::CursorImage, false, bytes.to_vec()),
            Payload::CursorPosition(bytes) => (FrameRecord::CursorPosition, false, bytes.to_vec()),
        };

        // A delta is applied to the record the peer last received, so its base
        // is the sequence of that record. A keyframe depends on nothing.
        let base = if kind == FrameRecord::TileDelta {
            self.last_frame_sequence
        } else {
            0
        };
        let sequence = self.sequence;
        self.write(writer, limits, kind, base, bytes)?;
        if is_frame {
            self.last_frame_sequence = sequence;
        }

        Ok(true)
    }

    /// One record, at the socket's own sequence and generation.
    fn write<W: Write>(
        &mut self,
        writer: &mut W,
        limits: &Limits,
        kind: FrameRecord,
        base: u32,
        payload: Vec<u8>,
    ) -> Result<(), PipelineError> {
        let record = Record::new(
            Channel::Frame,
            kind as u16,
            self.sequence,
            base,
            self.generation,
            payload,
        );
        record::write(writer, &record, limits)?;
        self.sequence = self.sequence.wrapping_add(1);

        Ok(())
    }
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

    use super::Pipeline;

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
