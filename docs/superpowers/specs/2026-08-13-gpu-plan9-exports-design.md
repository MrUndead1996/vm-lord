# Building safe Plan9 exports (#88)

A GPU partition is useless to a Linux guest without the host's driver package
and the WSL Linux userspace beside it, and the only way in is a Plan9 share.
This task decides what may be exported, proves each path is what it claims to
be before the VM is given access to it, and states what the guest is told --
which is a role, never a host path.

Discovery is #85 and reports adapters. Applying a mode to a running compute
system is #89, mounting in the guest is #94, and lifecycle and UI are #98. This
task ends at the boundary of `platform`: it builds a validated export set,
writes it into an HCS configuration document and grants the VM access to it.
Nothing calls that from `start.rs` yet, because a start cannot know a VM's GPU
mode until #89 records one -- `VmComputeSystemMapping` has no such field, and
`hcs_config::build` still rejects every mode but `None`. That is what the
`allow(dead_code)` on the module and on the two `hcs_config` functions is for,
and #89 removes it by becoming the caller.

## Decisions

Four questions were settled before design.

* **The manifest carries roles, not paths.** AppSandbox sent
  `share_name|guest_path|filter`, where `guest_path` was a Windows path the
  Linux agent parsed by taking its leaf and pasting it into
  `/usr/lib/wsl/drivers/<leaf>`; the file filter was ignored on Linux
  entirely. #94 asks for the opposite -- the guest computes only allowlisted
  targets -- so a share is `(name, role)` and the guest decides that `WslLib`
  is `/usr/lib/wsl/lib` and a `DriverPackage` is `/usr/lib/wsl/drivers/<package>`,
  rejecting anything its own allowlist does not cover. A host path would be
  useless to the guest and would leak host topology onto the wire and into
  guest logs. There is no file filter: the share is mounted whole, read-only,
  and nothing is copied.
* **Both modes export every resolved package.** `Default` means "give the VM
  the adapter the host prefers", and which one that is, is settled by HCS after
  the start -- while the exports have to be in the document before it. Guessing
  the preferred adapter diverges from HCS's own choice on exactly the
  multi-GPU hosts where it matters, and a missing package for the adapter HCS
  did choose breaks rendering outright, while a surplus package costs one
  read-only mount. Deduplication by canonical path usually collapses "every
  adapter" to one or two anyway.
* **A candidate that fails validation is dropped, not fatal.** GPU is applied
  best effort and never blocks a start, so one unresolvable directory must not
  fail a boot; the reason is already sayable in the `GpuFailure` /
  `GpuStatusCode` vocabulary from #84 and #85. An empty result is `None`, not
  an error.
* **The wire message is not defined here.** `agent.proto` states that arms from
  field 4 onwards arrive with the task that implements them, so the shape is
  designed against working code. #92 and #94 send the manifest; this task
  provides the single source of truth it is projected from.

## The types

One builder, two projections. The descriptor that holds a host path lives in
`platform` and never leaves it; the manifest type in `core` has no field a host
path could occupy, so the separation is enforced by the type rather than by
discipline.

`crates/core/src/gpu.rs`, beside `HostGpuCapabilities`:

```rust
pub struct GpuShareManifest {
    pub shares: Vec<GpuShare>,
}

pub struct GpuShare {
    /// The share's name, which is also `aname=` when the guest mounts it.
    pub name: String,
    pub role: GpuShareRole,
}

pub enum GpuShareRole {
    /// The host's WSL Linux userspace, from `System32\lxss\lib`.
    WslLib,
    /// One driver package; `package` is a DriverStore `FileRepository` folder
    /// name that has already been checked for its character set.
    DriverPackage { package: String },
}
```

`crates/platform/src/gpu_exports.rs`:

```rust
pub(crate) struct GpuExport {
    name: String,
    /// The canonical path (`\\?\C:\...`), never the one discovery reported.
    host_path: PathBuf,
    role: GpuShareRole,
}

pub(crate) struct GpuExports { /* non-empty, deduplicated, ordered */ }

impl GpuExports {
    /// `None` when there is nothing to export, which is not a failure.
    pub(crate) fn build(adapters: &[HostGpuAdapter]) -> Option<Self>;
    /// What the guest is told. Carries no host paths.
    pub(crate) fn manifest(&self) -> GpuShareManifest;
}
```

Share names are `vmlord.gpu.wsl-lib` and `vmlord.gpu.drv.<package>`. The
package name is the last component of the canonical path and is admitted only
from `[A-Za-z0-9._-]`, at most 96 characters: it lands both in HCS JSON and in
a `mount` option string, where a comma or a space would break parsing. No index
counter -- after deduplication a package is unique by itself, and a name that
reads in a log is worth more than a number. The guest never parses the name;
the role arrives as its own field, which is precisely what the old protocol
lacked.

`GpuShareManifest` is not serializable: it has no on-disk format, and the task
that starts sending it converts it to protobuf.

## Validation

Two roots, both derived from `GetSystemDirectoryW`:
`System32\DriverStore\FileRepository` for packages and `System32\lxss\lib` for
the WSL userspace. Nothing else is exportable, even if discovery some day
reports a path outside the DriverStore.

> **Superseded by TASK-107.** This design assumed the WSL Linux userspace is
> one directory, which is true only of the inbox WSL. Where WSL comes from the
> Store or the standalone installer, `System32\lxss\lib` holds the GPU
> vendor's libraries and the Microsoft ones a renderer links against --
> `libd3d12.so`, `libd3d12core.so`, `libdxcore.so` -- are installed beside the
> package under `Program Files\WSL\lib`. The first real host had eighteen
> NVIDIA files in the first directory and no `libd3d12.so` anywhere the guest
> could reach, and its probe stopped at `DEVICE_ONLY`. There is now a third
> root and a second userspace role, `WslD3d12`, and the new root is validated
> against `Program Files` by exactly the procedure below. `Program Files` is
> asked of the shell rather than read from `%ProgramFiles%`. In the guest the
> two halves mount beside each other and `/usr/lib/wsl/lib` becomes the
> read-only overlay of them, because that is the path everything downstream
> expects to find whole.

Every candidate goes through the same procedure:

1. `CreateFileW` with `FILE_FLAG_BACKUP_SEMANTICS`, asking only for attribute
   access, and deliberately **without** `FILE_FLAG_OPEN_REPARSE_POINT`, so a
   junction or symlink resolves to its target rather than to itself.
2. `GetFinalPathNameByHandleW(VOLUME_NAME_DOS)` for the canonical path. This
   closes both vectors the task names at once: `..` collapses, and a reparse
   point leading outside returns its target's path and then fails the root
   check. What is exported afterwards is that canonical path, not the one
   `HostGpuAdapter` reported.
3. `GetFileInformationByHandle`: it must be a directory. A file is never
   exported.
4. The root check is component-wise and case-insensitive, not a string prefix:
   `...\FileRepositoryEvil` passes a string prefix and fails a component-wise
   one. The roots themselves go through the same canonicalizer, so both sides
   of the comparison are normalized the same way.
5. Deduplication by canonical path, case-insensitively, keeping first-seen
   order: two adapters from one vendor usually share a `FileRepository`
   folder, and AppSandbox made the same check by hand with `_wcsicmp`.

A candidate that fails any step is dropped with a log line and the set is built
from the rest.

Time of check to time of use is not fully closable here and the code says so:
between validation and the start, a directory could in principle be swapped,
and holding a handle open prevents deletion but not renaming. The mitigation is
that the exported path is the canonical one, so a junction swapped afterwards
cannot redirect the export, and that both roots live under `System32`, which
takes administrator rights to write to.

`HcsGrantVmAccess` runs only after a candidate has passed all of the above, and
only against the canonical path. The order is fixed -- build, validate, grant,
write the section.

> **Superseded by TASK-98.** This design said a failed grant removes its export
> from the set, reasoning that handing a VM a share it cannot open trades a
> clear line in the host log for an opaque mount failure in the guest. The
> first real host disproved it: every one of these paths is under `System32`,
> whose DACLs belong to TrustedInstaller, so the grant is refused for all of
> them however elevated VMLord is -- and the rule removed the guest's entire GPU
> userspace. A Plan9 share does not need the grant: it is served by the host's
> own Plan9 server rather than opened by the VM's security principal, which is
> what makes it different from the VHDX files a start grants separately. The
> grant is now asked for and its answer logged at debug only.

The grant is injected as a function, the way `VmStartPipeline` injects
`access_granter`, so the module is testable without a live HCS.

## The HCS section

`hcs_config` gains a pair modelled on `apply_network_adapter` and
`remove_network_adapter`:

```rust
pub(crate) fn apply_plan9_shares(document: &str, exports: &GpuExports) -> Result<String, RepositoryError>;
pub(crate) fn remove_plan9_shares(document: &str) -> Result<String, RepositoryError>;
```

The section sits at `/VirtualMachine/Devices` under the key `Plan9`, and each
share is `{"Name", "AccessName", "Path", "Port": 50001, "Flags": 1}`. `Flags:
1` is read-only on the host side; the constant is not published in the SDK, so
a comment records that it comes from observed behaviour and from AppSandbox --
the same treatment the adapter interface class GUID got in #85. Port 50001 is
the one the agent connects to over `AF_VSOCK` on CID 2. Read-only is therefore
stated twice and independently: by the share's flag on the host and by
`MS_RDONLY` at the mount in the guest.

`remove_plan9_shares` exists for the reason `remove_network_adapter` does: a VM
whose GPU was switched off still has the previous start's shares in its stored
`config.json`. A document with no such section is returned byte for byte, so a
start that changes nothing writes nothing.

The export set is computed once per start and written before the compute system
is prepared, which is what makes it immutable for the lifetime of a boot: a
mode change takes a full VM restart, as the epic decided.

## Testing

Validation is separated from Win32 by an injected canonicalizer, so nearly all
of it is ordinary unit-testable code:

* a path outside the root; a reparse point resolving outside it; the
  same-prefix sibling root `FileRepositoryEvil`; a candidate that is not a
  directory;
* deduplication of two adapters onto one folder, including a case difference;
* adapters whose `driver_store` is `None`; an empty result reported as `None`;
* package names containing a comma, a space, a path separator, or `..`;
* uniqueness of share names;
* the manifest projected from a built set, which is where "only a name and a
  role leave the host" is asserted;
* a failing grant leaving every export in place (see the note above).

`hcs_config` tests cover the shape of the section, its removal, and that a
document needing no change comes back byte for byte.

One `#[ignore]`d real-host test, living in `gpu_exports.rs`'s own test module
rather than under `crates/platform/tests/`, because the builder is
`pub(crate)` and this task adds no public API to reach it from an integration
test: build a set from `discover_host_gpu()` and assert that every path exists,
is a directory, lies under `System32`, and that the share names are unique. On
a host without adapters it passes vacuously, because it is not entitled to
assert that GPU-PV exists.

Verified with `cargo check-windows` and `cargo test-windows`.

`ARCHITECTURE.md` gains a "GPU: what is exported to a guest" section: the two
roots, canonicalization as the way traversal and reparse escape are closed, the
role-based manifest instead of paths, and the export set being fixed once per
start and unchanged until the next one.
