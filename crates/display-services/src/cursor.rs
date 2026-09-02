//! Where the pointer is, and what to do with it.
//!
//! Task #114's module deliberately does not set `DRIVER_CURSOR_HOTSPOT`, so no
//! hotspot is readable -- and none is needed to draw the pointer: mutter has
//! already subtracted it, and the plane's `CRTC_X`/`CRTC_Y` are where the
//! bitmap's corner belongs. Those are signed and go negative at the left and
//! top edges -- which is the hotspot showing, and the reason an offscreen
//! cursor is cropped here rather than clamped and misdrawn there, the
//! protocol's coordinates being unsigned.
//!
//! Compositing wants a corner and gets one. A viewer that draws the cursor
//! itself wants the hotspot, and nothing in this guest has it; the host works
//! it out from the pointer positions it sent -- see the viewer's `cursor.rs`.

use vmlord_display_codec::Rect;

/// Where a cursor bitmap goes, and how much of it survives the frame's edges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    /// The left edge in frame coordinates, never negative.
    pub x: u32,
    /// The top edge in frame coordinates, never negative.
    pub y: u32,
    /// The part of the bitmap that is on the frame.
    pub crop: Rect,
    /// Whether any of it is.
    pub visible: bool,
}

/// Works out where a cursor plane lands and what of it shows.
#[must_use]
pub fn place(
    plane_x: i32,
    plane_y: i32,
    width: u32,
    height: u32,
    frame_width: u32,
    frame_height: u32,
) -> Placement {
    let (x, crop_x, visible_width) = axis(plane_x, width, frame_width);
    let (y, crop_y, visible_height) = axis(plane_y, height, frame_height);

    Placement {
        x,
        y,
        crop: Rect {
            x: crop_x,
            y: crop_y,
            width: visible_width,
            height: visible_height,
        },
        visible: visible_width > 0 && visible_height > 0,
    }
}

/// One axis of a placement: where it starts, what is cut off the near edge, and
/// what is left after the far edge cuts too.
fn axis(position: i32, size: u32, frame: u32) -> (u32, u32, u32) {
    let leading = u32::try_from(-position.min(0))
        .unwrap_or(u32::MAX)
        .min(size);
    let start = u32::try_from(position.max(0)).unwrap_or(u32::MAX);
    if start >= frame {
        return (start.min(frame), leading, 0);
    }

    let visible = (size - leading).min(frame - start);
    (start, leading, visible)
}

/// Draws a cursor bitmap over a frame, blending by its alpha channel.
///
/// Both are four bytes per pixel with the alpha last, which is what the plane
/// and the framebuffer already hold: `ARGB8888` and `BGRA8888` are DRM's names
/// for the channels' order in a word, and in memory the alpha is byte three of
/// either. Bytes rather than words because the frame reaches this from a
/// mapping and leaves it to the encoder, and both of those are bytes -- a
/// conversion on each side would be two whole-frame passes to blend a pointer.
///
/// The frame's alpha is left as it was: an `XRGB8888` framebuffer has none to
/// speak of and a viewer never reads it.
///
/// `frame_stride` is in bytes and `cursor_width` in pixels, as their sources
/// spell them. The loop's bounds come from `placement.crop`, which [`place`]
/// has already clipped to the frame, so no index here needs a guard of its own.
pub fn composite(
    frame: &mut [u8],
    frame_stride: u32,
    cursor: &[u8],
    cursor_width: u32,
    placement: &Placement,
) {
    if !placement.visible {
        return;
    }

    for row in 0..placement.crop.height {
        let source_row = ((placement.crop.y + row) * cursor_width + placement.crop.x) as usize * 4;
        let target_row =
            (placement.y + row) as usize * frame_stride as usize + placement.x as usize * 4;

        for column in 0..placement.crop.width as usize {
            let source_at = source_row + column * 4;
            let target_at = target_row + column * 4;
            let (Some(source), Some(target)) = (
                cursor.get(source_at..source_at + 4),
                frame.get(target_at..target_at + 4),
            ) else {
                return;
            };
            let blended = blend(
                [source[0], source[1], source[2], source[3]],
                [target[0], target[1], target[2], target[3]],
            );
            frame[target_at..target_at + 4].copy_from_slice(&blended);
        }
    }
}

/// One pixel of source over one pixel of destination, alpha last.
fn blend(source: [u8; 4], destination: [u8; 4]) -> [u8; 4] {
    let alpha = u32::from(source[3]);
    if alpha == 0 {
        return destination;
    }
    if alpha == 0xff {
        return source;
    }

    let inverse = 255 - alpha;
    let mut result = destination;
    for channel in 0..3 {
        let over = u32::from(source[channel]);
        let under = u32::from(destination[channel]);
        // Rounded rather than truncated: a cursor blended over a flat colour
        // repeatedly would otherwise drift darker with every frame.
        result[channel] = ((over * alpha + under * inverse + 127) / 255) as u8;
    }

    result
}

#[cfg(test)]
mod tests {
    use super::{composite, place};

    #[test]
    fn a_cursor_inside_the_frame_is_not_cropped() {
        let placement = place(100, 50, 64, 64, 1920, 1080);

        assert_eq!((placement.x, placement.y), (100, 50));
        assert_eq!(
            (placement.crop.x, placement.crop.y),
            (0, 0),
            "nothing is cut off a cursor the compositor placed inside the frame"
        );
        assert_eq!((placement.crop.width, placement.crop.height), (64, 64));
        assert!(placement.visible);
    }

    #[test]
    fn a_cursor_off_the_left_and_top_edges_is_cropped_and_clamped() {
        let placement = place(-30, -7, 64, 64, 1920, 1080);

        assert_eq!(
            (placement.x, placement.y),
            (0, 0),
            "the protocol carries unsigned coordinates, so the offscreen part is cut rather than sent"
        );
        assert_eq!((placement.crop.x, placement.crop.y), (30, 7));
        assert_eq!((placement.crop.width, placement.crop.height), (34, 57));
    }

    #[test]
    fn a_cursor_off_the_right_and_bottom_edges_keeps_only_what_shows() {
        let placement = place(1900, 1050, 64, 64, 1920, 1080);

        assert_eq!((placement.crop.width, placement.crop.height), (20, 30));
        assert!(placement.visible);
    }

    #[test]
    fn a_cursor_entirely_outside_the_frame_is_not_visible() {
        assert!(!place(-64, 0, 64, 64, 1920, 1080).visible);
        assert!(!place(1920, 0, 64, 64, 1920, 1080).visible);
        assert!(!place(0, -64, 64, 64, 1920, 1080).visible);
    }

    /// One pixel of a four-by-four frame, so an assertion can name a position
    /// rather than an offset.
    fn pixel(frame: &[u8], x: usize, y: usize) -> [u8; 4] {
        let at = y * 4 * 4 + x * 4;
        frame[at..at + 4].try_into().unwrap()
    }

    /// Opaque black, which is what the frames below are painted with.
    const BLACK: [u8; 4] = [0, 0, 0, 0xff];

    #[test]
    fn compositing_blends_by_alpha_and_leaves_the_rest_alone() {
        let mut frame = BLACK.repeat(4 * 4);
        // Opaque white, half-transparent white, and two fully transparent.
        let cursor = [
            [0xff, 0xff, 0xff, 0xff],
            [0xff, 0xff, 0xff, 0x80],
            [0, 0, 0, 0],
            [0, 0, 0, 0],
        ]
        .concat();
        let placement = place(1, 1, 2, 2, 4, 4);

        composite(&mut frame, 4 * 4, &cursor, 2, &placement);

        assert_eq!(
            pixel(&frame, 1, 1),
            [0xff, 0xff, 0xff, 0xff],
            "an opaque cursor pixel wins"
        );
        assert_eq!(
            pixel(&frame, 2, 1)[0],
            0x80,
            "a half-transparent pixel lands halfway between the two"
        );
        assert_eq!(
            pixel(&frame, 2, 1)[3],
            0xff,
            "and the frame keeps the alpha it had"
        );
        assert_eq!(
            pixel(&frame, 1, 2),
            BLACK,
            "a transparent pixel changes nothing"
        );
        assert_eq!(pixel(&frame, 0, 0), BLACK, "and nothing outside it moves");
    }

    #[test]
    fn compositing_a_cropped_cursor_never_writes_outside_the_frame() {
        for x in -12i32..12 {
            for y in -12i32..12 {
                let mut frame = vec![0u8; 8 * 8 * 4];
                let cursor = vec![0xff; 8 * 8 * 4];
                let placement = place(x, y, 8, 8, 8, 8);

                // The property: every write lands inside the buffer. An index
                // that escaped the crop would panic here rather than corrupt a
                // neighbouring row.
                composite(&mut frame, 8 * 4, &cursor, 8, &placement);
            }
        }
    }

    #[test]
    fn a_stride_wider_than_the_frame_writes_the_row_it_names() {
        // A framebuffer's stride is not promised to be `width * 4`, and the
        // rows a placement names are counted in it rather than in pixels.
        let stride = 6 * 4;
        let mut frame = vec![0u8; stride * 4];
        let cursor = [0xff, 0xff, 0xff, 0xff];
        let placement = place(1, 2, 1, 1, 4, 4);

        composite(&mut frame, stride as u32, &cursor, 1, &placement);

        let at = 2 * stride + 4;
        assert_eq!(&frame[at..at + 4], &[0xff, 0xff, 0xff, 0xff]);
        assert!(
            frame[..at].iter().all(|byte| *byte == 0),
            "nothing before that row was touched"
        );
    }

    #[test]
    fn an_invisible_cursor_composites_nothing() {
        let mut frame = BLACK.repeat(4 * 4);
        let cursor = vec![0xff; 4 * 4];
        let placement = place(-8, 0, 2, 2, 4, 4);

        composite(&mut frame, 4 * 4, &cursor, 2, &placement);

        assert!(frame.chunks_exact(4).all(|pixel| pixel == BLACK));
    }
}
