//! What a viewer remembers about one VM between sessions.
//!
//! Where the window was, how big it was, whether it was full screen, and which
//! encoding mode the user picked. Not settings -- nobody edits this, and losing
//! it costs one window position -- so it lives beside the application's
//! settings rather than in them: one small file per VM under
//! `%LOCALAPPDATA%\VMLord\display`.
//!
//! The format is `key = value` lines, written and read here and nowhere else.
//! A file that is missing, truncated, hand-edited or from a future version
//! reads as the defaults: a window that opens at 1920x1080 in the middle of
//! the screen is a worse outcome than the one that was asked for, and a much
//! better one than a viewer that will not start.
//!
//! No Win32 and no file system in the type itself, so the parsing is tested
//! without either.

use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

/// The size a VM with nothing remembered opens at.
pub const DEFAULT_SIZE: (u32, u32) = (1920, 1080);

/// How the encoder is asked to trade bandwidth against fidelity.
///
/// Two arms rather than three: `Motion` is task #123's, and a menu offering a
/// mode the guest refuses would be a menu that lies. `Auto` is a host-side
/// policy that resolves to `Desktop` until there is another mode to resolve
/// to, which is why it is remembered separately from what it resolves to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Quality {
    /// Let the viewer choose. Today that is always `Desktop`.
    #[default]
    Auto,
    /// Lossless tiles, whatever the picture is doing.
    Desktop,
}

impl Quality {
    /// The name this is written under.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Desktop => "desktop",
        }
    }

    /// The quality a name means, if it means one.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "auto" => Some(Self::Auto),
            "desktop" => Some(Self::Desktop),
            _ => None,
        }
    }
}

/// One VM's window, as it was left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowState {
    /// The restored window's left edge on the virtual desktop, if it has been
    /// placed once. `None` opens wherever Windows chooses.
    pub position: Option<(i32, i32)>,
    /// The restored client area.
    pub size: (u32, u32),
    /// Whether it was full screen when it was closed.
    pub fullscreen: bool,
    /// The encoding mode the user picked.
    pub quality: Quality,
}

impl Default for WindowState {
    fn default() -> Self {
        Self {
            position: None,
            size: DEFAULT_SIZE,
            fullscreen: false,
            quality: Quality::default(),
        }
    }
}

impl WindowState {
    /// The state a `key = value` file describes, filling in what it omits.
    #[must_use]
    pub fn parse(contents: &str) -> Self {
        let mut state = Self::default();
        let mut x = None;
        let mut y = None;

        for line in contents.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "x" => x = value.parse().ok(),
                "y" => y = value.parse().ok(),
                "width" => {
                    if let Ok(width) = value.parse() {
                        state.size.0 = width;
                    }
                }
                "height" => {
                    if let Ok(height) = value.parse() {
                        state.size.1 = height;
                    }
                }
                "fullscreen" => state.fullscreen = value == "true",
                "quality" => {
                    if let Some(quality) = Quality::parse(value) {
                        state.quality = quality;
                    }
                }
                // A key from a later version, which this one has no use for.
                _ => {}
            }
        }
        // A position is both halves or neither: half of one would put the
        // window somewhere nobody left it.
        state.position = x.zip(y);
        // A size with no pixels in it is one no window can open at.
        if state.size.0 == 0 || state.size.1 == 0 {
            state.size = DEFAULT_SIZE;
        }

        state
    }

    /// The file this state is written as.
    #[must_use]
    pub fn render(&self) -> String {
        let mut text = String::new();
        if let Some((x, y)) = self.position {
            let _ = writeln!(text, "x = {x}");
            let _ = writeln!(text, "y = {y}");
        }
        let _ = writeln!(text, "width = {}", self.size.0);
        let _ = writeln!(text, "height = {}", self.size.1);
        let _ = writeln!(text, "fullscreen = {}", self.fullscreen);
        let _ = writeln!(text, "quality = {}", self.quality.as_str());

        text
    }
}

/// One VM's file, and the reading and writing of it.
pub struct Store {
    path: PathBuf,
}

impl Store {
    /// The store at an explicit path.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The store for one VM under this user's application data.
    ///
    /// `None` when there is no `%LOCALAPPDATA%`, which is a session with no
    /// profile: the viewer runs, it just does not remember anything.
    #[must_use]
    pub fn for_vm(vm_name: &str) -> Option<Self> {
        let local = std::env::var_os("LOCALAPPDATA")?;

        Some(Self::new(
            PathBuf::from(local)
                .join("VMLord")
                .join("display")
                .join(format!("{}.conf", file_name(vm_name))),
        ))
    }

    /// Where this store writes.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// What was left last time, or the defaults.
    #[must_use]
    pub fn load(&self) -> WindowState {
        fs::read_to_string(&self.path)
            .as_deref()
            .map(WindowState::parse)
            .unwrap_or_default()
    }

    /// Writes the state, creating the directory if it is not there.
    ///
    /// # Errors
    ///
    /// [`std::io::Error`] from the directory or the write. Callers log it and
    /// carry on: what is lost is a window position.
    pub fn save(&self, state: &WindowState) -> std::io::Result<()> {
        if let Some(directory) = self.path.parent() {
            fs::create_dir_all(directory)?;
        }

        fs::write(&self.path, state.render())
    }
}

/// A VM's name as a file name.
///
/// Everything but letters, digits and the three safe punctuation marks becomes
/// an underscore. Two VMs whose names differ only in what is replaced would
/// share a file, and what they would share is a window position -- which is
/// worth less than the certainty that no name can escape the directory.
fn file_name(vm_name: &str) -> String {
    let mut name: String = vm_name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect();
    // A name that is all punctuation, or empty, still needs a file.
    if name.trim_matches('.').is_empty() {
        name = "vm".to_owned();
    }
    name.truncate(96);

    name
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_SIZE, Quality, Store, WindowState, file_name};

    #[test]
    fn a_state_survives_being_written_and_read_back() {
        let state = WindowState {
            position: Some((-1200, 40)),
            size: (2560, 1440),
            fullscreen: true,
            quality: Quality::Desktop,
        };

        assert_eq!(WindowState::parse(&state.render()), state);
    }

    #[test]
    fn a_vm_with_nothing_remembered_opens_at_the_default() {
        let state = WindowState::default();

        assert_eq!(state.size, DEFAULT_SIZE);
        assert_eq!(state.position, None);
        assert!(!state.fullscreen);
        assert_eq!(state.quality, Quality::Auto);
    }

    #[test]
    fn a_file_that_is_nonsense_reads_as_the_defaults() {
        // A viewer that will not start is a worse outcome than a window in the
        // wrong place.
        let state = WindowState::parse("\u{0}\u{1}not a file at all\nwidth = ??\n");

        assert_eq!(state, WindowState::default());
    }

    #[test]
    fn a_key_from_a_later_version_is_ignored_rather_than_fatal() {
        let state = WindowState::parse("width = 1280\nheight = 720\nmonitor = 3\n");

        assert_eq!(state.size, (1280, 720));
    }

    #[test]
    fn half_a_position_is_no_position() {
        let state = WindowState::parse("x = 40\nwidth = 1280\nheight = 720\n");

        assert_eq!(state.position, None);
    }

    #[test]
    fn a_size_with_no_pixels_in_it_is_replaced() {
        let state = WindowState::parse("width = 0\nheight = 0\n");

        assert_eq!(state.size, DEFAULT_SIZE);
    }

    #[test]
    fn a_name_cannot_escape_the_directory_it_is_written_in() {
        assert_eq!(file_name("../../windows/system32"), ".._.._windows_system32");
        assert_eq!(file_name(r"C:\evil"), "C__evil");
        assert_eq!(file_name(".."), "vm");
        assert_eq!(file_name(""), "vm");
    }

    #[test]
    fn a_state_written_to_a_store_comes_back_from_it() {
        let path = std::env::temp_dir()
            .join("vmlord-display-state")
            .join("one.conf");
        let store = Store::new(&path);
        let state = WindowState {
            position: Some((10, 20)),
            size: (1280, 720),
            fullscreen: false,
            quality: Quality::Desktop,
        };

        store.save(&state).expect("a written state");

        assert_eq!(store.load(), state);
    }

    #[test]
    fn a_store_with_no_file_yet_loads_the_defaults() {
        let store = Store::new("/nonexistent/vmlord/one.conf");

        assert_eq!(store.load(), WindowState::default());
    }
}
