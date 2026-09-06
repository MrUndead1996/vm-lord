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
    CodecError, CursorPosition, Decoder, Geometry, MAX_CURSOR_DIMENSION, OwnedCursorImage,
    PixelFormat, Rect, TileSize,
};
use vmlord_display_protocol::{
    record::Header,
    v1::{self, CursorHotspots, FrameRecord, PixelFormat as WireFormat, StreamConfig},
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
    /// The guest's cursor-hotspot table, parsed out of its Xcursor theme.
    ///
    /// Arrives only whole: the parts of one table are accumulated until a
    /// record ends it, and the table is then handed up in one piece.
    CursorHotspots(Vec<OwnedCursorImage>),
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
    /// The parts of a cursor-hotspot table received so far.
    ///
    /// Held until a part arrives with `last_part` set, which is when the
    /// table is whole and handed up. A rebuilt channel starts from a new
    /// `Video`, so a table split across a rebind is dropped rather than
    /// misassembled -- and the guest owes the new socket the table again.
    hotspot_parts: Vec<OwnedCursorImage>,
}

impl Video {
    /// A video pipeline with no stream yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            decoder: None,
            last_frame: None,
            hotspot_parts: Vec::new(),
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
            Ok(FrameRecord::CursorHotspots) => self.cursor_hotspots(payload),
            _ => {
                tracing::debug!(
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

        tracing::info!(
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
        tracing::trace!(
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
        tracing::trace!("delta {} changed {} tiles", header.sequence, damage.len());

        Ok(Update::Damage(damage))
    }

    /// Extends the accumulating hotspot table with one part, and hands the
    /// table up once a part ends it.
    ///
    /// A table that never ends -- its last record lost with a socket that
    /// died -- stays unfinished until the rebind clears it, and the guest
    /// owes the replacement socket the whole table again.
    fn cursor_hotspots(&mut self, payload: &[u8]) -> Result<Update, VideoError> {
        let message = CursorHotspots::decode(payload).map_err(|error| {
            VideoError::Rebind(format!("a cursor-hotspot record is unreadable: {error}"))
        })?;
        for entry in message.entries {
            match hotspot_entry(entry) {
                Some(image) => self.hotspot_parts.push(image),
                // An entry this build would not anchor is dropped, not
                // fatal: the rest of the table still anchors its shapes.
                None => tracing::debug!("a cursor-hotspot entry this build does not anchor"),
            }
        }

        if message.last_part {
            return Ok(Update::CursorHotspots(std::mem::take(
                &mut self.hotspot_parts,
            )));
        }

        Ok(Update::Nothing)
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

/// One entry of a hotspot table, as the codec holds a cursor.
///
/// The same rules a cursor image record is held to: a bitmap of the size it
/// names, within the codec's dimension cap, and a hotspot inside it. An
/// entry outside them is `None`, and nothing upstream sends one on purpose.
fn hotspot_entry(entry: v1::HotspotEntry) -> Option<OwnedCursorImage> {
    let (width, height) = (entry.width, entry.height);
    let expected = width.checked_mul(height)?.checked_mul(4)?;
    if width == 0
        || height == 0
        || width > MAX_CURSOR_DIMENSION
        || height > MAX_CURSOR_DIMENSION
        || entry.pixels.len() != expected as usize
        || entry.hotspot_x >= width
        || entry.hotspot_y >= height
    {
        return None;
    }

    Some(OwnedCursorImage {
        pixels: entry.pixels,
        width,
        height,
        hotspot_x: entry.hotspot_x,
        hotspot_y: entry.hotspot_y,
    })
}

/// A cursor bitmap as an alpha icon wants it: BGRA, premultiplied.
///
/// The codec hands over straight alpha, and `CreateIconIndirect` composites
/// premultiplied. Doing the multiply here rather than in the renderer keeps it
/// where it can be tested without a device -- and keeps a bitmap that does not
/// match its own dimensions from being read past: the output is always
/// `width * height * 4` bytes, and a short input is padded with transparency.
#[must_use]
pub fn premultiplied(image: &OwnedCursorImage) -> Vec<u8> {
    let pixels = image.width as usize * image.height as usize;
    let mut out = vec![0u8; pixels * 4];

    for (index, chunk) in image.pixels.chunks_exact(4).take(pixels).enumerate() {
        let alpha = u32::from(chunk[3]);
        let target = &mut out[index * 4..index * 4 + 4];
        for channel in 0..3 {
            target[channel] = u8::try_from(u32::from(chunk[channel]) * alpha / 255)
                .expect("a product of two bytes divided by 255");
        }
        target[3] = chunk[3];
    }

    out
}

#[cfg(test)]
mod tests {
    use prost::Message as _;
    use vmlord_display_codec::{
        Encoder, EncoderConfig, Frame, Geometry, OwnedCursorImage, Payload, PixelFormat, Rect,
        TileSize,
        scenes::{Generator, Scene},
    };
    use vmlord_display_protocol::{
        record::{Channel, Record},
        v1::{CursorHotspots, FrameRecord, HotspotEntry, PixelFormat as WireFormat, StreamConfig},
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
        let (video, text) = crate::log::capture::capture(|| {
            let mut video = Video::new();
            let config = config_record(320, 200, 32);
            video
                .apply(&config.header, &config.payload)
                .expect("a config");
            for record in stream(4) {
                let _ = video.apply(&record.header, &record.payload);
            }
            video
        });
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

    #[test]
    fn a_cursor_bitmap_is_premultiplied_without_reading_past_its_rows() {
        let image = OwnedCursorImage {
            // Two pixels: opaque white, then half-transparent white.
            pixels: vec![255, 255, 255, 255, 255, 255, 255, 128],
            width: 2,
            height: 1,
            hotspot_x: 0,
            hotspot_y: 0,
        };

        let bytes = super::premultiplied(&image);

        assert_eq!(bytes.len(), 8);
        assert_eq!(&bytes[0..4], &[255, 255, 255, 255]);
        // 255 * 128 / 255 == 128, in every channel but alpha.
        assert_eq!(&bytes[4..8], &[128, 128, 128, 128]);
    }

    #[test]
    fn a_cursor_bitmap_whose_pixels_do_not_match_its_size_is_padded_rather_than_read_past() {
        let image = OwnedCursorImage {
            pixels: vec![255; 4],
            width: 4,
            height: 4,
            hotspot_x: 0,
            hotspot_y: 0,
        };

        assert_eq!(super::premultiplied(&image).len(), 4 * 4 * 4);
    }

    /// A cursor-hotspot record carrying the entries named by
    /// `(width, height, hotspot_x, hotspot_y)`.
    fn hotspots_record(entries: Vec<HotspotEntry>, last_part: bool) -> Record {
        Record::new(
            Channel::Frame,
            FrameRecord::CursorHotspots as u16,
            0,
            0,
            0,
            CursorHotspots { entries, last_part }.encode_to_vec(),
        )
    }

    fn proto_entry(width: u32, height: u32, hotspot_x: u32, hotspot_y: u32) -> HotspotEntry {
        HotspotEntry {
            pixels: vec![0xaa; (width * height * 4) as usize],
            width,
            height,
            hotspot_x,
            hotspot_y,
        }
    }

    fn owned_image(width: u32, height: u32, hotspot_x: u32, hotspot_y: u32) -> OwnedCursorImage {
        OwnedCursorImage {
            pixels: vec![0xaa; (width * height * 4) as usize],
            width,
            height,
            hotspot_x,
            hotspot_y,
        }
    }

    #[test]
    fn a_hotspot_table_is_handed_up_only_once_it_is_whole() {
        let mut video = Video::new();

        let first = hotspots_record(vec![proto_entry(8, 8, 1, 1)], false);
        assert_eq!(
            video
                .apply(&first.header, &first.payload)
                .expect("a part of a table"),
            Update::Nothing,
            "a table without its last part is held, not shown"
        );

        let last = hotspots_record(vec![proto_entry(16, 16, 2, 2)], true);
        assert_eq!(
            video
                .apply(&last.header, &last.payload)
                .expect("the end of a table"),
            Update::CursorHotspots(vec![owned_image(8, 8, 1, 1), owned_image(16, 16, 2, 2)])
        );
    }

    #[test]
    fn an_unanchorable_hotspot_entry_is_dropped_and_the_rest_is_kept() {
        let mut video = Video::new();

        // The second entry's hotspot sits on its far edge, where no cursor
        // could be anchored; the codec would refuse it, so it is dropped
        // here rather than poisoning the table.
        let record = hotspots_record(
            vec![
                proto_entry(8, 8, 0, 0),
                proto_entry(8, 8, 8, 4),
                proto_entry(8, 8, 4, 0),
            ],
            true,
        );

        assert_eq!(
            video
                .apply(&record.header, &record.payload)
                .expect("a table"),
            Update::CursorHotspots(vec![owned_image(8, 8, 0, 0), owned_image(8, 8, 4, 0)])
        );
    }

    #[test]
    fn an_unreadable_hotspot_record_asks_for_a_rebind_rather_than_a_crash() {
        let mut video = Video::new();
        let record = hotspots_record(Vec::new(), true);
        let truncated = &record.payload[..record.payload.len() / 2];

        assert!(matches!(
            video.apply(&record.header, truncated),
            Err(VideoError::Rebind(_))
        ));
    }

    #[test]
    fn a_new_video_holds_no_half_received_table() {
        // A rebound channel starts from a new `Video`; the guest owes the
        // replacement socket the whole table again, so what the old one
        // half-delivered must not be mistaken for a table.
        let mut video = Video::new();
        let first = hotspots_record(vec![proto_entry(8, 8, 1, 1)], false);
        video.apply(&first.header, &first.payload).expect("a part");

        let mut fresh = Video::new();
        let last = hotspots_record(vec![proto_entry(16, 16, 2, 2)], true);
        assert_eq!(
            fresh.apply(&last.header, &last.payload).expect("a table"),
            Update::CursorHotspots(vec![owned_image(16, 16, 2, 2)])
        );
        let _ = video;
    }
}
