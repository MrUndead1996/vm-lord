# GPU-PV guest payload design

## Purpose

Task 93 provides the portable boundary between artifacts prepared on the host
and GPU recipes executed by the Linux guest. It owns versioned manifests,
host-side download and preparation, SHA-256 verification, cache readiness, and
provenance metadata.

The guest never needs Internet access for GPU provisioning. A cold host cache
may require the host to download a payload, but a download failure only makes
GPU support unavailable; it never prevents the VM from starting.

This design deliberately does not reuse AppSandbox source archives,
configuration, or binaries. AppSandbox is evidence for required behavior, not
a distribution channel or implementation dependency.

## Decisions

* GPU provisioning is offline from the guest's point of view, including its
  first installation of DKMS, kernel headers, and Mesa dependencies.
* Release automation prepares immutable VMLord GPU payload archives. The
  application does not resolve mutable upstream branches or transform an
  arbitrary source repository on an end user's machine.
* An embedded, schema-versioned catalog pins each archive URL, byte length,
  SHA-256, supported guest target, source revisions, and license metadata.
* The host cache is content-addressed and follows the same verify-on-every-hit,
  lock, partial-file, and atomic-publication rules as the image cache.
* A dedicated, read-only Plan9 share delivers a ready payload to the guest.
  DriverStore and `System32\\lxss\\lib` remain separate shares with their
  existing allowlists.
* Task 93 transports immutable inputs. Tasks 95 and 96 own idempotent guest
  installation and configuration.
* Complete GPU failure is `GpuState::Failed`. `GpuState::Degraded` is reserved
  for a path that renders successfully with less capability than requested.

## Required GPU stack

Linux mainline contains the general Hyper-V stack and `hyperv_drm`, but not
the WSL GPU-PV `dxgkrnl` interface. `hyperv_drm` is a synthetic display driver
and is not a replacement for `/dev/dxg`.

The payload therefore contains:

1. A minimal `dxgkrnl` DKMS source tree derived from an exact commit of
   `microsoft/WSL2-Linux-Kernel`.
2. VMLord-owned out-of-tree build and compatibility files, including
   `Kbuild`, `dkms.conf`, and the compatibility definitions needed by the
   supported Ubuntu kernel.
3. `include/uapi/misc/d3dkmthk.h`, which is outside the upstream
   `drivers/hv/dxgkrnl` directory but is required by the driver.
4. A local APT repository with the exact dependency closure needed by the
   target: DKMS, build tools, the target kernel's headers, module utilities,
   and the selected Mesa runtime.
5. A target-specific upstream Mesa build when the distribution packages do
   not provide working D3D12 Gallium and dzn Vulkan paths.

The proprietary WSL D3D12 and DXCore libraries are never copied into the
payload. They remain host-coupled files exposed read-only from
`System32\\lxss\\lib`. Readiness checks require the actual libraries used by
Mesa, including `libd3d12.so`, `libd3d12core.so`, and `libdxcore.so`; directory
existence alone is insufficient.

Diagnostics such as `mesa-utils` and `vulkan-tools` are not runtime
dependencies. They may be included in a developer payload, but the production
probe in task 97 must not require them.

## Supported targets

The first payload family supports Ubuntu 26.04 on `amd64`. Support is narrower
than VMLord's general Ubuntu image list: Ubuntu 24.04 and 22.04 VMs continue to
work, but GPU provisioning reports an unsupported guest target until a tested
payload is published for them.

A target is an exact tuple:

* distribution and release;
* architecture;
* kernel release and flavor;
* payload ABI version.

The catalog contains only tuples for which CI has compiled the DKMS module and
run the guest probe. A release-name match does not silently accept a different
kernel. Kernel upgrades remain best effort: DKMS may rebuild when compatible
headers are already present, but a kernel outside the catalog is not claimed
as supported.

## Release-time preparation

Release automation, implemented in Rust under `xtask`, builds one deterministic
archive for every supported target. Its inputs are pinned source commits and
VMLord-owned overlay files. It must not consume a mutable branch as an artifact
identity.

The archive contains this logical tree:

```text
payload.json
sources.json
licenses/
content/dxgkrnl/
content/apt/dists/
content/apt/pool/
content/mesa/                 # present only when distro Mesa is insufficient
```

Files are sorted, timestamps and ownership are normalized, and the same inputs
must produce the same archive SHA-256. The archive format must have a pure-Rust
reader available to the Windows application; it must not invoke `tar.exe`,
PowerShell, WMI, WSL, or another external process.

The local APT repository is a curated, tested dependency closure, not the
result of porting AppSandbox's dependency resolver. Its package metadata and
all referenced `.deb` files are part of the archive and covered by the archive
digest. Guest recipes configure APT to use only this repository while GPU
provisioning runs.

Mesa policy is explicit per target:

* `distro` means CI proved that packages in the local APT repository provide
  hardware D3D12 Gallium and dzn Vulkan paths;
* `bundled` means the archive carries a VMLord-built, unpatched upstream Mesa
  installation for that target.

Absence of dzn is not silently accepted as a complete task 96 result. A future
capability policy may make Vulkan optional, but this MVP requires both D3D12
Gallium and dzn.

## Catalog and payload manifests

The application embeds a catalog with a monotonically versioned schema. Each
entry records:

* stable payload ID and payload ABI version;
* exact guest target tuple;
* immutable HTTPS archive URL, byte length, archive SHA-256, and
  `payload.json` SHA-256;
* required renderer capabilities;
* Mesa policy;
* upstream repository URLs and exact commit IDs;
* VMLord source revision and builder version;
* SPDX license expressions and paths to included license texts.

Unknown schema versions, malformed digests, duplicate target tuples, mutable
source references, empty provenance fields, and non-HTTPS production URLs are
rejected before network or filesystem I/O. Tests may use an explicitly injected
local source.

`payload.json` inside the archive repeats its identity and contains a sorted
list of every other archive file with its path, size, and SHA-256. The catalog
pins the digest of `payload.json`, which avoids a self-referential manifest.
The host verifies this list after safe extraction. The guest can use the same
manifest without understanding the host catalog.

`sources.json` inside the archive distinguishes upstream material from VMLord
additions. It records:

* upstream URL, commit, source archive digest, and selected paths;
* VMLord source revision, builder version, and overlay file digests;
* target tuple and Mesa policy;
* SPDX expressions and license text paths.

After verification, the host generates `provenance.json` beside the cached
archive. It combines the validated catalog entry with `sources.json` and
records the archive and `payload.json` digests. It is cache metadata, not an
input to the archive digest, and can be regenerated deterministically.

Upstream `dxgkrnl` sources are `GPL-2.0`; `d3dkmthk.h` is
`GPL-2.0 WITH Linux-syscall-note`. VMLord overlay files record their own
licenses instead of being attributed to Microsoft. Build time may be recorded
outside deterministic content, but it must not affect the archive or prepared
manifest digest.

Licensing review does not block the MVP, but missing source, version, digest,
license expression, or license text does block publication of an artifact.

## Host cache

The portable payload component accepts a catalog entry and cache root and
returns either `ReadyGpuPayload` or a structured preparation failure. It has no
Windows API dependency and belongs outside `core` and `platform`, following the
existing `vmlord-image` boundary.

The cache layout is content-addressed:

```text
<cache>/gpu-payload/v1/<archive-sha256>/
    archive
    provenance.json
    files/
        payload.json
        sources.json
        ...
```

Preparation follows these rules:

1. Acquire a cross-process lock for the archive digest.
2. Rehash an existing archive and all prepared files on every cache hit.
3. Download into a unique partial file with bounded size and timeouts.
4. Reject a length or archive SHA-256 mismatch before extraction.
5. Extract into a unique temporary directory while rejecting absolute paths,
   `..`, symlinks, hard links, device entries, duplicate paths, and declared or
   expanded sizes above configured bounds.
6. Verify every file against `payload.json` and reject undeclared files.
7. Flush completed files and atomically rename the directory into place.
8. If another process won the race, discard the temporary directory and verify
   the winner before returning it.

Directory existence after the atomic rename is the readiness marker; a separate
`READY` file is unnecessary. Interrupted partial files and temporary
directories are safe to remove on the next preparation. A corrupt entry may be
quarantined and recreated with the same expected bytes; immutability does not
forbid repair.

Only `ReadyGpuPayload` exposes a path to lifecycle and platform code. Callers
cannot construct it from an unchecked directory.

## Delivery to the guest

The host cannot select a kernel-specific payload reliably from the Ubuntu
release name alone. The agent's capability report supplies the exact kernel
release and architecture. The host then selects the matching catalog entry and
prepares it; the guest itself performs no download.

To allow preparation after the VM has booted, every GPU-enabled VM receives a
dedicated, initially empty payload staging directory before HCS prepares the
compute system. That exact directory is exported as a read-only Plan9 share.
It is not the whole application cache.

When a payload becomes ready, task 93 materializes a temporary generation below
the staging directory with hard links where the cache and staging directory
share a volume, and verified copies otherwise. It verifies the generation,
atomically renames it to its final name, and publishes `current.json` last by a
second atomic rename. The guest may observe an ignored temporary name, but it
can select only the previous complete generation or the new one. Old
generations can be removed only after they are no longer current and no
provisioning request refers to them.

The logical share role is `GpuPayload`; the guest maps it through its own
allowlist to `/run/vmlord/gpu-payload`. The manifest contains a payload ID and
generation, never a host path. Host-side validation permits only VMLord-created
per-VM staging directories below the configured GPU staging root, canonicalizes
the exact directory before `HcsGrantVmAccess`, and exports nothing reached
through a reparse point.

The Plan9 share is read-only in both the HCS configuration and the guest mount.
The host may publish a generation after the mount is established, but the guest
does not act until the application sends an install request naming the
published generation.

## Runtime flow

1. A GPU-enabled VM starts even when no payload is cached.
2. Task 94 mounts the empty `GpuPayload`, WSL library, and DriverStore shares
   read-only and reconciles stale mounts.
3. The agent reports distribution, release, architecture, and exact kernel.
4. The application selects the exact catalog entry. No match produces a
   non-fatal unsupported-target GPU failure.
5. Task 93 verifies or downloads the payload, stages one complete generation,
   and returns its logical ID.
6. Task 95 uses only the local APT repository and `dxgkrnl` source from that
   generation, builds through DKMS, loads the module, and checks `/dev/dxg`.
7. Task 96 selects distro or bundled Mesa according to `payload.json`, writes
   only GPU loader configuration, and runs the task 97 probe.
8. The agent reports facts; the application derives `GuestReady`, `Degraded`,
   or `Failed` using the existing status model.

The host may retry a failed cold-cache download on a later start or explicit
GPU retry. Guest installation is idempotent and keyed by payload ID, so a
reconnect or repeated request does not reinstall an already active generation.

## Failure semantics

No payload failure aborts VM creation or start.

* No matching catalog target, download failure, digest mismatch, extraction
  failure, DKMS failure, missing `/dev/dxg`, or absence of every hardware
  renderer produces GPU `Failed` with a stable reason code.
* `Degraded` means hardware rendering works but fewer adapters or capabilities
  are available than requested.
* A cold cache being prepared is a progress condition, not `Degraded`.
* Corruption is logged with payload ID and expected digest, never with a URL
  containing credentials.

Payload selection starts after the guest reports its kernel, so a preparation
failure uses the existing `GpuStage::Guest`. A new `GpuStage` is unnecessary
unless the UI later needs to distinguish download progress from other guest
bring-up work.

## Task boundaries

* Task 91 supplies only `vmlord-agent` through the tools ISO. GPU payloads do
  not enlarge or rebuild that ISO.
* Task 92 reports the guest target needed for catalog selection and supports
  reconnect-safe requests.
* Task 93 owns release artifact format, catalog validation, download, cache,
  provenance verification, per-VM staging, and the `GpuPayload` share contract.
* Task 94 owns Plan9 transport, read-only mount reconciliation, allowlisted
  guest mount points, remount, and cleanup.
* Task 95 owns compatibility checks, local APT use, DKMS installation, module
  loading, and `/dev/dxg` validation.
* Task 96 owns Mesa selection, loader and ICD configuration, and removal of
  stale GPU-only configuration.
* Task 97 owns vendor-neutral hardware probes and reports facts rather than
  choosing UI states.
* Task 99 owns end-to-end coverage and user documentation.

## Out of scope

* AppSandbox archives, agent binaries, `asb_drm`, audio, input, clipboard,
  GNOME customization, and AppSandbox's prebuilt Mesa.
* Runtime resolution of mutable Git branches.
* A general Ubuntu dependency solver in the Windows application.
* `yum`/`dnf` payloads or non-Ubuntu guest recipes.
* Redistribution of proprietary WSL libraries.
* Prebuilt `dxgkrnl.ko` files; DKMS builds against the guest kernel.
* Artifact signatures and remote catalog updates. HTTPS plus an embedded
  digest is the MVP trust root; signing can reinforce it later without changing
  the payload identity.

## Acceptance criteria

* Catalog parsing rejects every unsupported schema or incomplete provenance
  record before I/O.
* Release automation produces the same digest twice from identical inputs and
  a different digest when any source or overlay file changes.
* At least one exact Ubuntu 26.04 `amd64` kernel tuple is compile-tested and
  probe-tested in CI before its catalog entry is shipped.
* A fresh supported VM provisions GPU successfully with guest networking
  disabled and a cold host cache.
* A warm cache permits the same flow with both host and guest networking
  disabled.
* Concurrent preparation publishes one verified cache entry and leaves no
  selectable partial generation.
* Truncated downloads, archive traversal, undeclared files, digest mismatch,
  and corrupt cache hits are rejected and covered by tests.
* Switching an existing VM from `None` to a GPU mode delivers the payload
  without recreating its system disk or tools ISO.
* Unsupported releases and kernels leave the VM running and report a stable
  GPU failure.
* D3D12 and dzn probes cannot pass through llvmpipe or another software
  renderer.
* The payload and provenance contain no AppSandbox source, binary, package, or
  configuration artifact.
* The guest agent remains statically linked and gains no dependency on a system
  C library.

## References

* [Microsoft WSL2 Linux kernel `dxgkrnl`](https://github.com/microsoft/WSL2-Linux-Kernel/tree/linux-msft-wsl-6.18.y/drivers/hv/dxgkrnl)
* [Microsoft WSL2 Linux kernel `d3dkmthk.h`](https://github.com/microsoft/WSL2-Linux-Kernel/blob/linux-msft-wsl-6.18.y/include/uapi/misc/d3dkmthk.h)
* [Mesa D3D12 driver](https://docs.mesa3d.org/drivers/d3d12.html)
* [Linux kernel license rules](https://www.kernel.org/doc/html/next/process/license-rules.html)
