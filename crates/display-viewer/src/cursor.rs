//! Where the guest's cursor bitmap is anchored.
//!
//! A cursor record cannot tell the host what a cursor's hotspot is. Mutter
//! has already subtracted it by the time capture sees anything: the cursor
//! plane's `CRTC_X`/`CRTC_Y` are the pointer's position *minus* the hotspot,
//! and the DRM plane carries no hotspot property to read it back from --
//! task #114's module deliberately does not set `DRIVER_CURSOR_HOTSPOT`,
//! because mutter hides the cursor plane of drivers that do. So the position
//! record names the bitmap's top-left corner and its hotspot fields are
//! zeros. The theme table below is the guest's way around that: what the
//! plane cannot carry, the theme it was drawn from can say outright.
//!
//! Windows anchors a cursor by its hotspot, so a bitmap handed to
//! `CreateIconIndirect` with zeros lands with its *corner* where the pointer
//! is. That is the whole of #170: the arrow is drawn down and to the right of
//! the pixel that receives the click, by however far the hotspot reaches into
//! the bitmap -- a couple of pixels for an arrow, half the bitmap for the
//! I-beam that text is edited with.
//!
//! ## The theme table, which knows most hotspots outright
//!
//! The primary source is the Xcursor theme the guest reads for its tray: the
//! guest parses the theme's images and sends them whole -- pixels and the
//! theme's own xhot/yhot -- and they are matched here by pixels alone. An
//! entry is the shape when its drawing, cropped to where it has alpha, is the
//! incoming bitmap's drawing byte for byte, and the theme's hotspot then
//! shifts by how far each drawing sits from its own bitmap's corner. That one
//! shift covers both ways a theme image and a capture differ without the
//! picture differing: the plane's padding around the sprite, and a capture
//! cropped at the frame's left and top edges. A shape the table names is
//! anchored the moment it arrives, before any pointer record, and is
//! remembered like a measured one.
//!
//! ## Measurement, for what the theme does not cover
//!
//! The table cannot replace measurement outright, because a theme is only a
//! sample of the cursors a desktop shows: applications pick their own,
//! XWayland draws its own, a rescaled HiDPI sprite is a picture no theme
//! shipped, and the tray may not have read a theme at all. For everything the
//! table does not name, the host is still the one end that can work the
//! hotspot out, because the host is what moves the pointer: the hotspot is
//! the position it last sent minus the corner the guest reports. That
//! subtraction only holds while the two are of the same instant, so it is
//! trusted on a pointer that is standing still: no motion sent since the
//! previous record, and a guest reporting the corner it reported last time.
//!
//! ## Why it is worked out once per shape
//!
//! Because a hotspot that moves is a cursor that jumps. The subtraction is
//! exact only in principle: the pointer this end sent is a guest pixel, the
//! guest's is a float that came back through the absolute axes' fixed range,
//! and the corner is rounded -- so the same shape can be measured a pixel
//! apart at two ends of the screen. Re-measuring a settled cursor would spend
//! that pixel as a visible twitch, and a measurement taken across a mode
//! change would spend a great deal more.
//!
//! So a hotspot belongs to a bitmap, and a bitmap keeps the one it was given
//! -- the theme's or the measured one. The guest sends its cursor with every
//! frame whether or not it changed, so
//! the bitmap is compared with the one on the window and an identical one is
//! nothing at all -- neither a measurement nor an icon rebuilt sixty times a
//! second. Shapes already worked out are remembered, which is what keeps the
//! arrow and the I-beam of an afternoon's editing to one movement each.

use vmlord_display_codec::{CursorPosition, OwnedCursorImage};

/// How many shapes are remembered.
///
/// A desktop cycles through a handful -- arrow, I-beam, hand, the resize
/// edges -- and forgetting the oldest costs one measurement, not a fault.
const REMEMBERED: usize = 16;

/// The guest's cursor, and the hotspot worked out for it.
#[derive(Debug, Default)]
pub struct GuestCursor {
    /// The bitmap on the window, anchored by `hotspot`.
    image: Option<OwnedCursorImage>,
    /// The drawn part of that bitmap, which is what bounds a hotspot.
    drawn: Option<Bounds>,
    /// Whether this shape's hotspot is known -- named by the theme table or
    /// measured -- or is still the one inherited from the shape before it.
    known: bool,
    /// The theme's images, each with its drawing worked out once at load.
    table: Vec<ThemeEntry>,
    /// Shapes already measured, newest last.
    measured: Vec<(Vec<u8>, (u32, u32))>,
    /// Where the viewer last told the guest its pointer is, in guest pixels.
    sent: Option<(u32, u32)>,
    /// Whether any motion has been sent since the last position record.
    moved: bool,
    /// The corner the previous position record named.
    previous: Option<(u32, u32)>,
    /// What the pointer points at, from the bitmap's corner.
    hotspot: (u32, u32),
}

/// The rectangle a bitmap draws anything in.
#[derive(Clone, Copy, Debug)]
struct Bounds {
    left: u32,
    top: u32,
    right: u32,
    bottom: u32,
}

impl Bounds {
    /// Whether a point is inside, edges included.
    fn holds(&self, (x, y): (u32, u32)) -> bool {
        x >= self.left && x <= self.right && y >= self.top && y <= self.bottom
    }

    /// How many pixels the rectangle is wide and tall.
    fn size(&self) -> (u32, u32) {
        (self.right - self.left + 1, self.bottom - self.top + 1)
    }
}

/// One image of the theme table, with its drawing worked out once at load.
#[derive(Debug)]
struct ThemeEntry {
    /// The image's own pixels, cropped to where it draws.
    drawn_pixels: Vec<u8>,
    /// The rectangle that crop covers, from the image's own (0,0).
    drawn: Bounds,
    /// The theme's hotspot, from the image's own (0,0).
    hotspot: (u32, u32),
}

impl ThemeEntry {
    /// Where this entry's hotspot sits in the bitmap the crop came from, if
    /// the entry is that bitmap's shape at all.
    ///
    /// The incoming bytes are already cropped to their own drawing, as this
    /// entry's were at load; the theme's hotspot counts from the theme
    /// image's own corner, so it moves by how far the two drawings are
    /// apart -- which is how the plane's padding and a cropped capture drop
    /// out of the answer.
    fn hotspot_in(
        &self,
        image: &OwnedCursorImage,
        drawn: Bounds,
        cropped: &[u8],
    ) -> Option<(u32, u32)> {
        if self.drawn.size() != drawn.size() || self.drawn_pixels != cropped {
            return None;
        }

        let hotspot = (
            self.hotspot
                .0
                .checked_add(drawn.left)?
                .checked_sub(self.drawn.left)?,
            self.hotspot
                .1
                .checked_add(drawn.top)?
                .checked_sub(self.drawn.top)?,
        );
        // A hotspot the shift lands outside the drawing, or off the bitmap
        // altogether, is not this theme's shape but something else that
        // happens to draw the same bytes.
        (drawn.holds(hotspot) && hotspot.0 < image.width && hotspot.1 < image.height)
            .then_some(hotspot)
    }
}

impl GuestCursor {
    /// A cursor nothing is known about yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a pointer position on its way to the guest.
    pub fn motion(&mut self, x: u32, y: u32) {
        self.sent = Some((x, y));
        self.moved = true;
    }

    /// Takes the guest's cursor-hotspot table, when one arrives.
    ///
    /// The table comes whole and replaces whatever one stood before. The
    /// entries are the Xcursor theme's images with the theme's own hotspots,
    /// and each drawing is cropped now, once, rather than at every lookup --
    /// lookups are rare, but the table is not small either.
    pub fn hotspots(&mut self, entries: Vec<OwnedCursorImage>) {
        self.table = entries
            .into_iter()
            .filter_map(|image| {
                // An image that draws nothing can match nothing.
                let drawn = drawn_bounds(&image)?;
                Some(ThemeEntry {
                    drawn_pixels: cropped(&image, drawn),
                    drawn,
                    hotspot: (image.hotspot_x, image.hotspot_y),
                })
            })
            .collect();
    }

    /// Takes the bitmap of a frame.
    ///
    /// Returns it anchored, for the window to put up, when it is a shape the
    /// window is not already showing -- and `None` for the same bitmap again,
    /// which is what most frames carry.
    pub fn bitmap(&mut self, mut image: OwnedCursorImage) -> Option<&OwnedCursorImage> {
        if self
            .image
            .as_ref()
            .is_some_and(|shown| same_shape(shown, &image))
        {
            return None;
        }

        let drawn = drawn_bounds(&image);
        match self.remembered(&image.pixels) {
            // A shape seen before is anchored the way it was, with nothing to
            // work out and nothing to jump.
            Some(hotspot) => {
                self.hotspot = hotspot;
                self.known = true;
            }
            None => match self.themed(&image, drawn) {
                // A shape the theme knows is anchored before any pointer
                // record -- and is remembered like a measured one, so it
                // keeps its anchor when the table is replaced or the memory
                // is evicted, and is never measured afterwards.
                Some(hotspot) => {
                    self.hotspot = hotspot;
                    self.known = true;
                    self.remember(&image.pixels, hotspot);
                }
                // A new one keeps the anchor of the shape before it until it
                // has been measured: a stale hotspot of a pixel or two is a
                // smaller wrong than the corner.
                None => self.known = false,
            },
        }

        image.hotspot_x = self.hotspot.0;
        image.hotspot_y = self.hotspot.1;
        self.drawn = drawn;

        Some(self.image.insert(image))
    }

    /// Takes one position record.
    ///
    /// Returns the bitmap to put on the window again when this shape's hotspot
    /// has just been worked out, and `None` the rest of the time -- which is
    /// every record of a shape already measured.
    pub fn anchor(&mut self, position: CursorPosition) -> Option<&OwnedCursorImage> {
        let corner = (position.x, position.y);
        let settled = !self.moved && self.previous == Some(corner) && position.visible;
        self.moved = false;
        self.previous = Some(corner);
        if !settled || self.known {
            return None;
        }

        let hotspot = self.candidate(corner)?;
        let pixels = self.image.as_ref()?.pixels.clone();
        self.known = true;
        self.remember(&pixels, hotspot);
        if hotspot == self.hotspot {
            return None;
        }

        self.hotspot = hotspot;
        let image = self.image.as_mut()?;
        image.hotspot_x = hotspot.0;
        image.hotspot_y = hotspot.1;

        Some(image)
    }

    /// What this corner says the hotspot is, if it says anything.
    fn candidate(&self, corner: (u32, u32)) -> Option<(u32, u32)> {
        let (x, y) = self.sent?;
        let candidate = (x.checked_sub(corner.0)?, y.checked_sub(corner.1)?);
        // Outside what the bitmap draws it is not a hotspot but two numbers of
        // different moments: the guest moved its own pointer, a record crossed
        // a motion in flight, or the desktop changed mode under both.
        self.drawn?.holds(candidate).then_some(candidate)
    }

    /// The hotspot the theme table names for this shape, if one of its
    /// images is it.
    fn themed(&self, image: &OwnedCursorImage, drawn: Option<Bounds>) -> Option<(u32, u32)> {
        let drawn = drawn?;
        let cropped = cropped(image, drawn);
        self.table
            .iter()
            .find_map(|entry| entry.hotspot_in(image, drawn, &cropped))
    }

    /// The hotspot this shape was measured at before, if it was.
    fn remembered(&self, pixels: &[u8]) -> Option<(u32, u32)> {
        self.measured
            .iter()
            .find(|(shape, _)| shape == pixels)
            .map(|(_, hotspot)| *hotspot)
    }

    /// Remembers a shape at the hotspot it was given, so the next appearance
    /// of the same bytes is anchored without working anything out.
    fn remember(&mut self, pixels: &[u8], hotspot: (u32, u32)) {
        if self.measured.len() == REMEMBERED {
            self.measured.remove(0);
        }
        self.measured.push((pixels.to_vec(), hotspot));
    }
}

/// Whether two bitmaps are the same picture.
fn same_shape(one: &OwnedCursorImage, other: &OwnedCursorImage) -> bool {
    one.width == other.width && one.height == other.height && one.pixels == other.pixels
}

/// The rectangle of a bitmap that has anything drawn in it.
///
/// A cursor plane is whatever size the hardware wants -- 64x64 is usual -- and
/// a 24-pixel arrow sits in the corner of it with the rest transparent. The
/// hotspot is a pixel of the drawing, never of the padding, so this is what a
/// measurement is checked against rather than the bitmap's own edges.
fn drawn_bounds(image: &OwnedCursorImage) -> Option<Bounds> {
    let mut bounds: Option<Bounds> = None;
    for y in 0..image.height {
        for x in 0..image.width {
            let at = ((y * image.width + x) as usize) * 4 + 3;
            if image.pixels.get(at).is_none_or(|alpha| *alpha == 0) {
                continue;
            }
            bounds = Some(match bounds {
                None => Bounds {
                    left: x,
                    top: y,
                    right: x,
                    bottom: y,
                },
                Some(bounds) => Bounds {
                    left: bounds.left.min(x),
                    top: bounds.top.min(y),
                    right: bounds.right.max(x),
                    bottom: bounds.bottom.max(y),
                },
            });
        }
    }

    bounds
}

/// The bytes a bitmap draws, tightly packed, inside its drawn rectangle.
fn cropped(image: &OwnedCursorImage, bounds: Bounds) -> Vec<u8> {
    let (width, height) = bounds.size();
    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
    for y in bounds.top..=bounds.bottom {
        let start = ((y * image.width + bounds.left) as usize) * 4;
        let end = start + width as usize * 4;
        pixels.extend_from_slice(&image.pixels[start..end]);
    }
    pixels
}

#[cfg(test)]
mod tests {
    use super::{GuestCursor, REMEMBERED};
    use vmlord_display_codec::{CursorPosition, OwnedCursorImage};

    /// A visible cursor at that corner.
    fn at(x: u32, y: u32) -> CursorPosition {
        CursorPosition {
            x,
            y,
            visible: true,
        }
    }

    /// A bitmap of the given size whose top-left `drawn` square is opaque,
    /// and whose `tag` makes it a shape of its own.
    fn bitmap(size: u32, drawn: u32, tag: u8) -> OwnedCursorImage {
        sprite(size, (0, 0), drawn, tag)
    }

    /// A bitmap of the given size whose drawing is a `drawn`-square of `tag`
    /// with its top-left at `origin`, and the rest transparent.
    fn sprite(size: u32, origin: (u32, u32), drawn: u32, tag: u8) -> OwnedCursorImage {
        let mut pixels = vec![0; (size * size * 4) as usize];
        for y in origin.1..origin.1 + drawn {
            for x in origin.0..origin.0 + drawn {
                let at = ((y * size + x) as usize) * 4;
                pixels[at] = tag;
                pixels[at + 3] = 255;
            }
        }

        OwnedCursorImage {
            pixels,
            width: size,
            height: size,
            hotspot_x: 0,
            hotspot_y: 0,
        }
    }

    /// A theme image: a `sprite` carrying the theme's own hotspot.
    fn theme(
        size: u32,
        origin: (u32, u32),
        drawn: u32,
        tag: u8,
        hotspot: (u32, u32),
    ) -> OwnedCursorImage {
        let mut image = sprite(size, origin, drawn, tag);
        image.hotspot_x = hotspot.0;
        image.hotspot_y = hotspot.1;
        image
    }

    /// The same bitmap with `left` columns and `top` rows cut off it, as the
    /// capture delivers it when the frame's edges cross the plane.
    fn crop(mut image: OwnedCursorImage, left: u32, top: u32) -> OwnedCursorImage {
        let width = image.width - left;
        let height = image.height - top;
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for y in top..image.height {
            let start = ((y * image.width + left) as usize) * 4;
            let end = start + width as usize * 4;
            pixels.extend_from_slice(&image.pixels[start..end]);
        }

        image.pixels = pixels;
        image.width = width;
        image.height = height;
        image
    }

    /// A cursor showing that shape, with the pointer standing at `sent`.
    fn showing(image: OwnedCursorImage, sent: (u32, u32)) -> GuestCursor {
        let mut cursor = GuestCursor::new();
        cursor.bitmap(image);
        cursor.motion(sent.0, sent.1);

        cursor
    }

    #[test]
    fn a_bitmap_is_anchored_at_its_corner_until_the_hotspot_is_known() {
        let mut cursor = GuestCursor::new();

        let image = cursor.bitmap(bitmap(64, 24, 1)).expect("the first bitmap");

        assert_eq!((image.hotspot_x, image.hotspot_y), (0, 0));
    }

    #[test]
    fn a_pointer_standing_still_names_the_hotspot() {
        let mut cursor = showing(bitmap(64, 24, 1), (100, 200));

        // The first record has nothing before it to agree with.
        assert!(cursor.anchor(at(89, 189)).is_none());

        let image = cursor.anchor(at(89, 189)).expect("the hotspot");
        assert_eq!((image.hotspot_x, image.hotspot_y), (11, 11));
    }

    #[test]
    fn a_measured_shape_is_never_measured_again() {
        // The whole of the jumping: a hotspot a pixel out at the other end of
        // the screen would move a cursor that is standing still.
        let mut cursor = showing(bitmap(64, 24, 1), (100, 200));
        cursor.anchor(at(89, 189));
        cursor.anchor(at(89, 189)).expect("the hotspot");

        cursor.motion(500, 500);
        assert!(cursor.anchor(at(490, 490)).is_none());
        assert!(cursor.anchor(at(490, 490)).is_none());
        assert!(cursor.anchor(at(490, 490)).is_none());
    }

    #[test]
    fn the_same_bitmap_again_is_not_a_new_cursor() {
        // What every frame carries: the guest cannot tell a new bitmap from
        // the one before it, so the window is not rebuilt sixty times a second.
        let mut cursor = GuestCursor::new();
        cursor.bitmap(bitmap(64, 24, 1)).expect("the first bitmap");

        assert!(cursor.bitmap(bitmap(64, 24, 1)).is_none());
    }

    #[test]
    fn a_new_shape_keeps_the_anchor_of_the_last_until_it_is_measured() {
        let mut cursor = showing(bitmap(64, 24, 1), (100, 200));
        cursor.anchor(at(89, 189));
        cursor.anchor(at(89, 189));

        let image = cursor.bitmap(bitmap(64, 24, 2)).expect("a second shape");

        assert_eq!((image.hotspot_x, image.hotspot_y), (11, 11));
    }

    #[test]
    fn a_shape_measured_before_is_anchored_the_moment_it_comes_back() {
        // The arrow and the I-beam of an afternoon's editing: one movement
        // each, not one every time the pointer crosses a text field.
        let mut cursor = showing(bitmap(64, 24, 1), (100, 200));
        cursor.anchor(at(89, 189));
        cursor.anchor(at(89, 189));

        cursor.bitmap(bitmap(64, 24, 2));
        cursor.motion(300, 300);
        cursor.anchor(at(296, 296));
        cursor.anchor(at(296, 296)).expect("the second hotspot");

        let image = cursor.bitmap(bitmap(64, 24, 1)).expect("the first again");

        assert_eq!((image.hotspot_x, image.hotspot_y), (11, 11));
    }

    #[test]
    fn the_oldest_shape_is_the_one_forgotten() {
        let mut cursor = showing(bitmap(64, 24, 0), (100, 200));
        cursor.anchor(at(89, 189));
        cursor.anchor(at(89, 189));

        for tag in 1..=REMEMBERED as u8 {
            cursor.bitmap(bitmap(64, 24, tag));
            cursor.motion(300, 300);
            cursor.anchor(at(296, 296));
            cursor.anchor(at(296, 296));
        }

        // The first shape is back, and has to be worked out again.
        let image = cursor.bitmap(bitmap(64, 24, 0)).expect("the first again");
        assert_eq!((image.hotspot_x, image.hotspot_y), (4, 4));
    }

    #[test]
    fn a_moving_pointer_names_nothing() {
        // The corner is a frame old and the position is current: their
        // difference is the travel between them, not a hotspot.
        let mut cursor = showing(bitmap(64, 24, 1), (100, 200));

        assert!(cursor.anchor(at(89, 189)).is_none());
        cursor.motion(104, 204);
        assert!(cursor.anchor(at(93, 193)).is_none());
        cursor.motion(108, 208);
        assert!(cursor.anchor(at(97, 197)).is_none());
    }

    #[test]
    fn a_corner_that_moved_on_its_own_names_nothing() {
        // Nothing was sent, so the guest moved its own pointer: the two
        // numbers are of different moments and their difference is noise.
        let mut cursor = showing(bitmap(64, 24, 1), (100, 200));
        cursor.anchor(at(89, 189));

        assert!(cursor.anchor(at(300, 400)).is_none());
    }

    #[test]
    fn a_hotspot_outside_what_the_bitmap_draws_is_refused() {
        // Inside a 64-pixel plane and well outside the 24 pixels of arrow in
        // the corner of it: the padding points at nothing.
        let mut cursor = showing(bitmap(64, 24, 1), (100, 200));

        assert!(cursor.anchor(at(60, 160)).is_none());
        assert!(cursor.anchor(at(60, 160)).is_none());
    }

    #[test]
    fn a_corner_past_the_pointer_is_refused() {
        // The corner is the pointer minus the hotspot, so it is never beyond
        // it; a record that says otherwise crossed a motion in flight.
        let mut cursor = showing(bitmap(64, 24, 1), (100, 200));

        assert!(cursor.anchor(at(105, 205)).is_none());
        assert!(cursor.anchor(at(105, 205)).is_none());
    }

    #[test]
    fn a_hidden_cursor_names_nothing() {
        let mut cursor = showing(bitmap(64, 24, 1), (100, 200));
        let hidden = CursorPosition {
            x: 89,
            y: 189,
            visible: false,
        };

        assert!(cursor.anchor(hidden).is_none());
        assert!(cursor.anchor(hidden).is_none());
    }

    #[test]
    fn a_position_without_a_bitmap_names_nothing() {
        let mut cursor = GuestCursor::new();
        cursor.motion(100, 200);

        assert!(cursor.anchor(at(89, 189)).is_none());
        assert!(cursor.anchor(at(89, 189)).is_none());
    }

    #[test]
    fn a_bitmap_with_nothing_drawn_in_it_measures_nothing() {
        let mut cursor = showing(bitmap(64, 0, 0), (100, 200));

        assert!(cursor.anchor(at(89, 189)).is_none());
        assert!(cursor.anchor(at(89, 189)).is_none());
    }

    #[test]
    fn a_table_shape_is_anchored_the_moment_it_arrives() {
        // No pointer record has ever been exchanged: the theme's word is
        // anchor enough on its own.
        let mut cursor = GuestCursor::new();
        cursor.hotspots(vec![theme(32, (0, 0), 24, 1, (3, 2))]);

        let image = cursor.bitmap(bitmap(64, 24, 1)).expect("the first bitmap");

        assert_eq!((image.hotspot_x, image.hotspot_y), (3, 2));
    }

    #[test]
    fn a_table_known_shape_is_never_measured() {
        // The settled records would name (11, 11) if they could; the theme
        // named (3, 2) the moment the bitmap arrived, and nothing may
        // overrule it.
        let mut cursor = GuestCursor::new();
        cursor.hotspots(vec![theme(32, (0, 0), 24, 1, (3, 2))]);
        cursor.bitmap(bitmap(64, 24, 1)).expect("the first bitmap");
        cursor.motion(100, 200);

        assert!(cursor.anchor(at(89, 189)).is_none());
        assert!(cursor.anchor(at(89, 189)).is_none());

        // Round-tripped through another shape, the anchor is still the
        // theme's.
        cursor.bitmap(bitmap(64, 24, 2));
        let image = cursor.bitmap(bitmap(64, 24, 1)).expect("the first again");
        assert_eq!((image.hotspot_x, image.hotspot_y), (3, 2));
    }

    #[test]
    fn the_plane_s_padding_does_not_defeat_the_match() {
        // The theme ships an 8x8 image whose drawing is 4 pixels; the plane
        // carries the same drawing in the corner of 64x64. Different sizes,
        // the same picture once the padding is cropped away.
        let mut cursor = GuestCursor::new();
        cursor.hotspots(vec![theme(8, (0, 0), 4, 1, (1, 2))]);

        let image = cursor.bitmap(bitmap(64, 4, 1)).expect("the plane bitmap");

        assert_eq!((image.hotspot_x, image.hotspot_y), (1, 2));
    }

    #[test]
    fn a_bitmap_cropped_at_the_left_and_top_still_matches_with_the_hotspot_shifted() {
        // The theme draws at (2, 3) of its own image, and the plane carries
        // the same picture with the sprite at its corner. The capture then
        // cuts the frame's left and top edges off -- the two columns and
        // three rows of padding, no more. What reaches the viewer draws at
        // its own (0, 0), and the hotspot it is given moves with it: (1, 1)
        // into the drawing, not the theme's absolute (3, 4).
        let mut cursor = GuestCursor::new();
        cursor.hotspots(vec![theme(8, (2, 3), 4, 1, (3, 4))]);

        let whole = cursor
            .bitmap(sprite(64, (2, 3), 4, 1))
            .expect("the whole plane");
        assert_eq!((whole.hotspot_x, whole.hotspot_y), (3, 4));

        let cropped = crop(sprite(64, (2, 3), 4, 1), 2, 3);
        let image = cursor.bitmap(cropped).expect("the cropped plane");
        assert_eq!((image.hotspot_x, image.hotspot_y), (1, 1));
    }

    #[test]
    fn a_crop_that_cuts_into_the_drawing_does_not_match_but_measures_fine() {
        // Here the padding was thinner than the crop: two columns and three
        // rows of the drawing itself are gone, and what is left is a
        // picture no theme image is. The corner anchors it until the
        // pointer settles.
        let mut cursor = GuestCursor::new();
        cursor.hotspots(vec![theme(8, (0, 0), 4, 1, (1, 1))]);

        let cropped = crop(sprite(64, (0, 0), 4, 1), 2, 3);
        let image = cursor.bitmap(cropped).expect("the cropped bitmap");
        assert_eq!((image.hotspot_x, image.hotspot_y), (0, 0));

        cursor.motion(100, 200);
        assert!(cursor.anchor(at(99, 200)).is_none());
        let image = cursor.anchor(at(99, 200)).expect("measured");
        assert_eq!((image.hotspot_x, image.hotspot_y), (1, 0));
    }

    #[test]
    fn a_match_whose_hotspot_lands_outside_the_drawing_is_refused() {
        // The theme names a hotspot in its own padding: nothing a real
        // theme says, and nothing the host believes. The entry is no match,
        // and the shape waits to be measured like any other.
        let mut cursor = GuestCursor::new();
        cursor.hotspots(vec![theme(8, (0, 0), 4, 1, (7, 7))]);

        let image = cursor.bitmap(bitmap(64, 4, 1)).expect("the plane bitmap");
        assert_eq!((image.hotspot_x, image.hotspot_y), (0, 0));

        cursor.motion(100, 200);
        assert!(cursor.anchor(at(97, 197)).is_none());
        let image = cursor.anchor(at(97, 197)).expect("measured");
        assert_eq!((image.hotspot_x, image.hotspot_y), (3, 3));
    }

    #[test]
    fn a_table_replaced_wholesale_no_longer_names_the_shape_it_loses() {
        // The first match also remembered the shape, and that memory does
        // carry it across the replacement -- but it is memory all the same.
        // Driven out by newer shapes, the shape falls back on the corner
        // and on measurement, because the only table that named it is gone.
        let mut cursor = GuestCursor::new();
        cursor.hotspots(vec![theme(8, (0, 0), 4, 1, (1, 1))]);
        cursor
            .bitmap(bitmap(64, 4, 1))
            .expect("matched from the first table");

        // A new theme without the shape, full enough of others to drive
        // the memory of the first match out.
        let replacement: Vec<_> = (2..=REMEMBERED as u8 + 2)
            .map(|tag| theme(8, (0, 0), 4, tag, (2, 2)))
            .collect();
        cursor.hotspots(replacement);
        for tag in 2..=REMEMBERED as u8 + 2 {
            cursor.bitmap(bitmap(64, 4, tag));
        }

        // What it is anchored by now is the previous shape's inheritance,
        // not a name: the first table said (1, 1) and nothing says that
        // any more.
        let image = cursor
            .bitmap(bitmap(64, 4, 1))
            .expect("the first shape again");
        assert_eq!((image.hotspot_x, image.hotspot_y), (2, 2));

        cursor.motion(100, 200);
        assert!(cursor.anchor(at(97, 197)).is_none());
        let image = cursor.anchor(at(97, 197)).expect("measured");
        assert_eq!((image.hotspot_x, image.hotspot_y), (3, 3));
    }

    #[test]
    fn the_same_table_serves_bitmaps_of_different_plane_sizes() {
        // The same sprite arrives in a 64x64 plane and a 256x256 one; the
        // drawing is the same bytes either way, and the theme names the
        // same spot in both.
        let mut cursor = GuestCursor::new();
        cursor.hotspots(vec![theme(8, (0, 0), 4, 1, (1, 2))]);

        let small = cursor.bitmap(bitmap(64, 4, 1)).expect("a 64x64 plane");
        assert_eq!((small.hotspot_x, small.hotspot_y), (1, 2));

        let large = cursor.bitmap(bitmap(256, 4, 1)).expect("a 256x256 plane");
        assert_eq!((large.hotspot_x, large.hotspot_y), (1, 2));
    }
}
