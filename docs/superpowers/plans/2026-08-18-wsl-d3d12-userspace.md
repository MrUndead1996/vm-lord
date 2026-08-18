# WSL D3D12 Userspace Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give a Linux guest the Microsoft D3D12 userspace it needs to render, which on this host does not live where VMLord looks for it, and present it at `/usr/lib/wsl/lib` where Mesa and the guest probe both expect it.

**Architecture:** A second export root and a second share role for the WSL package's `lib` directory, mounted at its own guest path. `/usr/lib/wsl/lib` stops being a mount point and becomes the merged view of the two read-only mounts, so that one canonical directory holds both halves of the userspace exactly as a real WSL guest presents it.

**Tech Stack:** Rust 2024, `windows` crate for `SHGetKnownFolderPath`, `prost` for the agent protocol, `libc::mount` in the guest agent.

**Spec:** No separate design document. This was settled while reviewing a real-host failure during TASK-98; the Background section below is the design record, and `docs/superpowers/specs/2026-08-13-gpu-plan9-exports-design.md` is the export design it extends.

## Background: what the real host showed

TASK-98 got GPU-PV working end to end as far as the kernel: the payload staged,
DKMS built `dxgkrnl`, the module loaded, `/dev/dxg` appeared, and the guest
probe moved from `NO_DEVICE` to `DEVICE_ONLY`. It then failed on userspace:

```
failed GPU check Libraries: a renderer would not find
  /usr/lib/wsl/lib/libd3d12.so, /usr/lib/wsl/lib/libdxcore.so
failed GPU check Opengl: eglinfo named no renderer: eglInitialize failed
failed GPU check Vulkan: Vulkan renders on the CPU: llvmpipe
```

The host was inspected directly. `C:\Windows\System32\lxss\lib` holds eighteen
files, every one of them an NVIDIA vendor library that the GPU driver installs
there. The three Microsoft libraries a renderer actually needs are in the WSL
package instead:

```
C:\Program Files\WSL\lib\libd3d12.so
C:\Program Files\WSL\lib\libd3d12core.so
C:\Program Files\WSL\lib\libdxcore.so
```

VMLord exports only `System32\lxss\lib`, and so does the AppSandbox backend
(`gpu_enum.c:488`). This is not a regression from either: it is the old inbox-WSL
layout, where one directory held everything. The Store and standalone WSL split
it in two, and a guest given only the first half has vendor libraries with
nothing to drive them.

### Why the shares cannot simply both mount at the same place

The guest maps a role to exactly one path and refuses a second share that claims
a path already taken (`gpu_targets::plan`, `Refusal::DuplicateTarget`). So a
second source needs a role and a path of its own.

### Why the merged directory cannot be a symlink farm inside the mount

The obvious next thought -- mount the second share elsewhere and symlink its
files into `/usr/lib/wsl/lib` -- does not work: the agent mounts every share
`MS_RDONLY` (`gpu_mounts.rs:249`), so nothing can be created inside one.
`/usr/lib/wsl/lib` therefore has to stop being a mount point and become the
merged view of two mounts that live elsewhere.

### The alternative that was rejected

Leaving `/usr/lib/wsl/lib` as it is and adding the second directory to
`ld.so.conf` alone would make the libraries loadable -- the agent already writes
a line per mounted directory that holds shared objects and runs `ldconfig` --
but it would leave `/usr/lib/wsl/lib` a half-populated directory that neither
the probe nor anything a person runs by hand would find complete. One canonical
directory is worth the extra machinery.

## Global Constraints

- Tracked as Vikunja **#107** under the GPU-PV epic (#11). Every commit subject
  is `TASK-107: comment`.
- The protocol gains an enum value, so `CURRENT_VERSION` goes from `1.5` to
  `1.6`. Major stays `1`: an agent that does not know the new role decodes it as
  `GPU_SHARE_ROLE_UNSPECIFIED` and refuses that one share, which is a session
  that still works with less.
- GPU stays best effort: nothing here may fail a VM start.
- `vmlord-platform` is the only crate that calls Windows APIs; the agent links
  no system C libraries (`cargo agent` must keep working with no C toolchain).
- Test commands: `cargo test-windows -p <crate> <filter>`, `cargo test -p vmlord-agent`
  for the guest crate, `cargo check-windows` for the workspace. Never prefix a
  command with `timeout`.

---

### Task 0: Spike — can overlayfs merge two 9p mounts?

The merged view is the one risky part of this plan, and it is cheap to settle
before anything is built around it. A read-only overlay with two lowerdirs and
no upperdir is the natural fit; whether overlayfs accepts 9p lowerdirs on the
Ubuntu guest kernel is a fact, not an opinion.

**This is a spike. Its output is an answer, and nothing it produces is kept.**

- [ ] **Step 1: Reproduce the two mounts by hand in a running guest**

On a VM built by the current branch, with `/usr/lib/wsl/lib` already mounted:

```sh
sudo mkdir -p /tmp/spike/{a,b,merged}
sudo mount -t 9p -o trans=virtio,version=9p2000.L,ro,aname=vmlord.gpu.wsl-lib \
    vmlord.gpu.wsl-lib /tmp/spike/a
# any second read-only 9p share, or a read-only bind of a plain directory,
# is enough to answer the question
sudo mount --bind -o ro /usr/share /tmp/spike/b
```

- [ ] **Step 2: Try the overlay**

```sh
sudo mount -t overlay overlay -o lowerdir=/tmp/spike/b:/tmp/spike/a /tmp/spike/merged
ls /tmp/spike/merged | head
```

Expected if it works: the union of both directories, and no `upperdir` required.

- [ ] **Step 3: Record the answer and clean up**

```sh
sudo umount /tmp/spike/merged /tmp/spike/a /tmp/spike/b
sudo rm -rf /tmp/spike
```

Write the answer into this plan under Task 4 before starting it:

- **Overlay works** → Task 4 mounts an overlay, and the symlink alternative is
  dropped.
- **Overlay refuses 9p lowerdirs** → Task 4 builds `/usr/lib/wsl/lib` as a real
  directory of symlinks into the two mounts, rebuilt on every attach so that a
  share the manifest dropped loses its links. Everything else in this plan is
  unaffected.

---

### Task 1: A role for the WSL package's libraries

**Files:**
- Modify: `crates/agent-protocol/proto/vmlord/agent/v1/agent.proto` (`GpuShareRole`)
- Modify: `crates/agent-protocol/src/handshake.rs:19` (`CURRENT_VERSION`)
- Modify: `crates/core/src/gpu.rs:359-372` (`GpuShareRole`, share-name constants, `GpuShare`)
- Test: `crates/core/src/gpu.rs` `mod tests`, `crates/agent-protocol/src` handshake tests

**Interfaces:**
- Consumes: nothing.
- Produces:
  - proto `GPU_SHARE_ROLE_WSL_D3D12 = 4`
  - `CURRENT_VERSION = ProtocolVersion { major: 1, minor: 6 }`
  - `vmlord_core::GpuShareRole::WslD3d12`
  - `pub const WSL_D3D12_SHARE: &str = "vmlord.gpu.wsl-d3d12";`
  - `GpuShare::wsl_d3d12() -> GpuShare`

- [ ] **Step 1: Write the failing test**

In `crates/core/src/gpu.rs` `mod tests`:

```rust
    #[test]
    fn the_d3d12_share_is_its_own_role_under_its_own_name() {
        let share = GpuShare::wsl_d3d12();

        assert_eq!(share.role, GpuShareRole::WslD3d12);
        assert_eq!(share.name, WSL_D3D12_SHARE);
        assert_ne!(
            share.name,
            GpuShare::wsl_lib().name,
            "two sources of one userspace are two shares, and a guest tells them \
             apart by name"
        );
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test-windows -p vmlord-core gpu::tests::the_d3d12_share`
Expected: FAIL — `no function or associated item named wsl_d3d12`.

- [ ] **Step 3: Add the role on the wire**

In `agent.proto`, inside `GpuShareRole`:

```proto
  // The Microsoft D3D12 userspace, which the WSL package installs beside
  // itself rather than into System32. On hosts old enough to have kept
  // everything in one place there is no such share and no such directory.
  GPU_SHARE_ROLE_WSL_D3D12 = 4;
```

Bump `CURRENT_VERSION` in `handshake.rs` to `minor: 6`. Leave `major` alone: an
agent that predates this value decodes it as `UNSPECIFIED` and refuses that one
share, which is a session that still mounts everything else.

- [ ] **Step 4: Add the role in the domain**

In `crates/core/src/gpu.rs`, add to `GpuShareRole`:

```rust
    /// The Microsoft D3D12 userspace from the WSL package.
    WslD3d12,
```

beside the existing share-name constants:

```rust
/// The share name the Microsoft D3D12 userspace is offered under.
pub const WSL_D3D12_SHARE: &str = "vmlord.gpu.wsl-d3d12";
```

and beside `wsl_lib`:

```rust
    /// The share for the Microsoft D3D12 userspace.
    ///
    /// Separate from [`Self::wsl_lib`] because the two are separate
    /// directories on every host that installs WSL from the Store: one holds
    /// the vendor's libraries and the other the Microsoft ones, and a guest
    /// needs both to render.
    #[must_use]
    pub fn wsl_d3d12() -> Self {
        Self {
            name: WSL_D3D12_SHARE.to_owned(),
            role: GpuShareRole::WslD3d12,
        }
    }
```

- [ ] **Step 5: Map the role on both sides of the wire**

In `crates/platform/src/agent_session.rs`, `wire_share` gains
`CoreShareRole::WslD3d12 => (GpuShareRole::WslD3d12, String::new()),`. The match
is total, so a missing arm fails to compile, which is where it should fail.

- [ ] **Step 6: Run the tests**

Run: `cargo test-windows -p vmlord-core -p vmlord-agent-protocol -p vmlord-platform`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/agent-protocol crates/core/src/gpu.rs crates/platform/src/agent_session.rs
git commit -m "TASK-107: Give the Microsoft D3D12 userspace a share role"
```

---

### Task 2: Find the WSL package's library directory

**Files:**
- Modify: `crates/platform/Cargo.toml` (add the `Win32_UI_Shell` feature)
- Modify: `crates/platform/src/gpu_exports.rs` (`ExportRoots`, `build_with`)
- Test: `crates/platform/src/gpu_exports.rs` `mod tests`

**Interfaces:**
- Consumes: `GpuShare::wsl_d3d12` (Task 1).
- Produces:
  - `ExportRoots.wsl_d3d12: Option<PathBuf>`
  - `ExportRoots::resolve(system32: &Path, program_files: Option<&Path>, canonicalize: Canonicalize<'_>)`
  - `fn program_files_directory() -> Option<PathBuf>`

- [ ] **Step 1: Write the failing tests**

```rust
    const PROGRAM_FILES: &str = r"C:\Program Files";
    const WSL_LIB_PACKAGE: &str = r"C:\Program Files\WSL\lib";

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
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test-windows -p vmlord-platform gpu_exports`
Expected: FAIL — `resolve` takes two arguments.

- [ ] **Step 3: Add the root**

In `ExportRoots`:

```rust
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
```

`resolve` gains the parameter and resolves the new root with the existing
`resolve_root`, against `program_files` rather than `system32`:

```rust
            wsl_d3d12: program_files.and_then(|program_files| {
                let Ok(program_files) = canonicalize(program_files) else {
                    log::debug!("Program Files could not be resolved; no D3D12 share");
                    return None;
                };
                resolve_root(
                    &program_files,
                    &program_files.join("WSL").join("lib"),
                    canonicalize,
                )
            }),
```

In `build_with`, push the D3D12 share **before** the WSL one, so the mount order
puts the Microsoft libraries first for the same reason the payload leads today:

```rust
    if let Some(wsl_d3d12) = &roots.wsl_d3d12 {
        exports.push(GpuExport {
            share: GpuShare::wsl_d3d12(),
            host_path: wsl_d3d12.clone(),
        });
    }
```

- [ ] **Step 4: Read Program Files natively**

Add `"Win32_UI_Shell"` to the `windows` features in `crates/platform/Cargo.toml`,
and beside `system_directory`:

```rust
/// The host's `Program Files`, as Windows spells it.
///
/// Asked of the shell rather than read from `%ProgramFiles%`: the environment
/// variable is inherited and can be anything, and this decides what gets
/// exported to a VM.
fn program_files_directory() -> Option<PathBuf> {
    // SAFETY: the returned buffer is owned by the caller and freed by
    // `CoTaskMemFree`, which `PWSTR::free` does; the call takes no borrowed
    // memory.
    let path = unsafe {
        SHGetKnownFolderPath(&FOLDERID_ProgramFiles, KF_FLAG_DEFAULT, None)
    }
    .inspect_err(|error| log::debug!("Program Files could not be read: {error}"))
    .ok()?;
    // SAFETY: a successful call returns a NUL-terminated wide string.
    let directory = PathBuf::from(unsafe { path.to_string() }.ok()?);
    // SAFETY: `path` came from the call above and is freed exactly once.
    unsafe { CoTaskMemFree(Some(path.as_ptr().cast())) };
    Some(directory)
}
```

and pass it in `GpuExports::build`:

```rust
        let roots = ExportRoots::resolve(&system32, program_files_directory().as_deref(), &canonicalize);
```

- [ ] **Step 5: Run the tests**

Run: `cargo test-windows -p vmlord-platform gpu_exports` then `cargo check-windows`
Expected: PASS, no warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/platform/Cargo.toml crates/platform/src/gpu_exports.rs
git commit -m "TASK-107: Export the WSL package's D3D12 libraries"
```

---

### Task 3: Report the D3D12 userspace in host capabilities

The create form warns that "the Linux GPU userspace is not installed on this
host". That verdict currently comes from `System32\lxss\lib` alone, which on
this host exists and is the wrong half.

**Files:**
- Modify: `crates/platform/src/gpu_discovery.rs:35-51` (`linux_payload_present`, `assemble`)
- Test: `crates/platform/src/gpu_discovery.rs` `mod tests`

**Interfaces:**
- Consumes: `program_files_directory` (Task 2).
- Produces: `GpuAvailability::linux_payload` that answers for both directories.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn a_host_with_only_the_vendor_libraries_has_no_usable_linux_payload() {
        // What the first real host had: System32\lxss\lib full of NVIDIA
        // libraries and no libd3d12.so anywhere a guest could reach.
        let capabilities = assemble(vec![adapter()], Ok(()), LinuxPayload {
            wsl_lib: true,
            wsl_d3d12: false,
        });

        assert!(!capabilities.linux_payload.is_available());
        let failure = capabilities.linux_payload.failure().expect("a reason");
        assert_eq!(failure.code, GpuStatusCode::HostLinuxPayloadMissing);
    }

    #[test]
    fn a_host_with_both_halves_has_a_usable_linux_payload() {
        let capabilities = assemble(vec![adapter()], Ok(()), LinuxPayload {
            wsl_lib: true,
            wsl_d3d12: true,
        });

        assert!(capabilities.linux_payload.is_available());
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test-windows -p vmlord-platform gpu_discovery`
Expected: FAIL — `assemble` takes a `bool`.

- [ ] **Step 3: Answer for both directories**

Replace the `payload_present: bool` parameter of `assemble` with:

```rust
/// Which halves of the Linux GPU userspace this host has.
///
/// Two fields because they are two directories on every host that installs WSL
/// from the Store: the vendor's libraries under `System32\lxss\lib`, and the
/// Microsoft ones beside the WSL package. A guest needs both, and a host with
/// one of them is a host that cannot render -- which is exactly what the first
/// real host looked like.
pub(crate) struct LinuxPayload {
    pub(crate) wsl_lib: bool,
    pub(crate) wsl_d3d12: bool,
}
```

`linux_payload_present` returns one, checking `System32\lxss\lib` as it does
today and `program_files_directory()?.join("WSL").join("lib")` for the other.
`assemble` reports `Available` only when both are set, and names the missing
half in the failure message.

- [ ] **Step 4: Run the tests**

Run: `cargo test-windows -p vmlord-platform gpu_discovery`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/platform/src/gpu_discovery.rs
git commit -m "TASK-107: Report both halves of the Linux GPU userspace"
```

---

### Task 4: One merged `/usr/lib/wsl/lib` in the guest

**Write the Task 0 answer at the top of this task before starting it.**

**Files:**
- Modify: `crates/agent/src/gpu_targets.rs` (paths and the role mapping)
- Modify: `crates/agent/src/gpu_mounts.rs` (the merged view, after the mounts)
- Test: `crates/agent/src/gpu_targets.rs` and `crates/agent/src/gpu_mounts.rs` `mod tests`

**Interfaces:**
- Consumes: `GpuShareRole::WslD3d12` on the wire (Task 1).
- Produces:
  - `pub const WSL_HOST_LIB: &str = "/usr/lib/wsl/host-lib";`
  - `pub const WSL_D3D12: &str = "/usr/lib/wsl/d3d12";`
  - `WSL_LIB` keeps its value `/usr/lib/wsl/lib` and stops being a mount target
  - `fn merge_wsl_lib(sources: &[PathBuf]) -> Result<(), String>`

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn the_two_userspace_shares_mount_beside_each_other_and_not_on_the_merged_view() {
        let planned = plan(&[
            share("vmlord.gpu.wsl-lib", GpuShareRole::WslLib),
            share("vmlord.gpu.wsl-d3d12", GpuShareRole::WslD3d12),
        ]);

        let paths: Vec<_> = planned
            .iter()
            .filter_map(|planned| match planned {
                Planned::Mount { path, .. } => Some(path.to_string_lossy().into_owned()),
                Planned::Refused { .. } => None,
            })
            .collect();

        assert_eq!(paths, vec![WSL_HOST_LIB.to_owned(), WSL_D3D12.to_owned()]);
        assert!(
            !paths.iter().any(|path| path == WSL_LIB),
            "the merged view is built over the mounts, never mounted on"
        );
    }

    #[test]
    fn a_role_this_build_does_not_know_is_refused_rather_than_mounted() {
        let planned = plan(&[share("vmlord.gpu.mystery", GpuShareRole::Unspecified)]);

        assert!(matches!(planned.as_slice(), [Planned::Refused { .. }]));
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p vmlord-agent gpu_targets`
Expected: FAIL — `WSL_HOST_LIB` is not defined.

- [ ] **Step 3: Move the mount targets and add the new one**

In `gpu_targets.rs`, `WSL_LIB` keeps its path but is no longer a target;
`WslLib` maps to `WSL_HOST_LIB` and `WslD3d12` to `WSL_D3D12`. The allowlist
comment gains the reason: `/usr/lib/wsl/lib` is composed from these two and must
not be something a manifest can mount over.

- [ ] **Step 4: Build the merged view**

In `gpu_mounts.rs`, after the mounts and before `refresh_libraries`, compose
`/usr/lib/wsl/lib` from whichever of the two mounts are present.

**If Task 0 said overlay works:**

```rust
/// Presents both halves of the WSL userspace as one directory.
///
/// A read-only overlay with no upper layer: the sources are read-only 9p
/// mounts, nothing writes to the result, and the kernel does the merging that
/// a farm of symlinks would otherwise have to maintain by hand. Remounted
/// rather than repaired, which is what makes a second attach idempotent.
fn merge_wsl_lib(sources: &[PathBuf]) -> Result<(), String> {
    // ... umount(WSL_LIB) if mounted, mkdir -p, then:
    // mount("overlay", WSL_LIB, "overlay", MS_RDONLY, "lowerdir=<d3d12>:<host-lib>")
}
```

The lower layers are ordered with the Microsoft libraries first, so that a name
present in both resolves to the one a renderer links against.

**If Task 0 said overlay refuses 9p:** build `/usr/lib/wsl/lib` as a plain
directory and fill it with symlinks to every entry of both mounts, removing the
links this agent wrote before rebuilding — the same "rewritten from the current
set" rule `refresh_libraries` already follows, and for the same reason.

Either way `refresh_libraries` is then given `/usr/lib/wsl/lib` rather than the
two mounts, so the linker learns one directory.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p vmlord-agent` then `cargo agent`
Expected: PASS, and the agent still cross-compiles for musl with no C toolchain.

- [ ] **Step 6: Commit**

```bash
git add crates/agent/src
git commit -m "TASK-107: Present one WSL userspace directory to the guest"
```

---

### Task 5: Verify on the host, then record it

**Files:**
- Modify: `ARCHITECTURE.md` (the "what is exported to a guest" section)
- Modify: `docs/superpowers/specs/2026-08-13-gpu-plan9-exports-design.md` (a superseding note, as TASK-98 added for the grant rule)

- [ ] **Step 1: Build a release and run it**

```bash
cargo dist --gpu-payload <the packed payload directory>
```

Create a VM with `Default`, and read the log. Expected, in order: the payload
staged, the GPU attached, the recipe reaching `Userspace` and `VulkanIcd`
without skipping, and the probe answering `RENDERS` rather than `DEVICE_ONLY`.

- [ ] **Step 2: If the probe still says `DEVICE_ONLY`**

Do not guess. The probe reports every check with the guest's own words, and the
`Libraries` check names the files it could not find. Take that message back to
Phase 1 of `superpowers:systematic-debugging` — the host directories are
inspectable directly from WSL under `/mnt/c`, which is how the split was found
in the first place.

- [ ] **Step 3: Record what is true**

In `ARCHITECTURE.md`, in the export section: two host sources, one guest
directory, and why the merge exists rather than a second `ld.so.conf` line. In
the TASK-88 spec, a superseding note that the WSL userspace is not one directory
on hosts that install WSL from the Store.

- [ ] **Step 4: Commit**

```bash
git add ARCHITECTURE.md docs/superpowers/specs
git commit -m "TASK-107: Record the two sources of the WSL userspace"
```

---

## Done when

- `cargo test-windows` passes across the workspace and `cargo agent` builds.
- A VM created with `Default` on this host reports `RENDERS`, with a hardware
  renderer named in the probe rather than `llvmpipe`.
- The create form's warning distinguishes a host missing either half of the
  userspace from one that has both.

## Out of scope

- Hosts whose WSL is neither inbox nor installed under Program Files. If one
  turns up, it is another source and another root, not a redesign.
- Bundled Mesa (`MesaPolicy::Bundled`); the payload in use is `distro`.
- The placeholder `archive_url` in `payload.spec.json`, which is TASK-98's note
  and belongs to whoever publishes a payload for real.
