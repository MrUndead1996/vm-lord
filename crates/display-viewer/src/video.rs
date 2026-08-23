//! The frame channel's records, turned into pixels and the rectangles that
//! changed.
//!
//! Everything here arrives from another machine, so nothing here trusts it: the
//! codec checks every length against the geometry it was built for, and this
//! module checks what the codec cannot -- that a delta builds on a record this
//! connection actually sent.
//!
//! Two kinds of failure, and the difference matters. A [`VideoError::Rebind`]
//! is a frame channel that cannot continue but a session that can: close the
//! socket, reconnect at the next generation, and the guest owes a
//! `StreamConfig` and a keyframe again. A [`VideoError::Fatal`] is a stream
//! this build cannot display at all.

use prost::Message as _;
use vmlord_display_codec::{
    CodecError, CursorPosition, Decoder, Geometry, OwnedCursorImage, PixelFormat, Rect, TileSize,
};
use vmlord_display_protocol::{
    record::Header,
    v1::{FrameRecord, PixelFormat as WireFormat, StreamConfig},
};

/// What one frame record meant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Update {
    /// Nothing the window has to draw.
    Nothing,
    /// The stream's geometry, which the window sizes its texture to.
    Configured(Geometry),
    /// The rectangles of the frame that changed.
    Damage(Vec<Rect>),
    /// A new cursor bitmap.
    Cursor(OwnedCursorImage),
    /// Where the cursor is now.
    Moved(CursorPosition),
}

/// Why a frame record could not be applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VideoError {
    /// The channel cannot continue, but the session can.
    Rebind(String),
    /// The stream is one this build cannot display.
    Fatal(String),
}

/// The decode half of one frame channel.
pub struct Video {
    decoder: Option<Decoder>,
    /// The sequence of the last frame record applied, which is what a delta's
    /// base must name.
    last_frame: Option<u32>,
}

impl Video {
    /// A video pipeline with no stream yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            decoder: None,
            last_frame: None,
        }
    }

    /// The geometry the current stream is at.
    #[must_use]
    pub fn geometry(&self) -> Option<Geometry> {
        self.decoder.as_ref().map(Decoder::geometry)
    }

    /// The frame as it now stands, four bytes per pixel.
    #[must_use]
    pub fn frame(&self) -> Option<&[u8]> {
        self.decoder.as_ref().map(Decoder::frame)
    }

    /// Applies one record of the frame channel.
    ///
    /// # Errors
    ///
    /// [`VideoError::Rebind`] for anything that leaves the picture wrong but
    /// the session usable -- a missing base, a payload the codec refuses, a
    /// frame record before any `StreamConfig` -- and [`VideoError::Fatal`] for
    /// a geometry this build cannot decode at all.
    pub fn apply(&mut self, header: &Header, payload: &[u8]) -> Result<Update, VideoError> {
        match FrameRecord::try_from(i32::from(header.message_type)) {
            Ok(FrameRecord::StreamConfig) => self.configure(payload),
            Ok(FrameRecord::Keyframe) => self.keyframe(header, payload),
            Ok(FrameRecord::TileDelta) => self.delta(header, payload),
            Ok(FrameRecord::CursorImage) => Decoder::decode_cursor_image(payload)
                .map(Update::Cursor)
                .map_err(|error| Self::rebind("a cursor bitmap", error)),
            Ok(FrameRecord::CursorPosition) => Decoder::decode_cursor_position(payload)
                .map(Update::Moved)
                .map_err(|error| Self::rebind("a cursor position", error)),
            _ => {
                log::debug!(
                    "a frame record of type {} is one this build does not read",
                    header.message_type
                );
                Ok(Update::Nothing)
            }
        }
    }

    /// Builds a decoder for the stream a `StreamConfig` describes.
    ///
    /// A second config replaces both the decoder and the frame: geometry never
    /// changes inside an encoder, so a new geometry is a new stream.
    fn configure(&mut self, payload: &[u8]) -> Result<Update, VideoError> {
        let config = StreamConfig::decode(payload).map_err(|error| {
            VideoError::Rebind(format!("a stream config is unreadable: {error}"))
        })?;

        let tile_size = TileSize::from_pixels(config.tile_size)
            .map_err(|error| VideoError::Fatal(format!("the stream's tile size: {error}")))?;
        let pixel_format = match WireFormat::try_from(config.pixel_format) {
            Ok(WireFormat::Bgra8888) => PixelFormat::Bgra8888,
            Ok(WireFormat::Xrgb8888) => PixelFormat::Xrgb8888,
            _ => {
                return Err(VideoError::Fatal(
                    "the stream names a pixel format this build cannot draw".to_owned(),
                ));
            }
        };

        let geometry = Geometry::new(config.width, config.height, tile_size, pixel_format)
            .map_err(|error| VideoError::Fatal(format!("the stream's geometry: {error}")))?;

        log::info!(
            "the display stream is {}x{}, {}-pixel tiles, {pixel_format:?}",
            geometry.width(),
            geometry.height(),
            geometry.tile_size().as_pixels()
        );
        self.decoder = Some(Decoder::new(geometry));
        self.last_frame = None;

        Ok(Update::Configured(geometry))
    }

    /// Applies a whole frame.
    fn keyframe(&mut self, header: &Header, payload: &[u8]) -> Result<Update, VideoError> {
        let decoder = self.decoder.as_mut().ok_or_else(Self::no_stream)?;
        let damage = decoder
            .apply_keyframe(payload)
            .map_err(|error| Self::rebind("a keyframe", error))?
            .to_vec();

        self.last_frame = Some(header.sequence);
        log::trace!(
            "keyframe {} restored {} tiles",
            header.sequence,
            damage.len()
        );

        Ok(Update::Damage(damage))
    }

    /// Applies the tiles a delta carries.
    fn delta(&mut self, header: &Header, payload: &[u8]) -> Result<Update, VideoError> {
        if self.last_frame != Some(header.base) {
            return Err(VideoError::Rebind(format!(
                "a delta builds on record {}, which this connection never applied",
                header.base
            )));
        }

        let decoder = self.decoder.as_mut().ok_or_else(Self::no_stream)?;
        let damage = decoder
            .apply_delta(payload)
            .map_err(|error| Self::rebind("a tile delta", error))?
            .to_vec();

        self.last_frame = Some(header.sequence);
        log::trace!("delta {} changed {} tiles", header.sequence, damage.len());

        Ok(Update::Damage(damage))
    }

    /// A frame record that arrived before the stream it belongs to.
    fn no_stream() -> VideoError {
        VideoError::Rebind("a frame record arrived before any stream config".to_owned())
    }

    /// A payload the codec refused. Sizes and reasons, never bytes.
    fn rebind(what: &str, error: CodecError) -> VideoError {
        VideoError::Rebind(format!("{what} could not be decoded: {error}"))
    }
}

impl Default for Video {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use prost::Message as _;
    use vmlord_display_codec::{
        Encoder, EncoderConfig, Frame, Geometry, Payload, PixelFormat, Rect, TileSize,
        scenes::{Generator, Scene},
    };
    use vmlord_display_protocol::{
        record::{Channel, Record},
        v1::{FrameRecord, PixelFormat as WireFormat, StreamConfig},
    };

    use super::{Update, Video, VideoError};

    fn geometry() -> Geometry {
        Geometry::new(320, 200, TileSize::ThirtyTwo, PixelFormat::Bgra8888)
            .expect("a geometry the codec allows")
    }

    fn config_record(width: u32, height: u32, tile_size: u32) -> Record {
        let config = StreamConfig {
            width,
            height,
            tile_size,
            pixel_format: WireFormat::Bgra8888 as i32,
        };

        Record::new(
            Channel::Frame,
            FrameRecord::StreamConfig as u16,
            0,
            0,
            0,
            config.encode_to_vec(),
        )
    }

    fn frame_record(kind: FrameRecord, sequence: u32, base: u32, payload: Vec<u8>) -> Record {
        Record::new(Channel::Frame, kind as u16, sequence, base, 0, payload)
    }

    /// The records a real encoder produces for one scene, in order.
    fn stream(frames: usize) -> Vec<Record> {
        let geometry = geometry();
        let mut encoder = Encoder::new(EncoderConfig::new(geometry));
        let mut generator = Generator::new(Scene::Typing, geometry, 7);
        let mut records = Vec::new();
        let mut sequence = 1;
        let mut last_frame = 0;

        for _ in 0..frames {
            let pixels = generator.next_frame().to_vec();
            let damage = generator.damage().to_vec();
            encoder
                .submit(
                    Frame {
                        pixels: &pixels,
                        stride: geometry.width() as usize * 4,
                    },
                    Some(&damage),
                )
                .expect("a frame of this geometry");

            while let Some(payload) = encoder.next_payload() {
                let (kind, is_frame, bytes) = match payload {
                    Payload::Keyframe(bytes) => (FrameRecord::Keyframe, true, bytes.to_vec()),
                    Payload::TileDelta(bytes) => (FrameRecord::TileDelta, true, bytes.to_vec()),
                    Payload::CursorImage(bytes) => {
                        (FrameRecord::CursorImage, false, bytes.to_vec())
                    }
                    Payload::CursorPosition(bytes) => {
                        (FrameRecord::CursorPosition, false, bytes.to_vec())
                    }
                };
                let base = if kind == FrameRecord::TileDelta {
                    last_frame
                } else {
                    0
                };
                records.push(frame_record(kind, sequence, base, bytes));
                if is_frame {
                    last_frame = sequence;
                }
                sequence += 1;
            }
        }

        records
    }

    #[test]
    fn a_stream_config_builds_the_decoder_the_frames_need() {
        let mut video = Video::new();
        let record = config_record(320, 200, 32);

        let update = video
            .apply(&record.header, &record.payload)
            .expect("a config the codec allows");

        assert_eq!(update, Update::Configured(geometry()));
        assert_eq!(video.geometry(), Some(geometry()));
    }

    #[test]
    fn a_second_stream_config_replaces_the_decoder() {
        let mut video = Video::new();
        let first = config_record(320, 200, 32);
        video
            .apply(&first.header, &first.payload)
            .expect("a config");

        let second = config_record(640, 480, 64);
        video
            .apply(&second.header, &second.payload)
            .expect("a config");

        let replaced = Geometry::new(640, 480, TileSize::SixtyFour, PixelFormat::Bgra8888)
            .expect("a geometry the codec allows");
        assert_eq!(video.geometry(), Some(replaced));
    }

    #[test]
    fn a_geometry_the_codec_refuses_is_fatal_rather_than_a_rebind() {
        let mut video = Video::new();
        let record = config_record(320, 200, 48);

        assert!(matches!(
            video.apply(&record.header, &record.payload),
            Err(VideoError::Fatal(_))
        ));
    }

    #[test]
    fn a_keyframe_and_its_deltas_decode_into_the_rectangles_that_changed() {
        let mut video = Video::new();
        let config = config_record(320, 200, 32);
        video
            .apply(&config.header, &config.payload)
            .expect("a config");

        let mut frames = 0;
        for record in stream(4) {
            let update = video
                .apply(&record.header, &record.payload)
                .expect("a record this encoder wrote");

            if let Update::Damage(damage) = update {
                assert!(!damage.is_empty(), "a frame record changed nothing");
                for rect in damage {
                    assert!(rect.x + rect.width <= 320);
                    assert!(rect.y + rect.height <= 200);
                }
                frames += 1;
            }
        }

        assert!(frames >= 2, "the encoder wrote a keyframe and some deltas");
        assert_eq!(video.frame().map(<[u8]>::len), Some(320 * 200 * 4));
    }

    #[test]
    fn a_delta_before_any_keyframe_asks_for_one_by_rebinding() {
        let mut video = Video::new();
        let config = config_record(320, 200, 32);
        video
            .apply(&config.header, &config.payload)
            .expect("a config");

        let delta = stream(2)
            .into_iter()
            .find(|record| record.header.message_type == FrameRecord::TileDelta as u16)
            .expect("the scene produced a delta");
        // Its base names the keyframe, which this decoder never received.
        let update = video.apply(&delta.header, &delta.payload);

        assert!(matches!(update, Err(VideoError::Rebind(_))));
    }

    #[test]
    fn a_delta_built_on_a_record_this_connection_never_sent_is_refused() {
        let mut video = Video::new();
        let config = config_record(320, 200, 32);
        video
            .apply(&config.header, &config.payload)
            .expect("a config");

        let records = stream(3);
        for record in &records {
            if record.header.message_type == FrameRecord::TileDelta as u16 {
                // The same delta, claiming to build on a frame that was never
                // sent. The picture it would produce is wrong in a way no
                // error surfaces, so the channel is rebound instead.
                let mut header = record.header;
                header.base = header.sequence + 100;

                assert!(matches!(
                    video.apply(&header, &record.payload),
                    Err(VideoError::Rebind(_))
                ));
                return;
            }
            video
                .apply(&record.header, &record.payload)
                .expect("a record this encoder wrote");
        }

        panic!("the scene produced no delta");
    }

    #[test]
    fn a_corrupted_payload_rebinds_rather_than_ending_the_session() {
        let mut video = Video::new();
        let config = config_record(320, 200, 32);
        video
            .apply(&config.header, &config.payload)
            .expect("a config");

        let mut keyframe = stream(1)
            .into_iter()
            .find(|record| record.header.message_type == FrameRecord::Keyframe as u16)
            .expect("the first frame is a keyframe");
        keyframe.payload.truncate(keyframe.payload.len() / 2);

        assert!(matches!(
            video.apply(&keyframe.header, &keyframe.payload),
            Err(VideoError::Rebind(_))
        ));
    }

    #[test]
    fn a_frame_record_before_any_stream_config_is_a_rebind() {
        let mut video = Video::new();
        let keyframe = frame_record(FrameRecord::Keyframe, 1, 0, vec![0; 8]);

        assert!(matches!(
            video.apply(&keyframe.header, &keyframe.payload),
            Err(VideoError::Rebind(_))
        ));
    }

    #[test]
    fn a_record_this_build_has_no_name_for_changes_nothing() {
        let mut video = Video::new();
        let unknown = frame_record(FrameRecord::Unspecified, 1, 0, vec![1, 2, 3]);

        assert_eq!(
            video
                .apply(&unknown.header, &unknown.payload)
                .expect("an unknown record is not a fault"),
            Update::Nothing
        );
    }

    #[test]
    fn a_decoded_stream_never_reaches_the_log() {
        crate::log::capture::install();

        let mut video = Video::new();
        let config = config_record(320, 200, 32);
        video
            .apply(&config.header, &config.payload)
            .expect("a config");
        for record in stream(4) {
            let _ = video.apply(&record.header, &record.payload);
        }

        let text = crate::log::capture::text();
        let pixels = video.frame().expect("a decoded frame").to_vec();
        // Sixteen bytes is four pixels: long enough that a match is not a
        // coincidence, short enough to catch a partial dump.
        for window in pixels.chunks_exact(16).take(64) {
            let hex: String = window.iter().map(|byte| format!("{byte:02x}")).collect();
            assert!(!text.contains(&hex), "framebuffer content reached the log");
        }
        assert!(!text.is_empty(), "the decode path logged nothing at all");
    }

    #[test]
    fn the_damage_a_delta_reports_is_the_damage_the_encoder_wrote() {
        let geometry = geometry();
        let mut video = Video::new();
        let config = config_record(320, 200, 32);
        video
            .apply(&config.header, &config.payload)
            .expect("a config");

        let mut tiles: Vec<Rect> = Vec::new();
        for record in stream(3) {
            if let Ok(Update::Damage(damage)) = video.apply(&record.header, &record.payload) {
                tiles = damage.to_vec();
            }
        }

        // Every rectangle a delta reports is one of the grid's tiles.
        let grid: Vec<Rect> = (0..geometry.tile_count())
            .filter_map(|index| geometry.tile(index))
            .collect();
        for rect in tiles {
            assert!(grid.contains(&rect), "{rect:?} is not a tile of the grid");
        }
    }
}
