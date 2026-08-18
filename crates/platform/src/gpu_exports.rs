//! What may be exported to a guest over Plan9, and what a guest is told about
//! it.
//!
//! Two system roots plus one exact per-VM root: the DriverStore's
//! `FileRepository`, `lxss\lib`, and the direct `gpu-payload` child of the VM
//! directory. Every candidate is canonicalized before it is judged, which is
//! what collapses `..` and resolves a reparse point to its target. System
//! candidates must remain below their system root; the per-VM candidate must
//! resolve to that exact direct child, not merely another descendant of the
//! VM directory. What is exported afterwards is the canonical path, not the
//! one discovery reported.

use std::path::{Component, Path, PathBuf};

use vmlord_core::{GpuShare, GpuShareManifest, HostGpuAdapter, RepositoryError};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE},
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_DIRECTORY,
            FILE_FLAG_BACKUP_SEMANTICS, FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES,
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle,
            GetFinalPathNameByHandleW, OPEN_EXISTING,
        },
        System::SystemInformation::GetSystemDirectoryW,
    },
    core::HSTRING,
};

use crate::error::windows_error;
use crate::layout::gpu_payload_staging_directory;

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

    pub(crate) fn share(&self) -> &GpuShare {
        &self.share
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

    /// Gives the VM access to every export, keeping only those it was given.
    ///
    /// Called after validation and never before it: a grant is what makes a
    /// path readable by the VM's own security principal, and handing one out
    /// for a path that has not been proven is how a check becomes decorative.
    ///
    /// An export the grant refused is dropped rather than fatal. Offering a VM
    /// a share it cannot open trades one clear line in the host's log for an
    /// opaque mount failure inside the guest.
    pub(crate) fn granted_to(
        self,
        hcs_id: &str,
        grant: &dyn Fn(&str, &Path) -> Result<(), RepositoryError>,
    ) -> Option<Self> {
        let exports: Vec<GpuExport> = self
            .exports
            .into_iter()
            .filter(|export| match grant(hcs_id, export.host_path()) {
                Ok(()) => true,
                Err(error) => {
                    log::warn!(
                        "not offering share \"{}\": the VM could not be given access to \"{}\": \
                         {error}",
                        export.name(),
                        export.host_path().display()
                    );
                    false
                }
            })
            .collect();

        (!exports.is_empty()).then_some(Self { exports })
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

impl GpuExports {
    /// Every share this host justifies for `adapters`, or `None` when there is
    /// nothing to export.
    ///
    /// Not a `Result`: a host with no WSL payload and no resolvable package is
    /// a host that gets no shares, which is an answer. What went wrong on the
    /// way to it is logged where it happened.
    pub(crate) fn build(adapters: &[HostGpuAdapter], vm_directory: &Path) -> Option<Self> {
        let system32 = system_directory()?;
        let canonicalize = canonical_directory;
        let roots = ExportRoots::resolve(&system32, &canonicalize);

        build_with_payload(adapters, &roots, vm_directory, &canonicalize)
    }
}

/// The longest path either call below is first asked for; both grow or fail
/// rather than truncate. 260 is what `gpu_discovery` uses for the same call.
const PATH_BUFFER: usize = 260;

/// The host's `System32`, as Windows spells it.
fn system_directory() -> Option<PathBuf> {
    let mut buffer = [0_u16; PATH_BUFFER];
    // SAFETY: `buffer` is passed as a sized slice; a zero return means the
    // call did not fill it.
    let length = unsafe { GetSystemDirectoryW(Some(&mut buffer)) } as usize;
    if length == 0 || length > buffer.len() {
        log::warn!("the system directory could not be read; nothing may be exported");
        return None;
    }

    Some(PathBuf::from(String::from_utf16_lossy(&buffer[..length])))
}

/// A kernel handle this module owns and closes exactly once.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from the successful `CreateFileW` below and
        // is closed only here.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// What `path` really is, provided it is a directory.
///
/// Opened **without** `FILE_FLAG_OPEN_REPARSE_POINT` on purpose: a junction or
/// symlink is followed, and the final path is its target's, so a link leading
/// out of an allowed root is caught by the root check instead of exporting
/// what it points at. `..` collapses in the same answer.
///
/// Time of check to time of use is not fully closable here: between this call
/// and the start, a directory could in principle be swapped, and an open
/// handle prevents deletion but not renaming. What limits it is that the path
/// exported afterwards is this canonical one -- a link swapped later cannot
/// redirect it. The two system roots live under `System32`, which takes
/// administrator rights to write to. The per-VM path is instead accepted only
/// when its final name is the exact canonical `gpu-payload` child; a privileged
/// concurrent replacement remains outside what this check can close.
fn canonical_directory(path: &Path) -> Result<PathBuf, RepositoryError> {
    let wide = HSTRING::from(path.as_os_str().to_string_lossy().as_ref());
    // SAFETY: `wide` outlives the call, and the returned handle is owned by
    // `OwnedHandle` and closed exactly once. `FILE_FLAG_BACKUP_SEMANTICS` is
    // what allows a directory to be opened at all.
    let handle = unsafe {
        CreateFileW(
            &wide,
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
    }
    .map_err(|error| windows_error("open a GPU export directory", None, error))?;
    let handle = OwnedHandle(handle);

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle is live for the call, and `information` is a
    // correctly sized structure this call fills in.
    unsafe { GetFileInformationByHandle(handle.0, &raw mut information) }
        .map_err(|error| windows_error("read a GPU export directory", None, error))?;
    if information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0 {
        return Err(RepositoryError::new(format!(
            "\"{}\" is not a directory and cannot be exported",
            path.display()
        )));
    }

    let mut buffer = vec![0_u16; PATH_BUFFER];
    loop {
        // SAFETY: the handle is live, and the buffer is passed with its own
        // length; a return larger than that length is the required size and
        // nothing was written.
        let length =
            unsafe { GetFinalPathNameByHandleW(handle.0, &mut buffer, FILE_NAME_NORMALIZED) }
                as usize;
        if length == 0 {
            return Err(windows_error(
                "resolve a GPU export directory",
                None,
                windows::core::Error::from_win32(),
            ));
        }
        if length >= buffer.len() {
            buffer = vec![0_u16; length + 1];
            continue;
        }

        let resolved = String::from_utf16_lossy(&buffer[..length]);
        return Ok(PathBuf::from(strip_extended_prefix(&resolved)));
    }
}

/// The ordinary form of a `\\?\C:\...` answer.
///
/// Only a drive path is unwrapped: `\\?\UNC\...` means something different,
/// and cutting its prefix off would produce a path that resolves nowhere.
fn strip_extended_prefix(path: &str) -> &str {
    let Some(rest) = path.strip_prefix(r"\\?\") else {
        return path;
    };
    let mut characters = rest.chars();
    match (characters.next(), characters.next(), characters.next()) {
        (Some(drive), Some(':'), Some('\\')) if drive.is_ascii_alphabetic() => rest,
        _ => path,
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
            log::debug!(
                "nothing to export from \"{}\": {error}",
                candidate.display()
            );
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

/// Adds only the exact staging child of a canonical VM directory.
pub(crate) fn build_with_payload(
    adapters: &[HostGpuAdapter],
    roots: &ExportRoots,
    vm_directory: &Path,
    canonicalize: Canonicalize<'_>,
) -> Option<GpuExports> {
    let mut exports = build_with(adapters, roots, canonicalize)
        .map(|exports| exports.exports)
        .unwrap_or_default();
    let candidate = gpu_payload_staging_directory(vm_directory);
    if let (Ok(vm), Ok(payload)) = (canonicalize(vm_directory), canonicalize(&candidate))
        && same_path(&vm.join("gpu-payload"), &payload)
    {
        exports.insert(
            0,
            GpuExport {
                share: GpuShare::payload(),
                host_path: payload,
            },
        );
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
        sync::Mutex,
    };

    use vmlord_core::{GpuShare, GpuShareRole, HostGpuAdapter, RepositoryError};

    use super::{ExportRoots, GpuExports, build_with, build_with_payload, strip_extended_prefix};

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
    fn payload_wsl_and_driver_package_have_distinct_roles_and_order() {
        let vm = Path::new(r"D:\VMLord\dev-linux");
        let payload = r"D:\VMLord\dev-linux\gpu-payload";
        let package = format!(r"{REPOSITORY}\nvltsi.inf_amd64_1");
        let canonicalize = canonicalizer(&[
            (SYSTEM32, SYSTEM32),
            (REPOSITORY, REPOSITORY),
            (
                r"C:\Windows\System32\lxss\lib",
                r"C:\Windows\System32\lxss\lib",
            ),
            (&package, &package),
            (r"D:\VMLord\dev-linux", r"D:\VMLord\dev-linux"),
            (payload, payload),
        ]);
        let roots = ExportRoots::resolve(Path::new(SYSTEM32), &canonicalize);
        let roles: Vec<_> =
            build_with_payload(&[adapter(Some(&package))], &roots, vm, &canonicalize)
                .unwrap()
                .manifest()
                .shares
                .into_iter()
                .map(|share| share.role)
                .collect();
        assert!(matches!(
            roles.as_slice(),
            [
                GpuShareRole::GpuPayload,
                GpuShareRole::WslLib,
                GpuShareRole::DriverPackage { .. }
            ]
        ));
    }

    #[test]
    fn a_payload_directory_reparsed_outside_its_vm_is_dropped() {
        let vm = Path::new(r"D:\VMLord\dev-linux");
        let canonicalize = canonicalizer(&[
            (SYSTEM32, SYSTEM32),
            (REPOSITORY, REPOSITORY),
            (r"D:\VMLord\dev-linux", r"D:\VMLord\dev-linux"),
            (r"D:\VMLord\dev-linux\gpu-payload", r"D:\attacker\payload"),
        ]);
        let roots = ExportRoots::resolve(Path::new(SYSTEM32), &canonicalize);
        assert!(build_with_payload(&[], &roots, vm, &canonicalize).is_none());
    }

    #[test]
    fn a_payload_directory_reparsed_to_a_nested_vm_descendant_is_dropped() {
        let vm = Path::new(r"D:\VMLord\dev-linux");
        let canonicalize = canonicalizer(&[
            (SYSTEM32, SYSTEM32),
            (REPOSITORY, REPOSITORY),
            (r"D:\VMLord\dev-linux", r"D:\VMLord\dev-linux"),
            (
                r"D:\VMLord\dev-linux\gpu-payload",
                r"D:\VMLord\dev-linux\attacker\payload",
            ),
        ]);
        let roots = ExportRoots::resolve(Path::new(SYSTEM32), &canonicalize);

        assert!(build_with_payload(&[], &roots, vm, &canonicalize).is_none());
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

    #[test]
    fn every_export_is_granted_before_it_is_offered() {
        let granted: Mutex<Vec<(String, PathBuf)>> = Mutex::new(Vec::new());
        let exports = GpuExports::for_test(vec![
            (
                GpuShare::wsl_lib(),
                PathBuf::from(r"C:\Windows\System32\lxss\lib"),
            ),
            (
                GpuShare::driver_package("nvltsi.inf_amd64_1").unwrap(),
                PathBuf::from(format!(r"{REPOSITORY}\nvltsi.inf_amd64_1")),
            ),
        ]);

        let survived = exports
            .granted_to("hcs-id", &|id, path| {
                granted
                    .lock()
                    .unwrap()
                    .push((id.to_owned(), path.to_path_buf()));
                Ok(())
            })
            .expect("both survive");

        assert_eq!(survived.iter().count(), 2);
        assert_eq!(
            granted.lock().unwrap().as_slice(),
            [
                (
                    "hcs-id".to_owned(),
                    PathBuf::from(r"C:\Windows\System32\lxss\lib")
                ),
                (
                    "hcs-id".to_owned(),
                    PathBuf::from(format!(r"{REPOSITORY}\nvltsi.inf_amd64_1"))
                )
            ]
        );
    }

    #[test]
    fn an_export_the_grant_refused_is_dropped_and_the_rest_survive() {
        let exports = GpuExports::for_test(vec![
            (
                GpuShare::wsl_lib(),
                PathBuf::from(r"C:\Windows\System32\lxss\lib"),
            ),
            (
                GpuShare::driver_package("nvltsi.inf_amd64_1").unwrap(),
                PathBuf::from(format!(r"{REPOSITORY}\nvltsi.inf_amd64_1")),
            ),
        ]);

        let survived = exports
            .granted_to("hcs-id", &|_, path| {
                if path.ends_with("lib") {
                    Err(RepositoryError::new("access denied"))
                } else {
                    Ok(())
                }
            })
            .expect("one survives");

        assert_eq!(survived.iter().count(), 1);
        assert_eq!(
            survived.iter().next().unwrap().name(),
            "vmlord.gpu.drv.nvltsi.inf_amd64_1"
        );
    }

    #[test]
    fn a_set_no_grant_survived_is_nothing_to_export() {
        let exports = GpuExports::for_test(vec![(
            GpuShare::wsl_lib(),
            PathBuf::from(r"C:\Windows\System32\lxss\lib"),
        )]);

        assert!(
            exports
                .granted_to("hcs-id", &|_, _| Err(RepositoryError::new("access denied")))
                .is_none()
        );
    }

    #[test]
    fn the_extended_prefix_is_stripped_from_a_drive_path() {
        // `GetFinalPathNameByHandleW` answers in `\\?\` form; HCS is given the
        // ordinary path, which is what the AppSandbox backend passed and what
        // a reader recognises in a log.
        assert_eq!(
            strip_extended_prefix(r"\\?\C:\Windows\System32\lxss\lib"),
            r"C:\Windows\System32\lxss\lib"
        );
    }

    #[test]
    fn a_unc_answer_keeps_its_prefix() {
        // `\\?\UNC\server\share` is not a drive path, and cutting four
        // characters off it would produce something that resolves nowhere.
        assert_eq!(
            strip_extended_prefix(r"\\?\UNC\server\share"),
            r"\\?\UNC\server\share"
        );
    }

    #[test]
    fn a_plain_path_is_left_alone() {
        assert_eq!(
            strip_extended_prefix(r"C:\Windows\System32"),
            r"C:\Windows\System32"
        );
    }

    #[test]
    #[ignore = "reads the real host's directories"]
    fn exports_built_on_this_host_are_sound() {
        // What this can assert is self-consistency: on a host with no GPU-PV
        // and no WSL there is nothing to export, and demanding either would be
        // a test that is permanently red on half the machines it runs on.
        let capabilities = crate::discover_host_gpu();
        let Some(exports) =
            super::GpuExports::build(&capabilities.adapters, Path::new(r"C:\VMLord\ignored"))
        else {
            println!("nothing to export on this host");
            return;
        };

        let mut names = Vec::new();
        for export in exports.iter() {
            println!("{} -> {}", export.name(), export.host_path().display());
            assert!(
                export.host_path().is_dir(),
                "an exported path must be a directory: {}",
                export.host_path().display()
            );
            assert!(
                export
                    .host_path()
                    .to_string_lossy()
                    .to_lowercase()
                    .contains(r"\system32\"),
                "an exported path must live under System32: {}",
                export.host_path().display()
            );
            names.push(export.name().to_owned());
        }

        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            unique.len(),
            names.len(),
            "share names must be unique: {names:?}"
        );
    }
}
