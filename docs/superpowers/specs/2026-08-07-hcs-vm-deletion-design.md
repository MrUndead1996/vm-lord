# TASK-32: HCS VM deletion

Design for deleting a VMLord-managed virtual machine on the native HCS backend:
the repository operation, the application workflow, the UI confirmation, and the
shared cleanup helpers that remove the duplication between deletion and the
existing creation rollback.

## Goal

Deleting a VM removes every resource VMLord created for it — the HCS compute
system, the stored configuration, the virtual disks, and the metadata mapping —
and nothing else. The user's source ISO image is never touched.

Deletion is destructive and irreversible, so the default path is the safe one: a
running VM is refused rather than killed, the UI asks for confirmation, and a
partially failed deletion leaves the VM visible and retryable rather than
half-gone.

## Scope

In scope: `VmDeleteRequest` and the `VmRepository::delete_vm` contract, a
`VmDeletionPipeline` in the platform crate, shared cleanup helpers, the
`WorkspaceApp::delete_vm` workflow, the UI confirmation dialog, unit tests, and
an ignored Hyper-V integration test.

Out of scope: networks and SSH keys. The native backend reports
`NetworkMode::None` and no SSH port because those are not wired to it yet
(TASK-10..12); there is nothing of that kind to remove. When they land, their
removal joins the deletion pipeline as further steps.

## Contract (`crates/core`)

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmDeleteRequest {
    pub name: String,
    /// Whether the VM's virtual disks are removed along with it.
    pub delete_disks: bool,
}
```

`VmRepository` gains a required method:

```rust
fn delete_vm(&mut self, request: VmDeleteRequest) -> Result<(), RepositoryError>;
```

Required rather than defaulted (unlike `open_display`/`open_ssh`): a backend
that cannot delete VMs must say so explicitly, not inherit silence.
`UnavailableRepository` reports its usual unavailability message, and the
transitional `AppSandboxBackend` adapter reports that the legacy backend does not
support deletion — its FFI surface has no deletion entry point.

## Shared cleanup (`crates/platform/src/cleanup.rs`)

Creation rollback (`create.rs::rollback`) already tears down a compute system and
removes a VM directory, collecting the failures into one message. Deletion needs
the same two steps. Both move into a new `pub(crate)` module:

- `type SystemTeardown = Box<dyn Fn(&str) -> Result<(), RepositoryError>>` — the
  injectable teardown both pipelines use to stay testable off Windows.
- `teardown_compute_system(id)` — `HcsSystem::open_if_present` +
  `terminate_and_wait` with `TEARDOWN_TIMEOUT` (30s, preserving creation's
  current bound). A compute system HCS does not know is `Ok(())` with a DEBUG
  log, not an error: HCS destroys a compute system when it stops, so a stopped
  VM routinely has none.
- `remove_vm_directory(path)` — removes the tree when it exists, with one
  consistent error text and log.
- `combine_failures(prefix, failures)` — folds accumulated failure messages into
  a single `RepositoryError`.

This also fixes a real defect in rollback: it currently tears down through
`HcsSystem::open`, so a creation that failed *before* the compute system existed
appends a misleading "open compute system failed" clause to the reported error.
`open_if_present` removes that clause.

After the move, `rollback` is a call to the two helpers, and the deletion
pipeline adds only what is genuinely its own.

## Deletion pipeline (`crates/platform/src/delete.rs`)

```rust
pub struct VmDeletionPipeline { system_teardown: SystemTeardown }

impl VmDeletionPipeline {
    pub fn production() -> Self;
    pub fn delete(
        &self,
        store: &MetadataStore,
        vm_name: &str,
        vm_directory: &Path,
        delete_disks: bool,
    ) -> Result<(), RepositoryError>;
}
```

Steps, in order:

1. **Look up the mapping.** No mapping means no VM to delete: return an error
   before touching HCS or the filesystem.
2. **Tear down the compute system** through the injected teardown. Failures are
   recorded, not raised — the remaining steps still run.
3. **Remove files.** With `delete_disks`, remove the whole VM directory. Without
   it, remove only `config.json` and leave `disks/` in place. Failures are
   recorded.
4. **Remove the mapping — last, and only if steps 2 and 3 were clean.** A VM
   whose resources are still partly present stays known to VMLord, stays visible
   in the list, and can be deleted again. Nothing becomes a ghost that the
   application can no longer reach.

When failures were recorded, `delete` returns them combined; the mapping is
intact.

Keeping the disks leaves the VM directory occupied, so creating a VM with the
same name afterwards fails on the existing-directory check in
`VmCreationPipeline::create`. The workflow surfaces this as a warning rather than
letting the user discover it later.

## Repository (`crates/platform/src/repository.rs`)

`HcsVmRepository::delete_vm`:

1. `require_initialized`.
2. Resolve the mapping and read the VM's current state from `list_known_vms` —
   the authoritative source, unlike the application layer's cached list. A VM
   HCS reports as `Running` is refused: "VM \"x\" is running; stop it before
   deleting it."
3. Run the pipeline against `layout::vm_directory(&self.storage_root, name)`.
4. On success, drop any held compute-system handle with
   `self.connections.remove(mapping.vm_id)`.

## Workflow (`crates/app`)

```rust
pub fn delete_vm(&mut self, request: VmDeleteRequest) -> Result<(), RepositoryError>;
```

1. `require_ready_backend("VM deletion")`.
2. Find the VM in the cached list; an unknown name is an error.
3. Refuse a VM whose cached state is not `Stopped`, with a diagnostic.
4. Call the repository. On success: an `Info` diagnostic that the VM was
   deleted, plus a `Warning` naming the retained disks when `delete_disks` is
   false, then `refresh()`. On failure: an `Error` diagnostic and
   `collect_diagnostics()`.

The state check exists at both layers deliberately. The workflow check is the
business rule — cheap, testable without Windows, and the source of the message
the user reads. The repository check is the guarantee, because the cached list
can be stale by the time the user clicks.

## UI (`crates/ui`)

`VmAction::Delete` stops falling through to `log_vm_action` and opens a
confirmation form:

```rust
struct DeleteVmForm { vm_name: String, delete_disks: bool, error: Option<String> }
enum DeleteVmDialogAction { Cancel, Submit }
```

`render_delete_vm_dialog` follows the existing create/edit/settings dialogs: a
modal window titled with the VM name, a line stating what will be removed (the
compute system, the configuration, and — when checked — the virtual disks), a
`Delete virtual disks` checkbox defaulting to checked, and `Delete` / `Cancel`
buttons.

On submit the dialog calls `WorkspaceApp::delete_vm`. Success closes the dialog,
clears `selected_vm_name`, and resets `last_refresh`; failure keeps the dialog
open with the error text. The Delete button in the action group stays enabled
only for `VmState::Stopped`, as it already is.

## Logging

Per the epic's convention, `DEBUG` through `ERROR`, no `TRACE`:

- `DEBUG` — a compute system HCS does not know; each path removed.
- `INFO` — deletion requested for a VM; deletion completed.
- `WARN` — disks retained at the user's request; the mapping kept because
  cleanup did not complete.
- `ERROR` — a refused running VM, a teardown failure, a filesystem failure.

## Testing

`crates/platform/src/delete.rs` unit tests, in the style of the force-stop
pipeline's (real temp directories, injected teardown closure):

- tears down the mapped compute system and removes the mapping;
- treats an absent compute system as success;
- rejects an unknown VM without touching HCS;
- `delete_disks: false` keeps `disks/` and removes `config.json`;
- a teardown failure keeps the mapping so the deletion can be retried;
- a filesystem failure keeps the mapping.

`crates/platform/src/cleanup.rs` unit tests: an absent directory is not an
error; `combine_failures` reports every recorded failure.

`crates/app` unit tests, with `delete_vm` recorded by `FakeRepository`:

- refuses a running VM without reaching the repository;
- refuses an unknown VM;
- refuses an unready backend;
- forwards `delete_disks` unchanged;
- emits the retained-disks warning only when disks are kept.

`crates/platform/tests/hyperv.rs`, `#[ignore]` as the rest of the file: create a
VM, delete it, and assert the directory is gone, the mapping is gone, and the
compute system no longer appears in the HCS enumeration.

## Documentation

`ARCHITECTURE.md` gains deletion in the description of the native backend's VM
lifecycle, alongside create/start/shutdown/force-stop.
