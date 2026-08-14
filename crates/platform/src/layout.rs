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

/// The raw capture of everything the guest wrote to its first serial port.
pub(crate) const COM1_LOG_FILE_NAME: &str = "com1.log";

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

/// The exact per-VM directory that may contain staged GPU payload generations.
pub(crate) fn gpu_payload_staging_directory(vm_directory: &Path) -> PathBuf {
    vm_directory.join("gpu-payload")
}

/// Returns the path of the VM's serial-console capture.
///
/// Beside `config.json` rather than under `disks/`: it describes what the VM
/// did, not what it is made of, and a deletion that removes the VM's disks must
/// not be the thing that decides whether its last boot output survives.
pub(crate) fn com1_log_path(vm_directory: &Path) -> PathBuf {
    vm_directory.join(COM1_LOG_FILE_NAME)
}

/// Returns the path of the transcript the readiness wait captures.
///
/// Beside `com1.log`, and for the same reason: it records what the VM did, not
/// what it is made of. Its existence also says that the wait got as far as
/// running something inside the guest, which the serial log alone does not.
pub(crate) fn cloud_init_status_log_path(vm_directory: &Path) -> PathBuf {
    vm_directory.join("cloud-init-status.log")
}

/// Returns the path of the VM's system disk.
pub(crate) fn system_disk_path(vm_directory: &Path) -> PathBuf {
    vm_directory.join("disks").join("system.vhdx")
}

/// Returns the path of the NoCloud seed the guest's cloud-init reads.
///
/// Beside `config.json` rather than under `disks/`: this is a configuration
/// medium, not one of the VM's disks, and `disks/` is what a deletion removes
/// when it is told to remove the VM's disks.
pub(crate) fn seed_path(vm_directory: &Path) -> PathBuf {
    vm_directory.join("seed.iso")
}

/// Returns the path of the optional tools volume for a cloud VM.
///
/// The agent itself is common to every VM but the ISO is per-VM, so it lives
/// with the seed it accompanies rather than beside the host executable.
pub(crate) fn tools_path(vm_directory: &Path) -> PathBuf {
    vm_directory.join("tools.iso")
}

/// Returns the path of the host's copy of the VM's agent secret.
///
/// Beside `config.json` rather than in `keys/`: that directory is the VM's SSH
/// identity, and this is what authenticates the agent inside the guest, which
/// is a different thing reached by a different route. The guest's copy travels
/// in the seed; this one is the host's and the VM is never granted access to
/// it.
pub(crate) fn agent_secret_path(vm_directory: &Path) -> PathBuf {
    vm_directory.join("agent.secret")
}

/// Returns the directory holding the VM's own SSH key pair.
///
/// A directory of its own rather than loose files beside `config.json`: it is
/// the whole of the VM's SSH identity, which lets a deletion that keeps the
/// disks remove the identity in one step.
pub(crate) fn ssh_keys_directory(vm_directory: &Path) -> PathBuf {
    vm_directory.join("keys")
}

/// Returns the path of the VM's own SSH private key.
pub(crate) fn ssh_key_path(vm_directory: &Path) -> PathBuf {
    ssh_keys_directory(vm_directory).join("id_ed25519")
}

/// Returns the path of the host keys VMLord has learned for this VM.
///
/// One file per VM rather than the user's own `~/.ssh/known_hosts`: a VM's key
/// is VMLord's business, it is learned automatically the first time the guest
/// answers, and a guest that was deleted and recreated must not be able to make
/// the user's own file grow entries -- or, worse, collide with a real host.
pub(crate) fn ssh_known_hosts_path(vm_directory: &Path) -> PathBuf {
    vm_directory.join("known_hosts")
}

/// Returns the path of the VM's own SSH public key.
///
/// The public half is derivable from the private one in microseconds, so this
/// file is a convenience rather than a necessity: it lets a person see which
/// key went into the guest without starting the VM.
pub(crate) fn ssh_public_key_path(vm_directory: &Path) -> PathBuf {
    ssh_keys_directory(vm_directory).join("id_ed25519.pub")
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{
        cloud_init_status_log_path, com1_log_path, configuration_path, seed_path, ssh_key_path,
        ssh_keys_directory, ssh_known_hosts_path, ssh_public_key_path, system_disk_path,
        tools_path, vm_directory,
    };

    #[test]
    fn a_vm_learns_host_keys_into_a_file_of_its_own() {
        let directory = vm_directory(Path::new("/vms"), "dev-linux").unwrap();

        assert_eq!(
            ssh_known_hosts_path(&directory),
            PathBuf::from("/vms").join("dev-linux").join("known_hosts")
        );
    }

    #[test]
    fn the_readiness_transcript_lives_beside_the_serial_log() {
        // Both record what the VM did rather than what it is made of, so both
        // sit beside `config.json` rather than under `disks/`.
        let directory = vm_directory(Path::new("/vms"), "dev-linux").unwrap();

        assert_eq!(
            cloud_init_status_log_path(&directory),
            PathBuf::from("/vms")
                .join("dev-linux")
                .join("cloud-init-status.log")
        );
    }

    #[test]
    fn the_agent_secret_lives_beside_the_configuration_not_among_the_disks() {
        // The guest's copy travels in the seed; this is the host's, and it is
        // what a deletion that keeps the disks has to take away.
        let directory = vm_directory(Path::new("/vms"), "dev-linux").unwrap();

        assert_eq!(
            super::agent_secret_path(&directory),
            PathBuf::from("/vms").join("dev-linux").join("agent.secret")
        );
    }

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
        assert_eq!(
            com1_log_path(&directory),
            PathBuf::from("/vms").join("dev-linux").join("com1.log")
        );
    }

    #[test]
    fn the_seed_lives_beside_the_configuration_not_among_the_disks() {
        // Not under `disks/`: the seed is a configuration medium, and `disks/` is
        // what `delete_vm` removes when asked to remove a VM's disks.
        let directory = vm_directory(Path::new("/vms"), "dev-linux").unwrap();

        assert_eq!(
            seed_path(&directory),
            PathBuf::from("/vms").join("dev-linux").join("seed.iso")
        );
    }

    #[test]
    fn the_tools_volume_lives_beside_the_cloud_seed() {
        let directory = vm_directory(Path::new("/vms"), "dev-linux").unwrap();

        assert_eq!(
            tools_path(&directory),
            PathBuf::from("/vms").join("dev-linux").join("tools.iso")
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
    fn both_halves_of_the_pair_live_in_the_vms_keys_directory() {
        // The directory is what a deletion that keeps the disks removes, so
        // nothing of the VM's SSH identity may sit outside it.
        let directory = vm_directory(Path::new("/vms"), "dev-linux").unwrap();
        let keys = ssh_keys_directory(&directory);

        assert!(ssh_key_path(&directory).starts_with(&keys));
        assert!(ssh_public_key_path(&directory).starts_with(&keys));
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
