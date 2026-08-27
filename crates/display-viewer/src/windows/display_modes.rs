//! The monitor the window is on, and the modes that monitor drives.
//!
//! Windows answers two different questions about a monitor and neither one
//! alone is the answer. `EnumDisplaySettingsW` walks the modes the *adapter*
//! will set on a device, which is the list the user may choose from; the
//! DisplayConfig API knows which *target* the device drives and what that
//! panel's preferred timing is, which is the mode a full-screen window wants.
//! So both are asked, the preference is optional, and a failure to resolve it
//! never costs the enumerated list.
//!
//! Refresh is an integer here because `dmDisplayFrequency` is one: Windows
//! reports 60 for a 59.94 Hz panel, and the protocol says so rather than
//! pretending to a precision the source does not have.

use std::time::Instant;

use windows::{
    Win32::{
        Devices::Display::{
            DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
            DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_PREFERRED_MODE, DISPLAYCONFIG_DEVICE_INFO_HEADER,
            DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
            DISPLAYCONFIG_TARGET_PREFERRED_MODE, DisplayConfigGetDeviceInfo,
            GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
        },
        Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, HWND},
        Graphics::Gdi::{
            DEVMODEW, ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_MODE, EnumDisplaySettingsW,
            GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFOEXW, MonitorFromWindow,
        },
    },
    core::PCWSTR,
};

use crate::{
    display_modes::{DisplayMode, normalize_modes},
    resize::DEBOUNCE,
};

/// How many times a `QueryDisplayConfig` racing the desktop is tried again.
///
/// The buffer is sized and filled in two calls, and an arrangement that
/// changes between them answers `ERROR_INSUFFICIENT_BUFFER`. That is worth
/// retrying and not worth retrying forever: a desktop changing this fast will
/// mark the snapshot stale again anyway.
const CONFIG_ATTEMPTS: usize = 4;

/// One `DEVMODEW` row as Windows reported it, before any policy.
///
/// Kept apart from [`DisplayMode`] so that everything above this line is
/// decided by rules that run on any machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawMode {
    /// `dmPelsWidth`, in pixels.
    pub width: u32,
    /// `dmPelsHeight`, in pixels.
    pub height: u32,
    /// `dmDisplayFrequency`, in whole hertz.
    pub refresh_hz: u32,
}

/// What the nearest monitor is and what it will drive.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MonitorSnapshot {
    /// The GDI device name, `\\.\DISPLAY1`. Stable while the arrangement is.
    pub identity: String,
    /// The mode the desktop is in now, when it is one the guest can be set to.
    pub current: Option<DisplayMode>,
    /// The panel's own preferred timing, when Windows resolves one.
    pub preferred: Option<DisplayMode>,
    /// Every admissible mode, normalized, sorted and deduplicated.
    pub modes: Vec<DisplayMode>,
}

impl MonitorSnapshot {
    /// Applies the crate's mode policy to what a device reported.
    ///
    /// The preferred mode joins the list: it is a mode the viewer may ask the
    /// guest for when a window fills the monitor, and a list without it would
    /// be a request nobody could make.
    ///
    /// A monitor's own mode is answered at its own size even when its refresh
    /// did not survive normalization: a panel that runs at 1920x1080 at 144 Hz
    /// is one the viewer opens at 1920x1080, at the fastest rate the guest is
    /// offered there. Dropping it to `None` instead would take a full-screen
    /// window off the monitor's native resolution over a rate.
    #[must_use]
    pub fn new(
        identity: String,
        current: Option<RawMode>,
        preferred: Option<RawMode>,
        modes: impl IntoIterator<Item = RawMode>,
    ) -> Self {
        let raw = |mode: RawMode| (mode.width, mode.height, mode.refresh_hz);
        let modes = normalize_modes(modes.into_iter().chain(preferred).map(raw));
        let admissible = |mode: RawMode| {
            let mode = DisplayMode::new(mode.width, mode.height, mode.refresh_hz)?;
            let at_this_size = || {
                modes
                    .iter()
                    .filter(|offered| offered.width == mode.width && offered.height == mode.height)
                    .max_by_key(|offered| offered.refresh_hz)
                    .copied()
            };
            modes.contains(&mode).then_some(mode).or_else(at_this_size)
        };

        Self {
            identity,
            current: current.and_then(admissible),
            preferred: preferred.and_then(admissible),
            modes,
        }
    }
}

/// The stale signal, debounced, and the list already sent.
///
/// A monitor change is a burst rather than an event: the display change, the
/// window's move onto the new arrangement and the DPI transition all arrive
/// within a few milliseconds of one another, and each one enumerated on its
/// own would be three mode lists for one thing the user did.
pub struct MonitorWatch {
    /// When the burst seen so far becomes due, if it does not grow again.
    pending: Option<Instant>,
    /// The snapshot last sent, which is never sent twice running.
    sent: Option<MonitorSnapshot>,
}

impl MonitorWatch {
    /// A watch that has seen nothing.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: None,
            sent: None,
        }
    }

    /// Marks the monitor stale, restarting the wait.
    pub fn observe(&mut self, now: Instant) {
        self.pending = Some(now + DEBOUNCE);
    }

    /// Whether the burst has settled and the monitor should be enumerated.
    pub fn due(&mut self, now: Instant) -> bool {
        let Some(deadline) = self.pending else {
            return false;
        };
        if now < deadline {
            return false;
        }
        self.pending = None;

        true
    }

    /// Whether `snapshot` is a list the guest has not been told.
    pub fn accept(&mut self, snapshot: MonitorSnapshot) -> bool {
        if self.sent.as_ref() == Some(&snapshot) {
            return false;
        }
        self.sent = Some(snapshot);

        true
    }

    /// Forgets what has been sent, so the next snapshot is sent again.
    ///
    /// What a new session means: the guest that was told is not the guest
    /// that is listening now.
    pub fn forget(&mut self) {
        self.pending = None;
        self.sent = None;
    }
}

impl Default for MonitorWatch {
    fn default() -> Self {
        Self::new()
    }
}

/// The monitor `hwnd` is nearest to, or `None` when Windows will not say.
///
/// Enumeration walks only that one device: what the viewer publishes is the
/// screen its window is on, not every screen the desktop happens to have.
#[must_use]
pub fn snapshot_for_window(hwnd: HWND) -> Option<MonitorSnapshot> {
    // SAFETY: `hwnd` names a window of this process; the nearest monitor is
    // defined for every window, valid or not.
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFOEXW {
        monitorInfo: windows::Win32::Graphics::Gdi::MONITORINFO {
            cbSize: u32::try_from(size_of::<MONITORINFOEXW>()).ok()?,
            ..Default::default()
        },
        ..Default::default()
    };
    // SAFETY: `monitor` came from `MonitorFromWindow`, and `info` lives across
    // the call with the extended size filled in, which is what makes Windows
    // write `szDevice` past the plain `MONITORINFO`.
    if !unsafe { GetMonitorInfoW(monitor, (&raw mut info).cast()) }.as_bool() {
        return None;
    }

    let device = PCWSTR(info.szDevice.as_ptr());
    let mut modes = Vec::new();
    for index in 0.. {
        let Some(mode) = settings(device, ENUM_DISPLAY_SETTINGS_MODE(index)) else {
            break;
        };
        modes.push(mode);
    }

    Some(MonitorSnapshot::new(
        String::from_utf16_lossy(&info.szDevice[..name_length(&info.szDevice)]),
        settings(device, ENUM_CURRENT_SETTINGS),
        preferred_mode(&info.szDevice),
        modes,
    ))
}

/// One row of `EnumDisplaySettingsW`, or `None` past the last one.
fn settings(device: PCWSTR, index: ENUM_DISPLAY_SETTINGS_MODE) -> Option<RawMode> {
    let mut devmode = DEVMODEW {
        dmSize: u16::try_from(size_of::<DEVMODEW>()).ok()?,
        ..Default::default()
    };
    // SAFETY: `device` points at the NUL-terminated name inside `info`, which
    // outlives the call, and `devmode` lives across it with its size set.
    if !unsafe { EnumDisplaySettingsW(device, index, &raw mut devmode) }.as_bool() {
        return None;
    }

    Some(RawMode {
        width: devmode.dmPelsWidth,
        height: devmode.dmPelsHeight,
        refresh_hz: devmode.dmDisplayFrequency,
    })
}

/// The preferred timing of the target `device` drives, when there is one.
///
/// Optional by design: the current desktop mode is not necessarily native,
/// and a machine that will not answer this question still has a mode list.
fn preferred_mode(device: &[u16; 32]) -> Option<RawMode> {
    for path in active_paths()? {
        let mut name = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
            header: header(
                DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>(),
                path.sourceInfo.adapterId,
                path.sourceInfo.id,
            )?,
            ..Default::default()
        };
        // SAFETY: the packet lives across the call with its header filled in,
        // which is the whole contract of this API.
        if unsafe { DisplayConfigGetDeviceInfo(&raw mut name.header) } != 0
            || name.viewGdiDeviceName[..name_length(&name.viewGdiDeviceName)]
                != device[..name_length(device)]
        {
            continue;
        }

        let mut mode = DISPLAYCONFIG_TARGET_PREFERRED_MODE {
            header: header(
                DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_PREFERRED_MODE,
                size_of::<DISPLAYCONFIG_TARGET_PREFERRED_MODE>(),
                path.targetInfo.adapterId,
                path.targetInfo.id,
            )?,
            ..Default::default()
        };
        // SAFETY: as above; this packet is the preferred-mode question.
        if unsafe { DisplayConfigGetDeviceInfo(&raw mut mode.header) } != 0 {
            return None;
        }
        let vsync = mode.targetMode.targetVideoSignalInfo.vSyncFreq;
        if vsync.Denominator == 0 {
            return None;
        }

        return Some(RawMode {
            width: mode.width,
            height: mode.height,
            // Rounded rather than truncated: 59.94 Hz is reported as 60 here
            // for the same reason `dmDisplayFrequency` reports it as 60.
            refresh_hz: (vsync.Numerator + vsync.Denominator / 2) / vsync.Denominator,
        });
    }

    None
}

/// The paths the desktop is currently lit by.
fn active_paths() -> Option<Vec<DISPLAYCONFIG_PATH_INFO>> {
    for _ in 0..CONFIG_ATTEMPTS {
        let (mut path_count, mut mode_count) = (0, 0);
        // SAFETY: two counters that live across the call.
        let sized = unsafe {
            GetDisplayConfigBufferSizes(
                QDC_ONLY_ACTIVE_PATHS,
                &raw mut path_count,
                &raw mut mode_count,
            )
        };
        if sized != ERROR_SUCCESS {
            return None;
        }

        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
        // SAFETY: both buffers are as long as the counters say and live
        // across the call, which is what the counters are for.
        let queried = unsafe {
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &raw mut path_count,
                paths.as_mut_ptr(),
                &raw mut mode_count,
                modes.as_mut_ptr(),
                None,
            )
        };
        match queried {
            ERROR_SUCCESS => {
                paths.truncate(path_count as usize);

                return Some(paths);
            }
            // The arrangement changed between sizing and filling. Ask again.
            ERROR_INSUFFICIENT_BUFFER => continue,
            _ => return None,
        }
    }

    None
}

/// The header that says which question a DisplayConfig packet is asking.
fn header(
    kind: windows::Win32::Devices::Display::DISPLAYCONFIG_DEVICE_INFO_TYPE,
    size: usize,
    adapter: windows::Win32::Foundation::LUID,
    id: u32,
) -> Option<DISPLAYCONFIG_DEVICE_INFO_HEADER> {
    Some(DISPLAYCONFIG_DEVICE_INFO_HEADER {
        r#type: kind,
        size: u32::try_from(size).ok()?,
        adapterId: adapter,
        id,
    })
}

/// How much of a fixed Win32 name buffer is the name.
fn name_length(name: &[u16]) -> usize {
    name.iter()
        .position(|unit| *unit == 0)
        .unwrap_or(name.len())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{MonitorSnapshot, MonitorWatch, RawMode};
    use crate::{display_modes::DisplayMode, resize::DEBOUNCE};

    fn raw(width: u32, height: u32, refresh_hz: u32) -> RawMode {
        RawMode {
            width,
            height,
            refresh_hz,
        }
    }

    fn mode(width: u32, height: u32, refresh_hz: u32) -> DisplayMode {
        DisplayMode::new(width, height, refresh_hz).expect("a valid fixture")
    }

    #[test]
    fn what_the_device_reported_becomes_the_normalized_list() {
        let snapshot = MonitorSnapshot::new(
            r"\\.\DISPLAY1".to_owned(),
            Some(raw(1920, 1080, 60)),
            None,
            [
                raw(320, 200, 60),
                raw(1920, 1080, 60),
                raw(2560, 1440, 165),
                raw(1280, 720, 60),
            ],
        );

        assert_eq!(
            snapshot.modes,
            vec![mode(1280, 720, 60), mode(1920, 1080, 60)],
            "only what the guest output can be driven at survives"
        );
        assert_eq!(snapshot.current, Some(mode(1920, 1080, 60)));
    }

    #[test]
    fn one_resolution_keeps_every_refresh_the_device_offers() {
        // At 1280x720 the guest encodes fast enough for both, so this is the
        // list with nothing but the duplicate taken out of it.
        let snapshot = MonitorSnapshot::new(
            r"\\.\DISPLAY1".to_owned(),
            None,
            None,
            [raw(1280, 720, 120), raw(1280, 720, 60), raw(1280, 720, 120)],
        );

        assert_eq!(
            snapshot.modes,
            vec![mode(1280, 720, 60), mode(1280, 720, 120)]
        );
    }

    #[test]
    fn a_monitor_at_a_rate_the_guest_cannot_encode_is_answered_at_its_own_size() {
        let snapshot = MonitorSnapshot::new(
            r"\\.\DISPLAY1".to_owned(),
            Some(raw(1920, 1080, 144)),
            Some(raw(1920, 1080, 144)),
            [raw(1920, 1080, 60), raw(1920, 1080, 144)],
        );

        assert_eq!(
            snapshot.modes,
            vec![mode(1920, 1080, 60)],
            "144 Hz at this size is a rate the stack has never delivered"
        );
        assert_eq!(
            snapshot.current,
            Some(mode(1920, 1080, 60)),
            "and the size the panel is on is still the size the window opens at"
        );
        assert_eq!(snapshot.preferred, Some(mode(1920, 1080, 60)));
    }

    #[test]
    fn a_preferred_mode_nobody_enumerated_joins_the_list() {
        let snapshot = MonitorSnapshot::new(
            r"\\.\DISPLAY1".to_owned(),
            None,
            Some(raw(2560, 1440, 60)),
            [raw(1920, 1080, 60)],
        );

        assert_eq!(snapshot.preferred, Some(mode(2560, 1440, 60)));
        assert_eq!(
            snapshot.modes,
            vec![mode(1920, 1080, 60), mode(2560, 1440, 60)],
            "the native mode is one the guest may be asked for"
        );
    }

    #[test]
    fn a_preference_windows_would_not_answer_for_discards_nothing() {
        let snapshot = MonitorSnapshot::new(
            r"\\.\DISPLAY1".to_owned(),
            Some(raw(1920, 1080, 60)),
            None,
            [raw(1920, 1080, 60), raw(1280, 720, 60)],
        );

        assert_eq!(snapshot.preferred, None);
        assert_eq!(
            snapshot.modes,
            vec![mode(1280, 720, 60), mode(1920, 1080, 60)]
        );
    }

    #[test]
    fn a_desktop_being_rearranged_is_enumerated_once() {
        // A monitor change is a burst: the display change, the move onto the
        // new arrangement, and the DPI transition all arrive together.
        let start = Instant::now();
        let mut watch = MonitorWatch::new();

        for step in 0..12u64 {
            watch.observe(start + Duration::from_millis(step * 10));
        }

        assert!(!watch.due(start + DEBOUNCE), "the burst has not settled");
        assert!(watch.due(start + Duration::from_millis(110) + DEBOUNCE));
        assert!(
            !watch.due(start + DEBOUNCE * 10),
            "one settled burst is one enumeration"
        );
    }

    #[test]
    fn the_same_monitor_reported_again_is_not_a_new_list() {
        let snapshot = || {
            MonitorSnapshot::new(
                r"\\.\DISPLAY1".to_owned(),
                Some(raw(1920, 1080, 60)),
                None,
                [raw(1920, 1080, 60)],
            )
        };
        let mut watch = MonitorWatch::new();

        assert!(watch.accept(snapshot()), "the first list is always new");
        assert!(!watch.accept(snapshot()));
        assert!(watch.accept(MonitorSnapshot::new(
            r"\\.\DISPLAY2".to_owned(),
            Some(raw(1280, 720, 60)),
            None,
            [raw(1280, 720, 60)],
        )));
    }

    #[test]
    fn a_new_session_is_told_the_monitor_again() {
        let snapshot = || {
            MonitorSnapshot::new(
                r"\\.\DISPLAY1".to_owned(),
                None,
                None,
                [raw(1920, 1080, 60)],
            )
        };
        let mut watch = MonitorWatch::new();
        assert!(watch.accept(snapshot()));

        watch.forget();

        assert!(
            watch.accept(snapshot()),
            "the guest that was told is not the guest that is listening now"
        );
    }
}
