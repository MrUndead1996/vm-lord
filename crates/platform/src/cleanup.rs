//! Removing the resources a VMLord VM is made of.
//!
//! Creation rollback and deletion tear the same two things down -- the HCS
//! compute system and the VM directory -- and report a partial failure the same
//! way, so both drive them from here.

use std::{fs, path::Path, time::Duration};

use vmlord_core::RepositoryError;

use crate::{HcsSystem, hcs::HCS_ACCESS_ALL};

/// A teardown needs nothing from the guest, so it completes as soon as HCS has
/// torn the compute system down; the bound only guards against a wedged Host
/// Compute Service.
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(30);

/// How a pipeline reaches HCS to tear a compute system down.
///
/// Injected rather than called directly so the pipelines can be tested without
/// a Hyper-V host.
pub(crate) type SystemTeardown = Box<dyn Fn(&str) -> Result<(), RepositoryError>>;

/// Terminates the compute system `id`, treating one HCS does not know as
/// already gone.
///
/// A compute system exists only while it is created or running: HCS destroys it
/// as the VM stops. A VM that is not running therefore routinely has none, and
/// that is a fact about its state rather than a failure to remove it.
pub(crate) fn teardown_compute_system(id: &str) -> Result<(), RepositoryError> {
    let Some(system) = HcsSystem::open_if_present(id, HCS_ACCESS_ALL)? else {
        log::debug!("HCS compute system \"{id}\" is already gone; nothing to tear down");
        return Ok(());
    };
    system.terminate_and_wait(TEARDOWN_TIMEOUT)
}

/// Removes `vm_directory` and everything under it, treating an absent directory
/// as already removed.
pub(crate) fn remove_vm_directory(vm_directory: &Path) -> Result<(), RepositoryError> {
    if !vm_directory.exists() {
        log::debug!("VM directory {} is already gone", vm_directory.display());
        return Ok(());
    }

    fs::remove_dir_all(vm_directory).map_err(|error| {
        let error = RepositoryError::new(format!(
            "failed to remove VM directory {}: {error}",
            vm_directory.display()
        ));
        log::error!("{error}");
        error
    })?;
    log::debug!("removed VM directory {}", vm_directory.display());
    Ok(())
}

/// Folds the failures a best-effort cleanup collected into a single error.
pub(crate) fn combine_failures(prefix: &str, failures: Vec<String>) -> RepositoryError {
    let error = RepositoryError::new(format!("{prefix}: {}", failures.join("; ")));
    log::error!("{error}");
    error
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::{combine_failures, remove_vm_directory};

    struct TempRoot(PathBuf);

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temp_root(label: &str) -> TempRoot {
        static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "vmlord-cleanup-test-{label}-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("test root should be created");
        TempRoot(path)
    }

    #[test]
    fn removes_a_vm_directory_with_everything_under_it() {
        let root = temp_root("populated");
        let vm_directory = root.0.join("vm");
        fs::create_dir_all(vm_directory.join("disks")).expect("disks directory should be created");
        fs::write(vm_directory.join("config.json"), b"{}").expect("configuration should be written");
        fs::write(vm_directory.join("disks").join("system.vhdx"), b"vhdx")
            .expect("disk should be written");

        remove_vm_directory(&vm_directory).expect("a populated VM directory should be removed");

        assert!(!vm_directory.exists());
    }

    #[test]
    fn an_absent_vm_directory_is_already_removed() {
        let root = temp_root("absent");

        remove_vm_directory(&root.0.join("never-created"))
            .expect("an absent VM directory must not be reported as a failure");
    }

    #[test]
    fn combined_failures_name_every_step_that_failed() {
        let error = combine_failures(
            "deletion of VM \"dev\" did not complete",
            vec!["teardown failed".into(), "removal failed".into()],
        );

        let message = error.to_string();
        assert!(message.contains("deletion of VM \"dev\" did not complete"));
        assert!(message.contains("teardown failed"));
        assert!(message.contains("removal failed"));
    }
}
