//! Whether a remembered window still has a screen to open on.
//!
//! A position is remembered from the session that left it, and a desktop can
//! change between two sessions: a laptop comes back without its dock, a second
//! monitor is unplugged, the arrangement is rebuilt with the negative half on
//! the other side. A window opened at coordinates nobody can see any more is a
//! viewer that looks like it failed to start.
//!
//! So the position is checked against the monitors there are now, and a
//! window with nowhere to be is put somewhere there is. Everything here is
//! virtual-desktop pixels: the viewer is per-monitor DPI aware, so a
//! coordinate means the same thing on a 100% monitor and on a 150% one, and
//! nothing has to be scaled between sessions.
//!
//! No Win32 here, so the rules are tested on any machine.

/// A rectangle on the virtual desktop, in physical pixels.
///
/// Right and bottom edges are exclusive, the way Win32's own are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    /// The left edge.
    pub left: i32,
    /// The top edge.
    pub top: i32,
    /// One past the right edge.
    pub right: i32,
    /// One past the bottom edge.
    pub bottom: i32,
}

/// How much of a window has to be on a monitor for it to count as reachable.
///
/// A strip of title bar wide enough to grab: a window with less than this
/// showing is one the user cannot drag back into view.
pub const MIN_VISIBLE: (i32, i32) = (120, 32);

/// Where a window remembered at `position` should open.
///
/// `Some` position to open at, `None` to let Windows choose -- which is what
/// an empty desktop means, and it cannot happen while there is a window to
/// open. A position that is still reachable comes back unchanged, including
/// one that hangs off an edge: a window left half over the side was left there
/// on purpose.
#[must_use]
pub fn opening_position(
    position: (i32, i32),
    size: (u32, u32),
    monitors: &[Rect],
) -> Option<(i32, i32)> {
    // A desktop that answered nothing is not a reason to move a window the
    // user placed.
    let Some((first, rest)) = monitors.split_first() else {
        return Some(position);
    };
    let window = rectangle(position, size);
    if monitors.iter().any(|monitor| reachable(&window, monitor)) {
        return Some(position);
    }

    // Nowhere to be, so somewhere there is: the monitor the window is nearest
    // to being on, and the first -- which is the primary one -- when it is on
    // none of them. `max_by_key` would answer the last of equal ones, and
    // equal here means zero.
    let mut target = first;
    let mut most = overlap(&window, first);
    for monitor in rest {
        let area = overlap(&window, monitor);
        if area > most {
            most = area;
            target = monitor;
        }
    }

    Some(centred(target, size))
}

/// The rectangle a window of this size at this position covers.
fn rectangle(position: (i32, i32), size: (u32, u32)) -> Rect {
    Rect {
        left: position.0,
        top: position.1,
        right: position.0.saturating_add(width(size.0)),
        bottom: position.1.saturating_add(width(size.1)),
    }
}

/// A dimension as a coordinate, with anything absurd clamped rather than lost.
fn width(size: u32) -> i32 {
    i32::try_from(size).unwrap_or(i32::MAX)
}

/// Whether enough of this window is on this monitor to be grabbed.
///
/// A window smaller than the strip is asked for all of itself instead: what
/// the user grabs is the window, not the strip.
fn reachable(window: &Rect, monitor: &Rect) -> bool {
    let (across, down) = intersection(window, monitor);
    let wanted_across = MIN_VISIBLE.0.min(window.right.saturating_sub(window.left));
    let wanted_down = MIN_VISIBLE.1.min(window.bottom.saturating_sub(window.top));

    across >= wanted_across && down >= wanted_down
}

/// How many pixels of the window are on the monitor.
fn overlap(window: &Rect, monitor: &Rect) -> i64 {
    let (across, down) = intersection(window, monitor);

    i64::from(across) * i64::from(down)
}

/// The width and the height the two rectangles share, both zero when they do
/// not meet.
fn intersection(window: &Rect, monitor: &Rect) -> (i32, i32) {
    // Saturating, because both edges come from a file the viewer did not
    // write this session: a coordinate at the end of the range is nonsense
    // rather than a reason to panic in a debug build.
    let across = window
        .right
        .min(monitor.right)
        .saturating_sub(window.left.max(monitor.left));
    let down = window
        .bottom
        .min(monitor.bottom)
        .saturating_sub(window.top.max(monitor.top));

    (across.max(0), down.max(0))
}

/// A position that centres a window of this size on this monitor.
///
/// The monitor's own corner when the window does not fit on it, which is a
/// window remembered from a larger monitor: the corner is where the title bar
/// is, and a title bar off the top is one nobody can reach.
fn centred(monitor: &Rect, size: (u32, u32)) -> (i32, i32) {
    let spare = |low: i32, high: i32, size: i32| low + (high.saturating_sub(low) - size).max(0) / 2;

    (
        spare(monitor.left, monitor.right, width(size.0)),
        spare(monitor.top, monitor.bottom, width(size.1)),
    )
}

#[cfg(test)]
mod tests {
    use super::{Rect, opening_position};

    /// A 1920x1080 monitor at the origin, and a 1920x1080 one to the left of
    /// it -- the arrangement a second monitor set as primary's neighbour has.
    fn two_monitors() -> Vec<Rect> {
        vec![
            Rect {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            },
            Rect {
                left: -1920,
                top: 0,
                right: 0,
                bottom: 1080,
            },
        ]
    }

    #[test]
    fn a_window_still_on_a_monitor_opens_where_it_was_left() {
        let position = opening_position((100, 80), (1280, 720), &two_monitors());

        assert_eq!(position, Some((100, 80)));
    }

    #[test]
    fn a_window_on_the_monitor_left_of_the_primary_keeps_its_negative_place() {
        // The coordinate that looks like a fault and is not: a monitor to the
        // left of the primary one has negative x.
        let position = opening_position((-1800, 40), (1280, 720), &two_monitors());

        assert_eq!(position, Some((-1800, 40)));
    }

    #[test]
    fn a_window_hanging_off_an_edge_is_left_hanging_off_it() {
        // Half over the side is where the user put it, and enough of the title
        // bar is showing to drag it back.
        let position = opening_position((1300, 900), (1280, 720), &two_monitors());

        assert_eq!(position, Some((1300, 900)));
    }

    #[test]
    fn a_window_whose_monitor_is_gone_comes_back_on_one_that_is_there() {
        let only_the_primary = &two_monitors()[..1];

        let position =
            opening_position((-1800, 40), (1280, 720), only_the_primary).expect("a position");

        assert!(
            position.0 >= 0 && position.0 + 1280 <= 1920,
            "a window on a monitor that is gone opens on one that is not: {position:?}"
        );
        assert!(position.1 >= 0 && position.1 + 720 <= 1080);
    }

    #[test]
    fn a_window_with_a_sliver_showing_is_pulled_back_into_view() {
        // Ten pixels of edge is not a title bar anybody can grab.
        let position =
            opening_position((1910, 1070), (1280, 720), &two_monitors()).expect("a position");

        assert_ne!(position, (1910, 1070));
        assert!(position.0 + 1280 <= 1920 && position.1 + 720 <= 1080);
    }

    #[test]
    fn a_window_smaller_than_a_title_bar_is_left_where_it_is_if_all_of_it_shows() {
        // What can be grabbed is the window, so a window with less of it than
        // the strip asks for is reachable when the whole of it is on screen.
        let position = opening_position((40, 40), (80, 24), &two_monitors());

        assert_eq!(position, Some((40, 40)));
    }

    #[test]
    fn a_window_larger_than_the_monitor_left_opens_at_its_corner() {
        let small = vec![Rect {
            left: 0,
            top: 0,
            right: 1024,
            bottom: 768,
        }];

        let position = opening_position((4000, 4000), (2560, 1440), &small);

        assert_eq!(position, Some((0, 0)));
    }

    #[test]
    fn a_position_at_the_end_of_the_range_is_nonsense_rather_than_a_panic() {
        // The file is not one the viewer wrote this session, and a hand-edited
        // coordinate must not take the window down with it.
        let position =
            opening_position((i32::MIN, i32::MAX), (1280, 720), &two_monitors()).expect("a place");

        assert!(position.0 >= 0 && position.0 + 1280 <= 1920);
        assert!(position.1 >= 0 && position.1 + 720 <= 1080);
    }

    #[test]
    fn a_desktop_no_monitor_could_be_read_from_leaves_the_position_alone() {
        // `EnumDisplayMonitors` answering nothing is not a reason to move a
        // window the user placed.
        let position = opening_position((100, 80), (1280, 720), &[]);

        assert_eq!(position, Some((100, 80)));
    }
}
