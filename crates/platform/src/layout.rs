//! On-disk layout of a VMLord-managed virtual machine.
//!
//! Creation, start and the repository all have to agree on where a VM's
//! configuration and disks live; this module is the single place that decides
//! it.

use std::{
    ffi::OsStr,
    path::{Component, Path, PathBuf},
};

use vmlord_core::RepositoryError;

/// The HCS configuration document creation writes and start re-creates the
/// compute system from.
pub(crate) const CONFIGURATION_FILE_NAME: &str = "config.json";

/// Returns the directory holding everything VM `vm_name` is made of.
///
/// The name comes from the user and is used as a directory name, so anything
/// that is not a single plain path component -- a separator, `..`, a drive
/// prefix -- is rejected rather than allowed to escape `storage_root`.
pub(crate) fn vm_directory(storage_root: &Path, vm_name: &str) -> Result<PathBuf, RepositoryError> {
    let mut components = Path::new(vm_name).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(component)), None) if component == OsStr::new(vm_name) => {
            Ok(storage_root.join(component))
        }
        _ => {
            let error = RepositoryError::new(format!(
                "VM name \"{vm_name}\" cannot be used as a directory name"
            ));
            log::error!("{error}");
            Err(error)
        }
    }
}

/// Returns the path of the VM's stored HCS configuration document.
pub(crate) fn configuration_path(vm_directory: &Path) -> PathBuf {
    vm_directory.join(CONFIGURATION_FILE_NAME)
}

/// Returns the path of the VM's system disk.
pub(crate) fn system_disk_path(vm_directory: &Path) -> PathBuf {
    vm_directory.join("disks").join("system.vhdx")
}

/// Returns the path of the VM's own SSH private key.
pub(crate) fn ssh_key_path(vm_directory: &Path) -> PathBuf {
    vm_directory.join("keys").join("id_ed25519")
}

/// Returns the path of the VM's own SSH public key.
///
/// The public half is derivable from the private one in microseconds, so this
/// file is a convenience rather than a necessity: it lets a person see which
/// key went into the guest without starting the VM.
pub(crate) fn ssh_public_key_path(vm_directory: &Path) -> PathBuf {
    vm_directory.join("keys").join("id_ed25519.pub")
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{
        configuration_path, ssh_key_path, ssh_public_key_path, system_disk_path, vm_directory,
    };

    #[test]
    fn a_plain_name_becomes_a_directory_under_the_storage_root() {
        let directory = vm_directory(Path::new("/vms"), "dev-linux").unwrap();

        assert_eq!(directory, PathBuf::from("/vms").join("dev-linux"));
        assert_eq!(
            configuration_path(&directory),
            PathBuf::from("/vms").join("dev-linux").join("config.json")
        );
        assert_eq!(
            system_disk_path(&directory),
            PathBuf::from("/vms")
                .join("dev-linux")
                .join("disks")
                .join("system.vhdx")
        );
    }

    #[test]
    fn a_vms_key_pair_lives_beside_its_disks() {
        let directory = vm_directory(Path::new("/vms"), "dev-linux").unwrap();

        assert_eq!(
            ssh_key_path(&directory),
            PathBuf::from("/vms")
                .join("dev-linux")
                .join("keys")
                .join("id_ed25519")
        );
        assert_eq!(
            ssh_public_key_path(&directory),
            PathBuf::from("/vms")
                .join("dev-linux")
                .join("keys")
                .join("id_ed25519.pub")
        );
    }

    #[test]
    fn a_name_that_is_not_a_single_component_is_rejected() {
        for name in ["..", ".", "", "a/b", "a\\b", "/absolute"] {
            assert!(
                vm_directory(Path::new("/vms"), name).is_err(),
                "\"{name}\" must not be usable as a VM directory name"
            );
        }
    }
}
