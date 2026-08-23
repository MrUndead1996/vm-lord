//! Where the pointer is, and what to do with it.
//!
//! Task #114's module deliberately does not set `DRIVER_CURSOR_HOTSPOT`, so no
//! hotspot is readable -- and none is needed: mutter places the plane where the
//! image is drawn, so the hotspot is `(0, 0)` and the position is the plane's
//! `CRTC_X`/`CRTC_Y`. Those are signed and go negative at the left and top
//! edges, while the protocol's coordinates are not, which is why an offscreen
//! cursor is cropped here rather than clamped and misdrawn there.

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
/// `frame` and `cursor` are both `ARGB8888`-shaped words; the frame's alpha is
/// left as it was, because an `XRGB8888` framebuffer has none to speak of and a
/// viewer never reads it.
///
/// The loop's bounds come from `placement.crop`, which [`place`] has already
/// clipped to the frame, so no index here needs a guard of its own.
pub fn composite(
    frame: &mut [u32],
    frame_stride_pixels: u32,
    cursor: &[u32],
    cursor_width: u32,
    placement: &Placement,
) {
    if !placement.visible {
        return;
    }

    for row in 0..placement.crop.height {
        let source_row = (placement.crop.y + row) * cursor_width + placement.crop.x;
        let target_row = (placement.y + row) * frame_stride_pixels + placement.x;

        for column in 0..placement.crop.width {
            let Some(source) = cursor.get((source_row + column) as usize) else {
                return;
            };
            let Some(target) = frame.get_mut((target_row + column) as usize) else {
                return;
            };
            *target = blend(*source, *target);
        }
    }
}

/// One pixel of source over one pixel of destination.
fn blend(source: u32, destination: u32) -> u32 {
    let alpha = source >> 24;
    if alpha == 0 {
        return destination;
    }
    if alpha == 0xff {
        return source;
    }

    let inverse = 255 - alpha;
    let mut result = destination & 0xff00_0000;
    for shift in [0, 8, 16] {
        let over = (source >> shift) & 0xff;
        let under = (destination >> shift) & 0xff;
        // Rounded rather than truncated: a cursor blended over a flat colour
        // repeatedly would otherwise drift darker with every frame.
        let channel = (over * alpha + under * inverse + 127) / 255;
        result |= (channel & 0xff) << shift;
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

    #[test]
    fn compositing_blends_by_alpha_and_leaves_the_rest_alone() {
        let mut frame = vec![0xff00_0000u32; 4 * 4];
        // Opaque white, half-transparent white, and two fully transparent.
        let cursor = [0xffff_ffffu32, 0x80ff_ffff, 0x0000_0000, 0x0000_0000];
        let placement = place(1, 1, 2, 2, 4, 4);

        composite(&mut frame, 4, &cursor, 2, &placement);

        assert_eq!(frame[4 + 1], 0xffff_ffff, "an opaque cursor pixel wins");
        assert_eq!(
            frame[4 + 2] & 0x00ff_0000,
            0x0080_0000,
            "a half-transparent pixel lands halfway between the two"
        );
        assert_eq!(
            frame[2 * 4 + 1],
            0xff00_0000,
            "a transparent pixel changes nothing"
        );
        assert_eq!(
            frame[0], 0xff00_0000,
            "and nothing outside the cursor moves"
        );
    }

    #[test]
    fn compositing_a_cropped_cursor_never_writes_outside_the_frame() {
        for x in -12i32..12 {
            for y in -12i32..12 {
                let mut frame = vec![0u32; 8 * 8];
                let cursor = vec![0xffff_ffffu32; 8 * 8];
                let placement = place(x, y, 8, 8, 8, 8);

                // The property: every write lands inside the buffer. An index
                // that escaped the crop would panic here rather than corrupt a
                // neighbouring row.
                composite(&mut frame, 8, &cursor, 8, &placement);
            }
        }
    }

    #[test]
    fn an_invisible_cursor_composites_nothing() {
        let mut frame = vec![0xff00_0000u32; 4 * 4];
        let cursor = vec![0xffff_ffffu32; 4];
        let placement = place(-8, 0, 2, 2, 4, 4);

        composite(&mut frame, 4, &cursor, 2, &placement);

        assert!(frame.iter().all(|pixel| *pixel == 0xff00_0000));
    }
}
