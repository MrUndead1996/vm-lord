//! The output's size, as the module exposes it.
//!
//! Two files in `/sys/module/vmlord_drm/parameters/`. `modes` holds the list
//! the connector offers, as comma-separated `WxH@HZ`; `mode` holds the one of
//! them the connector marks preferred. Writing either hotplugs the connector;
//! the compositor is what commits a mode, so a write is a request and never an
//! answer. What actually came up is read off the framebuffers the capture
//! thread sees, which is the one source this crate treats as the truth.
//!
//! The bounds below are the module's own, restated because a request outside
//! them is worth refusing on the socket it arrived on rather than as an
//! `-ERANGE` from a `write`.

use std::{
    fmt::Write as _,
    fs, io,
    path::{Path, PathBuf},
};

use vmlord_display_protocol::v1::DisplayTiming;

/// Where the module publishes the mode it drives.
pub const MODE_PARAMETER: &str = "/sys/module/vmlord_drm/parameters/mode";
/// Where it publishes every mode the connector offers.
pub const MODES_PARAMETER: &str = "/sys/module/vmlord_drm/parameters/modes";

/// The narrowest mode the module will drive.
pub const MIN_WIDTH: u32 = 640;
/// The shortest.
pub const MIN_HEIGHT: u32 = 480;
/// The widest, which is the MVP's target.
pub const MAX_WIDTH: u32 = 2560;
/// The tallest.
pub const MAX_HEIGHT: u32 = 1440;
/// The fastest refresh this output builds a mode at.
///
/// Not a limit of `drm_cvt_mode`, which will build faster ones: a limit of
/// what a software vblank timer and a capture thread keep up with, and the
/// same number the protocol publishes.
pub const MAX_REFRESH_HZ: u32 = 144;

/// How many modes the connector offers at once.
///
/// The module parses a write into a fixed array before it takes its lock, so
/// this is a number both sides have to agree on rather than a guideline.
pub const MAX_MODES: usize = 32;

/// How long a written list may be, including its newline.
pub const MAX_MODES_BYTES: usize = 512;

/// What a mode this build cannot drive is answered with.
pub const FALLBACK: DisplayTiming = DisplayTiming {
    width: 1920,
    height: 1080,
    refresh_hz: 60,
};

/// The module's mode parameters.
pub struct Output {
    path: PathBuf,
}

impl Output {
    /// The output whose mode lives at `path`.
    ///
    /// A path rather than the constant so the tests can drive a plain file.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The module's parameter file, which is where a guest's is.
    #[must_use]
    pub fn for_guest() -> Self {
        Self::new(MODE_PARAMETER)
    }

    /// The mode the module says it drives, or the fallback.
    ///
    /// Never an error: a development machine has no such file, and a broker
    /// that refused to start a session over a missing sysfs entry would be
    /// worse than one that offers the mode the module defaults to.
    #[must_use]
    pub fn current(&self) -> DisplayTiming {
        fs::read_to_string(&self.path)
            .ok()
            .as_deref()
            .and_then(parse)
            .unwrap_or(FALLBACK)
    }

    /// Asks the module to mark `mode` preferred and hotplug the connector.
    ///
    /// # Errors
    ///
    /// [`io::Error`] from the write, which is what a module that refused the
    /// mode or a guest that has no such module answers with.
    pub fn request(&self, mode: &DisplayTiming) -> io::Result<()> {
        fs::write(&self.path, format!("{}\n", written(mode)))
    }

    /// Replaces the whole list of modes the connector offers.
    ///
    /// Validated here rather than left to the module's `-EINVAL`: the module
    /// swaps its list only after parsing the whole write, so a list it would
    /// refuse is one that leaves the guest on the modes it already had, and a
    /// host that could not say why would have nothing to report.
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::InvalidInput`] for a list this output cannot offer --
    /// empty, longer than [`MAX_MODES`], repeating a mode, or naming one
    /// outside the bounds -- and [`io::Error`] from the write otherwise.
    pub fn replace_modes(&self, modes: &[DisplayTiming]) -> io::Result<()> {
        fs::write(self.modes_path(), list(modes)?)
    }

    /// Where the mode is written, for a log line.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The `modes` parameter beside the `mode` one.
    ///
    /// Derived rather than carried: both are files in one directory, in sysfs
    /// and in the tests alike.
    #[must_use]
    pub fn modes_path(&self) -> PathBuf {
        self.path.with_file_name("modes")
    }
}

/// One mode as the module's parameters spell it.
fn written(mode: &DisplayTiming) -> String {
    format!("{}x{}@{}", mode.width, mode.height, mode.refresh_hz)
}

/// The bytes a valid `modes` write is made of.
fn list(modes: &[DisplayTiming]) -> io::Result<String> {
    let refused = |detail: &str| io::Error::new(io::ErrorKind::InvalidInput, detail.to_owned());
    if modes.is_empty() {
        // A connector with no modes is an output nothing can be lit on.
        return Err(refused("a connector offers at least one mode"));
    }
    if modes.len() > MAX_MODES {
        return Err(refused("more modes than the module holds"));
    }

    let mut text = String::new();
    for (index, mode) in modes.iter().enumerate() {
        if !drivable(mode) {
            return Err(refused("a mode outside what this output drives"));
        }
        if modes[..index].contains(mode) {
            return Err(refused("the same mode twice"));
        }
        if index > 0 {
            text.push(',');
        }
        let _ = write!(text, "{}", written(mode));
    }
    text.push('\n');
    if text.len() > MAX_MODES_BYTES {
        return Err(refused("a list longer than the module reads"));
    }

    Ok(text)
}

/// Whether the module builds a mode for this timing exactly as asked.
///
/// Exactly: a list is normalized by the host before it arrives, so a value
/// that would have to be rounded is a disagreement about the contract rather
/// than a size somebody dragged a window to.
#[must_use]
pub fn drivable(mode: &DisplayTiming) -> bool {
    admissible(mode.width, mode.height) == Some((mode.width, mode.height))
        && (1..=MAX_REFRESH_HZ).contains(&mode.refresh_hz)
}

/// The mode a `mode` file holds, if it holds one.
fn parse(contents: &str) -> Option<DisplayTiming> {
    let (width, rest) = contents.trim().split_once('x')?;
    let (height, refresh_hz) = rest.split_once('@')?;

    Some(DisplayTiming {
        width: width.parse().ok()?,
        height: height.parse().ok()?,
        refresh_hz: refresh_hz.parse().ok()?,
    })
}

/// A size the module will drive, from one that was asked for.
///
/// Widths go to a multiple of eight and heights to an even number, because
/// that is the granularity `drm_cvt_mode` rounds to: asking for a size it
/// cannot build would mean a mode that never equals the request, and a host
/// that asked again on every frame because of it. Sizes outside the bounds are
/// `None` rather than clamped -- a window dragged to nothing is not a request
/// for 640x480.
#[must_use]
pub fn admissible(width: u32, height: u32) -> Option<(u32, u32)> {
    if width < MIN_WIDTH || height < MIN_HEIGHT {
        return None;
    }

    let width = (width.min(MAX_WIDTH) / 8) * 8;
    let height = (height.min(MAX_HEIGHT) / 2) * 2;
    if width < MIN_WIDTH || height < MIN_HEIGHT {
        return None;
    }

    Some((width, height))
}

#[cfg(test)]
mod tests {
    use vmlord_display_protocol::v1::DisplayTiming;

    use super::{FALLBACK, MAX_MODES, MAX_REFRESH_HZ, Output, admissible};

    fn timing(width: u32, height: u32, refresh_hz: u32) -> DisplayTiming {
        DisplayTiming {
            width,
            height,
            refresh_hz,
        }
    }

    /// An output whose parameter files are a directory of plain files.
    fn scratch(name: &str) -> (Output, std::path::PathBuf) {
        let directory = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("a directory");

        (Output::new(directory.join("mode")), directory)
    }

    #[test]
    fn a_mode_file_is_read_as_the_mode_it_holds() {
        let (output, directory) = scratch("vmlord-output-read");
        std::fs::write(directory.join("mode"), "2560x1440@144\n").expect("a mode file");

        assert_eq!(output.current(), timing(2560, 1440, 144));
    }

    #[test]
    fn a_machine_with_no_module_reports_the_fallback_rather_than_failing() {
        // This development machine, and a guest whose module has not loaded:
        // a broker that refused a session over it would be worse than one
        // that offers the size the module defaults to.
        assert_eq!(Output::new("/nonexistent/mode").current(), FALLBACK);
    }

    #[test]
    fn a_size_is_rounded_to_what_the_modes_are_built_on() {
        // Asking for a size `drm_cvt_mode` cannot build would mean a mode that
        // never equals the request, and a host that asked again for it on
        // every frame.
        assert_eq!(admissible(1727, 971), Some((1720, 970)));
        assert_eq!(admissible(1920, 1080), Some((1920, 1080)));
    }

    #[test]
    fn a_size_beyond_the_output_is_taken_down_to_it() {
        assert_eq!(admissible(3840, 2160), Some((2560, 1440)));
    }

    #[test]
    fn a_window_with_almost_no_area_is_not_a_request_at_all() {
        assert_eq!(admissible(320, 240), None);
        assert_eq!(admissible(0, 0), None);
        assert_eq!(admissible(1920, 100), None);
    }

    #[test]
    fn a_mode_list_is_written_in_the_form_the_module_parses() {
        let (output, directory) = scratch("vmlord-output-modes");

        output
            .replace_modes(&[timing(1920, 1080, 60), timing(2560, 1440, 144)])
            .expect("a written list");

        assert_eq!(
            std::fs::read_to_string(directory.join("modes")).expect("the list back"),
            "1920x1080@60,2560x1440@144\n"
        );
    }

    #[test]
    fn a_list_the_module_has_no_room_for_is_refused_without_touching_it() {
        // The module parses into a fixed array before it takes its lock, so a
        // list it could not hold is one the host must not send.
        let (output, directory) = scratch("vmlord-output-too-many");
        output
            .replace_modes(&[timing(1920, 1080, 60)])
            .expect("a written list");

        let long: Vec<_> = (0..=MAX_MODES)
            .map(|step| timing(640 + step as u32 * 8, 480, 60))
            .collect();

        assert!(output.replace_modes(&long).is_err());
        assert_eq!(
            std::fs::read_to_string(directory.join("modes")).expect("the list back"),
            "1920x1080@60\n",
            "a refused list leaves the one the guest is on"
        );
    }

    #[test]
    fn a_list_that_says_the_same_mode_twice_is_refused() {
        let (output, _) = scratch("vmlord-output-duplicate");

        assert!(
            output
                .replace_modes(&[timing(1920, 1080, 60), timing(1920, 1080, 60)])
                .is_err()
        );
    }

    #[test]
    fn a_mode_this_output_cannot_drive_is_refused() {
        let (output, _) = scratch("vmlord-output-invalid");

        for mode in [
            timing(1920, 1080, 0),
            timing(1920, 1080, MAX_REFRESH_HZ + 1),
            timing(3840, 2160, 60),
            timing(1727, 1080, 60),
        ] {
            assert!(
                output.replace_modes(&[mode]).is_err(),
                "{mode:?} is not a mode this output builds"
            );
        }
    }

    #[test]
    fn an_empty_list_is_not_a_list() {
        // A connector with no modes is an output a compositor cannot light.
        let (output, _) = scratch("vmlord-output-empty");

        assert!(output.replace_modes(&[]).is_err());
    }

    #[test]
    fn the_mode_asked_for_carries_its_refresh() {
        let (output, directory) = scratch("vmlord-output-request");

        output.request(&timing(1280, 720, 120)).expect("a request");

        assert_eq!(
            std::fs::read_to_string(directory.join("mode")).expect("the mode back"),
            "1280x720@120\n"
        );
        assert_eq!(output.current(), timing(1280, 720, 120));
    }
}
