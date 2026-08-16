//! Where a GPU share is allowed to be mounted in this guest.
//!
//! The host names a share and says what it holds; this module is the only
//! place that turns that into a path. Nothing here trusts the manifest: a
//! package name arrives from another machine and becomes part of a filesystem
//! path, which is exactly where "the other side already validated it" stops
//! being a reason not to validate it again.

use std::path::{Path, PathBuf};

use vmlord_agent_protocol::v1::{GpuShare, GpuShareRole};

/// Where the host's WSL Linux userspace is mounted.
///
/// WSL's own path, because the Mesa D3D12 driver and the vendor libraries in a
/// DriverStore package expect to find each other where WSL puts them.
pub const WSL_LIB: &str = "/usr/lib/wsl/lib";

/// The directory each driver package is mounted below, one per package.
pub const DRIVER_ROOT: &str = "/usr/lib/wsl/drivers";

/// Where the VM's own prepared GPU payload is mounted.
///
/// VMLord's rather than the distribution's or the administrator's, which is
/// what `/opt` is for.
pub const PAYLOAD: &str = "/opt/vmlord/gpu-payload";

/// The longest package folder name that may become part of a path.
///
/// The same bound `vmlord_core` puts on the host side of this, restated
/// because this end is what builds the path.
const MAX_PACKAGE_NAME: usize = 96;

/// The longest share name this guest will put in a mount option string.
const MAX_SHARE_NAME: usize = 128;

/// One share of a manifest, once the guest has decided what to do with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Planned {
    /// A share this guest has a target for.
    Mount { share: String, path: PathBuf },
    /// A share this guest will not mount, and why.
    Refused { share: String, reason: Refusal },
}

/// Why a share got no target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// A role this build has never heard of, which is a newer host talking to
    /// an older agent.
    UnknownRole,
    /// A name that cannot be put in a comma-separated mount option string.
    InvalidName,
    /// A package name that cannot become one path component.
    InvalidPackage,
    /// A target an earlier share of the same manifest already claimed.
    DuplicateTarget,
}

impl Refusal {
    /// What to tell the host, which reads this in its log.
    pub const fn message(self) -> &'static str {
        match self {
            Self::UnknownRole => "this agent has no target for that share's role",
            Self::InvalidName => "that share name cannot be used as a 9p aname",
            Self::InvalidPackage => "that driver package name cannot become a path",
            Self::DuplicateTarget => "an earlier share of this manifest claimed that target",
        }
    }
}

/// Decides where every share of a manifest goes.
///
/// The order is the manifest's, so that the host can read the answer back
/// against what it sent without matching by name. A share that is refused
/// leaves the rest of the manifest alone: GPU is best effort on this end too,
/// and one package the host named oddly must not cost a guest its WSL
/// userspace.
pub fn plan(shares: &[GpuShare]) -> Vec<Planned> {
    let mut planned = Vec::with_capacity(shares.len());
    let mut claimed: Vec<PathBuf> = Vec::with_capacity(shares.len());

    for share in shares {
        let name = share.name.clone();
        let path = match target(share) {
            Ok(path) => path,
            Err(reason) => {
                planned.push(Planned::Refused {
                    share: name,
                    reason,
                });
                continue;
            }
        };

        if claimed.contains(&path) {
            planned.push(Planned::Refused {
                share: name,
                reason: Refusal::DuplicateTarget,
            });
            continue;
        }

        claimed.push(path.clone());
        planned.push(Planned::Mount { share: name, path });
    }

    planned
}

/// The target one share asks for, or why it gets none.
fn target(share: &GpuShare) -> Result<PathBuf, Refusal> {
    if !is_mountable_name(&share.name) {
        return Err(Refusal::InvalidName);
    }

    match share.role() {
        GpuShareRole::WslLib => Ok(PathBuf::from(WSL_LIB)),
        GpuShareRole::GpuPayload => Ok(PathBuf::from(PAYLOAD)),
        GpuShareRole::DriverPackage => {
            if !is_path_component(&share.package) {
                return Err(Refusal::InvalidPackage);
            }
            Ok(Path::new(DRIVER_ROOT).join(&share.package))
        }
        GpuShareRole::Unspecified => Err(Refusal::UnknownRole),
    }
}

/// Whether a name can travel in the comma-separated options of `mount(2)`.
///
/// A separator, a quote or a space in an `aname` would be read by the kernel's
/// option parser as structure rather than as text.
fn is_mountable_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_SHARE_NAME
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
        })
}

/// Whether a package name can be exactly one path component.
///
/// `.` and `..` are the components that would not be one: they would move the
/// target rather than name it.
fn is_path_component(package: &str) -> bool {
    is_mountable_name(package)
        && package.len() <= MAX_PACKAGE_NAME
        && package != "."
        && package != ".."
}

/// Whether a path is one this agent mounts shares at.
///
/// What makes an unmount safe: the cleanup reads the mount table rather than a
/// list this process kept, so it needs a rule for telling its own mounts from
/// everything else the guest has mounted.
pub fn is_managed(path: &Path) -> bool {
    path == Path::new(WSL_LIB)
        || path == Path::new(PAYLOAD)
        || path.parent() == Some(Path::new(DRIVER_ROOT))
}

#[cfg(test)]
mod tests {
    use vmlord_agent_protocol::v1::{GpuShare, GpuShareRole};

    use super::{Planned, Refusal, is_managed, plan};
    use std::path::{Path, PathBuf};

    fn share(name: &str, role: GpuShareRole, package: &str) -> GpuShare {
        GpuShare {
            name: name.to_owned(),
            role: i32::from(role),
            package: package.to_owned(),
        }
    }

    #[test]
    fn every_role_this_build_knows_has_a_target() {
        // A role with no target is a share that is exported to the guest and
        // then never mounted, which reads on the host as a working GPU.
        let planned = plan(&[
            share("vmlord.gpu.wsl-lib", GpuShareRole::WslLib, ""),
            share("vmlord.gpu.payload", GpuShareRole::GpuPayload, ""),
            share(
                "vmlord.gpu.drv.nv_dispi.inf_amd64_1234",
                GpuShareRole::DriverPackage,
                "nv_dispi.inf_amd64_1234",
            ),
        ]);

        assert_eq!(
            planned,
            vec![
                Planned::Mount {
                    share: "vmlord.gpu.wsl-lib".to_owned(),
                    path: PathBuf::from("/usr/lib/wsl/lib"),
                },
                Planned::Mount {
                    share: "vmlord.gpu.payload".to_owned(),
                    path: PathBuf::from("/opt/vmlord/gpu-payload"),
                },
                Planned::Mount {
                    share: "vmlord.gpu.drv.nv_dispi.inf_amd64_1234".to_owned(),
                    path: PathBuf::from("/usr/lib/wsl/drivers/nv_dispi.inf_amd64_1234"),
                },
            ]
        );
    }

    #[test]
    fn a_package_name_that_would_leave_its_directory_is_refused() {
        // A package name becomes a path component, so a traversal in it would
        // mount a host share over a directory of the guest's own.
        for package in ["..", ".", "", "a/b", "a\\b", "a,b", "../../etc"] {
            let planned = plan(&[share(
                "vmlord.gpu.drv.x",
                GpuShareRole::DriverPackage,
                package,
            )]);
            assert_eq!(
                planned,
                vec![Planned::Refused {
                    share: "vmlord.gpu.drv.x".to_owned(),
                    reason: Refusal::InvalidPackage,
                }],
                "package {package:?} must not become a path"
            );
        }
    }

    #[test]
    fn an_over_long_package_name_is_refused() {
        let package = "a".repeat(97);
        let planned = plan(&[share(
            "vmlord.gpu.drv.x",
            GpuShareRole::DriverPackage,
            &package,
        )]);

        assert_eq!(
            planned,
            vec![Planned::Refused {
                share: "vmlord.gpu.drv.x".to_owned(),
                reason: Refusal::InvalidPackage,
            }]
        );
    }

    #[test]
    fn a_share_name_that_is_not_option_safe_is_refused() {
        // The name is handed to `mount(2)` inside a comma-separated option
        // string, where a comma or an equals sign is structure.
        for name in ["", "vmlord,gpu", "vmlord gpu", "aname=x", "vmlord/gpu"] {
            let planned = plan(&[share(name, GpuShareRole::WslLib, "")]);
            assert_eq!(
                planned,
                vec![Planned::Refused {
                    share: name.to_owned(),
                    reason: Refusal::InvalidName,
                }],
                "share name {name:?} must not reach a mount option string"
            );
        }
    }

    #[test]
    fn a_role_this_build_does_not_know_is_refused_rather_than_guessed_at() {
        // A newer host may name a role this agent has never heard of. Mounting
        // it somewhere plausible would put host content at a path chosen by a
        // build that did not know what the content was.
        let unknown = GpuShare {
            name: "vmlord.gpu.future".to_owned(),
            role: 97,
            package: String::new(),
        };

        assert_eq!(
            plan(&[unknown]),
            vec![Planned::Refused {
                share: "vmlord.gpu.future".to_owned(),
                reason: Refusal::UnknownRole,
            }]
        );
    }

    #[test]
    fn two_shares_claiming_one_target_leave_the_first_one_mounted() {
        // Mounting the second over the first would hide content the host
        // exported and reported as mounted.
        let planned = plan(&[
            share("vmlord.gpu.wsl-lib", GpuShareRole::WslLib, ""),
            share("vmlord.gpu.wsl-lib.again", GpuShareRole::WslLib, ""),
        ]);

        assert_eq!(
            planned,
            vec![
                Planned::Mount {
                    share: "vmlord.gpu.wsl-lib".to_owned(),
                    path: PathBuf::from("/usr/lib/wsl/lib"),
                },
                Planned::Refused {
                    share: "vmlord.gpu.wsl-lib.again".to_owned(),
                    reason: Refusal::DuplicateTarget,
                },
            ]
        );
    }

    #[test]
    fn only_the_agents_own_targets_are_managed() {
        // The cleanup unmounts what this says yes to, so a path outside the
        // three roots must never be one of them.
        assert!(is_managed(Path::new("/usr/lib/wsl/lib")));
        assert!(is_managed(Path::new("/opt/vmlord/gpu-payload")));
        assert!(is_managed(Path::new("/usr/lib/wsl/drivers/nv_dispi.inf")));

        assert!(!is_managed(Path::new("/usr/lib/wsl")));
        assert!(!is_managed(Path::new("/usr/lib/wsl/drivers")));
        assert!(!is_managed(Path::new("/usr/lib/wsl/drivers/a/b")));
        assert!(!is_managed(Path::new("/opt/vmlord")));
        assert!(!is_managed(Path::new("/")));
    }
}
