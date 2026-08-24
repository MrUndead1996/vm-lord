//! Where the guest's picture sits on the client area.
//!
//! One value, and both consumers read it: the renderer draws into it and the
//! input policy maps points through it. Because there is one of them rather
//! than one per consumer, the pointer cannot drift from the picture.
//!
//! The picture is letterboxed: scaled to fit, whole, centred, with the aspect
//! ratio kept and the ground showing at two edges. Never stretched and never
//! cropped -- a stretched desktop is one whose circles are ovals, and a
//! cropped one is a desktop with a corner the user cannot reach.
//!
//! Most of the time there is nothing to scale. The window drives the guest's
//! mode, so a settled window and a settled desktop are the same size and the
//! renderer copies rather than samples. Scaling is what the seconds between a
//! drag and the guest's answer look like.

/// The rectangle the guest's picture occupies, and the frame it shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    /// The picture's left edge in client pixels.
    pub x: i32,
    /// Its top edge in client pixels.
    pub y: i32,
    /// How wide it is on screen, which differs from the frame while the guest
    /// is catching up with the window.
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
/// The largest rectangle of the frame's aspect ratio that fits, centred.
/// Returns `None` for a client area or a frame with no pixels in it, which is
/// a window mid-resize rather than a fault.
#[must_use]
pub fn place(
    guest_width: u32,
    guest_height: u32,
    client_width: i32,
    client_height: i32,
) -> Option<Placement> {
    let client_width = u32::try_from(client_width).ok()?;
    let client_height = u32::try_from(client_height).ok()?;
    if client_width == 0 || client_height == 0 || guest_width == 0 || guest_height == 0 {
        return None;
    }

    // Wide arithmetic throughout: 2560 * 1440 overflows nothing, but the
    // cross-multiplication of two of them would.
    let by_width = u64::from(client_width) * u64::from(guest_height);
    let by_height = u64::from(client_height) * u64::from(guest_width);
    let (width, height) = if by_width <= by_height {
        // The window is the narrower of the two shapes: the width is what
        // runs out, and the bars are above and below.
        (
            client_width,
            fit(client_width, guest_height, guest_width).max(1),
        )
    } else {
        (
            fit(client_height, guest_width, guest_height).max(1),
            client_height,
        )
    };

    Some(Placement {
        // Integer division leaves any odd pixel on the right or the bottom,
        // which is a pixel of ground rather than a pixel of picture.
        x: ((client_width - width) / 2) as i32,
        y: ((client_height - height) / 2) as i32,
        width,
        height,
        guest_width,
        guest_height,
    })
}

/// One axis scaled by the ratio of the other two, in wide arithmetic.
fn fit(along: u32, numerator: u32, denominator: u32) -> u32 {
    let scaled = u64::from(along) * u64::from(numerator) / u64::from(denominator);

    u32::try_from(scaled).unwrap_or(u32::MAX)
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
    fn a_window_the_shape_of_the_frame_shows_it_whole_with_no_bars() {
        let placement = place(1920, 1080, 1280, 720).expect("a placement");

        assert_eq!((placement.x, placement.y), (0, 0));
        assert_eq!((placement.width, placement.height), (1280, 720));
    }

    #[test]
    fn a_window_wider_than_the_frame_gets_bars_at_the_sides() {
        // Pillarbox: the height runs out first, and the picture is centred in
        // what is left. Never stretched -- a desktop whose circles are ovals.
        let placement = place(1000, 1000, 1600, 1000).expect("a placement");

        assert_eq!((placement.width, placement.height), (1000, 1000));
        assert_eq!((placement.x, placement.y), (300, 0));
    }

    #[test]
    fn a_window_taller_than_the_frame_gets_bars_above_and_below() {
        let placement = place(1000, 1000, 1000, 1600).expect("a placement");

        assert_eq!((placement.width, placement.height), (1000, 1000));
        assert_eq!((placement.x, placement.y), (0, 300));
    }

    #[test]
    fn a_frame_larger_than_the_window_is_scaled_down_rather_than_cropped() {
        // A desktop with a corner the user cannot reach is what cropping
        // would cost, and the seconds between a drag and the guest's answer
        // are exactly when it would happen.
        let placement = place(1920, 1080, 960, 540).expect("a placement");

        assert_eq!((placement.width, placement.height), (960, 540));
        assert_eq!(
            (placement.guest_width, placement.guest_height),
            (1920, 1080)
        );
    }

    #[test]
    fn an_odd_leftover_pixel_is_ground_rather_than_picture() {
        let placement = place(100, 100, 201, 100).expect("a placement");

        assert_eq!(placement.x, 50);
        assert_eq!(placement.width, 100);
    }

    #[test]
    fn a_window_with_no_area_has_no_placement() {
        assert!(place(800, 600, 0, 720).is_none());
        assert!(place(800, 600, 1280, -1).is_none());
        assert!(place(0, 600, 1280, 720).is_none());
    }

    #[test]
    fn a_point_on_the_picture_maps_to_the_pixel_under_it() {
        let placement = place(800, 600, 800, 600).expect("a placement");

        assert_eq!(placement.to_guest(0, 0), Some((0, 0)));
        assert_eq!(placement.to_guest(799, 599), Some((799, 599)));
    }

    #[test]
    fn a_point_on_a_bar_is_off_the_picture() {
        // The pointer follows the picture rather than the window: a click on
        // the ground is not a click on the desktop.
        let placement = place(1000, 1000, 1600, 1000).expect("a placement");

        assert_eq!(placement.to_guest(299, 500), None);
        assert_eq!(placement.to_guest(1300, 500), None);
        assert_eq!(placement.to_guest(300, 500), Some((0, 500)));
    }

    #[test]
    fn a_scaled_placement_maps_through_the_scale() {
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
        let placement = place(800, 600, 800, 600).expect("a placement");

        assert_eq!(placement.to_guest_clamped(-30, -30), (0, 0));
        assert_eq!(placement.to_guest_clamped(5000, 5000), (799, 599));
    }

    #[test]
    fn a_frame_at_2560x1440_in_a_window_of_the_same_shape_needs_no_scale() {
        // The MVP's target, and the state a settled window is in: the guest
        // is on the mode the window asked for, so the renderer copies rather
        // than samples.
        let placement = place(2560, 1440, 2560, 1440).expect("a placement");

        assert_eq!((placement.x, placement.y), (0, 0));
        assert_eq!((placement.width, placement.height), (2560, 1440));
    }
}
