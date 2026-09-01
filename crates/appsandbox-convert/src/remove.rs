//! Everything of AppSandbox's, and only it.
//!
//! The inventory is read out of AppSandbox's own sources: what its disk
//! builder wrote, what its first-boot script installed, and what its agent
//! wrote while it ran. Each entry is here because VMLord's own stack collides
//! with it or because it names a program the conversion is removing -- not
//! because it carries the application's name.

use std::{fs, path::Path};

use crate::{Conversion, ConvertError, root::guest_path};

/// The units whose files and enablement symlinks both go.
pub(crate) const UNITS: [&str; 7] = [
    "appsandbox-agent.service",
    "appsandbox-audio.service",
    "appsandbox-display.service",
    "appsandbox-input.service",
    "asb-evict-simpledrm.service",
    "appsandbox-firstboot.service",
    // A *user* unit: installed under `/etc/systemd/user`, not beside the rest.
    "appsandbox-clipboard.service",
];

/// Single files, each by its guest-absolute path.
pub(crate) const FILES: [&str; 26] = [
    "/usr/local/bin/appsandbox-agent",
    "/usr/local/bin/appsandbox-audio",
    "/usr/local/bin/appsandbox-clipboard",
    "/usr/local/bin/appsandbox-display",
    "/usr/local/bin/appsandbox-input",
    "/usr/local/bin/appsandbox-gpu",
    "/usr/local/bin/appsandbox-firstboot.sh",
    // Emits the same five Mesa variables VMLord's own generator emits, at
    // AppSandbox's prefix. Generators are additive: both would run.
    "/etc/systemd/user-environment-generators/50-appsandbox-gpu",
    "/etc/modprobe.d/asb_drm.conf",
    "/etc/modules-load.d/asb_drm.conf",
    // VMLord's GPU payload installs a dxgkrnl of its own.
    "/etc/modules-load.d/dxgkrnl.conf",
    // Loaded for AppSandbox's audio daemon alone.
    "/etc/modules-load.d/snd-aloop.conf",
    "/etc/ld.so.conf.d/wsl-mesa.conf",
    "/etc/ld.so.conf.d/appsandbox-wsl-deps.conf",
    // Written per boot by the agent, listing the 9P mounts it made.
    "/etc/ld.so.conf.d/wsl.conf",
    "/etc/vulkan/icd.d/dzn_icd.x86_64.json",
    "/etc/default/grub.d/99-appsandbox-no-efifb.cfg",
    // A static address on a subnet AppSandbox served.
    "/etc/netplan/99-appsandbox.yaml",
    "/etc/appsandbox-admin-user",
    "/etc/appsandbox-ssh-enabled",
    "/etc/appsandbox-hostname",
    "/etc/appsandbox-timezone",
    "/etc/appsandbox-locale",
    "/etc/appsandbox-keyboard",
    "/etc/appsandbox-admin-hash",
    "/var/lib/appsandbox-firstboot.done",
];

/// Directories removed whole.
pub(crate) const TREES: [&str; 6] = [
    "/opt/appsandbox",
    "/opt/wsl-mesa",
    "/etc/apt/appsandbox-sources.list.d",
    // Versioned by DKMS: the stem names it and every `<stem>-<version>` goes.
    "/usr/src/asb_drm",
    "/usr/src/dxgkrnl",
    // The drop-in directory itself, whose one file unsets the environment
    // VMLord's own generator sets for the compositor.
    "/etc/systemd/user/org.gnome.Shell@.service.d",
];

/// The DKMS packages whose source, state and built modules go.
pub(crate) const DKMS_PACKAGES: [&str; 2] = ["asb_drm", "dxgkrnl"];

/// Where an enablement symlink lives, for a system unit and for a user one.
const WANTS: [&str; 3] = [
    "/etc/systemd/system/multi-user.target.wants",
    "/etc/systemd/system/graphical.target.wants",
    "/etc/systemd/user/graphical-session.target.wants",
];

/// The two directories a unit file itself lives in.
const UNIT_DIRECTORIES: [&str; 2] = ["/etc/systemd/system", "/etc/systemd/user"];

pub(crate) fn run(conversion: &Conversion) -> Result<(), ConvertError> {
    let root = &conversion.root;

    // The symlinks before the units they point at: a unit file removed first
    // leaves systemd a dangling want, which is a state neither program owns.
    for unit in UNITS {
        for wants in WANTS {
            remove_file(&guest_path(root, wants)?.join(unit))?;
        }
        for directory in UNIT_DIRECTORIES {
            remove_file(&guest_path(root, directory)?.join(unit))?;
        }
    }

    for file in FILES {
        remove_file(&guest_path(root, file)?)?;
    }

    for tree in TREES {
        remove_matching_trees(&guest_path(root, tree)?)?;
    }

    for package in DKMS_PACKAGES {
        remove_tree(&guest_path(root, &format!("/var/lib/dkms/{package}"))?)?;
        remove_built_modules(root, package)?;
    }

    Ok(())
}

/// The `.ko` a DKMS install left under every kernel's `updates` directory.
fn remove_built_modules(root: &Path, package: &str) -> Result<(), ConvertError> {
    let modules = guest_path(root, "/lib/modules")?;
    let Ok(kernels) = fs::read_dir(&modules) else {
        return Ok(());
    };
    for kernel in kernels.flatten() {
        for directory in ["updates/dkms", "updates", &format!("updates/{package}")] {
            let built = kernel.path().join(directory);
            for extension in ["ko", "ko.zst", "ko.xz"] {
                remove_file(&built.join(format!("{package}.{extension}")))?;
            }
        }
    }
    Ok(())
}

/// Removes `stem` and every sibling whose name begins with `stem-`.
fn remove_matching_trees(stem: &Path) -> Result<(), ConvertError> {
    remove_tree(stem)?;
    let (Some(parent), Some(name)) = (
        stem.parent(),
        stem.file_name().and_then(|name| name.to_str()),
    ) else {
        return Ok(());
    };
    let Ok(entries) = fs::read_dir(parent) else {
        return Ok(());
    };
    let prefix = format!("{name}-");
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            remove_tree(&entry.path())?;
        }
    }
    Ok(())
}

/// A path that is not there is the state this asks for, not a failure.
///
/// `remove_file` rather than a check first: a broken symlink -- an enablement
/// pointing at a unit already gone -- does not exist as far as `Path::exists`
/// is concerned, and is exactly what has to be removed.
fn remove_file(path: &Path) -> Result<(), ConvertError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ConvertError::new(format!(
            "{} could not be removed: {error}",
            path.display()
        ))),
    }
}

fn remove_tree(path: &Path) -> Result<(), ConvertError> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ConvertError::new(format!(
            "{} could not be removed: {error}",
            path.display()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::run;
    use crate::fixture::AppSandboxGuest;

    fn cleared() -> AppSandboxGuest {
        let guest = AppSandboxGuest::new();
        run(&guest.conversion()).expect("removed");
        guest
    }

    #[test]
    fn every_appsandbox_unit_and_its_enablement_symlink_are_gone() {
        let guest = cleared();
        for unit in [
            "appsandbox-agent.service",
            "appsandbox-audio.service",
            "appsandbox-display.service",
            "appsandbox-input.service",
            "asb-evict-simpledrm.service",
        ] {
            assert!(
                !guest.root().join("etc/systemd/system").join(unit).exists(),
                "{unit}"
            );
            assert!(
                guest
                    .root()
                    .join("etc/systemd/system/multi-user.target.wants")
                    .join(unit)
                    .symlink_metadata()
                    .is_err(),
                "{unit} is still enabled"
            );
        }
    }

    #[test]
    fn the_user_level_clipboard_unit_is_gone_too() {
        let guest = cleared();
        assert!(
            !guest
                .root()
                .join("etc/systemd/user/appsandbox-clipboard.service")
                .exists()
        );
    }

    #[test]
    fn the_compositor_drop_in_that_unsets_vmlords_environment_is_gone() {
        let guest = cleared();
        assert!(
            !guest
                .root()
                .join("etc/systemd/user/org.gnome.Shell@.service.d/no-gpu.conf")
                .exists()
        );
    }

    #[test]
    fn the_environment_generator_that_competes_with_vmlords_is_gone() {
        let guest = cleared();
        assert!(
            !guest
                .root()
                .join("etc/systemd/user-environment-generators/50-appsandbox-gpu")
                .exists()
        );
    }

    #[test]
    fn both_dkms_trees_and_their_built_modules_are_gone() {
        let guest = cleared();
        for path in [
            "usr/src/asb_drm-1.0.0",
            "usr/src/dxgkrnl-1.0.0",
            "var/lib/dkms/asb_drm",
            "var/lib/dkms/dxgkrnl",
            "lib/modules/6.14.0-24-generic/updates/dkms/asb_drm.ko",
            "lib/modules/6.14.0-24-generic/updates/dkms/dxgkrnl.ko",
        ] {
            assert!(!guest.root().join(path).exists(), "{path}");
        }
    }

    #[test]
    fn the_mesa_tree_its_linker_lines_and_its_icd_are_gone() {
        let guest = cleared();
        for path in [
            "opt/wsl-mesa",
            "etc/ld.so.conf.d/wsl-mesa.conf",
            "etc/ld.so.conf.d/appsandbox-wsl-deps.conf",
            "etc/ld.so.conf.d/wsl.conf",
            "etc/vulkan/icd.d/dzn_icd.x86_64.json",
        ] {
            assert!(!guest.root().join(path).exists(), "{path}");
        }
    }

    #[test]
    fn the_module_configuration_that_blacklists_what_vmlord_expects_is_gone() {
        let guest = cleared();
        for path in [
            "etc/modprobe.d/asb_drm.conf",
            "etc/modules-load.d/asb_drm.conf",
            "etc/modules-load.d/dxgkrnl.conf",
            "etc/modules-load.d/snd-aloop.conf",
        ] {
            assert!(!guest.root().join(path).exists(), "{path}");
        }
    }

    #[test]
    fn the_static_netplan_goes_and_the_cloud_init_lock_stays() {
        let guest = cleared();
        assert!(!guest.root().join("etc/netplan/99-appsandbox.yaml").exists());
        assert!(
            guest
                .root()
                .join("etc/cloud/cloud.cfg.d/99-disable-network-config.cfg")
                .exists(),
            "cloud-init would write a second netplan for the same interface"
        );
    }

    #[test]
    fn the_staged_tree_the_markers_and_the_firstboot_program_are_gone() {
        let guest = cleared();
        for path in [
            "opt/appsandbox",
            "etc/appsandbox-admin-user",
            "etc/appsandbox-ssh-enabled",
            "var/lib/appsandbox-firstboot.done",
            "usr/local/bin/appsandbox-firstboot.sh",
            "etc/systemd/system/appsandbox-firstboot.service",
            "etc/systemd/system/multi-user.target.wants/appsandbox-firstboot.service",
            "etc/apt/appsandbox-sources.list.d",
            "etc/default/grub.d/99-appsandbox-no-efifb.cfg",
        ] {
            assert!(!guest.root().join(path).exists(), "{path}");
        }
    }

    #[test]
    fn what_describes_the_guest_rather_than_the_source_application_stays() {
        let guest = cleared();
        for path in [
            "etc/fstab",
            "etc/passwd",
            // Resolves under VMLord too: it mounts the host's WSL libraries
            // at the same `/usr/lib/wsl/lib`.
            "usr/local/bin/nvidia-smi",
        ] {
            assert!(guest.root().join(path).exists(), "{path} was removed");
        }
    }

    #[test]
    fn removing_twice_is_removing_once() {
        let guest = cleared();
        run(&guest.conversion()).expect("removed again");
    }
}
