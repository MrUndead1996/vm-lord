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

use std::path::{Path, PathBuf};

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
        System::{Com::CoTaskMemFree, SystemInformation::GetSystemDirectoryW},
        UI::Shell::{FOLDERID_ProgramFiles, KF_FLAG_DEFAULT, SHGetKnownFolderPath},
    },
    core::HSTRING,
};

use crate::error::windows_error;
use crate::{layout::gpu_payload_staging_directory, paths::is_within};

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

    /// Asks for VM access to every export, and offers all of them either way.
    ///
    /// Called after validation and never before it: a grant is what makes a
    /// path readable by the VM's own security principal, and handing one out
    /// for a path that has not been proven is how a check becomes decorative.
    ///
    /// The grant is best effort, and a refusal is expected rather than
    /// exceptional. Every GPU share lives under `System32`, whose DACLs belong
    /// to TrustedInstaller, so `HcsGrantVmAccess` is refused there however
    /// elevated VMLord is. It does not need to succeed: a Plan9 share is
    /// served by the host's own Plan9 server rather than opened by the VM's
    /// security principal, which is the difference between these shares and
    /// the VHDX files a start grants separately. The AppSandbox backend asks
    /// for the same grants on the same paths and ignores the answer, and its
    /// guests render.
    ///
    /// Dropping a share over a refusal is what this used to do, and it removed
    /// the guest's entire GPU userspace on every real host.
    pub(crate) fn granted_to(
        self,
        hcs_id: &str,
        grant: &dyn Fn(&str, &Path) -> Result<(), RepositoryError>,
    ) -> Self {
        for export in &self.exports {
            if let Err(error) = grant(hcs_id, export.host_path()) {
                // Debug, not warn: this is the ordinary answer for a path
                // under `System32`, and a warning per share per start would be
                // a log that reports the expected as a fault.
                tracing::debug!(
                    "share \"{}\" is offered without an explicit grant: the VM could not be \
                     given access to \"{}\": {error}",
                    export.name(),
                    export.host_path().display()
                );
            }
        }

        self
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
    pub(crate) fn build(
        adapters: &[HostGpuAdapter],
        vm_directory: &Path,
        payload: Option<&Path>,
    ) -> Option<Self> {
        let system32 = system_directory()?;
        let canonicalize = canonical_directory;
        let roots = ExportRoots::resolve(
            &system32,
            program_files_directory().as_deref(),
            &canonicalize,
        );

        build_with_payload(adapters, &roots, vm_directory, payload, &canonicalize)
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
        tracing::warn!("the system directory could not be read; nothing may be exported");
        return None;
    }

    Some(PathBuf::from(String::from_utf16_lossy(&buffer[..length])))
}

/// The host's `Program Files`, as Windows spells it.
///
/// Asked of the shell rather than read from `%ProgramFiles%`: the environment
/// variable is inherited and can be anything, and this decides what gets
/// exported to a VM.
pub(crate) fn program_files_directory() -> Option<PathBuf> {
    // SAFETY: the call takes no borrowed memory, and the buffer it returns is
    // the caller's to free.
    let path = unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramFiles, KF_FLAG_DEFAULT, None) }
        .inspect_err(|error| tracing::debug!("Program Files could not be read: {error}"))
        .ok()?;
    // SAFETY: a successful call returns a NUL-terminated wide string.
    let directory = unsafe { path.to_string() }
        .inspect_err(|error| tracing::debug!("Program Files is not valid UTF-16: {error}"))
        .ok()
        .map(PathBuf::from);
    // SAFETY: `path` came from the call above and is freed exactly once.
    unsafe { CoTaskMemFree(Some(path.as_ptr().cast())) };

    directory
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
    /// The WSL package's own `lib`, which holds the Microsoft D3D12 userspace.
    ///
    /// Its own root rather than a second candidate under `System32`: the Store
    /// and standalone WSL install it beside the package, and it is checked
    /// against Program Files for the same reason the others are checked
    /// against `System32`.
    wsl_d3d12: Option<PathBuf>,
}

impl ExportRoots {
    pub(crate) fn resolve(
        system32: &Path,
        program_files: Option<&Path>,
        canonicalize: Canonicalize<'_>,
    ) -> Self {
        let wsl_d3d12 = program_files.and_then(|program_files| {
            let Ok(program_files) = canonicalize(program_files) else {
                tracing::debug!("Program Files could not be resolved; no D3D12 share");
                return None;
            };
            resolve_root(
                &program_files,
                &program_files.join("WSL").join("lib"),
                canonicalize,
            )
        });

        let Ok(system32) = canonicalize(system32) else {
            tracing::warn!("the system directory could not be resolved; nothing may be exported");
            return Self {
                driver_packages: None,
                wsl_lib: None,
                wsl_d3d12,
            };
        };

        Self {
            driver_packages: resolve_root(
                &system32,
                &system32.join("DriverStore").join("FileRepository"),
                canonicalize,
            ),
            wsl_lib: resolve_root(&system32, &system32.join("lxss").join("lib"), canonicalize),
            wsl_d3d12,
        }
    }
}

fn resolve_root(root: &Path, candidate: &Path, canonicalize: Canonicalize<'_>) -> Option<PathBuf> {
    match canonicalize(candidate) {
        Ok(resolved) if is_within(root, &resolved) => Some(resolved),
        Ok(resolved) => {
            tracing::warn!(
                "refusing to export from \"{}\": it resolves to \"{}\", outside \"{}\"",
                candidate.display(),
                resolved.display(),
                root.display()
            );
            None
        }
        Err(error) => {
            tracing::debug!(
                "nothing to export from \"{}\": {error}",
                candidate.display()
            );
            None
        }
    }
}

/// Every share `adapters` justify, in the order a guest should mount them.
///
/// The WSL userspace comes first: a driver package without it renders nothing,
/// and a partial set is what a guest gets when something below is dropped. Of
/// its two halves the Microsoft one leads, so that a name present in both
/// resolves to the library a renderer links against.
pub(crate) fn build_with(
    adapters: &[HostGpuAdapter],
    roots: &ExportRoots,
    canonicalize: Canonicalize<'_>,
) -> Option<GpuExports> {
    let mut exports: Vec<GpuExport> = Vec::new();

    if let Some(wsl_d3d12) = &roots.wsl_d3d12 {
        exports.push(GpuExport {
            share: GpuShare::wsl_d3d12(),
            host_path: wsl_d3d12.clone(),
        });
    }

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
                tracing::warn!(
                    "not exporting the driver package of \"{}\": {error}",
                    adapter.name
                );
                continue;
            }
        };
        if !is_within(root, &resolved) {
            tracing::warn!(
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
            tracing::warn!(
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

/// Adds `payload`, provided it really lies inside this VM's staging root.
///
/// `payload` is the generation directory staging produced, not the staging
/// root itself: the root also holds the `ready` markers and lock files that
/// make a swap atomic, while the guest reads `sources.json` at the root of the
/// share it mounts. Naming the root would offer a guest a directory it finds
/// no payload in.
///
/// `None` is a VM nothing was staged for, which is a set of shares without a
/// payload rather than no shares at all.
pub(crate) fn build_with_payload(
    adapters: &[HostGpuAdapter],
    roots: &ExportRoots,
    vm_directory: &Path,
    payload: Option<&Path>,
    canonicalize: Canonicalize<'_>,
) -> Option<GpuExports> {
    let mut exports = build_with(adapters, roots, canonicalize)
        .map(|exports| exports.exports)
        .unwrap_or_default();
    if let Some(candidate) = payload
        && let (Ok(vm), Ok(payload)) = (canonicalize(vm_directory), canonicalize(candidate))
        // Inside the VM's own staging root and deeper than it. Outside is a
        // reparse point aiming somewhere this VM has no business reading, and
        // the root itself holds the `ready` markers and locks of a swap rather
        // than a payload -- a guest mounting it would find no `sources.json`.
        // The check is against canonical paths, so a junction cannot pass it
        // and export elsewhere.
        && let staging = gpu_payload_staging_directory(&vm)
        && payload != staging
        && is_within(&staging, &payload)
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
    const PROGRAM_FILES: &str = r"C:\Program Files";
    const WSL_LIB_PACKAGE: &str = r"C:\Program Files\WSL\lib";

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
    fn the_wsl_packages_libraries_become_their_own_share() {
        let canonicalize = canonicalizer(&[
            (SYSTEM32, SYSTEM32),
            (REPOSITORY, REPOSITORY),
            (PROGRAM_FILES, PROGRAM_FILES),
            (WSL_LIB_PACKAGE, WSL_LIB_PACKAGE),
        ]);
        let roots = ExportRoots::resolve(
            Path::new(SYSTEM32),
            Some(Path::new(PROGRAM_FILES)),
            &canonicalize,
        );

        let roles: Vec<_> = build_with(&[], &roots, &canonicalize)
            .expect("the D3D12 directory alone is worth exporting")
            .manifest()
            .shares
            .into_iter()
            .map(|share| share.role)
            .collect();

        assert!(
            roles.contains(&GpuShareRole::WslD3d12),
            "the Microsoft libraries are what a renderer needs: {roles:?}"
        );
    }

    #[test]
    fn a_host_with_no_wsl_package_offers_no_d3d12_share() {
        // An inbox WSL keeps everything under System32, and a host with no WSL
        // at all has neither directory. Both are a guest with less, not an
        // error.
        let canonicalize = canonicalizer(&[
            (SYSTEM32, SYSTEM32),
            (REPOSITORY, REPOSITORY),
            (
                r"C:\Windows\System32\lxss\lib",
                r"C:\Windows\System32\lxss\lib",
            ),
        ]);
        let roots = ExportRoots::resolve(Path::new(SYSTEM32), None, &canonicalize);

        let roles: Vec<_> = build_with(&[], &roots, &canonicalize)
            .expect("the WSL directory is still there")
            .manifest()
            .shares
            .into_iter()
            .map(|share| share.role)
            .collect();

        assert_eq!(roles, vec![GpuShareRole::WslLib]);
    }

    #[test]
    fn a_d3d12_directory_reparsed_outside_program_files_is_dropped() {
        // The same rule the System32 roots follow: a root that canonicalizes
        // out of its parent is a redirection, and everything under it would
        // inherit it.
        let canonicalize = canonicalizer(&[
            (SYSTEM32, SYSTEM32),
            (REPOSITORY, REPOSITORY),
            (PROGRAM_FILES, PROGRAM_FILES),
            (WSL_LIB_PACKAGE, r"D:\attacker\lib"),
        ]);
        let roots = ExportRoots::resolve(
            Path::new(SYSTEM32),
            Some(Path::new(PROGRAM_FILES)),
            &canonicalize,
        );

        assert!(build_with(&[], &roots, &canonicalize).is_none());
    }

    #[test]
    fn payload_wsl_and_driver_package_have_distinct_roles_and_order() {
        let vm = Path::new(r"D:\VMLord\dev-linux");
        let payload = r"D:\VMLord\dev-linux\gpu-payload\generations\e7664769";
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
        let roots = ExportRoots::resolve(Path::new(SYSTEM32), None, &canonicalize);
        let roles: Vec<_> = build_with_payload(
            &[adapter(Some(&package))],
            &roots,
            vm,
            Some(Path::new(payload)),
            &canonicalize,
        )
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
    fn the_staged_generation_is_what_the_payload_share_offers() {
        // The guest reads `sources.json` at the root of the share. Staging
        // writes it inside `generations/<digest>`, beside the `ready` markers
        // and lock files that make the swap atomic, so the share has to name
        // the generation rather than the staging root the guest would find
        // nothing in.
        let vm = Path::new(r"D:\VMLord\dev-linux");
        let generation = r"D:\VMLord\dev-linux\gpu-payload\generations\e7664769";
        let canonicalize = canonicalizer(&[
            (SYSTEM32, SYSTEM32),
            (REPOSITORY, REPOSITORY),
            (r"D:\VMLord\dev-linux", r"D:\VMLord\dev-linux"),
            (generation, generation),
        ]);
        let roots = ExportRoots::resolve(Path::new(SYSTEM32), None, &canonicalize);

        let exports =
            build_with_payload(&[], &roots, vm, Some(Path::new(generation)), &canonicalize)
                .expect("a staged generation is something to export");

        let payload = exports
            .iter()
            .find(|export| matches!(export.share().role, GpuShareRole::GpuPayload))
            .expect("the payload share must be offered");
        assert_eq!(payload.host_path(), Path::new(generation));
    }

    #[test]
    fn a_vm_with_nothing_staged_is_offered_no_payload_share() {
        let vm = Path::new(r"D:\VMLord\dev-linux");
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
        ]);
        let roots = ExportRoots::resolve(Path::new(SYSTEM32), None, &canonicalize);

        let exports =
            build_with_payload(&[adapter(Some(&package))], &roots, vm, None, &canonicalize)
                .expect("the host still has a driver package to offer");

        assert!(
            !exports
                .iter()
                .any(|export| matches!(export.share().role, GpuShareRole::GpuPayload)),
            "a payload that was never staged is not a share"
        );
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
        let roots = ExportRoots::resolve(Path::new(SYSTEM32), None, &canonicalize);
        assert!(
            build_with_payload(
                &[],
                &roots,
                vm,
                Some(&vm.join("gpu-payload")),
                &canonicalize
            )
            .is_none()
        );
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
        let roots = ExportRoots::resolve(Path::new(SYSTEM32), None, &canonicalize);

        assert!(
            build_with_payload(
                &[],
                &roots,
                vm,
                Some(&vm.join("gpu-payload")),
                &canonicalize
            )
            .is_none()
        );
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
        let roots = ExportRoots::resolve(Path::new(SYSTEM32), None, &canonicalize);

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
        let roots = ExportRoots::resolve(Path::new(SYSTEM32), None, &canonicalize);

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
        let roots = ExportRoots::resolve(Path::new(SYSTEM32), None, &canonicalize);

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
        let roots = ExportRoots::resolve(Path::new(SYSTEM32), None, &canonicalize);

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
        let roots = ExportRoots::resolve(Path::new(SYSTEM32), None, &canonicalize);

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
        let roots = ExportRoots::resolve(Path::new(SYSTEM32), None, &canonicalize);

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
        let roots = ExportRoots::resolve(Path::new(SYSTEM32), None, &canonicalize);

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
        let roots = ExportRoots::resolve(Path::new(SYSTEM32), None, &canonicalize);

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
        let roots = ExportRoots::resolve(Path::new(SYSTEM32), None, &canonicalize);

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
        let roots = ExportRoots::resolve(Path::new(SYSTEM32), None, &canonicalize);

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

        let survived = exports.granted_to("hcs-id", &|id, path| {
            granted
                .lock()
                .unwrap()
                .push((id.to_owned(), path.to_path_buf()));
            Ok(())
        });

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
    fn an_export_the_grant_refused_is_still_offered() {
        // Every GPU share lives under `System32`, whose DACLs belong to
        // TrustedInstaller, so `HcsGrantVmAccess` is refused there however
        // elevated VMLord is. It does not matter: a Plan9 share is read by the
        // host's own Plan9 server, not opened by the VM's security principal,
        // and dropping the share over the refusal removes the guest's whole
        // GPU userspace to no purpose.
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

        let survived = exports.granted_to("hcs-id", &|_, path| {
            if path.ends_with("lib") {
                Err(RepositoryError::new("access denied"))
            } else {
                Ok(())
            }
        });

        assert_eq!(survived.iter().count(), 2);
    }

    #[test]
    fn a_set_no_grant_survived_is_still_offered_in_full() {
        let exports = GpuExports::for_test(vec![(
            GpuShare::wsl_lib(),
            PathBuf::from(r"C:\Windows\System32\lxss\lib"),
        )]);

        let survived =
            exports.granted_to("hcs-id", &|_, _| Err(RepositoryError::new("access denied")));

        assert_eq!(survived.iter().count(), 1);
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
        let Some(exports) = super::GpuExports::build(
            &capabilities.adapters,
            Path::new(r"C:\VMLord\ignored"),
            None,
        ) else {
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
