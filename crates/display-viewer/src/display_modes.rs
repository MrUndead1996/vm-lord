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

    fn key(self) -> (u64, u32, u32, u32) {
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
}
