//! The one share a VM's display is offered.
//!
//! Its own module and its own share rather than a role inside the GPU
//! manifest: a GPU attach that fails must not be able to take the display with
//! it, and the two are mounted, applied and reported on separately from here
//! to the guest.

use std::path::{Path, PathBuf};

use vmlord_core::{DISPLAY_PAYLOAD_SHARE, DisplayShare, RepositoryError};

use crate::{layout::display_payload_staging_directory, paths::is_within};

/// One directory offered to a guest over Plan9, and the name it mounts it by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DisplayExport {
    share: DisplayShare,
    /// The canonical path, which is what HCS is given and what the VM is
    /// granted access to.
    host_path: PathBuf,
}

impl DisplayExport {
    pub(crate) fn name(&self) -> &str {
        &self.share.name
    }

    pub(crate) fn host_path(&self) -> &Path {
        &self.host_path
    }

    pub(crate) fn share(&self) -> &DisplayShare {
        &self.share
    }
}

/// How a path is resolved to the canonical form HCS is given.
type Canonicalize<'a> = &'a dyn Fn(&Path) -> Result<PathBuf, RepositoryError>;

/// The display share for `payload`, provided it really lies inside this VM's
/// staging root.
///
/// `payload` is the generation directory staging produced, not the staging
/// root itself: the root also holds the ready markers and lock files that make
/// a swap atomic, while the guest reads `payload.json` at the root of the share
/// it mounts. Naming the root would offer a guest a directory it finds no
/// payload in.
///
/// `None` is a VM nothing was staged for, which is a VM whose display is
/// degraded rather than a VM that cannot start.
pub(crate) fn build(
    vm_directory: &Path,
    payload: Option<&Path>,
    canonicalize: Canonicalize<'_>,
) -> Option<DisplayExport> {
    let candidate = payload?;
    let (Ok(vm), Ok(payload)) = (canonicalize(vm_directory), canonicalize(candidate)) else {
        return None;
    };
    let staging = display_payload_staging_directory(&vm);
    // Inside the VM's own staging root and deeper than it. Outside is a reparse
    // point aiming somewhere this VM has no business reading, and the root
    // itself holds the markers and locks of a swap rather than a payload. The
    // check is against canonical paths, so a junction cannot pass it and export
    // elsewhere.
    if payload == staging || !is_within(&staging, &payload) {
        return None;
    }
    Some(DisplayExport {
        share: DisplayShare {
            name: DISPLAY_PAYLOAD_SHARE.to_owned(),
        },
        host_path: payload,
    })
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use vmlord_core::RepositoryError;

    use super::build;

    static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

    struct TemporaryDirectory(PathBuf);

    impl TemporaryDirectory {
        fn new(label: &str) -> Self {
            let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "vmlord-display-export-{label}-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TemporaryDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn canonicalize(path: &Path) -> Result<PathBuf, RepositoryError> {
        fs::canonicalize(path)
            .map_err(|error| RepositoryError::new(format!("{}: {error}", path.display())))
    }

    #[test]
    fn only_a_generation_inside_this_vms_staging_root_is_exported() {
        let temporary = TemporaryDirectory::new("containment");
        let vm = temporary.path().join("dev-linux");
        let staging = vm.join("display-payload");
        let generation = staging.join("generations").join("abc");
        fs::create_dir_all(&generation).unwrap();
        let elsewhere = temporary.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        let gpu = vm.join("gpu-payload").join("generations").join("abc");
        fs::create_dir_all(&gpu).unwrap();

        assert_eq!(
            build(&vm, Some(&generation), &canonicalize)
                .expect("a generation of this VM is exportable")
                .name(),
            "vmlord.display.payload"
        );
        assert!(
            build(&vm, Some(&staging), &canonicalize).is_none(),
            "the staging root holds markers and locks, not a payload"
        );
        assert!(
            build(&vm, Some(&elsewhere), &canonicalize).is_none(),
            "outside this VM is outside"
        );
        assert!(
            build(&vm, Some(&gpu), &canonicalize).is_none(),
            "the GPU's staging is not the display's to export"
        );
        assert!(build(&vm, None, &canonicalize).is_none());
    }

    #[test]
    fn an_export_carries_the_canonical_path_hcs_is_given() {
        let temporary = TemporaryDirectory::new("canonical");
        let vm = temporary.path().join("dev-linux");
        let generation = vm.join("display-payload").join("generations").join("abc");
        fs::create_dir_all(&generation).unwrap();

        let export = build(&vm, Some(&generation), &canonicalize).unwrap();

        assert_eq!(export.host_path(), canonicalize(&generation).unwrap());
    }
}
