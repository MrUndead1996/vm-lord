# HCS VM Deletion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Delete a VMLord-managed VM — its HCS compute system, stored configuration, virtual disks and metadata mapping — from the UI, behind a confirmation dialog, refusing a VM that is still running.

**Architecture:** A new `VmDeletionPipeline` in `crates/platform` performs the removal in a fixed order and drops the metadata mapping last, so a partially failed deletion leaves the VM visible and retryable. The two steps it shares with creation rollback (tearing down the compute system, removing the VM directory) move into a new `cleanup` module used by both. `VmRepository` gains a required `delete_vm`, `WorkspaceApp::delete_vm` carries the business rules, and the UI adds a confirmation dialog with a "delete disks" checkbox.

**Tech Stack:** Rust 2024 edition, workspace crates `vmlord-core` / `vmlord-app` / `vmlord-platform` / `vmlord-ui` / `vmlord-legacy-backend`, `windows` 0.61 crate for HCS, `egui`/`eframe` for the UI, `log` for logging.

**Spec:** `docs/superpowers/specs/2026-08-07-hcs-vm-deletion-design.md`

## Global Constraints

- Branch: `task-32-delete-vm`. Never commit to `main`.
- Every commit subject is `TASK-32: <subject>`, imperative mood.
- Every commit is authored as the agent:
  `GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local git commit -m "TASK-32: ..."`
- Do not open a merge request. That needs explicit approval from the user.
- Logging uses `log` at `DEBUG`, `INFO`, `WARN`, `ERROR`. `TRACE` is not used anywhere in this project.
- `crates/ui` must contain no business logic and must never call Windows APIs. It talks only to `vmlord_app::WorkspaceApp`.
- `crates/platform` is the only crate allowed to call Windows APIs, and `unsafe` stays inside it.
- The user's source ISO image is never deleted by any code in this plan.
- Networks and SSH keys are out of scope: the native backend does not wire them yet.

### Commands (verified in this environment)

This is a WSL host targeting Windows. Windows test binaries execute through WSL
interop, so the platform crate's tests really run here.

- Host-side tests (`core`, `app`): `cargo test -p vmlord-core -p vmlord-app`
- Platform tests: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu`
- Whole workspace build: `cargo build --target=x86_64-pc-windows-gnu`
- Lints: `cargo clippy --target=x86_64-pc-windows-gnu --all-targets`

**Baselines before any change:** `vmlord-platform` reports `81 passed; 0 failed;
1 ignored` for the lib tests and `10 ignored` for `tests/hyperv.rs`;
`vmlord-core` reports 7 passed and `vmlord-app` 8 passed. Clippy already emits
pre-existing warnings (`result_large_err` six times, one
`manual_is_multiple_of`) — do not chase them, just do not add new ones.

The `#[ignore]`d tests in `crates/platform/tests/hyperv.rs` require an elevated
Windows host with Hyper-V. They are not run by this plan; Task 6 only requires
that the new one compiles.

---

## File Structure

**Create:**
- `crates/platform/src/cleanup.rs` — removal steps shared by creation rollback and deletion: compute-system teardown, VM directory removal, failure aggregation.
- `crates/platform/src/delete.rs` — `VmDeletionPipeline`: the ordered deletion of one VM's resources.

**Modify:**
- `crates/platform/src/create.rs` — rollback and `production()` delegate to `cleanup`.
- `crates/platform/src/lib.rs` — declare the two new modules, export `VmDeletionPipeline`.
- `crates/core/src/lib.rs` — `VmDeleteRequest`, `VmRepository::delete_vm`.
- `crates/platform/src/repository.rs` — `HcsVmRepository::delete_vm`, holding the pipeline and refusing a running VM.
- `crates/app/src/lib.rs` — `WorkspaceApp::delete_vm`, `UnavailableRepository::delete_vm`, `FakeRepository::delete_vm`.
- `crates/legacy-backend/src/windows.rs` — `AppSandboxBackend::delete_vm` reporting the legacy backend cannot delete.
- `crates/ui/src/lib.rs` — `DeleteVmForm`, `DeleteVmDialogAction`, `render_delete_vm_dialog`, the `VmAction::Delete` branch.
- `crates/platform/tests/hyperv.rs` — an `#[ignore]`d end-to-end deletion test.
- `ARCHITECTURE.md` — deletion in the native backend's VM lifecycle.

---

### Task 1: Shared cleanup module

Extracts the two removal steps creation rollback already performs, so deletion
can drive the same code. Also fixes a real defect: rollback tears down through
`HcsSystem::open`, which fails when the compute system does not exist, appending
a misleading "open compute system failed" clause to the error reported for a
creation that failed before the system existed.

**Files:**
- Create: `crates/platform/src/cleanup.rs`
- Modify: `crates/platform/src/lib.rs:531-546` (module declarations)
- Modify: `crates/platform/src/create.rs:1-62` (imports, `production`), `crates/platform/src/create.rs:159-235` (`rollback`, `teardown_hcs_system`)

**Interfaces:**
- Consumes: `HcsSystem::open_if_present(id, HCS_ACCESS_ALL) -> Result<Option<HcsSystem>, RepositoryError>`, `HcsSystem::terminate_and_wait(Duration) -> Result<(), RepositoryError>` (both already exist in `crates/platform/src/hcs.rs`).
- Produces:
  - `pub(crate) type SystemTeardown = Box<dyn Fn(&str) -> Result<(), RepositoryError>>`
  - `pub(crate) fn teardown_compute_system(id: &str) -> Result<(), RepositoryError>`
  - `pub(crate) fn remove_vm_directory(vm_directory: &Path) -> Result<(), RepositoryError>`
  - `pub(crate) fn combine_failures(prefix: &str, failures: Vec<String>) -> RepositoryError`

- [ ] **Step 1: Declare the module**

The module list in `crates/platform/src/lib.rs:531-546` is alphabetical, and
`cleanup` sorts before `create`, so the list starts:

```rust
mod cleanup;
mod create;
mod enumerate;
```

- [ ] **Step 2: Write the failing tests**

Create `crates/platform/src/cleanup.rs` containing only the module docs, the
imports and the test module below. The functions do not exist yet, so this must
fail to compile.

```rust
//! Removing the resources a VMLord VM is made of.
//!
//! Creation rollback and deletion tear the same two things down -- the HCS
//! compute system and the VM directory -- and report a partial failure the same
//! way, so both drive them from here.

use std::{fs, path::Path, time::Duration};

use vmlord_core::RepositoryError;

use crate::{HcsSystem, hcs::HCS_ACCESS_ALL};

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
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu cleanup`
Expected: FAIL — `cannot find function 'remove_vm_directory' in this scope` and
the same for `combine_failures`.

- [ ] **Step 4: Implement the module**

Insert this above the `#[cfg(test)] mod tests` block in
`crates/platform/src/cleanup.rs`:

```rust
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
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu cleanup`
Expected: PASS — 3 tests.

- [ ] **Step 6: Point creation at the shared code**

In `crates/platform/src/create.rs`:

Replace the local teardown type alias and the `HcsSystem` import usage. The file
currently declares its own `SystemTeardown`-shaped boxed closure type and a
private `teardown_hcs_system`. Change the imports to include the cleanup module:

```rust
use crate::{
    cleanup::{self, SystemTeardown},
    hcs_config::HcsVmConfigBuilder,
    layout,
    metadata::{MetadataStore, VmComputeSystemMapping},
};
```

Keep whatever else the existing `use crate::{...}` block already imports that is
still used, and drop `HcsSystem`/`HCS_ACCESS_ALL` from it if nothing else in the
file uses them. Delete the file's own teardown type alias if it declares one, and
delete `fn teardown_hcs_system` at `crates/platform/src/create.rs:230-235`.

In `VmCreationPipeline::production`, the teardown becomes:

```rust
system_teardown: Box::new(cleanup::teardown_compute_system),
```

`create_hcs_system`'s ambiguous-create cleanup call changes from
`teardown_hcs_system(id)` to `cleanup::teardown_compute_system(id)`.

Replace the body of `rollback` (`crates/platform/src/create.rs:159-191`) with:

```rust
    fn rollback(
        &self,
        vm_directory: &Path,
        mapping: &VmComputeSystemMapping,
        system_created: bool,
        error: RepositoryError,
    ) -> RepositoryError {
        // `combine_failures` logs the whole message at ERROR, and `failures`
        // starts with this error, so logging it separately here would emit the
        // same content twice on every rollback.
        let mut failures = vec![error.to_string()];

        if system_created
            && let Err(teardown_error) = (self.system_teardown)(&mapping.hcs_compute_system_id)
        {
            failures.push(format!("rollback teardown also failed: {teardown_error}"));
        }
        if let Err(remove_error) = cleanup::remove_vm_directory(vm_directory) {
            failures.push(format!("rollback also failed: {remove_error}"));
        }

        cleanup::combine_failures(
            &format!("creation of VM \"{}\" failed", mapping.vm_name),
            failures,
        )
    }
```

- [ ] **Step 7: Run the full platform suite**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu`
Expected: PASS — 84 passed, 1 ignored in the lib tests (81 before, plus the 3
new ones). The creation tests must be untouched: they assert only on the
`"creation of VM"` prefix, `"injected disk failure"`, `"timed out"` and
`"image"`, all of which the new message keeps.

- [ ] **Step 8: Commit**

```bash
git add crates/platform/src/cleanup.rs crates/platform/src/create.rs crates/platform/src/lib.rs
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
  git commit -m "TASK-32: Share VM resource cleanup between creation rollback and deletion"
```

---

### Task 2: Deletion pipeline

**Files:**
- Create: `crates/platform/src/delete.rs`
- Modify: `crates/platform/src/lib.rs` (declare `mod delete;`, export `VmDeletionPipeline`)

**Interfaces:**
- Consumes: `cleanup::{SystemTeardown, teardown_compute_system, remove_vm_directory, combine_failures}` from Task 1; `MetadataStore::find_by_vm_name(&str) -> Result<Option<VmComputeSystemMapping>, RepositoryError>`; `MetadataStore::remove(Uuid) -> Result<(), RepositoryError>`; `layout::configuration_path(&Path) -> PathBuf`.
- Produces: `pub struct VmDeletionPipeline` with `pub fn production() -> Self` and
  `pub fn delete(&self, store: &MetadataStore, vm_name: &str, vm_directory: &Path, delete_disks: bool) -> Result<(), RepositoryError>`.

- [ ] **Step 1: Declare and export the module**

In `crates/platform/src/lib.rs`, add `mod delete;` after `mod create;`, and the
export after the `create` one:

```rust
pub use create::VmCreationPipeline;
pub use delete::VmDeletionPipeline;
```

- [ ] **Step 2: Write the failing tests**

Create `crates/platform/src/delete.rs` with the module docs, imports and this
test module. Modelled on the force-stop pipeline's tests
(`crates/platform/src/force_stop.rs`): real temporary directories, an injected
teardown closure recording the compute systems it was asked to tear down.

```rust
//! Deleting an HCS-backed virtual machine and everything it is made of.

use std::{fs, path::Path};

use vmlord_core::RepositoryError;

use crate::{
    cleanup::{self, SystemTeardown},
    layout,
    metadata::MetadataStore,
};

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
        },
    };

    use uuid::Uuid;
    use vmlord_core::RepositoryError;

    use super::VmDeletionPipeline;
    use crate::metadata::{MetadataStore, VmComputeSystemMapping};

    struct TempRoot(PathBuf);

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temp_root(label: &str) -> TempRoot {
        static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "vmlord-delete-test-{label}-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("test root should be created");
        TempRoot(path)
    }

    struct Fixture {
        _root: TempRoot,
        store: MetadataStore,
        mapping: VmComputeSystemMapping,
        vm_directory: PathBuf,
        teardowns: Arc<Mutex<Vec<String>>>,
    }

    /// A VM as it looks on disk once creation is done: a configuration document
    /// and a system disk under the VM's own directory.
    fn fixture(label: &str) -> Fixture {
        let root = temp_root(label);
        let vm_directory = root.0.join("dev");
        fs::create_dir_all(vm_directory.join("disks")).expect("disks directory should be created");
        fs::write(vm_directory.join("config.json"), b"{}").expect("configuration should be written");
        fs::write(vm_directory.join("disks").join("system.vhdx"), b"vhdx")
            .expect("system disk should be written");

        let mapping = VmComputeSystemMapping {
            vm_id: Uuid::new_v4(),
            vm_name: "dev".into(),
            hcs_compute_system_id: "vmlord-dev".into(),
            disk_gb: 20,
        };
        let store = MetadataStore::new(root.0.join("vm-mapping.json"));
        store
            .insert(mapping.clone())
            .expect("mapping should be persisted");

        Fixture {
            store,
            mapping,
            vm_directory,
            teardowns: Arc::new(Mutex::new(Vec::new())),
            _root: root,
        }
    }

    fn pipeline(teardowns: &Arc<Mutex<Vec<String>>>, fail: bool) -> VmDeletionPipeline {
        let teardowns = Arc::clone(teardowns);
        VmDeletionPipeline::for_test(move |id: &str| {
            teardowns.lock().unwrap().push(id.to_owned());
            if fail {
                return Err(RepositoryError::new("injected teardown failure"));
            }
            Ok(())
        })
    }

    #[test]
    fn removes_the_compute_system_the_directory_and_the_mapping() {
        let fixture = fixture("happy");

        pipeline(&fixture.teardowns, false)
            .delete(&fixture.store, "dev", &fixture.vm_directory, true)
            .expect("deletion should succeed");

        assert_eq!(
            fixture.teardowns.lock().unwrap().as_slice(),
            std::slice::from_ref(&fixture.mapping.hcs_compute_system_id)
        );
        assert!(!fixture.vm_directory.exists());
        assert!(
            fixture
                .store
                .find_by_vm_name("dev")
                .expect("the store should be readable")
                .is_none(),
            "a fully deleted VM must no longer be known to VMLord"
        );
    }

    #[test]
    fn keeping_the_disks_removes_the_configuration_but_not_the_disk() {
        let fixture = fixture("keep-disks");

        pipeline(&fixture.teardowns, false)
            .delete(&fixture.store, "dev", &fixture.vm_directory, false)
            .expect("deletion should succeed");

        assert!(
            !fixture.vm_directory.join("config.json").exists(),
            "the configuration describes a VM that no longer exists"
        );
        assert!(
            fixture.vm_directory.join("disks").join("system.vhdx").exists(),
            "the disks must survive when the user asked to keep them"
        );
        assert!(
            fixture
                .store
                .find_by_vm_name("dev")
                .expect("the store should be readable")
                .is_none()
        );
    }

    #[test]
    fn rejects_an_unknown_vm_without_touching_hcs_or_the_filesystem() {
        let fixture = fixture("unknown");

        let error = pipeline(&fixture.teardowns, false)
            .delete(&fixture.store, "missing-vm", &fixture.vm_directory, true)
            .expect_err("an unknown VM must not be deleted");

        assert!(error.to_string().contains("missing-vm"));
        assert!(fixture.teardowns.lock().unwrap().is_empty());
        assert!(fixture.vm_directory.exists());
    }

    #[test]
    fn a_failed_teardown_keeps_the_mapping_so_the_deletion_can_be_retried() {
        let fixture = fixture("teardown-failure");

        let error = pipeline(&fixture.teardowns, true)
            .delete(&fixture.store, "dev", &fixture.vm_directory, true)
            .expect_err("a failed teardown must be reported");

        assert!(error.to_string().contains("injected teardown failure"));
        assert!(
            fixture
                .store
                .find_by_vm_name("dev")
                .expect("the store should be readable")
                .is_some(),
            "a VM whose resources are still present must stay known to VMLord"
        );
    }

    #[test]
    fn removes_the_files_even_when_the_teardown_failed() {
        let fixture = fixture("keeps-going");

        pipeline(&fixture.teardowns, true)
            .delete(&fixture.store, "dev", &fixture.vm_directory, true)
            .expect_err("a failed teardown must be reported");

        assert!(
            !fixture.vm_directory.exists(),
            "a failed teardown must not stop the remaining cleanup"
        );
    }

    #[test]
    fn an_already_removed_configuration_does_not_fail_a_kept_disks_deletion() {
        let fixture = fixture("no-config");
        fs::remove_file(fixture.vm_directory.join("config.json"))
            .expect("the configuration should be removable");

        pipeline(&fixture.teardowns, false)
            .delete(&fixture.store, "dev", &fixture.vm_directory, false)
            .expect("an already-removed configuration is not a failure");

        assert!(
            fixture
                .store
                .find_by_vm_name("dev")
                .expect("the store should be readable")
                .is_none()
        );
    }

    #[test]
    fn production_pipeline_is_available_to_the_repository() {
        let _: fn() -> VmDeletionPipeline = VmDeletionPipeline::production;
        let _: fn(&VmDeletionPipeline, &MetadataStore, &str, &Path, bool) -> Result<(), RepositoryError> =
            VmDeletionPipeline::delete;
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu delete`
Expected: FAIL — `cannot find struct, variant or union type 'VmDeletionPipeline'`.

- [ ] **Step 4: Implement the pipeline**

Insert above the test module in `crates/platform/src/delete.rs`:

```rust
/// Deletes VMs known to [`MetadataStore`].
pub struct VmDeletionPipeline {
    system_teardown: SystemTeardown,
}

impl VmDeletionPipeline {
    /// Creates a pipeline backed by the real HCS API.
    #[must_use]
    pub fn production() -> Self {
        Self {
            system_teardown: Box::new(cleanup::teardown_compute_system),
        }
    }

    #[cfg(test)]
    fn for_test(teardown: impl Fn(&str) -> Result<(), RepositoryError> + 'static) -> Self {
        Self {
            system_teardown: Box::new(teardown),
        }
    }

    /// Removes everything VMLord created for the VM named `vm_name`: its HCS
    /// compute system, its files under `vm_directory`, and its mapping.
    ///
    /// The steps run in that order and each one runs even if an earlier one
    /// failed, because a resource left behind is not a reason to leave the
    /// others behind too. The mapping is removed last and only when nothing
    /// failed: a VM whose resources are still partly present stays known to
    /// VMLord, stays visible to the user, and can be deleted again. Removing it
    /// from the store first would turn a partial failure into orphaned files
    /// and compute systems the application can no longer reach.
    ///
    /// With `delete_disks` the whole VM directory goes; without it only the
    /// stored configuration does, and the disks are left for the user. The
    /// image the VM was installed from is never touched: it belongs to the
    /// user, not to the VM.
    pub fn delete(
        &self,
        store: &MetadataStore,
        vm_name: &str,
        vm_directory: &Path,
        delete_disks: bool,
    ) -> Result<(), RepositoryError> {
        let mapping = store.find_by_vm_name(vm_name)?.ok_or_else(|| {
            let error = RepositoryError::new(format!("no HCS mapping found for VM \"{vm_name}\""));
            log::error!("{error}");
            error
        })?;

        log::info!(
            "deleting VM \"{}\" ({}) as HCS compute system \"{}\", {}",
            mapping.vm_name,
            mapping.vm_id,
            mapping.hcs_compute_system_id,
            if delete_disks {
                "disks included"
            } else {
                "keeping its disks"
            }
        );

        let mut failures = Vec::new();
        if let Err(error) = (self.system_teardown)(&mapping.hcs_compute_system_id) {
            failures.push(format!("its compute system was not torn down: {error}"));
        }
        if let Err(error) = remove_files(vm_directory, delete_disks) {
            failures.push(format!("its files were not removed: {error}"));
        }

        if !failures.is_empty() {
            log::warn!(
                "VM \"{}\" ({}) stays known to VMLord because its deletion did not complete",
                mapping.vm_name,
                mapping.vm_id
            );
            return Err(cleanup::combine_failures(
                &format!("deletion of VM \"{}\" did not complete", mapping.vm_name),
                failures,
            ));
        }

        store.remove(mapping.vm_id)?;
        if !delete_disks {
            log::warn!(
                "the disks of VM \"{}\" were kept under {}",
                mapping.vm_name,
                vm_directory.display()
            );
        }
        log::info!("deleted VM \"{}\" ({})", mapping.vm_name, mapping.vm_id);
        Ok(())
    }
}

impl Default for VmDeletionPipeline {
    fn default() -> Self {
        Self::production()
    }
}

/// Removes the VM's files, honouring the user's choice about its disks.
fn remove_files(vm_directory: &Path, delete_disks: bool) -> Result<(), RepositoryError> {
    if delete_disks {
        return cleanup::remove_vm_directory(vm_directory);
    }

    let configuration = layout::configuration_path(vm_directory);
    if !configuration.exists() {
        log::debug!(
            "the configuration of the deleted VM at {} is already gone",
            configuration.display()
        );
        return Ok(());
    }
    fs::remove_file(&configuration).map_err(|error| {
        let error = RepositoryError::new(format!(
            "failed to remove the HCS configuration {}: {error}",
            configuration.display()
        ));
        log::error!("{error}");
        error
    })?;
    log::debug!("removed the HCS configuration {}", configuration.display());
    Ok(())
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu delete`
Expected: PASS — 7 tests.

- [ ] **Step 6: Run the full platform suite and lints**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu`
Expected: PASS — 91 passed, 1 ignored.

Run: `cargo clippy -p vmlord-platform --target=x86_64-pc-windows-gnu --all-targets`
Expected: no new warnings beyond the pre-existing `result_large_err` ones.

- [ ] **Step 7: Commit**

```bash
git add crates/platform/src/delete.rs crates/platform/src/lib.rs
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
  git commit -m "TASK-32: Add the VM deletion pipeline"
```

---

### Task 3: Repository contract and backends

Adds `delete_vm` to the repository boundary and implements it in all three
backends. The whole workspace compiles again at the end of this task.

**Files:**
- Modify: `crates/core/src/lib.rs:52-60` (request types), `crates/core/src/lib.rs:139-158` (`VmRepository`)
- Modify: `crates/platform/src/repository.rs`
- Modify: `crates/app/src/lib.rs:435-471` (`UnavailableRepository`)
- Modify: `crates/legacy-backend/src/windows.rs:354-359` (after `force_stop_vm`)

**Interfaces:**
- Consumes: `VmDeletionPipeline::{production, delete}` from Task 2; `list_known_vms(&HcsClient, &MetadataStore) -> Result<Vec<KnownVm>, RepositoryError>` where `KnownVm { mapping: VmComputeSystemMapping, state: Option<HcsSystemState> }`.
- Produces:
  - `vmlord_core::VmDeleteRequest { name: String, delete_disks: bool }`
  - `vmlord_core::VmRepository::delete_vm(&mut self, request: VmDeleteRequest) -> Result<(), RepositoryError>`

- [ ] **Step 1: Write the failing test**

Add to the test module in `crates/platform/src/repository.rs` — extend the
existing `every_operation_refuses_to_run_before_initialization` test and add a
`delete_request` helper next to the existing `update_request` one:

```rust
    fn delete_request() -> VmDeleteRequest {
        VmDeleteRequest {
            name: "dev".into(),
            delete_disks: true,
        }
    }
```

and inside `every_operation_refuses_to_run_before_initialization`, after the
existing `update_vm` assertion:

```rust
        assert_not_initialized(repository.delete_vm(delete_request()));
```

Extend that test module's import line to bring in `VmDeleteRequest`:

```rust
    use vmlord_core::{
        GpuMode, NetworkMode, RepositoryError, VmDeleteRequest, VmRepository, VmUpdateRequest,
    };
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu refuses_to_run_before_initialization`
Expected: FAIL to compile — `cannot find struct 'VmDeleteRequest' in crate 'vmlord_core'`.

- [ ] **Step 3: Add the request type and the trait method**

In `crates/core/src/lib.rs`, after `VmUpdateRequest`:

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmDeleteRequest {
    pub name: String,
    /// Whether the VM's virtual disks are removed along with it.
    ///
    /// Keeping them leaves the VM's directory in place, so a later VM of the
    /// same name cannot reuse that directory.
    pub delete_disks: bool,
}
```

In the `VmRepository` trait, after `force_stop_vm`:

```rust
    /// Removes the VM and every resource VMLord created for it.
    ///
    /// Required rather than defaulted: a backend that cannot delete VMs has to
    /// say so, not inherit silence.
    fn delete_vm(&mut self, request: VmDeleteRequest) -> Result<(), RepositoryError>;
```

- [ ] **Step 4: Implement it in the HCS repository**

In `crates/platform/src/repository.rs`:

Add `VmDeleteRequest` to the `use vmlord_core::{...}` list, add `VmDeletionPipeline`
to the `use crate::{...}` list, add the field to the struct after `force_stop`:

```rust
    delete: VmDeletionPipeline,
```

and to `HcsVmRepository::new`, after `force_stop: VmForceStopPipeline::production(),`:

```rust
            delete: VmDeletionPipeline::production(),
```

Add this private helper to the `impl HcsVmRepository` block, next to `mapping`:

```rust
    /// Reports whether HCS currently runs the VM behind `mapping`.
    ///
    /// The application layer's cached list can be stale by the time the user
    /// acts on it, so a destructive operation asks HCS itself.
    fn is_running(&self, mapping: &VmComputeSystemMapping) -> Result<bool, RepositoryError> {
        Ok(list_known_vms(&self.client, &self.store)?
            .into_iter()
            .find(|known| known.mapping.vm_id == mapping.vm_id)
            .is_some_and(|known| matches!(known.state, Some(HcsSystemState::Running))))
    }
```

Add the trait method after `force_stop_vm` in `impl VmRepository for HcsVmRepository`:

```rust
    /// Deletes the VM and everything VMLord created for it.
    ///
    /// A running VM is refused rather than torn down under its guest: deletion
    /// is irreversible, and stopping is the user's decision to make
    /// deliberately.
    fn delete_vm(&mut self, request: VmDeleteRequest) -> Result<(), RepositoryError> {
        self.require_initialized()?;

        let mapping = self.mapping(&request.name)?;
        if self.is_running(&mapping)? {
            let error = RepositoryError::new(format!(
                "VM \"{}\" is running; stop it before deleting it",
                request.name
            ));
            log::error!("{error}");
            return Err(error);
        }

        let vm_directory = layout::vm_directory(&self.storage_root, &request.name)?;
        self.delete
            .delete(&self.store, &request.name, &vm_directory, request.delete_disks)?;
        // The VM is gone, so any handle still held for it refers to nothing.
        self.connections.remove(mapping.vm_id);
        Ok(())
    }
```

- [ ] **Step 5: Implement it in the other two backends**

In `crates/app/src/lib.rs`, in `impl VmRepository for UnavailableRepository`,
after `force_stop_vm`:

```rust
    fn delete_vm(&mut self, _request: VmDeleteRequest) -> Result<(), RepositoryError> {
        Err(RepositoryError::new(self.message.clone()))
    }
```

and add `VmDeleteRequest` to the crate's `use vmlord_core::{...}` list at the top
of the file.

In `crates/legacy-backend/src/windows.rs`, in `impl VmRepository for
AppSandboxBackend`, after `force_stop_vm`:

```rust
    fn delete_vm(&mut self, request: vmlord_core::VmDeleteRequest) -> Result<(), RepositoryError> {
        Err(RepositoryError::new(format!(
            "the legacy AppSandbox backend cannot delete VM \"{}\"; \
             run VMLord on the native HCS backend to delete VMs",
            request.name
        )))
    }
```

- [ ] **Step 6: Run everything**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu`
Expected: PASS — 91 passed, 1 ignored.

Run: `cargo build --target=x86_64-pc-windows-gnu`
Expected: the whole workspace builds, `vmlord-legacy-backend` and `vmlord-ui`
included.

- [ ] **Step 7: Commit**

```bash
git add crates/core/src/lib.rs crates/platform/src/repository.rs crates/app/src/lib.rs crates/legacy-backend/src/windows.rs
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
  git commit -m "TASK-32: Add VM deletion to the repository contract"
```

---

### Task 4: Deletion workflow

**Files:**
- Modify: `crates/app/src/lib.rs` — `WorkspaceApp::delete_vm`, `FakeRepository::delete_vm`, new tests
- Test: `crates/app/src/lib.rs` (the crate's `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `VmRepository::delete_vm(VmDeleteRequest)` from Task 3.
- Produces: `WorkspaceApp::delete_vm(&mut self, request: VmDeleteRequest) -> Result<(), RepositoryError>`.

- [ ] **Step 1: Write the failing tests**

In `crates/app/src/lib.rs`, first give `FakeRepository` the new method so the
tests can observe the request — add to `impl VmRepository for FakeRepository`,
after `force_stop_vm`:

```rust
        fn delete_vm(&mut self, request: VmDeleteRequest) -> Result<(), RepositoryError> {
            self.actions
                .push(format!("delete:{}:{}", request.name, request.delete_disks));
            Ok(())
        }
```

`FakeRepository::list_vms` reports a single stopped VM named `dev`. Two tests
need it running instead, so add a field to the struct and use it:

```rust
    struct FakeRepository {
        should_fail: bool,
        create_should_fail: bool,
        vm_is_running: bool,
        actions: Vec<String>,
    }
```

In `FakeRepository::list_vms`, replace the hard-coded `state: VmState::Stopped,`
with:

```rust
            state: if self.vm_is_running {
                VmState::Running {
                    agent_status: vmlord_core::AgentStatus::Unknown,
                }
            } else {
                VmState::Stopped
            },
```

Every existing `FakeRepository { ... }` literal in the test module gains
`vm_is_running: false,`. There are seven of them.

Then add a helper and the new tests at the end of the test module:

```rust
    fn app_with(vm_is_running: bool) -> WorkspaceApp {
        let mut app = WorkspaceApp::new(Box::new(FakeRepository {
            should_fail: false,
            create_should_fail: false,
            vm_is_running,
            actions: Vec::new(),
        }));
        app.start();
        app
    }

    fn delete_request(delete_disks: bool) -> VmDeleteRequest {
        VmDeleteRequest {
            name: "dev".into(),
            delete_disks,
        }
    }

    #[test]
    fn deletes_a_stopped_vm_through_the_repository() {
        let mut app = app_with(false);

        app.delete_vm(delete_request(true)).unwrap();

        assert!(
            app.diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.message == "VM \"dev\" deleted")
        );
    }

    #[test]
    fn refuses_to_delete_a_running_vm() {
        let mut app = app_with(true);

        let error = app
            .delete_vm(delete_request(true))
            .expect_err("a running VM must not be deleted");

        assert!(error.to_string().contains("running"));
        assert!(
            app.diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.level == DiagnosticLevel::Error)
        );
    }

    #[test]
    fn refuses_to_delete_an_unknown_vm() {
        let mut app = app_with(false);

        let error = app
            .delete_vm(VmDeleteRequest {
                name: "missing-vm".into(),
                delete_disks: true,
            })
            .expect_err("an unknown VM must not be deleted");

        assert!(error.to_string().contains("missing-vm"));
    }

    #[test]
    fn refuses_to_delete_without_a_ready_backend() {
        let mut app = WorkspaceApp::new(Box::new(FakeRepository {
            should_fail: true,
            create_should_fail: false,
            vm_is_running: false,
            actions: Vec::new(),
        }));
        app.start();

        let error = app
            .delete_vm(delete_request(true))
            .expect_err("deletion needs a ready backend");

        assert!(error.to_string().contains("ready backend"));
    }

    #[test]
    fn warns_when_the_disks_are_kept() {
        let mut app = app_with(false);

        app.delete_vm(delete_request(false)).unwrap();

        assert!(
            app.diagnostics().iter().any(|diagnostic| {
                diagnostic.level == DiagnosticLevel::Warning
                    && diagnostic.message.contains("disks")
            }),
            "keeping the disks leaves the VM directory behind and the user must be told"
        );
    }

    #[test]
    fn deleting_with_the_disks_does_not_warn_about_them() {
        let mut app = app_with(false);

        app.delete_vm(delete_request(true)).unwrap();

        assert!(!app.diagnostics().iter().any(|diagnostic| {
            diagnostic.level == DiagnosticLevel::Warning && diagnostic.message.contains("disks")
        }));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-app delete`
Expected: FAIL — `no method named 'delete_vm' found for struct 'WorkspaceApp'`.

- [ ] **Step 3: Implement the workflow**

In `crates/app/src/lib.rs`, add to `impl WorkspaceApp`, after `force_stop_vm`:

```rust
    /// Deletes a VM and every resource VMLord created for it.
    ///
    /// A VM that is not stopped is refused here rather than stopped
    /// automatically: deletion cannot be undone, so ending a running guest is a
    /// decision the user makes on purpose. The repository checks this again
    /// against HCS itself, because this list is a cache and can be stale.
    pub fn delete_vm(&mut self, request: VmDeleteRequest) -> Result<(), RepositoryError> {
        self.require_ready_backend("VM deletion")?;

        let vm_state = self
            .vms
            .iter()
            .find(|vm| vm.name == request.name)
            .map(|vm| vm.state)
            .ok_or_else(|| {
                let error =
                    RepositoryError::new(format!("VM \"{}\" was not found", request.name));
                self.diagnostics.push(Diagnostic {
                    level: DiagnosticLevel::Error,
                    message: error.to_string(),
                });
                error
            })?;
        if !matches!(vm_state, VmState::Stopped) {
            let error = RepositoryError::new(format!(
                "VM \"{}\" is running; stop it before deleting it",
                request.name
            ));
            log::error!("{error}");
            self.diagnostics.push(Diagnostic {
                level: DiagnosticLevel::Error,
                message: error.to_string(),
            });
            return Err(error);
        }

        let name = request.name.clone();
        let kept_disks = !request.delete_disks;
        log::info!("requesting deletion of VM {name}");

        match self.repository.delete_vm(request) {
            Ok(()) => {
                self.diagnostics.push(Diagnostic {
                    level: DiagnosticLevel::Info,
                    message: format!("VM \"{name}\" deleted"),
                });
                if kept_disks {
                    self.diagnostics.push(Diagnostic {
                        level: DiagnosticLevel::Warning,
                        message: format!(
                            "The disks of VM \"{name}\" were kept; its directory still exists \
                             and a new VM cannot reuse that name until it is removed"
                        ),
                    });
                }
                self.refresh();
                Ok(())
            }
            Err(error) => {
                log::error!("failed to delete VM {name}: {error}");
                self.diagnostics.push(Diagnostic {
                    level: DiagnosticLevel::Error,
                    message: format!("Failed to delete VM \"{name}\": {error}"),
                });
                self.collect_diagnostics();
                Err(error)
            }
        }
    }
```

Add `VmDeleteRequest` to the crate-level `use vmlord_core::{...}` list if Task 3
did not already, and `VmDeleteRequest` to the test module's `use super::*`-adjacent
imports if needed (the test module uses `use super::*`, so the crate-level import
covers it).

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-app`
Expected: PASS — 14 tests (8 existing plus 6 new).

- [ ] **Step 5: Run the host suite and lints**

Run: `cargo test -p vmlord-core -p vmlord-app`
Expected: PASS.

Run: `cargo clippy -p vmlord-app --target=x86_64-pc-windows-gnu --all-targets`
Expected: no new warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/app/src/lib.rs
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
  git commit -m "TASK-32: Add the VM deletion workflow"
```

---

### Task 5: Delete confirmation dialog

`crates/ui` has no test harness for rendering — its `#[cfg(test)] mod tests`
covers only the pure request-building functions. This task is therefore verified
by compilation, lints, and reading the wiring; there is no rendering test to
write.

**Files:**
- Modify: `crates/ui/src/lib.rs:27-46` (`VmlordUi` construction and struct), `:102-160` (forms and dialog actions), `:238-274` (`VmAction` dispatch), `:357-375` (dialog handling), and a new `render_delete_vm_dialog` next to `render_edit_vm_dialog` at `:612`

**Interfaces:**
- Consumes: `WorkspaceApp::delete_vm(VmDeleteRequest) -> Result<(), RepositoryError>` from Task 4.
- Produces: nothing consumed by later tasks.

- [ ] **Step 1: Add the form state**

In `crates/ui/src/lib.rs`, add `VmDeleteRequest` to the `use vmlord_core::{...}`
list. Add the field to `VmlordUi` after `edit_vm_form`:

```rust
    delete_vm_form: Option<DeleteVmForm>,
```

and to the struct literal inside `run`, after `edit_vm_form: None,`:

```rust
                delete_vm_form: None,
```

Add the form and its dialog action next to `EditVmForm`:

```rust
struct DeleteVmForm {
    vm_name: String,
    delete_disks: bool,
    error: Option<String>,
}

impl DeleteVmForm {
    fn for_vm(vm_name: &str) -> Self {
        Self {
            vm_name: vm_name.to_owned(),
            // Deleting the disks is what "delete the VM" normally means;
            // keeping them is the deliberate exception.
            delete_disks: true,
            error: None,
        }
    }
}

enum DeleteVmDialogAction {
    Cancel,
    Submit,
}
```

- [ ] **Step 2: Open the dialog instead of logging the click**

In `update`, the `VmAction::Delete` case currently falls into the catch-all
`_ => self.application.log_vm_action(action)` arm at
`crates/ui/src/lib.rs:271`. Add an explicit arm before it:

```rust
                VmAction::Delete => {
                    if let Some(name) = self.selected_vm_name.clone() {
                        self.delete_vm_form = Some(DeleteVmForm::for_vm(&name));
                        self.create_vm_form = None;
                        self.edit_vm_form = None;
                    }
                }
```

- [ ] **Step 3: Handle the dialog's outcome**

After the `edit_dialog_action` block that ends `update` (currently
`crates/ui/src/lib.rs:357-374`), add:

```rust
        let delete_dialog_action = self
            .delete_vm_form
            .as_mut()
            .and_then(|form| render_delete_vm_dialog(context, form));
        match delete_dialog_action {
            Some(DeleteVmDialogAction::Cancel) => self.delete_vm_form = None,
            Some(DeleteVmDialogAction::Submit) => {
                let request = self.delete_vm_form.as_ref().map(|form| VmDeleteRequest {
                    name: form.vm_name.clone(),
                    delete_disks: form.delete_disks,
                });
                if let Some(request) = request {
                    match self.application.delete_vm(request) {
                        Ok(()) => {
                            self.delete_vm_form = None;
                            self.selected_vm_name = None;
                            self.last_refresh = Instant::now();
                        }
                        Err(error) => {
                            if let Some(form) = &mut self.delete_vm_form {
                                form.error = Some(error.to_string());
                            }
                        }
                    }
                }
            }
            None => {}
        }
```

- [ ] **Step 4: Render the dialog**

Add after `render_edit_vm_dialog` (which ends at `crates/ui/src/lib.rs:698`):

```rust
fn render_delete_vm_dialog(
    context: &egui::Context,
    form: &mut DeleteVmForm,
) -> Option<DeleteVmDialogAction> {
    let mut open = true;
    let mut action = None;
    egui::Window::new(format!("Delete VM: {}", form.vm_name))
        .collapsible(false)
        .resizable(false)
        .default_width(420.0)
        .open(&mut open)
        .show(context, |ui| {
            ui.label(format!(
                "VM \"{}\" and its stored configuration will be removed. This cannot be undone.",
                form.vm_name
            ));
            ui.add_space(8.0);
            ui.checkbox(&mut form.delete_disks, "Delete virtual disks");
            if form.delete_disks {
                ui.small("The VM's virtual disks are deleted with it. The image it was installed from is not touched.");
            } else {
                ui.small("The virtual disks are kept, so the VM's directory stays in place and a new VM cannot reuse that name.");
            }

            if let Some(error) = &form.error {
                ui.add_space(4.0);
                ui.colored_label(egui::Color32::LIGHT_RED, error);
            }

            ui.separator();
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let delete = ui.add(
                    egui::Button::new(egui::RichText::new("Delete").color(egui::Color32::WHITE))
                        .fill(egui::Color32::from_rgb(192, 57, 43)),
                );
                if delete.clicked() {
                    action = Some(DeleteVmDialogAction::Submit);
                }
                if ui.button("Cancel").clicked() {
                    action = Some(DeleteVmDialogAction::Cancel);
                }
            });
        });

    if !open && action.is_none() {
        action = Some(DeleteVmDialogAction::Cancel);
    }
    action
}
```

- [ ] **Step 5: Build and lint**

Run: `cargo build --target=x86_64-pc-windows-gnu`
Expected: the workspace builds.

Run: `cargo clippy -p vmlord-ui --target=x86_64-pc-windows-gnu --all-targets`
Expected: no new warnings.

Run: `cargo test -p vmlord-core -p vmlord-app`
Expected: PASS — nothing regressed.

- [ ] **Step 6: Verify the wiring by reading**

Confirm all three by inspection:
- `VmAction::Delete` no longer reaches `log_vm_action`;
- the Delete button's `can_delete` at `crates/ui/src/lib.rs:944` still gates on
  `matches!(vm.state, VmState::Stopped)`;
- no Windows API call and no business rule were added to `crates/ui`.

- [ ] **Step 7: Commit**

```bash
git add crates/ui/src/lib.rs
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
  git commit -m "TASK-32: Connect the Delete button to the deletion workflow"
```

---

### Task 6: Integration test and documentation

**Files:**
- Modify: `crates/platform/tests/hyperv.rs` (imports at `:17-24`, new test at the end)
- Modify: `ARCHITECTURE.md`

**Interfaces:**
- Consumes: `VmDeletionPipeline::{production, delete}`, `VmCreationPipeline::create`, `list_known_vms`, `MetadataStore`.
- Produces: nothing.

- [ ] **Step 1: Add the end-to-end test**

Add `VmDeletionPipeline` to the `use vmlord_platform::{...}` list, then append to
`crates/platform/tests/hyperv.rs`:

```rust
/// Exercises TASK-32's deletion against the real Host Compute Service: creates
/// a VM, deletes it, and confirms nothing it was made of is left behind --
/// neither the compute system, nor its directory, nor its metadata mapping.
///
/// Run elevated with:
/// `cargo test -p vmlord-platform --test hyperv -- --ignored --exact deletes_a_created_vm_completely --nocapture`
#[test]
#[ignore = "requires an elevated Windows host with Hyper-V/HCS enabled"]
fn deletes_a_created_vm_completely() {
    let root = std::env::temp_dir().join(format!("vmlord-hcs-delete-e2e-{}", std::process::id()));
    fs::create_dir_all(&root).expect("test root should be created");
    let image_path = root.join("installer.iso");
    fs::write(&image_path, b"placeholder installer media").expect("test image should be written");

    let request = VmCreateRequest {
        name: format!("vmlord-e2e-delete-test-{}", std::process::id()),
        image_path: image_path.to_string_lossy().into_owned(),
        ram_mb: 512,
        disk_gb: 1,
        cpu_cores: 1,
        gpu_mode: GpuMode::None,
        network_mode: NetworkMode::None,
        username: "admin".into(),
        password: "not used by create".into(),
        ssh_enabled: false,
        ssh_deploy_key: false,
    };
    let store = MetadataStore::new(root.join("vm-mapping.json"));
    let vm_directory = root.join("vm");

    let mapping = VmCreationPipeline::production()
        .create(&store, &request, &vm_directory)
        .expect("VM creation should succeed on an elevated Hyper-V host");
    println!(
        "created HCS compute system \"{}\" for VM {}",
        mapping.hcs_compute_system_id, mapping.vm_id
    );

    let deleted = VmDeletionPipeline::production().delete(&store, &request.name, &vm_directory, true);

    // Best-effort cleanup regardless of the assertions below.
    let _ = fs::remove_dir_all(&root);

    deleted.expect("deletion should succeed on an elevated Hyper-V host");
    assert!(
        !vm_directory.exists(),
        "the VM directory must be gone once the VM is deleted"
    );
    assert!(
        store
            .find_by_vm_name(&request.name)
            .expect("the store should be readable")
            .is_none(),
        "a deleted VM must no longer be known to VMLord"
    );
    assert!(
        HcsSystem::open_if_present(&mapping.hcs_compute_system_id, HCS_ACCESS_ALL)
            .expect("HCS should answer whether it still knows the compute system")
            .is_none(),
        "HCS must no longer know the compute system of a deleted VM"
    );
}
```

`HcsSystem::open_if_present` is already public from `crates/platform/src/hcs.rs`.
The test file's local `HCS_ACCESS_ALL` constant at `crates/platform/tests/hyperv.rs:30`
is the one to use.

- [ ] **Step 2: Verify the test compiles and stays ignored**

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu --test hyperv`
Expected: `0 passed; 0 failed; 11 ignored` — the new test compiles and is skipped
without an elevated Hyper-V host.

- [ ] **Step 3: Document deletion in ARCHITECTURE.md**

Two edits.

First, `ARCHITECTURE.md:208-209` currently ends the enumeration paragraph with a
sentence that deletion is still outstanding:

```
to its `HcsSystem` handle through the same store. Remaining HCS lifecycle work
(delete) still resolves a VM to its compute system through this store.
```

Delete that stale second sentence, leaving the paragraph ending at "through the
same store." (The earlier mention at `ARCHITECTURE.md:198-201` already lists
delete among the lifecycle work resolving through `MetadataStore` and stays as
it is.)

Second, add this paragraph after the `VmForceStopPipeline` one that ends at
`ARCHITECTURE.md:247`, before the "An HCS compute system is a runtime object"
paragraph:

```markdown
`platform::VmDeletionPipeline` removes everything a VM is made of: its compute
system, the `config.json` creation wrote, its disks, and its `MetadataStore`
mapping. Each step runs even if an earlier one failed -- a resource left behind
is no reason to leave the others -- and the mapping is dropped last and only
when nothing failed. That order is what keeps a partial failure recoverable: a
VM whose resources are still partly present stays known to VMLord, stays listed,
and can be deleted again, whereas dropping the mapping first would orphan files
and compute systems the application can no longer reach. A running VM is refused
rather than terminated under its guest, because deletion cannot be undone. The
disks can be kept at the user's request, which leaves the VM's directory in
place and therefore reserves its name; the image the VM was installed from is
never touched.
```

- [ ] **Step 4: Full verification**

Run: `cargo test -p vmlord-core -p vmlord-app`
Expected: PASS.

Run: `cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu`
Expected: PASS — 91 passed, 1 ignored in the lib tests, 11 ignored in `hyperv`.

Run: `cargo build --target=x86_64-pc-windows-gnu`
Expected: the workspace builds.

Run: `cargo clippy --target=x86_64-pc-windows-gnu --all-targets`
Expected: only the pre-existing warnings (`result_large_err` and
`manual_is_multiple_of`).

- [ ] **Step 5: Commit**

```bash
git add crates/platform/tests/hyperv.rs ARCHITECTURE.md
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local \
  git commit -m "TASK-32: Cover VM deletion with a Hyper-V integration test"
```

---

## Manual verification on a real Hyper-V host

The unit tests never touch HCS, so the following is done by the user on an
elevated Windows host, as the epic (#24) prescribes for every subtask:

1. Run VMLord, create a VM, leave it stopped, press Delete, confirm with
   "Delete virtual disks" checked. The VM disappears from the list, its
   directory under the configured VM storage path is gone, and `vm-mapping.json`
   no longer names it.
2. Create a VM, start it, and confirm the Delete button stays disabled while it
   runs.
3. Create a VM, delete it with "Delete virtual disks" unchecked. The VM
   disappears from the list, `disks/system.vhdx` survives, `config.json` is gone,
   and the diagnostics pane carries the warning about the kept disks.
4. Check the log file at the configured path for the DEBUG/INFO/WARN lines the
   deletion emits.

## Out of scope

Networks, SSH keys and GPU assignment: the native backend does not create them
yet (TASK-10..12), so there is nothing of that kind to remove. When they land,
their removal joins `VmDeletionPipeline::delete` as further best-effort steps
before the mapping is dropped.
