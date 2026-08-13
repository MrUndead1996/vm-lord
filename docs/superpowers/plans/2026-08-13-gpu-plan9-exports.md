# Safe GPU Plan9 Exports Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a validated, deduplicated set of host directories that may be exported to a guest over Plan9, project it into both an HCS `Plan9.Shares` section and a role-based guest manifest, and grant the VM access only to paths that passed validation.

**Architecture:** `vmlord_core::gpu` owns the guest-facing manifest types and the rule for what a share may be named. `vmlord_platform::gpu_exports` owns the host side: it resolves the two allowed roots under `System32`, canonicalizes every candidate through a Windows handle so traversal and reparse escape collapse, drops anything that lands outside its root, deduplicates by canonical path, and grants VM access. `vmlord_platform::hcs_config` writes the resulting set into a stored configuration document, mirroring the existing `apply_network_adapter` / `remove_network_adapter` pair. Nothing calls this from `start.rs`: a start has no GPU mode to read until #89 records one.

**Tech Stack:** Rust 2024, `windows` 0.61 (`Win32_Storage_FileSystem`, `Win32_System_SystemInformation` -- both already enabled), `serde_json`, no new dependencies.

**Spec:** `docs/superpowers/specs/2026-08-13-gpu-plan9-exports-design.md`

## Global Constraints

* Branch is `task-88-gpu-plan9-exports`; every commit subject is prefixed `TASK-88: `.
* All `unsafe` stays inside `crates/platform`, in the module that owns the call. Every `unsafe` block carries a `// SAFETY:` comment, as the rest of the crate does.
* No WMI, no PowerShell, no spawned process.
* `vmlord-core` must not depend on `windows`; it is built for Linux too.
* Two allowed roots, both derived from `GetSystemDirectoryW`: `System32\DriverStore\FileRepository` (driver packages) and `System32\lxss\lib` (WSL Linux userspace).
* Share names: `vmlord.gpu.wsl-lib`, and `vmlord.gpu.drv.<package>` where `<package>` matches `[A-Za-z0-9._-]{1,96}` and is neither `.` nor `..`.
* HCS share shape: `{"Name", "AccessName", "Path", "Port": 50001, "Flags": 1}` under `/VirtualMachine/Devices` → `Plan9` → `Shares`.
* A candidate that fails any check is dropped with a log line; an empty result is `None`, never an error. GPU is best effort and never blocks a start.
* Verification: `cargo check-windows` and `cargo test-windows`. Never prefix commands with `timeout`.

---

### Task 1: The guest-facing manifest types

The names and the character rule live in `core` because they are domain rules, not Windows facts: `platform` builds shares, a later task sends them, and neither may have its own idea of what a share may be called.

**Files:**
- Modify: `crates/core/src/gpu.rs` (append after `HostGpuAdapter`, before `#[cfg(test)] mod tests`)
- Modify: `crates/core/src/lib.rs:12-16` (the `pub use gpu::{...}` list)
- Test: `crates/core/src/gpu.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: nothing.
- Produces: `GpuShareManifest { shares: Vec<GpuShare> }`, `GpuShare { name: String, role: GpuShareRole }`, `GpuShareRole::{WslLib, DriverPackage { package: String }}`, `GpuShare::wsl_lib() -> GpuShare`, `GpuShare::driver_package(package: &str) -> Option<GpuShare>`, `pub const WSL_LIB_SHARE: &str`.

- [ ] **Step 1: Write the failing tests**

Append to the existing `mod tests` in `crates/core/src/gpu.rs` (extend its `use super::{...}` line with `GpuShare, GpuShareRole, WSL_LIB_SHARE`):

```rust
    #[test]
    fn the_wsl_lib_share_is_named_once_and_carries_its_role() {
        let share = GpuShare::wsl_lib();

        assert_eq!(share.name, WSL_LIB_SHARE);
        assert_eq!(share.role, GpuShareRole::WslLib);
    }

    #[test]
    fn a_driver_package_share_is_named_after_its_folder() {
        let share = GpuShare::driver_package("nvltsi.inf_amd64_5b0e0dc41b0dbf1e")
            .expect("an ordinary DriverStore folder name is admissible");

        assert_eq!(share.name, "vmlord.gpu.drv.nvltsi.inf_amd64_5b0e0dc41b0dbf1e");
        assert_eq!(
            share.role,
            GpuShareRole::DriverPackage {
                package: "nvltsi.inf_amd64_5b0e0dc41b0dbf1e".to_owned()
            }
        );
    }

    #[test]
    fn a_package_name_that_could_break_a_mount_option_is_refused() {
        // A share name becomes `aname=` in a comma-separated mount option
        // string and a JSON string in the HCS document; a separator or a
        // traversal component in it would be read by something downstream.
        for refused in [
            "",
            ".",
            "..",
            "pkg,other",
            "pkg other",
            r"pkg\other",
            "pkg/other",
            "pkg\"other",
            "пакет",
            &"a".repeat(97),
        ] {
            assert!(
                GpuShare::driver_package(refused).is_none(),
                "{refused:?} must not become a share name"
            );
        }
    }

    #[test]
    fn a_package_name_at_the_limit_is_still_admissible() {
        assert!(GpuShare::driver_package(&"a".repeat(96)).is_some());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-core gpu::tests`
Expected: FAIL -- `cannot find type GpuShare in this scope` (the types do not exist yet).

- [ ] **Step 3: Write the implementation**

Append to `crates/core/src/gpu.rs`, after `HostGpuAdapter`:

```rust
/// The Plan9 shares a guest is offered, as the guest is told about them.
///
/// Roles, never host paths. Where a share is mounted is the guest's decision,
/// taken from its own allowlist: a host path would be useless to it and would
/// put the host's topology on the wire and into guest logs. Not serializable
/// -- it has no on-disk format, and the task that starts sending it converts
/// it to protobuf.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GpuShareManifest {
    pub shares: Vec<GpuShare>,
}

/// One share: what it is called on the wire, and what it holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GpuShare {
    /// The share's name, which is also `aname=` when the guest mounts it.
    pub name: String,
    pub role: GpuShareRole,
}

/// What a guest is meant to make of a share.
///
/// The guest never parses the name to find this out: an agent that derived
/// meaning from a string would have to be updated in step with whatever
/// produced it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GpuShareRole {
    /// The host's WSL Linux userspace.
    WslLib,
    /// One driver package, named by its DriverStore folder.
    DriverPackage { package: String },
}

/// The share name the host's WSL Linux userspace is offered under.
pub const WSL_LIB_SHARE: &str = "vmlord.gpu.wsl-lib";

/// What every driver package share's name starts with.
const DRIVER_PACKAGE_SHARE_PREFIX: &str = "vmlord.gpu.drv.";

/// The longest package folder name that may become part of a share name.
///
/// A share name travels in a comma-separated `mount` option string, so it is
/// bounded for the guest's sake rather than by anything Windows imposes.
const MAX_PACKAGE_NAME: usize = 96;

impl GpuShare {
    /// The share for the host's WSL Linux userspace.
    #[must_use]
    pub fn wsl_lib() -> Self {
        Self {
            name: WSL_LIB_SHARE.to_owned(),
            role: GpuShareRole::WslLib,
        }
    }

    /// The share for one driver package, or `None` when the folder's name
    /// cannot safely become a share name.
    ///
    /// The name ends up in an HCS JSON document and in a `mount` option
    /// string, so a separator, a quote, a space or a traversal component in it
    /// would be read by something downstream as structure rather than as text.
    #[must_use]
    pub fn driver_package(package: &str) -> Option<Self> {
        if package.is_empty() || package.len() > MAX_PACKAGE_NAME {
            return None;
        }
        if package == "." || package == ".." {
            return None;
        }
        if !package
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-'))
        {
            return None;
        }

        Some(Self {
            name: format!("{DRIVER_PACKAGE_SHARE_PREFIX}{package}"),
            role: GpuShareRole::DriverPackage {
                package: package.to_owned(),
            },
        })
    }
}
```

Extend the re-export list in `crates/core/src/lib.rs` (keep it alphabetical):

```rust
pub use gpu::{
    GpuAssignment, GpuAvailability, GpuFailure, GpuMode, GpuShare, GpuShareManifest, GpuShareRole,
    GpuStage, GpuState, GpuStatusCode, GuestGpuDetail, GuestGpuReport, HostGpuAdapter,
    HostGpuCapabilities, NativeGpuDetail, VmGpuFacts, VmGpuStatus, WSL_LIB_SHARE,
};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-core gpu::tests`
Expected: PASS, including the pre-existing `host_status_codes_have_stable_strings`.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/gpu.rs crates/core/src/lib.rs
git commit -m "TASK-88: Add GPU share manifest types

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 2: Resolving the allowed roots and building the export set

All of the decision-making, none of the Windows calls: the canonicalizer is a function parameter, so every rejection below is a unit test rather than a host someone has to find.

**Files:**
- Create: `crates/platform/src/gpu_exports.rs`
- Modify: `crates/platform/src/lib.rs:27` (add `mod gpu_exports;` between `mod gpu_discovery;` and `mod gpu_enumerate;`)
- Test: `crates/platform/src/gpu_exports.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `GpuShare`, `GpuShareManifest`, `GpuShareRole`, `HostGpuAdapter`, `RepositoryError` from `vmlord_core`.
- Produces: `GpuExport { name(), host_path() }`, `GpuExports { iter(), manifest(), for_test() }`, `ExportRoots::resolve(system32: &Path, canonicalize: Canonicalize<'_>) -> ExportRoots`, `type Canonicalize<'a> = &'a dyn Fn(&Path) -> Result<PathBuf, RepositoryError>`, `fn build_with(adapters: &[HostGpuAdapter], roots: &ExportRoots, canonicalize: Canonicalize<'_>) -> Option<GpuExports>`.

- [ ] **Step 1: Write the failing tests**

Create `crates/platform/src/gpu_exports.rs` containing only this test module for now:

```rust
#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        path::{Path, PathBuf},
    };

    use vmlord_core::{GpuShareRole, HostGpuAdapter, RepositoryError};

    use super::{ExportRoots, GpuExports, build_with};

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
                .ok_or_else(|| RepositoryError::new(format!("no such directory: {}", path.display())))
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
            (r"C:\Windows\System32\lxss\lib", r"C:\Windows\System32\lxss\lib"),
            (&package, &package),
        ]);
        let roots = ExportRoots::resolve(Path::new(SYSTEM32), &canonicalize);

        let exports =
            build_with(&[adapter(Some(&package))], &roots, &canonicalize).expect("two shares");

        let names: Vec<_> = exports.iter().map(|export| export.name().to_owned()).collect();
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

        let exports = build_with(&[adapter(Some(&package))], &roots, &canonicalize).expect("one share");

        assert_eq!(exports.iter().count(), 1);
        assert_eq!(exports.iter().next().unwrap().name(), "vmlord.gpu.drv.nvltsi.inf_amd64_1");
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
            (r"C:\Windows\System32\lxss\lib", r"C:\Windows\System32\lxss\lib"),
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-platform gpu_exports`
Expected: FAIL -- `unresolved import super::{ExportRoots, build_with}`.

- [ ] **Step 3: Write the implementation**

Prepend to `crates/platform/src/gpu_exports.rs`, above the test module:

```rust
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
            wsl_lib: resolve_root(
                &system32,
                &system32.join("lxss").join("lib"),
                canonicalize,
            ),
        }
    }
}

fn resolve_root(system32: &Path, candidate: &Path, canonicalize: Canonicalize<'_>) -> Option<PathBuf> {
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
```

Register the module in `crates/platform/src/lib.rs`, keeping the list alphabetical:

```rust
mod gpu_discovery;
mod gpu_enumerate;
mod gpu_exports;
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-platform gpu_exports`
Expected: PASS -- ten tests.

Then run `cargo check-windows` and fix any warning it reports; the crate is warning-free today and `GpuExports::manifest` is the only item nothing calls yet, which `#[cfg(test)]` usage covers.

- [ ] **Step 5: Commit**

```bash
git add crates/platform/src/gpu_exports.rs crates/platform/src/lib.rs
git commit -m "TASK-88: Build GPU exports from allowlisted roots

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 3: Canonicalizing a directory through a Windows handle

The one file in this task's set that contains `unsafe`. It answers a single question -- "what is this path really, and is it a directory" -- and the previous task's logic is what decides anything.

**Files:**
- Modify: `crates/platform/src/gpu_exports.rs` (add the Windows half above the tests; add tests to the existing test module)
- Modify: `crates/platform/Cargo.toml` (nothing to add; `Win32_Storage_FileSystem` and `Win32_System_SystemInformation` are already enabled -- confirm before assuming)

**Interfaces:**
- Consumes: `ExportRoots::resolve`, `build_with` from Task 2; `windows_error` from `crate::error`.
- Produces: `GpuExports::build(adapters: &[HostGpuAdapter]) -> Option<GpuExports>`, `fn canonical_directory(path: &Path) -> Result<PathBuf, RepositoryError>`, `fn strip_extended_prefix(path: &str) -> &str`.

- [ ] **Step 1: Write the failing tests**

Add to the test module in `crates/platform/src/gpu_exports.rs` (extend its `use super::{...}` with `strip_extended_prefix`):

```rust
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
        let Some(exports) = super::GpuExports::build(&capabilities.adapters) else {
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
        assert_eq!(unique.len(), names.len(), "share names must be unique: {names:?}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-platform gpu_exports`
Expected: FAIL -- `strip_extended_prefix` and `GpuExports::build` are not defined.

- [ ] **Step 3: Write the implementation**

Extend the imports at the top of `crates/platform/src/gpu_exports.rs`:

```rust
use std::path::{Component, Path, PathBuf};

use vmlord_core::{GpuShare, GpuShareManifest, HostGpuAdapter, RepositoryError};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE},
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_DIRECTORY,
            FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
            FILE_SHARE_WRITE, FILE_NAME_NORMALIZED, GetFileInformationByHandle,
            GetFinalPathNameByHandleW, OPEN_EXISTING,
        },
        System::SystemInformation::GetSystemDirectoryW,
    },
    core::HSTRING,
};

use crate::error::windows_error;
```

Add the production entry point and the Windows half:

```rust
impl GpuExports {
    /// Every share this host justifies for `adapters`, or `None` when there is
    /// nothing to export.
    ///
    /// Not a `Result`: a host with no WSL payload and no resolvable package is
    /// a host that gets no shares, which is an answer. What went wrong on the
    /// way to it is logged where it happened.
    pub(crate) fn build(adapters: &[HostGpuAdapter]) -> Option<Self> {
        let system32 = system_directory()?;
        let canonicalize = canonical_directory;
        let roots = ExportRoots::resolve(&system32, &canonicalize);

        build_with(adapters, &roots, &canonicalize)
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
/// redirect it -- and that both allowed roots live under `System32`, which
/// takes administrator rights to write to.
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
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-platform gpu_exports`
Expected: PASS -- thirteen tests, one reported as ignored.

Run: `cargo check-windows`
Expected: no warnings. If `windows` 0.61 names any imported item differently, fix the import rather than the call: the API shape above is what the crate's other modules use.

Optionally, on a Windows host with GPU-PV, run the ignored test and read its output:
`cargo test-windows -p vmlord-platform gpu_exports -- --ignored --nocapture`

- [ ] **Step 5: Commit**

```bash
git add crates/platform/src/gpu_exports.rs
git commit -m "TASK-88: Canonicalize export paths through a directory handle

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 4: Granting the VM access to what passed

**Files:**
- Modify: `crates/platform/src/gpu_exports.rs` (add `granted_to` to `impl GpuExports`; add tests)
- Test: `crates/platform/src/gpu_exports.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `GpuExports::for_test`, `GpuShare::{wsl_lib, driver_package}`.
- Produces: `GpuExports::granted_to(self, hcs_id: &str, grant: &dyn Fn(&str, &Path) -> Result<(), RepositoryError>) -> Option<GpuExports>`.

- [ ] **Step 1: Write the failing tests**

Add to the test module:

Add `use std::sync::Mutex;` to the test module's imports and extend its
`use vmlord_core::{...}` line with `GpuShare`; then:

```rust
    #[test]
    fn every_export_is_granted_before_it_is_offered() {
        let granted: Mutex<Vec<(String, PathBuf)>> = Mutex::new(Vec::new());
        let exports = GpuExports::for_test(vec![
            (GpuShare::wsl_lib(), PathBuf::from(r"C:\Windows\System32\lxss\lib")),
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
                ("hcs-id".to_owned(), PathBuf::from(r"C:\Windows\System32\lxss\lib")),
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
            (GpuShare::wsl_lib(), PathBuf::from(r"C:\Windows\System32\lxss\lib")),
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-platform gpu_exports`
Expected: FAIL -- `no method named granted_to`.

- [ ] **Step 3: Write the implementation**

Add to `impl GpuExports` in `crates/platform/src/gpu_exports.rs`:

```rust
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
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-platform gpu_exports`
Expected: PASS -- sixteen tests, one ignored.

- [ ] **Step 5: Commit**

```bash
git add crates/platform/src/gpu_exports.rs
git commit -m "TASK-88: Grant VM access only to validated exports

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 5: Writing and removing the Plan9 section

**Files:**
- Modify: `crates/platform/src/hcs_config.rs` (add the pair beside `remove_network_adapter`, around line 300; add the constants beside `NETWORK_ADAPTERS_KEY` at line 305)
- Test: `crates/platform/src/hcs_config.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `GpuExports` (`iter`, `for_test`), `GpuExport::{name, host_path}` from Tasks 2 and 4; the module's existing `parse` and `write_target` helpers.
- Produces: `hcs_config::apply_plan9_shares(document: &str, exports: &GpuExports) -> Result<String, RepositoryError>`, `hcs_config::remove_plan9_shares(document: &str) -> Result<String, RepositoryError>`.

- [ ] **Step 1: Write the failing tests**

Add to the test module in `crates/platform/src/hcs_config.rs` (its `use super::*;`-style imports already bring the module into scope; add `use crate::gpu_exports::GpuExports;` and `use vmlord_core::GpuShare;` inside the test module):

```rust
    fn exports() -> GpuExports {
        GpuExports::for_test(vec![
            (
                GpuShare::wsl_lib(),
                PathBuf::from(r"C:\Windows\System32\lxss\lib"),
            ),
            (
                GpuShare::driver_package("nvltsi.inf_amd64_1").unwrap(),
                PathBuf::from(r"C:\Windows\System32\DriverStore\FileRepository\nvltsi.inf_amd64_1"),
            ),
        ])
    }

    #[test]
    fn plan9_shares_are_written_read_only_on_the_agent_port() {
        let document = HcsVmConfigBuilder::build(
            &request(),
            &system_disk_path(),
            &seed_path(),
            None,
            VM_ID,
        )
        .expect("the configuration must build");

        let updated = apply_plan9_shares(&document, &exports()).expect("shares must apply");

        let value: serde_json::Value = serde_json::from_str(&updated).expect("valid JSON");
        let shares = value
            .pointer("/VirtualMachine/Devices/Plan9/Shares")
            .and_then(serde_json::Value::as_array)
            .expect("the Plan9 section must hold an array of shares");
        assert_eq!(shares.len(), 2);
        assert_eq!(shares[0]["Name"], "vmlord.gpu.wsl-lib");
        assert_eq!(shares[0]["AccessName"], "vmlord.gpu.wsl-lib");
        assert_eq!(shares[0]["Path"], r"C:\Windows\System32\lxss\lib");
        assert_eq!(shares[0]["Port"], 50001);
        assert_eq!(shares[0]["Flags"], 1, "1 is read-only");
        assert_eq!(shares[1]["Name"], "vmlord.gpu.drv.nvltsi.inf_amd64_1");
    }

    #[test]
    fn applying_shares_twice_replaces_rather_than_appends() {
        let document = HcsVmConfigBuilder::build(
            &request(),
            &system_disk_path(),
            &seed_path(),
            None,
            VM_ID,
        )
        .expect("the configuration must build");

        let once = apply_plan9_shares(&document, &exports()).expect("shares must apply");
        let twice = apply_plan9_shares(&once, &exports()).expect("shares must apply again");

        assert_eq!(once, twice, "a start that changes nothing writes nothing");
    }

    #[test]
    fn removing_shares_takes_the_whole_section() {
        let document = HcsVmConfigBuilder::build(
            &request(),
            &system_disk_path(),
            &seed_path(),
            None,
            VM_ID,
        )
        .expect("the configuration must build");
        let with_shares = apply_plan9_shares(&document, &exports()).expect("shares must apply");

        let without = remove_plan9_shares(&with_shares).expect("shares must be removable");

        let value: serde_json::Value = serde_json::from_str(&without).expect("valid JSON");
        assert!(
            value.pointer("/VirtualMachine/Devices/Plan9").is_none(),
            "a VM whose GPU was switched off must not keep the previous start's shares"
        );
    }

    #[test]
    fn removing_shares_from_a_document_without_any_changes_nothing() {
        let document = HcsVmConfigBuilder::build(
            &request(),
            &system_disk_path(),
            &seed_path(),
            None,
            VM_ID,
        )
        .expect("the configuration must build");

        assert_eq!(
            remove_plan9_shares(&document).expect("nothing to remove"),
            document,
            "a document needing no change comes back byte for byte"
        );
    }
```

Match the existing tests' fixtures: read the top of `hcs_config.rs`'s test module and reuse whatever it already calls to build a `request()`, a system disk path, a seed path and `VM_ID`, rather than inventing new helpers.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-platform hcs_config`
Expected: FAIL -- `cannot find function apply_plan9_shares`.

- [ ] **Step 3: Write the implementation**

Add to `crates/platform/src/hcs_config.rs`, after `remove_network_adapter`:

```rust
/// Returns `document` with `exports` written into its `Plan9` section.
///
/// The whole section is replaced rather than merged: the export set is
/// computed once per start and is what the VM boots with, so a leftover share
/// from a previous start is stale by definition.
pub(crate) fn apply_plan9_shares(
    document: &str,
    exports: &GpuExports,
) -> Result<String, RepositoryError> {
    let mut configuration = parse(document)?;
    let devices = write_target(&mut configuration, DEVICES_POINTER)?
        .as_object_mut()
        .ok_or_else(|| {
            let error = RepositoryError::new(format!(
                "the stored HCS configuration has no \"{DEVICES_POINTER}\" object to attach \
                 Plan9 shares to"
            ));
            log::error!("{error}");
            error
        })?;

    let shares: Vec<Plan9Share<'_>> = exports
        .iter()
        .map(|export| Plan9Share {
            name: export.name(),
            access_name: export.name(),
            path: export.host_path(),
            port: PLAN9_PORT,
            flags: PLAN9_FLAG_READ_ONLY,
        })
        .collect();
    let shares = serde_json::to_value(shares).map_err(|error| {
        RepositoryError::new(format!("failed to serialize the VM's Plan9 shares: {error}"))
    })?;
    devices.insert(
        PLAN9_KEY.to_owned(),
        serde_json::json!({ "Shares": shares }),
    );

    serde_json::to_string(&configuration).map_err(|error| {
        RepositoryError::new(format!(
            "failed to serialize the HCS VM configuration with its Plan9 shares: {error}"
        ))
    })
}

/// Returns `document` without its `Plan9` section.
///
/// A VM whose GPU was switched off still has the previous start's shares in
/// its stored configuration, and leaving them would hand the guest driver
/// directories it no longer asks for. A document that has no such section is
/// returned byte for byte, so a start that changes nothing writes nothing.
pub(crate) fn remove_plan9_shares(document: &str) -> Result<String, RepositoryError> {
    let mut configuration = parse(document)?;
    let removed = configuration
        .pointer_mut(DEVICES_POINTER)
        .and_then(serde_json::Value::as_object_mut)
        .and_then(|devices| devices.remove(PLAN9_KEY));
    if removed.is_none() {
        return Ok(document.to_owned());
    }

    serde_json::to_string(&configuration).map_err(|error| {
        RepositoryError::new(format!(
            "failed to serialize the HCS VM configuration without its Plan9 shares: {error}"
        ))
    })
}

/// One share as HCS reads it.
#[derive(Serialize)]
struct Plan9Share<'a> {
    #[serde(rename = "Name")]
    name: &'a str,
    /// What the guest passes as `aname=`; the same string as `Name`, because
    /// a second name would be one more thing for the two sides to disagree
    /// about.
    #[serde(rename = "AccessName")]
    access_name: &'a str,
    #[serde(rename = "Path")]
    path: &'a Path,
    #[serde(rename = "Port")]
    port: u32,
    #[serde(rename = "Flags")]
    flags: u32,
}
```

And beside the other keys near `NETWORK_ADAPTERS_KEY`:

```rust
const PLAN9_KEY: &str = "Plan9";
/// The HvSocket port the host's Plan9 server answers on, and the one the guest
/// agent connects to before it mounts.
const PLAN9_PORT: u32 = 50001;
/// Read-only. The flag values are not published in any SDK header; this is
/// what Hyper-V honours and what the AppSandbox backend passed, and read-only
/// is stated a second time by the guest's own `MS_RDONLY` mount.
const PLAN9_FLAG_READ_ONLY: u32 = 1;
```

Add `use crate::gpu_exports::GpuExports;` to the module's imports.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-platform hcs_config`
Expected: PASS, including every pre-existing `hcs_config` test.

Run: `cargo check-windows`
Expected: no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/platform/src/hcs_config.rs
git commit -m "TASK-88: Write GPU Plan9 shares into the HCS configuration

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 6: Documentation and full verification

**Files:**
- Modify: `ARCHITECTURE.md` (new section after "GPU: what the host can do", which ends just before "### VM update contract")
- Modify: `docs/superpowers/specs/2026-08-13-gpu-plan9-exports-design.md` (the testing section's sentence about where the real-host test lives)

- [ ] **Step 1: Write the architecture section**

Insert into `ARCHITECTURE.md`, between the end of "GPU: what the host can do" and "### VM update contract":

```markdown
### GPU: what is exported to a guest

A GPU partition is useless to a Linux guest without the host's driver package
and the WSL Linux userspace beside it, and the way in is a Plan9 share.
`vmlord_platform::gpu_exports` decides what may be shared, and the answer is
two directories and nothing else: `System32\DriverStore\FileRepository`, for
the driver packages behind the host's adapters, and `System32\lxss\lib`, for
the Linux userspace WSL stages.

Every candidate is canonicalized before it is judged -- opened as a directory
handle, without `FILE_FLAG_OPEN_REPARSE_POINT`, and resolved with
`GetFinalPathNameByHandleW`. That is what collapses `..` and what turns a
junction into its target, so a link leading out of an allowed root fails the
root check instead of quietly exporting whatever it points at. The root check
is component-wise and case-insensitive, because `...\FileRepositoryEvil` passes
a string prefix and is a different directory. A root that itself resolves
outside `System32` admits nothing at all. What is exported afterwards is the
canonical path, not the one discovery reported, and the set is deduplicated by
it: two adapters from one vendor usually share a `FileRepository` folder.

A candidate that fails any of this is dropped with a log line and the rest are
still offered, and a set with nothing in it is `None` rather than an error: GPU
is applied best effort and never blocks a start. `HcsGrantVmAccess` runs only
after a path has passed -- a grant before the check is what makes the check
decorative -- and an export the grant refused is dropped too, because offering
a VM a share it cannot open trades a clear line in the host log for an opaque
mount failure in the guest.

What the guest is told is a `GpuShareManifest`: for each share, a name and a
role -- `WslLib`, or `DriverPackage` with the package's folder name. Never a
host path. Where a share is mounted is the guest's decision, taken from its own
allowlist, so the host cannot dictate a path into a guest filesystem and the
host's topology does not travel. Share names are `vmlord.gpu.wsl-lib` and
`vmlord.gpu.drv.<package>`, restricted to `[A-Za-z0-9._-]`, because a name ends
up both in the HCS document and in a comma-separated `mount` option string.

`hcs_config::apply_plan9_shares` writes the set into the stored configuration
under `Devices/Plan9`, each share carrying port 50001 and the read-only flag;
`remove_plan9_shares` takes the section away again for a VM whose GPU was
switched off. Read-only is therefore stated twice and independently: by the
share's flag on the host and by the guest's own `MS_RDONLY` mount. The set is
computed once per start and written before the compute system is prepared,
which is what makes it immutable for the lifetime of a boot -- changing a GPU
mode takes a full VM restart.
```

- [ ] **Step 2: Correct the spec's note on the real-host test**

In `docs/superpowers/specs/2026-08-13-gpu-plan9-exports-design.md`, replace the sentence beginning "One `#[ignore]`d real-host test in `crates/platform/tests/gpu_exports.rs`" with:

```markdown
One `#[ignore]`d real-host test, living in `gpu_exports.rs`'s own test module
rather than under `crates/platform/tests/`, because the builder is
`pub(crate)` and this task adds no public API to reach it from an integration
test: build a set from `discover_host_gpu()` and assert that every path exists,
is a directory, lies under `System32`, and that the share names are unique. On
a host without adapters it passes vacuously, because it is not entitled to
assert that GPU-PV exists.
```

- [ ] **Step 3: Run the full verification**

Run: `cargo check-windows`
Expected: no errors, no warnings.

Run: `cargo test-windows`
Expected: every test passes; the ignored real-host test is reported as ignored.

Run: `cargo test -p vmlord-core`
Expected: PASS -- the core crate builds and tests on Linux too, which is what keeps `windows` out of it.

- [ ] **Step 4: Commit**

```bash
git add ARCHITECTURE.md docs/superpowers/specs/2026-08-13-gpu-plan9-exports-design.md
git commit -m "TASK-88: Document safe GPU Plan9 exports

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

## What this task deliberately does not do

* No call from `start.rs`. A start cannot know a VM's GPU mode:
  `VmComputeSystemMapping` has no such field and `hcs_config::build` still
  rejects every mode but `None`. #89 records the mode and applies assignment
  after start; it is the caller of `GpuExports::build`, `granted_to` and
  `apply_plan9_shares`.
* No protobuf message. `agent.proto` states that arms from field 4 onwards
  arrive with the task that implements them; #92 and #94 send the manifest and
  convert `GpuShareManifest` there.
* No guest paths. `/usr/lib/wsl/lib` and `/usr/lib/wsl/drivers/<package>` are
  #94's allowlist, not this task's output.
* No file filters. AppSandbox carried a semicolon-separated filename whitelist
  for its Windows guest's copy step; a Linux guest mounts the share whole and
  read-only, and nothing is copied.
