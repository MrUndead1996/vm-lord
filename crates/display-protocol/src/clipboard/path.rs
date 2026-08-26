//! Whether a path from a peer may become a path on this filesystem.
//!
//! A wire path is checked lexically and completely before anything is opened.
//! The check is the same on both platforms, so a tree that a Linux guest
//! accepts is a tree a Windows viewer can also create: a name that is legal on
//! ext4 and impossible on NTFS would otherwise turn one peer's copy into the
//! other peer's half-written directory.
//!
//! Nothing here touches a filesystem. Containment is the receiver's job and is
//! obtained from directory handles, never from the text of a path; what this
//! rules out is the text that would make that job unsafe in the first place.

use std::{error::Error, fmt};

/// The most a relative wire path may measure, in UTF-8 bytes.
///
/// Bytes rather than characters: what has to fit is a `PATH_MAX` on one side
/// and a `MAX_PATH`-shaped API on the other, and both of those count storage.
pub const MAX_PATH_BYTES: usize = 1024;

/// The most components one wire path may have.
pub const MAX_DEPTH: usize = 64;

/// Names Windows resolves to a device however deep the directory is.
const RESERVED: [&str; 4] = ["CON", "PRN", "AUX", "NUL"];

/// The families of those names that carry a digit.
const RESERVED_NUMBERED: [&str; 2] = ["COM", "LPT"];

/// Characters no Windows filesystem holds, beyond the ones with their own
/// refusal below.
const FORBIDDEN: [char; 5] = ['<', '>', '"', '|', '*'];

/// Why a path from a peer is not a path this side will create.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathError {
    /// A path with nothing in it.
    Empty,
    /// Longer than [`MAX_PATH_BYTES`].
    TooLong {
        /// What arrived.
        bytes: usize,
        /// What is allowed.
        limit: usize,
    },
    /// More components than [`MAX_DEPTH`].
    TooDeep {
        /// What arrived.
        depth: usize,
        /// What is allowed.
        limit: usize,
    },
    /// Rooted, so not relative to a staging directory at all.
    Absolute,
    /// A `..` component, which is the whole reason this module exists.
    Traversal,
    /// A `.` component, which names a directory that already exists.
    Dot,
    /// Two separators in a row, or a trailing one.
    EmptyComponent,
    /// A NUL, which truncates a name at every C boundary below here.
    Nul,
    /// A control character, which no filesystem here should have to hold.
    Control,
    /// A backslash, which is a separator on the other platform.
    Separator,
    /// A colon, which is a drive letter or an NTFS stream.
    Colon,
    /// One of [`FORBIDDEN`].
    Forbidden,
    /// A component Windows silently trims and then cannot find.
    TrailingDotOrSpace,
    /// A Windows device name, with or without an extension.
    Reserved,
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "the path is empty"),
            Self::TooLong { bytes, limit } => {
                write!(f, "the path is {bytes} bytes, over the {limit}-byte limit")
            }
            Self::TooDeep { depth, limit } => {
                write!(f, "the path is {depth} deep, over the limit of {limit}")
            }
            Self::Absolute => write!(f, "the path is absolute"),
            Self::Traversal => write!(f, "the path has a `..` component"),
            Self::Dot => write!(f, "the path has a `.` component"),
            Self::EmptyComponent => write!(f, "the path has an empty component"),
            Self::Nul => write!(f, "the path has a NUL"),
            Self::Control => write!(f, "the path has a control character"),
            Self::Separator => write!(f, "the path has a backslash"),
            Self::Colon => write!(f, "the path has a colon"),
            Self::Forbidden => write!(f, "the path has a character Windows refuses"),
            Self::TrailingDotOrSpace => {
                write!(f, "a component ends in a dot or a space")
            }
            Self::Reserved => write!(f, "a component is a reserved device name"),
        }
    }
}

impl Error for PathError {}

/// A relative path that both platforms can create, as it arrived.
///
/// There is no constructor but [`ValidatedPath::parse`], so a value of this
/// type is a path that has been through every rule above -- which is what lets
/// the adapters take one without checking it again.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ValidatedPath {
    path: String,
    key: String,
}

impl ValidatedPath {
    /// Checks a wire path, lexically and without touching a filesystem.
    ///
    /// # Errors
    ///
    /// [`PathError`] naming the first rule the path breaks.
    pub fn parse(path: &str) -> Result<Self, PathError> {
        if path.is_empty() {
            return Err(PathError::Empty);
        }
        if path.len() > MAX_PATH_BYTES {
            return Err(PathError::TooLong {
                bytes: path.len(),
                limit: MAX_PATH_BYTES,
            });
        }
        if path.starts_with('/') {
            return Err(PathError::Absolute);
        }

        let components: Vec<&str> = path.split('/').collect();
        if components.len() > MAX_DEPTH {
            return Err(PathError::TooDeep {
                depth: components.len(),
                limit: MAX_DEPTH,
            });
        }

        for component in &components {
            check_component(component)?;
        }

        Ok(Self {
            key: components
                .iter()
                .map(|component| component.to_lowercase())
                .collect::<Vec<_>>()
                .join("/"),
            path: path.to_owned(),
        })
    }

    /// The path as the peer wrote it, which is what gets created.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.path
    }

    /// The components, in the order a receiver opens them.
    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.path.split('/')
    }

    /// How many components there are.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.path.split('/').count()
    }

    /// What this path is called where names are compared case-insensitively.
    ///
    /// Two entries whose keys match are one file on Windows, so a tree that
    /// holds both is a tree that arrives complete on one platform and
    /// overwritten on the other. The receiver refuses the second.
    #[must_use]
    pub fn windows_key(&self) -> &str {
        &self.key
    }
}

/// Every rule that applies to one component of a path.
fn check_component(component: &str) -> Result<(), PathError> {
    for character in component.chars() {
        match character {
            '\0' => return Err(PathError::Nul),
            '\\' => return Err(PathError::Separator),
            ':' => return Err(PathError::Colon),
            character if character.is_control() => return Err(PathError::Control),
            '?' => return Err(PathError::Forbidden),
            character if FORBIDDEN.contains(&character) => return Err(PathError::Forbidden),
            _ => {}
        }
    }

    match component {
        "" => return Err(PathError::EmptyComponent),
        "." => return Err(PathError::Dot),
        ".." => return Err(PathError::Traversal),
        _ => {}
    }

    if component.ends_with('.') || component.ends_with(' ') {
        return Err(PathError::TrailingDotOrSpace);
    }

    if is_reserved(component) {
        return Err(PathError::Reserved);
    }

    Ok(())
}

/// Whether a component is a device name, extension or no extension.
fn is_reserved(component: &str) -> bool {
    let stem = component
        .split_once('.')
        .map_or(component, |(stem, _)| stem)
        .to_ascii_uppercase();

    if RESERVED.contains(&stem.as_str()) {
        return true;
    }

    RESERVED_NUMBERED.iter().any(|family| {
        stem.strip_prefix(family)
            .is_some_and(|rest| matches!(rest, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_this_side_can_create_is_kept_as_it_arrived() {
        for path in [
            "a.txt",
            "notes/todo.txt",
            "a/b/c/d.bin",
            "Отчёт 2026.txt",
            "螺旋/データ.csv",
            "dotted.name.tar.gz",
            "com0.txt",
            "CONSOLE",
        ] {
            let parsed = ValidatedPath::parse(path).expect("a path both platforms can hold");

            assert_eq!(parsed.as_str(), path);
        }
    }

    #[test]
    fn a_path_that_would_escape_or_could_not_be_written_is_refused() {
        let cases: [(&str, PathError); 18] = [
            ("", PathError::Empty),
            ("/etc/passwd", PathError::Absolute),
            ("../secrets", PathError::Traversal),
            ("a/../../b", PathError::Traversal),
            ("./a", PathError::Dot),
            ("a/./b", PathError::Dot),
            ("a//b", PathError::EmptyComponent),
            ("a/", PathError::EmptyComponent),
            ("C:/windows", PathError::Colon),
            ("a/b:stream", PathError::Colon),
            ("a\\b", PathError::Separator),
            ("a\0b", PathError::Nul),
            ("a\tb", PathError::Control),
            ("what?.txt", PathError::Forbidden),
            ("a/trailing.", PathError::TrailingDotOrSpace),
            ("a/trailing ", PathError::TrailingDotOrSpace),
            ("NUL", PathError::Reserved),
            ("a/com1.txt", PathError::Reserved),
        ];

        for (path, expected) in cases {
            assert_eq!(
                ValidatedPath::parse(path),
                Err(expected),
                "{path} was not refused as it should have been"
            );
        }
    }

    #[test]
    fn a_path_past_the_wire_limits_is_refused_by_the_limit_it_passed() {
        let long = format!("{}.txt", "n".repeat(MAX_PATH_BYTES));
        assert_eq!(
            ValidatedPath::parse(&long),
            Err(PathError::TooLong {
                bytes: long.len(),
                limit: MAX_PATH_BYTES,
            })
        );

        let deep = vec!["d"; MAX_DEPTH + 1].join("/");
        assert_eq!(
            ValidatedPath::parse(&deep),
            Err(PathError::TooDeep {
                depth: MAX_DEPTH + 1,
                limit: MAX_DEPTH,
            })
        );

        let at_the_limit = vec!["d"; MAX_DEPTH].join("/");
        assert!(ValidatedPath::parse(&at_the_limit).is_ok());
    }

    #[test]
    fn a_multibyte_path_is_measured_in_bytes_not_characters() {
        // Every one of these is two bytes, so half as many fit as would if the
        // limit were counted in characters.
        let path = "ж".repeat(MAX_PATH_BYTES / 2);
        assert!(ValidatedPath::parse(&path).is_ok());

        let over = "ж".repeat(MAX_PATH_BYTES / 2 + 1);
        assert!(matches!(
            ValidatedPath::parse(&over),
            Err(PathError::TooLong { .. })
        ));
    }

    #[test]
    fn two_paths_that_are_one_file_on_windows_share_a_key() {
        let lower = ValidatedPath::parse("notes/todo.txt").expect("a path");
        let upper = ValidatedPath::parse("Notes/TODO.TXT").expect("a path");

        assert_eq!(lower.windows_key(), upper.windows_key());
        // The original is what gets created, so a Linux tree keeps its names.
        assert_ne!(lower.as_str(), upper.as_str());

        let other = ValidatedPath::parse("notes/todo.txt.bak").expect("a path");
        assert_ne!(lower.windows_key(), other.windows_key());
    }

    #[test]
    fn the_components_are_what_the_receiver_walks() {
        let parsed = ValidatedPath::parse("a/b/c.txt").expect("a path");

        assert_eq!(parsed.components().collect::<Vec<_>>(), ["a", "b", "c.txt"]);
        assert_eq!(parsed.depth(), 3);
    }
}
