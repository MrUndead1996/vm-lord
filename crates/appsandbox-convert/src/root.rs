//! Joining a guest-absolute path onto the root the conversion was handed.
//!
//! Every path the conversion touches goes through here. A path that is not
//! guest-absolute, or that climbs back out of the root once its `.` and `..`
//! are resolved, is a refusal rather than a write somewhere else on the
//! machine doing the conversion.

use std::path::{Component, Path, PathBuf};

use crate::ConvertError;

pub(crate) fn guest_path(root: &Path, absolute: &str) -> Result<PathBuf, ConvertError> {
    let Some(relative) = absolute.strip_prefix('/') else {
        return Err(ConvertError::new(format!(
            "{absolute} is not a path inside the guest"
        )));
    };
    let mut joined = root.to_path_buf();
    for component in Path::new(relative).components() {
        match component {
            Component::Normal(part) => joined.push(part),
            Component::CurDir => {}
            _ => {
                return Err(ConvertError::new(format!(
                    "{absolute} leads out of the guest's root"
                )));
            }
        }
    }
    Ok(joined)
}

#[cfg(test)]
mod tests {
    use super::guest_path;
    use std::path::Path;

    #[test]
    fn a_guest_absolute_path_lands_under_the_root() {
        let joined = guest_path(Path::new("/mnt/guest"), "/etc/hostname").expect("joined");
        assert_eq!(joined, Path::new("/mnt/guest/etc/hostname"));
    }

    #[test]
    fn a_path_that_is_not_guest_absolute_is_refused() {
        assert!(guest_path(Path::new("/mnt/guest"), "etc/hostname").is_err());
    }

    #[test]
    fn a_path_that_climbs_out_of_the_root_is_refused() {
        assert!(guest_path(Path::new("/mnt/guest"), "/etc/../../escape").is_err());
    }
}
