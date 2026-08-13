//! What may be exported to a guest over Plan9, and what a guest is told about
//! it.
//!
//! Two roots and nothing else: the DriverStore's `FileRepository`, for driver
//! packages, and `lxss\lib`, for the Linux userspace WSL stages. Every
//! candidate is canonicalized before it is judged, which is what collapses
//! `..` and resolves a reparse point to its target -- a junction leading out
//! of a root then fails the root check instead of quietly exporting whatever
//! it points at. What is exported afterwards is the canonical path, not the
//! one discovery reported.
//!
//! Nothing in the running application calls this yet: a start cannot know a
//! VM's GPU mode until the task that applies HCS assignment records one, and
//! that task is this module's caller. The allow below goes away with it.
#![allow(dead_code)]

use std::path::{Component, Path, PathBuf};

use vmlord_core::{GpuShare, GpuShareManifest, HostGpuAdapter, RepositoryError};

/// Resolves a path to its canonical form, failing if it is not a directory.
pub(crate) type Canonicalize<'a> = &'a dyn Fn(&Path) -> Result<PathBuf, RepositoryError>;

/// One host directory offered to a guest.
pub(crate) struct GpuExport {
    share: GpuShare,
    /// The canonical path, which is what HCS is given and what the VM is
    /// granted access to.
    host_path: PathBuf,
}

impl GpuExport {
    pub(crate) fn name(&self) -> &str {
        &self.share.name
    }

    pub(crate) fn host_path(&self) -> &Path {
        &self.host_path
    }
}

/// Every share a VM is to be offered, deduplicated and in mount order.
///
/// Non-empty by construction: "there is nothing to export" is `None` from
/// [`build_with`], not an empty set that later code would have to test for.
pub(crate) struct GpuExports {
    exports: Vec<GpuExport>,
}

impl GpuExports {
    pub(crate) fn iter(&self) -> impl Iterator<Item = &GpuExport> {
        self.exports.iter()
    }

    /// What the guest is told: names and roles, no host paths.
    pub(crate) fn manifest(&self) -> GpuShareManifest {
        GpuShareManifest {
            shares: self
                .exports
                .iter()
                .map(|export| export.share.clone())
                .collect(),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(exports: Vec<(GpuShare, PathBuf)>) -> Self {
        Self {
            exports: exports
                .into_iter()
                .map(|(share, host_path)| GpuExport { share, host_path })
                .collect(),
        }
    }
}

/// The directories a share may come from, as they actually are on this host.
///
/// A root that canonicalizes outside `System32` is dropped rather than
/// trusted: everything under it would inherit that redirection, and a check
/// against a moved root would pass while exporting somewhere else entirely.
pub(crate) struct ExportRoots {
    driver_packages: Option<PathBuf>,
    wsl_lib: Option<PathBuf>,
}

impl ExportRoots {
    pub(crate) fn resolve(system32: &Path, canonicalize: Canonicalize<'_>) -> Self {
        let Ok(system32) = canonicalize(system32) else {
            log::warn!("the system directory could not be resolved; nothing may be exported");
            return Self {
                driver_packages: None,
                wsl_lib: None,
            };
        };

        Self {
            driver_packages: resolve_root(
                &system32,
                &system32.join("DriverStore").join("FileRepository"),
                canonicalize,
            ),
            wsl_lib: resolve_root(&system32, &system32.join("lxss").join("lib"), canonicalize),
        }
    }
}

fn resolve_root(
    system32: &Path,
    candidate: &Path,
    canonicalize: Canonicalize<'_>,
) -> Option<PathBuf> {
    match canonicalize(candidate) {
        Ok(resolved) if is_within(system32, &resolved) => Some(resolved),
        Ok(resolved) => {
            log::warn!(
                "refusing to export from \"{}\": it resolves to \"{}\", outside \"{}\"",
                candidate.display(),
                resolved.display(),
                system32.display()
            );
            None
        }
        Err(error) => {
            log::debug!("nothing to export from \"{}\": {error}", candidate.display());
            None
        }
    }
}

/// Every share `adapters` justify, in the order a guest should mount them.
///
/// The WSL payload comes first: a driver package without it renders nothing,
/// and a partial set is what a guest gets when something below is dropped.
pub(crate) fn build_with(
    adapters: &[HostGpuAdapter],
    roots: &ExportRoots,
    canonicalize: Canonicalize<'_>,
) -> Option<GpuExports> {
    let mut exports: Vec<GpuExport> = Vec::new();

    if let Some(wsl_lib) = &roots.wsl_lib {
        exports.push(GpuExport {
            share: GpuShare::wsl_lib(),
            host_path: wsl_lib.clone(),
        });
    }

    for adapter in adapters {
        let Some(driver_store) = &adapter.driver_store else {
            continue;
        };
        let Some(root) = &roots.driver_packages else {
            continue;
        };

        let resolved = match canonicalize(driver_store) {
            Ok(resolved) => resolved,
            Err(error) => {
                log::warn!(
                    "not exporting the driver package of \"{}\": {error}",
                    adapter.name
                );
                continue;
            }
        };
        if !is_within(root, &resolved) {
            log::warn!(
                "not exporting \"{}\" for \"{}\": it resolves to \"{}\", outside \"{}\"",
                driver_store.display(),
                adapter.name,
                resolved.display(),
                root.display()
            );
            continue;
        }
        if exports
            .iter()
            .any(|export| same_path(export.host_path(), &resolved))
        {
            continue;
        }

        let Some(folder) = resolved
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
        else {
            continue;
        };
        let Some(share) = GpuShare::driver_package(&folder) else {
            log::warn!(
                "not exporting \"{}\": \"{folder}\" cannot become a share name",
                resolved.display()
            );
            continue;
        };

        exports.push(GpuExport {
            share,
            host_path: resolved,
        });
    }

    (!exports.is_empty()).then_some(GpuExports { exports })
}

/// Whether `path` is `root` or lies under it, compared component by component.
///
/// Not a string prefix: `...\FileRepositoryEvil` starts with
/// `...\FileRepository` and is a different directory.
fn is_within(root: &Path, path: &Path) -> bool {
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

fn same_path(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        path::{Path, PathBuf},
    };

    use vmlord_core::{GpuShareRole, HostGpuAdapter, RepositoryError};

    use super::{ExportRoots, build_with};

    const SYSTEM32: &str = r"C:\Windows\System32";
    const REPOSITORY: &str = r"C:\Windows\System32\DriverStore\FileRepository";

    /// A canonicalizer over a fixed table: anything not in it does not exist,
    /// and an entry mapping elsewhere is a reparse point pointing there.
    fn canonicalizer(
        entries: &[(&str, &str)],
    ) -> impl Fn(&Path) -> Result<PathBuf, RepositoryError> + use<> {
        let table: HashMap<String, String> = entries
            .iter()
            .map(|(from, to)| ((*from).to_lowercase(), (*to).to_owned()))
            .collect();
        move |path: &Path| {
            table
                .get(&path.to_string_lossy().to_lowercase())
                .map(PathBuf::from)
                .ok_or_else(|| {
                    RepositoryError::new(format!("no such directory: {}", path.display()))
                })
        }
    }

    fn adapter(driver_store: Option<&str>) -> HostGpuAdapter {
        HostGpuAdapter {
            name: "Microsoft Virtual Render Driver".to_owned(),
            instance_id: r"PCI\VEN_10DE&DEV_1234\3&11583659&0&08".to_owned(),
            interface_path: r"\\?\pci#ven_10de".to_owned(),
            driver_store: driver_store.map(PathBuf::from),
            service: Some("nvlddmkm".to_owned()),
        }
    }

    #[test]
    fn a_package_and_the_wsl_payload_become_two_shares() {
        let package = format!(r"{REPOSITORY}\nvltsi.inf_amd64_1");
        let canonicalize = canonicalizer(&[
            (SYSTEM32, SYSTEM32),
            (REPOSITORY, REPOSITORY),
            (
                r"C:\Windows\System32\lxss\lib",
                r"C:\Windows\System32\lxss\lib",
            ),
            (&package, &package),
        ]);
        let roots = ExportRoots::resolve(Path::new(SYSTEM32), &canonicalize);

        let exports =
            build_with(&[adapter(Some(&package))], &roots, &canonicalize).expect("two shares");

        let names: Vec<_> = exports
            .iter()
            .map(|export| export.name().to_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                "vmlord.gpu.wsl-lib".to_owned(),
                "vmlord.gpu.drv.nvltsi.inf_amd64_1".to_owned()
            ],
            "the payload comes first: a driver package without it renders nothing"
        );
        assert_eq!(
            exports
                .iter()
                .map(|export| export.host_path().to_path_buf())
                .collect::<Vec<_>>(),
            vec![
                PathBuf::from(r"C:\Windows\System32\lxss\lib"),
                PathBuf::from(&package)
            ]
        );
    }

    #[test]
    fn a_host_without_wsl_still_exports_its_packages() {
        let package = format!(r"{REPOSITORY}\nvltsi.inf_amd64_1");
        let canonicalize = canonicalizer(&[
            (SYSTEM32, SYSTEM32),
            (REPOSITORY, REPOSITORY),
            (&package, &package),
        ]);
        let roots = ExportRoots::resolve(Path::new(SYSTEM32), &canonicalize);

        let exports =
            build_with(&[adapter(Some(&package))], &roots, &canonicalize).expect("one share");

        assert_eq!(exports.iter().count(), 1);
        assert_eq!(
            exports.iter().next().unwrap().name(),
            "vmlord.gpu.drv.nvltsi.inf_amd64_1"
        );
    }

    #[test]
    fn a_package_outside_the_repository_is_dropped() {
        let outside = r"C:\Temp\evil";
        let canonicalize = canonicalizer(&[
            (SYSTEM32, SYSTEM32),
            (REPOSITORY, REPOSITORY),
            (outside, outside),
        ]);
        let roots = ExportRoots::resolve(Path::new(SYSTEM32), &canonicalize);

        assert!(build_with(&[adapter(Some(outside))], &roots, &canonicalize).is_none());
    }

    #[test]
    fn a_package_whose_reparse_point_leads_out_is_dropped() {
        // The path looks like a package; its canonical form is somewhere else
        // entirely, which is exactly what a junction escape looks like.
        let package = format!(r"{REPOSITORY}\nvltsi.inf_amd64_1");
        let canonicalize = canonicalizer(&[
            (SYSTEM32, SYSTEM32),
            (REPOSITORY, REPOSITORY),
            (&package, r"D:\attacker\payload"),
        ]);
        let roots = ExportRoots::resolve(Path::new(SYSTEM32), &canonicalize);

        assert!(build_with(&[adapter(Some(&package))], &roots, &canonicalize).is_none());
    }

    #[test]
    fn a_sibling_root_with_the_same_prefix_is_not_the_root() {
        // `FileRepositoryEvil` passes a string-prefix test and must fail a
        // component-wise one.
        let package = r"C:\Windows\System32\DriverStore\FileRepositoryEvil\pkg";
        let canonicalize = canonicalizer(&[
            (SYSTEM32, SYSTEM32),
            (REPOSITORY, REPOSITORY),
            (package, package),
        ]);
        let roots = ExportRoots::resolve(Path::new(SYSTEM32), &canonicalize);

        assert!(build_with(&[adapter(Some(package))], &roots, &canonicalize).is_none());
    }

    #[test]
    fn a_repository_root_that_leaves_system32_is_refused_wholesale() {
        let package = format!(r"{REPOSITORY}\nvltsi.inf_amd64_1");
        let canonicalize = canonicalizer(&[
            (SYSTEM32, SYSTEM32),
            (REPOSITORY, r"D:\attacker"),
            (&package, r"D:\attacker\nvltsi.inf_amd64_1"),
        ]);
        let roots = ExportRoots::resolve(Path::new(SYSTEM32), &canonicalize);

        assert!(
            build_with(&[adapter(Some(&package))], &roots, &canonicalize).is_none(),
            "a redirected root cannot admit anything, however consistent the candidates look"
        );
    }

    #[test]
    fn a_wsl_payload_that_leaves_system32_is_dropped() {
        let canonicalize = canonicalizer(&[
            (SYSTEM32, SYSTEM32),
            (REPOSITORY, REPOSITORY),
            (r"C:\Windows\System32\lxss\lib", r"E:\elsewhere\lib"),
        ]);
        let roots = ExportRoots::resolve(Path::new(SYSTEM32), &canonicalize);

        assert!(build_with(&[], &roots, &canonicalize).is_none());
    }

    #[test]
    fn adapters_sharing_a_package_export_it_once() {
        let package = format!(r"{REPOSITORY}\nvltsi.inf_amd64_1");
        let same_folder_other_case = package.to_uppercase();
        let canonicalize = canonicalizer(&[
            (SYSTEM32, SYSTEM32),
            (REPOSITORY, REPOSITORY),
            (&package, &package),
            (&same_folder_other_case, &same_folder_other_case),
        ]);
        let roots = ExportRoots::resolve(Path::new(SYSTEM32), &canonicalize);

        let exports = build_with(
            &[
                adapter(Some(&package)),
                adapter(Some(&same_folder_other_case)),
                adapter(None),
            ],
            &roots,
            &canonicalize,
        )
        .expect("one share");

        assert_eq!(
            exports.iter().count(),
            1,
            "two adapters from one vendor share a FileRepository folder"
        );
    }

    #[test]
    fn a_package_folder_that_cannot_be_named_is_dropped() {
        let package = format!(r"{REPOSITORY}\pkg with spaces");
        let canonicalize = canonicalizer(&[
            (SYSTEM32, SYSTEM32),
            (REPOSITORY, REPOSITORY),
            (&package, &package),
        ]);
        let roots = ExportRoots::resolve(Path::new(SYSTEM32), &canonicalize);

        assert!(build_with(&[adapter(Some(&package))], &roots, &canonicalize).is_none());
    }

    #[test]
    fn the_manifest_says_only_a_name_and_a_role() {
        let package = format!(r"{REPOSITORY}\nvltsi.inf_amd64_1");
        let canonicalize = canonicalizer(&[
            (SYSTEM32, SYSTEM32),
            (REPOSITORY, REPOSITORY),
            (
                r"C:\Windows\System32\lxss\lib",
                r"C:\Windows\System32\lxss\lib",
            ),
            (&package, &package),
        ]);
        let roots = ExportRoots::resolve(Path::new(SYSTEM32), &canonicalize);

        let manifest = build_with(&[adapter(Some(&package))], &roots, &canonicalize)
            .expect("two shares")
            .manifest();

        assert_eq!(manifest.shares.len(), 2);
        assert_eq!(manifest.shares[0].role, GpuShareRole::WslLib);
        assert_eq!(
            manifest.shares[1].role,
            GpuShareRole::DriverPackage {
                package: "nvltsi.inf_amd64_1".to_owned()
            }
        );
    }
}
