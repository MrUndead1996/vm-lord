//! Where the guest's picture sits on the client area.
//!
//! One value, and both consumers read it: the renderer copies into it and the
//! input policy maps points through it. Today it is the crop the renderer
//! already performed -- the picture at the top left, cut off at the window's
//! edges. #120 replaces [`place`] with letterboxing, and because there is one
//! of it rather than one per consumer, the pointer follows the picture without
//! a second change.

/// The rectangle the guest's picture occupies, and the frame it shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    /// The picture's left edge in client pixels.
    pub x: i32,
    /// Its top edge in client pixels.
    pub y: i32,
    /// How wide it is on screen, which #120 makes differ from the frame.
    pub width: u32,
    /// How tall it is on screen.
    pub height: u32,
    /// The frame's width in guest pixels.
    pub guest_width: u32,
    /// The frame's height in guest pixels.
    pub guest_height: u32,
}

/// Where a frame of this size sits on a client area of that size.
///
/// Today: the top left corner, cut off at the window's edges, which is what
/// the renderer already does. Returns `None` for a client area or a frame with
/// no pixels in it, which is a window mid-resize rather than a fault.
#[must_use]
pub fn place(
    guest_width: u32,
    guest_height: u32,
    client_width: i32,
    client_height: i32,
) -> Option<Placement> {
    let client_width = u32::try_from(client_width).ok()?;
    let client_height = u32::try_from(client_height).ok()?;
    let width = guest_width.min(client_width);
    let height = guest_height.min(client_height);
    if width == 0 || height == 0 {
        return None;
    }

    Some(Placement {
        x: 0,
        y: 0,
        width,
        height,
        guest_width,
        guest_height,
    })
}

impl Placement {
    /// The guest pixel under a client point, or `None` off the picture.
    #[must_use]
    pub fn to_guest(&self, x: i32, y: i32) -> Option<(u32, u32)> {
        let inside_x = u32::try_from(x.checked_sub(self.x)?).ok()?;
        let inside_y = u32::try_from(y.checked_sub(self.y)?).ok()?;
        if inside_x >= self.width || inside_y >= self.height {
            return None;
        }

        Some((self.scale_x(inside_x), self.scale_y(inside_y)))
    }

    /// The same, with points off the picture pulled onto its nearest edge.
    ///
    /// What a drag that left the window sends: the guest keeps receiving
    /// motion, and every coordinate on the wire is one it has a pixel for.
    #[must_use]
    pub fn to_guest_clamped(&self, x: i32, y: i32) -> (u32, u32) {
        let inside = |value: i32, origin: i32, size: u32| -> u32 {
            let offset = value.saturating_sub(origin).max(0);
            u32::try_from(offset)
                .unwrap_or(u32::MAX)
                .min(size.saturating_sub(1))
        };

        (
            self.scale_x(inside(x, self.x, self.width)),
            self.scale_y(inside(y, self.y, self.height)),
        )
    }

    fn scale_x(&self, inside: u32) -> u32 {
        scale(inside, self.width, self.guest_width)
    }

    fn scale_y(&self, inside: u32) -> u32 {
        scale(inside, self.height, self.guest_height)
    }
}

/// One axis, in wide arithmetic so that a 4K frame cannot overflow it.
fn scale(inside: u32, on_screen: u32, guest: u32) -> u32 {
    if on_screen == 0 || guest == 0 {
        return 0;
    }

    let scaled = u64::from(inside) * u64::from(guest) / u64::from(on_screen);

    u32::try_from(scaled)
        .unwrap_or(u32::MAX)
        .min(guest.saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::{Placement, place};

    #[test]
    fn a_picture_smaller_than_the_window_is_placed_whole() {
        let placement = place(800, 600, 1280, 720).expect("a placement");

        assert_eq!((placement.x, placement.y), (0, 0));
        assert_eq!((placement.width, placement.height), (800, 600));
    }

    #[test]
    fn a_picture_larger_than_the_window_is_cut_at_its_edges() {
        let placement = place(1920, 1080, 1280, 720).expect("a placement");

        assert_eq!((placement.width, placement.height), (1280, 720));
        assert_eq!(
            (placement.guest_width, placement.guest_height),
            (1920, 1080)
        );
    }

    #[test]
    fn a_window_with_no_area_has_no_placement() {
        assert!(place(800, 600, 0, 720).is_none());
        assert!(place(800, 600, 1280, -1).is_none());
    }

    #[test]
    fn a_point_on_the_picture_maps_to_the_pixel_under_it() {
        let placement = place(800, 600, 1280, 720).expect("a placement");

        assert_eq!(placement.to_guest(0, 0), Some((0, 0)));
        assert_eq!(placement.to_guest(799, 599), Some((799, 599)));
    }

    #[test]
    fn a_point_off_the_picture_maps_to_nothing() {
        let placement = place(800, 600, 1280, 720).expect("a placement");

        assert_eq!(placement.to_guest(800, 300), None);
        assert_eq!(placement.to_guest(300, 600), None);
        assert_eq!(placement.to_guest(-1, 300), None);
    }

    #[test]
    fn a_placement_smaller_than_its_frame_scales_rather_than_crops() {
        // Not what `place` builds today, and exactly what #120 will: the
        // mapping is written for it so that #120 changes one function.
        let placement = Placement {
            x: 40,
            y: 10,
            width: 400,
            height: 300,
            guest_width: 800,
            guest_height: 600,
        };

        assert_eq!(placement.to_guest(40, 10), Some((0, 0)));
        assert_eq!(placement.to_guest(240, 160), Some((400, 300)));
        assert_eq!(placement.to_guest(439, 309), Some((798, 598)));
        assert_eq!(placement.to_guest(39, 10), None);
    }

    #[test]
    fn a_point_off_the_picture_still_clamps_onto_it() {
        // What a drag that leaves the window sends: motion continues, and
        // every coordinate on the wire is a pixel the guest has.
        let placement = place(800, 600, 1280, 720).expect("a placement");

        assert_eq!(placement.to_guest_clamped(-30, -30), (0, 0));
        assert_eq!(placement.to_guest_clamped(5000, 5000), (799, 599));
    }
}
