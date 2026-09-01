//! The tray's icon, drawn rather than read from a theme.
//!
//! A theme-file name would make the icon depend on what the guest happens to
//! have installed, so the tray carries its own pixels instead: a 24×24 tile
//! with the V of VMLord on it, in the ARGB32 form the StatusNotifierItem
//! specification asks for. Twenty-four pixels is the smallest size a panel
//! scales up without complaint, and a tray icon that arrives as a pixmap
//! needs nothing else on disk.

use ksni::Icon;

/// The icon's width and height, in pixels.
const SIZE: i32 = 24;

/// The radius of the tile's rounded corners.
const CORNER: i32 = 5;

/// Half the width of the monogram's strokes.
const STROKE_HALF_WIDTH: f32 = 2.0;

/// The tile: an opaque, near-black slate that reads on a light panel and
/// stays quiet on a dark one.
const TILE: [u8; 4] = [0xFF, 0x2D, 0x2D, 0x3A];

/// The monogram: white, because it is the one thing that must survive any
/// panel colour.
const STROKE: [u8; 4] = [0xFF, 0xFF, 0xFF, 0xFF];

/// The icon, as one ARGB32 pixmap.
///
/// Each pixel is four bytes in the specification's order -- alpha, red, green,
/// blue -- the order of the word `0xAARRGGBB` written out byte by byte.
#[must_use]
pub fn monogram() -> Icon {
    Icon {
        width: SIZE,
        height: SIZE,
        data: (0..SIZE)
            .flat_map(|y| (0..SIZE).map(move |x| pixel(x, y)))
            .flatten()
            .collect(),
    }
}

/// One pixel of the icon.
fn pixel(x: i32, y: i32) -> [u8; 4] {
    let (x, y) = (f32::from(x as i16) + 0.5, f32::from(y as i16) + 0.5);
    if !inside_tile(x, y) {
        return [0; 4];
    }
    if on_monogram(x, y) { STROKE } else { TILE }
}

/// Whether the pixel centre sits in the tile: everywhere but the four
/// quarter-circles that round it.
fn inside_tile(x: f32, y: f32) -> bool {
    let inner = f32::from(CORNER as i16);
    let edge = f32::from(SIZE as i16) - inner;
    let (corner_x, corner_y) = match (x, y) {
        (x, y) if x < inner && y < inner => (inner, inner),
        (x, y) if x >= edge && y < inner => (edge, inner),
        (x, y) if x < inner && y >= edge => (inner, edge),
        (x, y) if x >= edge && y >= edge => (edge, edge),
        // Not in a corner's quadrant, so nothing rounds it away.
        _ => return true,
    };

    (x - corner_x).hypot(y - corner_y) <= inner
}

/// Whether the pixel centre is on one of the two strokes of the V.
fn on_monogram(x: f32, y: f32) -> bool {
    // The apex sits low so the letter keeps its weight inside the tile, and
    // the arms stop short of the edge so nothing is clipped.
    distance_to_segment(x, y, (5.0, 5.0), (12.0, 18.5)) <= STROKE_HALF_WIDTH
        || distance_to_segment(x, y, (12.0, 18.5), (19.0, 5.0)) <= STROKE_HALF_WIDTH
}

/// The distance from a point to the segment between two others.
fn distance_to_segment(x: f32, y: f32, from: (f32, f32), to: (f32, f32)) -> f32 {
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let length_squared = dx * dx + dy * dy;
    let t = (((x - from.0) * dx + (y - from.1) * dy) / length_squared).clamp(0.0, 1.0);
    let (nearest_x, nearest_y) = (from.0 + t * dx, from.1 + t * dy);

    (x - nearest_x).hypot(y - nearest_y)
}

#[cfg(test)]
mod tests {
    use super::{SIZE, monogram};

    /// The pixel at a coordinate, as its four bytes.
    fn pixel_at(icon: &ksni::Icon, x: i32, y: i32) -> [u8; 4] {
        let start = ((y * SIZE + x) * 4) as usize;
        icon.data[start..start + 4].try_into().unwrap()
    }

    #[test]
    fn the_icon_is_the_square_it_says_it_is() {
        let icon = monogram();

        assert_eq!((icon.width, icon.height), (SIZE, SIZE));
        assert_eq!(icon.data.len(), (SIZE * SIZE * 4) as usize);
    }

    #[test]
    fn the_corners_are_transparent_and_the_tile_is_not() {
        let icon = monogram();

        assert_eq!(pixel_at(&icon, 0, 0), [0; 4], "the top-left quarter-circle");
        assert_eq!(
            pixel_at(&icon, SIZE - 1, SIZE - 1),
            [0; 4],
            "the bottom-right"
        );
        assert_eq!(
            pixel_at(&icon, 3, 12),
            [0xFF, 0x2D, 0x2D, 0x3A],
            "the tile beside the V"
        );
    }

    #[test]
    fn the_monogram_is_white_where_the_strokes_run() {
        let icon = monogram();

        // The apex of the V, and one point on the left arm.
        assert_eq!(pixel_at(&icon, 11, 18), [0xFF; 4]);
        assert_eq!(pixel_at(&icon, 6, 6), [0xFF; 4]);
    }
}
