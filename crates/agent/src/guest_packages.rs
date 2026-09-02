//! Installing a package through whichever manager this guest turned out to
//! have.
//!
//! Every recipe here needs the same few things -- a compiler, DKMS, the
//! running kernel's headers, the distribution's Mesa -- and every one of them
//! used to ask for them by writing `apt-get` and a Debian package name into
//! the call. That is a guess about the guest twice over: about the program
//! that installs, and about what the thing is called once it does.
//!
//! Both answers come from here instead. The program and its arguments come
//! from the manager [`guest_platform`](crate::guest_platform) found, and the
//! names come from a table keyed by that manager -- not by a distribution,
//! because `linux-headers-$(uname -r)` and `linux-headers` are apt's and
//! pacman's conventions rather than Ubuntu's and Arch's, and two distributions
//! sharing a manager share the name for free.

use std::time::Duration;

use crate::{
    command::{self, Outcome},
    guest_platform::PackageManager,
};

/// How long one install may take.
///
/// Generous, because it covers a package list that has to be fetched first and
/// a compiler that has to be unpacked, and bounded because an install behind a
/// broken NAT would otherwise be an agent that never answers its host again.
pub const INSTALL_BUDGET: Duration = Duration::from_secs(300);

/// Something a recipe needs installed, named by what it is for.
///
/// Never by what one distribution calls it: the name is what
/// [`names`] decides, once the manager is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Package {
    /// The framework that rebuilds an out-of-tree module for a new kernel.
    Dkms,
    /// A compiler and make -- what building that module needs beside DKMS.
    BuildTools,
    /// Headers for the kernel that is running now, which is the one DKMS
    /// builds against.
    KernelHeaders,
    /// The GNOME extension a tray icon shows through.
    AppIndicator,
    /// The distribution's own Mesa: a GL driver and a Vulkan loader.
    Mesa,
    /// The two programs the render probe runs to find out what draws.
    RenderTools,
}

impl Package {
    /// What this is for, in words, for the guest that has no manager to name
    /// it with.
    const fn purpose(self) -> &'static str {
        match self {
            Self::Dkms => "DKMS",
            Self::BuildTools => "a compiler",
            Self::KernelHeaders => "the running kernel's headers",
            Self::AppIndicator => "the AppIndicator extension",
            Self::Mesa => "the distribution's Mesa",
            Self::RenderTools => "the render probe's programs",
        }
    }
}

impl PackageManager {
    /// The arguments that install, before the package names.
    ///
    /// Non-interactive in all four: nothing here is attached to a terminal,
    /// and a manager that stops to ask a question is a stage that times out.
    const fn install_arguments(self) -> &'static [&'static str] {
        match self {
            Self::Apt => &["install", "-y"],
            Self::Pacman => &["-S", "--needed", "--noconfirm"],
            Self::Dnf => &["install", "-y"],
            Self::Zypper => &["--non-interactive", "install"],
        }
    }

    /// The arguments that refresh the package lists.
    const fn refresh_arguments(self) -> &'static [&'static str] {
        match self {
            Self::Apt => &["update"],
            Self::Pacman => &["-Sy", "--noconfirm"],
            Self::Dnf => &["makecache"],
            Self::Zypper => &["--non-interactive", "refresh"],
        }
    }

    /// What has to be in the environment for this manager to run unattended.
    ///
    /// Only apt has anything to say: without `DEBIAN_FRONTEND` a package whose
    /// postinst wants to configure something opens a dialog on a terminal that
    /// is not there. The rest take their non-interactivity as a flag.
    const fn environment(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::Apt => &[("DEBIAN_FRONTEND", "noninteractive")],
            Self::Pacman | Self::Dnf | Self::Zypper => &[],
        }
    }
}

/// What this manager calls a package, which is sometimes more than one name.
///
/// The kernel release is a parameter because two of these conventions put it
/// in the name: apt and dnf ship one headers package per kernel, pacman and
/// zypper one per kernel flavour.
#[must_use]
pub fn names(package: Package, manager: PackageManager, kernel_release: &str) -> Vec<String> {
    let fixed = |names: &[&str]| names.iter().map(|name| (*name).to_owned()).collect();

    match (package, manager) {
        (Package::Dkms, _) => fixed(&["dkms"]),

        (Package::BuildTools, PackageManager::Apt) => fixed(&["build-essential"]),
        (Package::BuildTools, PackageManager::Pacman) => fixed(&["base-devel"]),
        (Package::BuildTools, PackageManager::Dnf | PackageManager::Zypper) => {
            fixed(&["gcc", "make"])
        }

        (Package::KernelHeaders, PackageManager::Apt) => {
            vec![format!("linux-headers-{kernel_release}")]
        }
        (Package::KernelHeaders, PackageManager::Dnf) => {
            vec![format!("kernel-devel-{kernel_release}")]
        }
        (Package::KernelHeaders, PackageManager::Pacman) => fixed(&["linux-headers"]),
        (Package::KernelHeaders, PackageManager::Zypper) => fixed(&["kernel-default-devel"]),

        // The one name every distribution that packages this extension at all
        // agreed on, which is why it is a single arm rather than four.
        (Package::AppIndicator, _) => fixed(&["gnome-shell-extension-appindicator"]),

        (Package::Mesa, PackageManager::Apt) => {
            fixed(&["libgl1-mesa-dri", "mesa-vulkan-drivers", "libvulkan1"])
        }
        (Package::Mesa, PackageManager::Pacman) => {
            fixed(&["mesa", "vulkan-swrast", "vulkan-icd-loader"])
        }
        (Package::Mesa, PackageManager::Dnf) => {
            fixed(&["mesa-dri-drivers", "mesa-vulkan-drivers", "vulkan-loader"])
        }
        (Package::Mesa, PackageManager::Zypper) => fixed(&["Mesa-dri", "libvulkan1"]),

        (Package::RenderTools, PackageManager::Apt | PackageManager::Pacman) => {
            fixed(&["mesa-utils", "vulkan-tools"])
        }
        (Package::RenderTools, PackageManager::Dnf) => fixed(&["glx-utils", "vulkan-tools"]),
        (Package::RenderTools, PackageManager::Zypper) => fixed(&["Mesa-demo-x", "vulkan-tools"]),
    }
}

/// Every name a set of packages resolves to, in the order they were asked for.
#[must_use]
pub fn name_list(
    packages: &[Package],
    manager: PackageManager,
    kernel_release: &str,
) -> Vec<String> {
    packages
        .iter()
        .flat_map(|package| names(*package, manager, kernel_release))
        .collect()
}

/// Those names as one phrase, for the line a stage reports.
#[must_use]
pub fn describe(packages: &[Package], manager: PackageManager, kernel_release: &str) -> String {
    let names = name_list(packages, manager, kernel_release);
    match names.split_last() {
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
        None => "nothing".to_owned(),
    }
}

/// Installs a set of packages, refreshing the package lists and trying once
/// more if the first attempt fails.
///
/// The retry is here rather than at three of the call sites because the reason
/// for it is the same everywhere: a cloud image's package lists are as old as
/// the image, and a stale list is the ordinary way an install of a
/// kernel-specific package fails on a VM's first boot. A guest that already
/// has what it needs never reaches this function at all -- every caller looks
/// before it installs -- so the refresh costs nothing on the second start of a
/// VM with no network.
pub fn install(
    manager: PackageManager,
    packages: &[Package],
    kernel_release: &str,
) -> (Outcome, String) {
    let names = name_list(packages, manager, kernel_release);
    let outcome = attempt(manager, &names);
    if outcome.succeeded() {
        return (outcome, describe(packages, manager, kernel_release));
    }

    let _ = command::run(
        manager.program(),
        manager.refresh_arguments(),
        manager.environment(),
        INSTALL_BUDGET,
    );

    (
        attempt(manager, &names),
        describe(packages, manager, kernel_release),
    )
}

/// One install, with this manager's own arguments and environment.
fn attempt(manager: PackageManager, names: &[String]) -> Outcome {
    let mut arguments: Vec<&str> = manager.install_arguments().to_vec();
    arguments.extend(names.iter().map(String::as_str));
    command::run(
        manager.program(),
        &arguments,
        manager.environment(),
        INSTALL_BUDGET,
    )
}

/// What a failed install is reported as running.
#[must_use]
pub fn install_command(manager: PackageManager) -> String {
    format!("{} {}", manager.program(), manager.install_arguments()[0])
}

/// What a guest with no manager this build knows is told, in the voice of a
/// stage that cannot go on.
///
/// Named by purpose rather than by package, because there is no manager here
/// to decide what any of it would have been called.
#[must_use]
pub fn no_manager(packages: &[Package]) -> String {
    let wanted: Vec<&str> = packages.iter().map(|package| package.purpose()).collect();
    let wanted = match wanted.split_last() {
        Some((last, [])) => (*last).to_owned(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
        None => "anything".to_owned(),
    };
    format!(
        "this guest carries no package manager vmlord-agent knows how to drive, so {wanted} \
         cannot be installed"
    )
}

#[cfg(test)]
mod tests {
    use super::{Package, describe, install_command, name_list, names, no_manager};
    use crate::guest_platform::{MANAGERS, PackageManager};

    #[test]
    fn the_headers_name_follows_the_manager_rather_than_the_distribution() {
        assert_eq!(
            names(
                Package::KernelHeaders,
                PackageManager::Apt,
                "6.8.0-45-generic"
            ),
            ["linux-headers-6.8.0-45-generic"],
            "apt ships one headers package per kernel, so the release is in the name"
        );
        assert_eq!(
            names(
                Package::KernelHeaders,
                PackageManager::Pacman,
                "6.8.0-45-generic"
            ),
            ["linux-headers"],
            "pacman ships one per kernel flavour, and the running release is not in it"
        );
    }

    #[test]
    fn every_manager_has_a_name_for_every_package() {
        for manager in MANAGERS {
            for package in [
                Package::Dkms,
                Package::BuildTools,
                Package::KernelHeaders,
                Package::AppIndicator,
                Package::Mesa,
                Package::RenderTools,
            ] {
                assert!(
                    !names(package, manager, "6.8.0-45-generic").is_empty(),
                    "{manager} has no name for {package:?}"
                );
            }
        }
    }

    #[test]
    fn a_set_of_packages_reads_as_one_phrase() {
        assert_eq!(
            describe(
                &[Package::Dkms, Package::BuildTools, Package::KernelHeaders],
                PackageManager::Apt,
                "6.8.0-45-generic"
            ),
            "dkms, build-essential and linux-headers-6.8.0-45-generic"
        );
        assert_eq!(
            describe(&[Package::Dkms], PackageManager::Pacman, "6.8"),
            "dkms"
        );
    }

    #[test]
    fn a_package_that_is_several_names_expands_to_all_of_them() {
        assert_eq!(
            name_list(&[Package::Mesa], PackageManager::Apt, "6.8"),
            ["libgl1-mesa-dri", "mesa-vulkan-drivers", "libvulkan1"]
        );
    }

    #[test]
    fn a_guest_with_no_manager_is_told_what_it_is_missing_in_words() {
        let reason = no_manager(&[Package::Dkms, Package::BuildTools, Package::KernelHeaders]);
        assert!(
            reason.ends_with(
                "so DKMS, a compiler and the running kernel's headers cannot be installed"
            ),
            "there is no manager here to name a package with, so the reason names purposes: \
             {reason}"
        );
    }

    #[test]
    fn the_reported_command_is_the_one_that_would_have_run() {
        assert_eq!(install_command(PackageManager::Apt), "apt-get install");
        assert_eq!(install_command(PackageManager::Pacman), "pacman -S");
    }
}
