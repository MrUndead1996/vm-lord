//! Normalized host display modes and selection policy.

/// Smallest and largest geometries the guest output and codec drive.
pub const MIN_WIDTH: u32 = 640;
pub const MIN_HEIGHT: u32 = 480;
pub const MAX_WIDTH: u32 = 2560;
pub const MAX_HEIGHT: u32 = 1440;
/// Largest refresh this protocol revision publishes.
pub const MAX_REFRESH_HZ: u32 = 144;
/// The mode used before any host enumeration succeeds.
pub const SAFE_MODE: DisplayMode = DisplayMode {
    width: 1920,
    height: 1080,
    refresh_hz: 60,
};

/// One resolution and integer refresh variant exposed by a host monitor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DisplayMode {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

impl DisplayMode {
    /// A mode both the virtual output and its CVT builder can drive.
    #[must_use]
    pub const fn new(width: u32, height: u32, refresh_hz: u32) -> Option<Self> {
        if width < MIN_WIDTH
            || width > MAX_WIDTH
            || height < MIN_HEIGHT
            || height > MAX_HEIGHT
            || !width.is_multiple_of(8)
            || !height.is_multiple_of(2)
            || refresh_hz == 0
            || refresh_hz > MAX_REFRESH_HZ
        {
            return None;
        }
        Some(Self {
            width,
            height,
            refresh_hz,
        })
    }

    pub(crate) fn key(self) -> (u64, u32, u32, u32) {
        (
            u64::from(self.width) * u64::from(self.height),
            self.width,
            self.height,
            self.refresh_hz,
        )
    }
}

/// Validates, sorts, and deduplicates raw `(width, height, Hz)` values.
#[must_use]
pub fn normalize_modes(modes: impl IntoIterator<Item = (u32, u32, u32)>) -> Vec<DisplayMode> {
    let mut normalized: Vec<_> = modes
        .into_iter()
        .filter_map(|(width, height, refresh_hz)| DisplayMode::new(width, height, refresh_hz))
        .collect();
    normalized.sort_unstable_by_key(|mode| mode.key());
    normalized.dedup();
    normalized
}

/// The safe default when present, otherwise the largest available mode.
#[must_use]
pub fn fallback_mode(modes: &[DisplayMode]) -> DisplayMode {
    if modes.contains(&SAFE_MODE) {
        return SAFE_MODE;
    }
    modes
        .iter()
        .copied()
        .max_by_key(|mode| mode.key())
        .unwrap_or(SAFE_MODE)
}

/// Retains a usable selection and applies the fallback policy otherwise.
#[must_use]
pub fn select_mode(selected: Option<DisplayMode>, modes: &[DisplayMode]) -> DisplayMode {
    selected
        .filter(|selected| modes.contains(selected))
        .unwrap_or_else(|| fallback_mode(modes))
}

/// The first system-menu command id a mode is offered under.
///
/// `WM_SYSCOMMAND` masks the low four bits off a command, so every id is a
/// multiple of sixteen below `0xF000`, where the system's own live. The block
/// starts above the fixed items in `windows::window`.
pub const SC_MODE_FIRST: usize = 0x9100;

/// The distance between two of them.
pub const SC_MODE_STEP: usize = 0x10;

/// How many modes are offered at once.
///
/// The guest's limit as much as the menu's: the module parses a write into a
/// fixed array of this many before it takes its lock, and a longer list is one
/// it refuses whole. A monitor with sixty admissible modes is ordinary, so the
/// cut is made here rather than discovered on the socket.
pub const MAX_MENU_MODES: usize = 32;

/// The command id the mode at `index` is offered under.
#[must_use]
pub fn menu_command(index: usize) -> Option<usize> {
    (index < MAX_MENU_MODES).then(|| SC_MODE_FIRST + index * SC_MODE_STEP)
}

/// Which mode a system command names, if it names one.
#[must_use]
pub fn menu_index(command: usize) -> Option<usize> {
    let offset = command.checked_sub(SC_MODE_FIRST)?;
    if offset % SC_MODE_STEP != 0 {
        return None;
    }
    let index = offset / SC_MODE_STEP;

    (index < MAX_MENU_MODES).then_some(index)
}

/// The modes to offer, out of everything the monitor drives.
///
/// The largest [`MAX_MENU_MODES`], because a list that has to be cut is cut
/// from the bottom: nobody picks 640x480 on a 1440p panel, and the mode that
/// is already selected is kept whatever its size.
#[must_use]
pub fn offered(modes: &[DisplayMode], keep: Option<DisplayMode>) -> Vec<DisplayMode> {
    if modes.len() <= MAX_MENU_MODES {
        return modes.to_vec();
    }

    let mut offered: Vec<_> = modes[modes.len() - MAX_MENU_MODES..].to_vec();
    if let Some(keep) = keep.filter(|keep| modes.contains(keep))
        && !offered.contains(&keep)
    {
        // The smallest of the kept ones makes room: what the user is on now
        // is not a mode to drop for being small.
        offered[0] = keep;
        offered.sort_unstable_by_key(|mode| mode.key());
    }

    offered
}

/// How a mode reads in the menu.
#[must_use]
pub fn label(mode: DisplayMode) -> String {
    format!("{} x {} @ {} Hz", mode.width, mode.height, mode.refresh_hz)
}

#[cfg(test)]
mod tests {
    use super::{DisplayMode, fallback_mode, normalize_modes, select_mode};

    fn mode(width: u32, height: u32, refresh_hz: u32) -> DisplayMode {
        DisplayMode::new(width, height, refresh_hz).expect("a valid fixture")
    }

    #[test]
    fn invalid_geometry_and_refresh_are_removed() {
        let modes = normalize_modes([
            (639, 480, 60),
            (1920, 1079, 60),
            (1920, 1080, 0),
            (1920, 1080, 145),
            (1920, 1080, 60),
        ]);

        assert_eq!(modes, vec![mode(1920, 1080, 60)]);
    }

    #[test]
    fn refresh_variants_survive_while_exact_duplicates_do_not() {
        let modes = normalize_modes([(1920, 1080, 120), (1920, 1080, 60), (1920, 1080, 120)]);

        assert_eq!(modes, vec![mode(1920, 1080, 60), mode(1920, 1080, 120)]);
    }

    #[test]
    fn fallback_prefers_full_hd_at_sixty() {
        let modes = [mode(1920, 1080, 60), mode(2560, 1440, 144)];

        assert_eq!(fallback_mode(&modes), mode(1920, 1080, 60));
    }

    #[test]
    fn fallback_uses_largest_resolution_then_refresh_when_full_hd_is_absent() {
        let modes = [
            mode(1280, 720, 60),
            mode(1600, 900, 75),
            mode(1600, 900, 120),
        ];

        assert_eq!(fallback_mode(&modes), mode(1600, 900, 120));
    }

    #[test]
    fn an_empty_list_gets_the_safe_synthetic_mode() {
        assert_eq!(fallback_mode(&[]), mode(1920, 1080, 60));
    }

    #[test]
    fn a_selection_still_available_is_retained_before_the_fallback() {
        let retained = mode(2560, 1440, 120);
        let modes = [mode(1920, 1080, 60), retained];

        assert_eq!(select_mode(Some(retained), &modes), retained);
    }

    #[test]
    fn a_menu_command_names_the_mode_it_was_built_from() {
        for index in 0..super::MAX_MENU_MODES {
            let command = super::menu_command(index).expect("a command for every offered mode");

            assert_eq!(super::menu_index(command), Some(index));
            assert_eq!(command % 0x10, 0, "the low four bits are the system's");
            assert!(command < 0xF000, "and 0xF000 upwards is the system's too");
        }
    }

    #[test]
    fn a_command_that_is_not_a_mode_is_not_read_as_one() {
        assert_eq!(super::menu_command(super::MAX_MENU_MODES), None);
        // The fixed items below the block, and a system command above it.
        assert_eq!(super::menu_index(0x9050), None);
        assert_eq!(super::menu_index(0xF060), None);
        // And an id inside the block that is not on the step.
        assert_eq!(super::menu_index(super::SC_MODE_FIRST + 4), None);
    }

    #[test]
    fn a_mode_reads_as_its_three_numbers() {
        assert_eq!(super::label(mode(1920, 1080, 60)), "1920 x 1080 @ 60 Hz");
    }

    #[test]
    fn a_monitor_with_more_modes_than_the_guest_holds_is_cut_from_the_bottom() {
        let modes = normalize_modes((0..50).map(|step| (640 + step * 8, 480, 60)));
        assert!(modes.len() > super::MAX_MENU_MODES);

        let offered = super::offered(&modes, None);

        assert_eq!(offered.len(), super::MAX_MENU_MODES);
        assert_eq!(
            offered.last(),
            modes.last(),
            "the largest mode is never the one dropped"
        );
    }

    #[test]
    fn the_mode_in_use_survives_the_cut_however_small_it_is() {
        let modes = normalize_modes((0..50).map(|step| (640 + step * 8, 480, 60)));
        let smallest = modes[0];

        let offered = super::offered(&modes, Some(smallest));

        assert_eq!(offered.len(), super::MAX_MENU_MODES);
        assert!(offered.contains(&smallest));
        assert_eq!(offered.last(), modes.last());
    }

    #[test]
    fn a_list_the_guest_already_holds_is_left_alone() {
        let modes = normalize_modes([(1280, 720, 60), (1920, 1080, 60)]);

        assert_eq!(super::offered(&modes, None), modes);
    }
}
