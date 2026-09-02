//! Where the guest's cursor bitmap is anchored.
//!
//! The guest cannot tell the host what a cursor's hotspot is. Mutter has
//! already subtracted it by the time capture sees anything: the cursor plane's
//! `CRTC_X`/`CRTC_Y` are the pointer's position *minus* the hotspot, and the
//! DRM plane carries no hotspot property to read it back from -- task #114's
//! module deliberately does not set `DRIVER_CURSOR_HOTSPOT`, because mutter
//! hides the cursor plane of drivers that do. So the position record names the
//! bitmap's top-left corner and its hotspot fields are zeros.
//!
//! Windows anchors a cursor by its hotspot, so a bitmap handed to
//! `CreateIconIndirect` with zeros lands with its *corner* where the pointer
//! is. That is the whole of #170: the arrow is drawn down and to the right of
//! the pixel that receives the click, by however far the hotspot reaches into
//! the bitmap -- a couple of pixels for an arrow, half the bitmap for the
//! I-beam that text is edited with.
//!
//! The host is the one end that can work the hotspot out, because the host is
//! what moves the pointer: the hotspot is the position it last sent minus the
//! corner the guest reports. That subtraction only holds while the two are the
//! same instant, so it is trusted on a pointer that is standing still -- no
//! motion sent since the previous record, and a guest that reports the corner
//! it reported last time. A moving pointer names nothing and keeps whatever
//! was worked out before.

use vmlord_display_codec::{CursorPosition, OwnedCursorImage};

/// The guest's cursor, and the hotspot worked out for it.
#[derive(Debug, Default)]
pub struct GuestCursor {
    /// The last bitmap the guest sent, with the hotspot below stamped on it.
    image: Option<OwnedCursorImage>,
    /// Where the viewer last told the guest its pointer is, in guest pixels.
    sent: Option<(u32, u32)>,
    /// Whether any motion has been sent since the last position record.
    moved: bool,
    /// The corner the previous position record named.
    previous: Option<(u32, u32)>,
    /// What the pointer points at, from the bitmap's corner.
    hotspot: (u32, u32),
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

    /// Takes a new bitmap, and hands it back anchored for the window.
    pub fn bitmap(&mut self, mut image: OwnedCursorImage) -> &OwnedCursorImage {
        image.hotspot_x = self.hotspot.0;
        image.hotspot_y = self.hotspot.1;

        self.image.insert(image)
    }

    /// Takes one position record.
    ///
    /// Returns the bitmap to put on the window again when the hotspot it is
    /// anchored by has changed, and `None` while it stands.
    pub fn anchor(&mut self, position: CursorPosition) -> Option<&OwnedCursorImage> {
        let corner = (position.x, position.y);
        let settled = !self.moved && self.previous == Some(corner) && position.visible;
        self.moved = false;
        self.previous = Some(corner);
        if !settled {
            return None;
        }

        let image = self.image.as_mut()?;
        let (x, y) = self.sent?;
        let hotspot = (x.checked_sub(corner.0)?, y.checked_sub(corner.1)?);
        // A hotspot outside the bitmap is not one: what it says is that the
        // two numbers are of different moments -- the guest moved its own
        // pointer, or a record crossed a motion in flight.
        if hotspot.0 >= image.width || hotspot.1 >= image.height || hotspot == self.hotspot {
            return None;
        }

        self.hotspot = hotspot;
        image.hotspot_x = hotspot.0;
        image.hotspot_y = hotspot.1;

        Some(image)
    }
}

#[cfg(test)]
mod tests {
    use super::GuestCursor;
    use vmlord_display_codec::{CursorPosition, OwnedCursorImage};

    /// A bitmap of the given size, with the hotspot a guest always sends.
    fn bitmap(width: u32, height: u32) -> OwnedCursorImage {
        OwnedCursorImage {
            pixels: vec![0; (width * height * 4) as usize],
            width,
            height,
            hotspot_x: 0,
            hotspot_y: 0,
        }
    }

    /// A visible cursor at that corner.
    fn at(x: u32, y: u32) -> CursorPosition {
        CursorPosition {
            x,
            y,
            visible: true,
        }
    }

    #[test]
    fn a_bitmap_is_anchored_at_its_corner_until_the_hotspot_is_known() {
        let mut cursor = GuestCursor::new();

        let image = cursor.bitmap(bitmap(24, 24));

        assert_eq!((image.hotspot_x, image.hotspot_y), (0, 0));
    }

    #[test]
    fn a_pointer_standing_still_names_the_hotspot() {
        let mut cursor = GuestCursor::new();
        cursor.bitmap(bitmap(24, 24));
        cursor.motion(100, 200);

        // The first record has nothing before it to agree with.
        assert!(cursor.anchor(at(89, 189)).is_none());

        let image = cursor.anchor(at(89, 189)).expect("the hotspot");
        assert_eq!((image.hotspot_x, image.hotspot_y), (11, 11));
    }

    #[test]
    fn a_hotspot_that_still_holds_is_not_applied_again() {
        let mut cursor = GuestCursor::new();
        cursor.bitmap(bitmap(24, 24));
        cursor.motion(100, 200);
        cursor.anchor(at(89, 189));
        cursor.anchor(at(89, 189)).expect("the hotspot");

        assert!(cursor.anchor(at(89, 189)).is_none());
    }

    #[test]
    fn every_later_bitmap_is_anchored_by_what_was_worked_out() {
        let mut cursor = GuestCursor::new();
        cursor.bitmap(bitmap(24, 24));
        cursor.motion(100, 200);
        cursor.anchor(at(89, 189));
        cursor.anchor(at(89, 189));

        let image = cursor.bitmap(bitmap(24, 24));

        assert_eq!((image.hotspot_x, image.hotspot_y), (11, 11));
    }

    #[test]
    fn a_moving_pointer_names_nothing() {
        // The corner is a frame old and the position is current: their
        // difference is the travel between them, not a hotspot.
        let mut cursor = GuestCursor::new();
        cursor.bitmap(bitmap(24, 24));

        cursor.motion(100, 200);
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
        let mut cursor = GuestCursor::new();
        cursor.bitmap(bitmap(24, 24));
        cursor.motion(100, 200);
        cursor.anchor(at(89, 189));

        assert!(cursor.anchor(at(300, 400)).is_none());
    }

    #[test]
    fn a_hotspot_outside_the_bitmap_is_refused() {
        let mut cursor = GuestCursor::new();
        cursor.bitmap(bitmap(24, 24));
        cursor.motion(100, 200);

        // Half a screen apart: a stale position against a fresh corner.
        assert!(cursor.anchor(at(40, 100)).is_none());
        assert!(cursor.anchor(at(40, 100)).is_none());
    }

    #[test]
    fn a_corner_past_the_pointer_is_refused() {
        // The corner is the pointer minus the hotspot, so it is never beyond
        // it; a record that says otherwise crossed a motion in flight.
        let mut cursor = GuestCursor::new();
        cursor.bitmap(bitmap(24, 24));
        cursor.motion(100, 200);

        assert!(cursor.anchor(at(105, 205)).is_none());
        assert!(cursor.anchor(at(105, 205)).is_none());
    }

    #[test]
    fn a_hidden_cursor_names_nothing() {
        let mut cursor = GuestCursor::new();
        cursor.bitmap(bitmap(24, 24));
        cursor.motion(100, 200);
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
}
