//! Path comparisons the exports rely on.
//!
//! Not a string prefix anywhere here: two paths that share a textual prefix are
//! routinely different directories, and an export that got that wrong would
//! hand a guest a directory belonging to something else.

use std::path::{Component, Path};

/// Whether `path` is `root` or lies under it, compared component by component.
///
/// Not a string prefix: `...\FileRepositoryEvil` starts with
/// `...\FileRepository` and is a different directory.
pub(crate) fn is_within(root: &Path, path: &Path) -> bool {
    let mut root_components = root.components();
    let mut path_components = path.components();

    loop {
        match (root_components.next(), path_components.next()) {
            (None, _) => return true,
            (Some(_), None) => return false,
            (Some(expected), Some(actual)) if component_eq(expected, actual) => {}
            (Some(_), Some(_)) => return false,
        }
    }
}

/// Windows paths are case-insensitive, and the two spellings of one directory
/// are the same directory.
fn component_eq(left: Component<'_>, right: Component<'_>) -> bool {
    left.as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::is_within;

    #[test]
    fn a_shared_textual_prefix_is_not_containment() {
        assert!(is_within(
            Path::new("C:/vms/dev"),
            Path::new("C:/vms/dev/display-payload")
        ));
        assert!(!is_within(
            Path::new("C:/vms/dev"),
            Path::new("C:/vms/dev-evil/display-payload")
        ));
    }

    #[test]
    fn a_root_contains_itself() {
        assert!(is_within(Path::new("C:/vms/dev"), Path::new("C:/vms/dev")));
    }
}
