//! The output's size, as the module exposes it.
//!
//! One file, `/sys/module/vmlord_drm/parameters/mode`, holding `WxH`. Writing
//! it moves the connector's only mode and hotplugs it; the compositor is what
//! commits the new mode, so a write is a request and never an answer. What
//! actually came up is read off the framebuffers the capture thread sees,
//! which is the one source this crate treats as the truth.
//!
//! The bounds below are the module's own, restated because a request outside
//! them is worth refusing on the socket it arrived on rather than as an
//! `-ERANGE` from a `write`.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// Where the module publishes the mode it drives.
pub const MODE_PARAMETER: &str = "/sys/module/vmlord_drm/parameters/mode";

/// The narrowest mode the module will drive.
pub const MIN_WIDTH: u32 = 640;
/// The shortest.
pub const MIN_HEIGHT: u32 = 480;
/// The widest, which is the MVP's target.
pub const MAX_WIDTH: u32 = 2560;
/// The tallest.
pub const MAX_HEIGHT: u32 = 1440;

/// What a mode this build cannot drive is answered with.
pub const FALLBACK: (u32, u32) = (1920, 1080);

/// The module's mode parameter.
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

    /// The size the module says it drives, or the fallback.
    ///
    /// Never an error: a development machine has no such file, and a broker
    /// that refused to start a session over a missing sysfs entry would be
    /// worse than one that offers the size the module defaults to.
    #[must_use]
    pub fn current(&self) -> (u32, u32) {
        fs::read_to_string(&self.path)
            .ok()
            .as_deref()
            .and_then(parse)
            .unwrap_or(FALLBACK)
    }

    /// Asks the module for a mode.
    ///
    /// # Errors
    ///
    /// [`io::Error`] from the write, which is what a module that refused the
    /// size or a guest that has no such module answers with.
    pub fn request(&self, width: u32, height: u32) -> io::Result<()> {
        fs::write(&self.path, format!("{width}x{height}\n"))
    }

    /// Where the mode is written, for a log line.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// The size a mode file holds, if it holds one.
fn parse(contents: &str) -> Option<(u32, u32)> {
    let (width, height) = contents.trim().split_once('x')?;

    Some((width.parse().ok()?, height.parse().ok()?))
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
    use super::{FALLBACK, Output, admissible};

    #[test]
    fn a_mode_file_is_read_as_the_size_it_holds() {
        let directory = std::env::temp_dir().join("vmlord-output-read");
        std::fs::create_dir_all(&directory).expect("a directory");
        let path = directory.join("mode");
        std::fs::write(&path, "2560x1440\n").expect("a mode file");

        assert_eq!(Output::new(&path).current(), (2560, 1440));
    }

    #[test]
    fn a_machine_with_no_module_reports_the_fallback_rather_than_failing() {
        // This development machine, and a guest whose module has not loaded:
        // a broker that refused a session over it would be worse than one
        // that offers the size the module defaults to.
        assert_eq!(Output::new("/nonexistent/mode").current(), FALLBACK);
    }

    #[test]
    fn a_request_is_written_in_the_form_the_module_parses() {
        let directory = std::env::temp_dir().join("vmlord-output-write");
        std::fs::create_dir_all(&directory).expect("a directory");
        let path = directory.join("mode");
        let output = Output::new(&path);

        output.request(1280, 720).expect("a written mode");

        assert_eq!(
            std::fs::read_to_string(&path).expect("the mode back"),
            "1280x720\n"
        );
        assert_eq!(output.current(), (1280, 720));
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
}
