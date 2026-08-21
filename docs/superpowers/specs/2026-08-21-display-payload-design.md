# Versioned display payload design

## Purpose

Task #113 gives the display stack its own guest-side artifact: a versioned,
content-addressed archive holding everything a guest needs for VMLord's display
that its own apt cannot provide, and the machinery that puts it there, keeps it
there across kernel upgrades, updates it on request and takes one step back
when an update does not work.

The mechanism is the GPU payload's, reused rather than copied. The lifecycle is
not: a display failure is a display failure, and nothing here changes what GPU
is doing for a VM or reads its status.

## Decisions

* The display payload carries the **whole guest side of display**: the DKMS
  sources of VMLord's own DRM module now, and the guest display services of
  task #115 later. One artifact, one version, one compatibility check.
* Its version is a **semver of its own**, unrelated to VMLord's version, and
  each entry declares the **range of display protocol revisions** its services
  speak. The host will not select an entry whose range does not cover the
  revision this build implements.
* The catalog, cache, archive expansion, staging and release layout are
  **extracted into `vmlord-payload`** and shared with `vmlord-gpu-payload`.
  Payload *selection*, the entry document and the manifest cross-check stay in
  each payload's own crate.
* VMLord ships **its own DRM module**, `vmlord_drm`. AppSandbox's `asb_drm` is
  the evidence of task #111 for what shape works, not a source to port.
* **Installation is automatic and idempotent; version updates never are.** A
  start installs what is missing and rebuilds what a kernel upgrade broke. A
  newer payload in the release becomes an offer, and moving to it takes an
  explicit action.
* An update ends in **health verification**, and a failed verification **rolls
  back one version**. A successful rollback is a working display on the previous
  version, not a degraded one.
* Every failure of the display payload is `Degraded` display with a stage and a
  code. None of them fails a VM start, the agent session, or GPU.
* `kernel_release` is recorded as **`proven_on`**, never as a selector. DKMS
  builds against the running kernel's headers, and Ubuntu upgrades kernels
  unattended.
* A **signed manifest** is prepared for and not implemented here. See
  *Deliberately not in this task*.

## What the payload is for

Task #111 settled that neither `simpledrm`, `hyperv_drm` nor `vkms` can be the
output device of a VMLord desktop, and that VMLord therefore ships a minimal DRM
module of its own, delivered as DKMS. A kernel module that must be built against
the guest's running kernel is not something cloud-init installs from an archive:
it is an artifact VMLord produces, versions, verifies and updates. That is this
task.

Task #115 adds the guest display services -- a privileged DRM/uinput broker and
an unprivileged capture/encode process -- which are VMLord binaries speaking the
protocol of task #118. They travel the same way, in the same archive, under the
same version. Giving them a delivery channel of their own would mean two
versioned artifacts per guest whose compatibility with each other nothing states.

## Crate layout

### `vmlord-payload`

Everything that is true of any payload:

* `Sha256Digest` -- the hex-encoded digest type and its parsing.
* `archive` -- ZIP expansion under an expanded-size limit, a file-count limit
  and path rules that reject absolute paths, `..`, and anything that would leave
  the destination.
* `cache` -- the content-addressed host cache: a lock per payload, a partial
  file, atomic publication, and rehashing on **every** hit rather than trust in
  a directory's name.
* `staging` -- `generations/<digest>` beside `ready/<digest>.json`, and the
  canonicalization that proves a staged generation is strictly inside the VM's
  own staging root.
* `release` -- the layout of a shipped payload: `<payload_id>.json` and
  `<payload_id>.zip` in a named subdirectory of the executable's directory. The
  subdirectory is a parameter, not a constant: `gpu-payload` for one caller and
  `display-payload` for the other.
* `catalog` -- reading that directory into entries, generic over the entry type.

The catalog is generic over a trait:

```rust
pub trait PayloadEntry: Sized {
    fn from_json(document: &str) -> Result<Self, PayloadError>;
    fn payload_id(&self) -> &str;
    fn archive_sha256(&self) -> &Sha256Digest;
    fn payload_manifest_sha256(&self) -> &Sha256Digest;
    fn expanded_size_limit(&self) -> u64;
    fn file_count_limit(&self) -> u64;
}
```

That is enough for the shared half to enforce what is true of every release: a
file must be named for the `payload_id` it contains, its archive must be beside
it, an unreadable or invalid document fails the whole catalog, and an archive no
entry claims is ignored.

Selection is deliberately **not** in the trait. GPU selects on a triple and
prefers the newest proven kernel; display selects on a triple, a version and a
protocol range. One function serving both would be one function with two modes.

### `vmlord-gpu-payload` after the extraction

Its own entry document (`mesa_policy`, `required_renderers`, `sources`,
`licenses`), its own manifest cross-check, its own selection, and re-exports of
the shared types under the names `platform::gpu_staging` and
`platform::gpu_exports` already use. `ReadyGpuPayload` and `StagedGpuPayload`
become aliases of the shared types.

**The extraction changes no GPU behavior, and the existing GPU tests are the
proof.** They are moved only where the code they cover moved.

### `vmlord-display-payload`

The display entry document, the display manifest, and selection. Portable: no
network, no Windows, no catalog compiled in.

## The catalog entry

One entry describes one version of the payload for one guest:

```json
{
  "schema_version": 1,
  "payload_id": "display-ubuntu-24.04-amd64-0.1.0",
  "version": "0.1.0",
  "target": {
    "distribution": "ubuntu",
    "release": "24.04",
    "architecture": "amd64",
    "payload_abi": 1
  },
  "proven_on": "6.8.0-137-generic",
  "protocol": { "major": 1, "min_minor": 0, "max_minor": 0 },
  "archive_sha256": "...",
  "payload_manifest_sha256": "...",
  "expanded_size_limit": 33554432,
  "file_count_limit": 512,
  "sources": [ { "url": "...", "commit": "...", "version": "..." } ],
  "licenses": [ { "spdx": "GPL-2.0", "path": "licenses/GPL-2.0.txt" } ]
}
```

`version` is the payload's own semver and is what the DKMS package is versioned
by. `proven_on` records the kernel a build was proven against; it is never
matched on. `protocol` is the range of display protocol revisions the payload's
services speak -- a major and a closed range of minors, mirroring how
`vmlord-display-protocol` negotiates: a differing major cannot be negotiated at
all, and minors negotiate down.

The three supported releases are three entries and three archives sharing one
`version`: the same module and services, built for 22.04, 24.04 and 26.04 amd64.

### Selection

1. Filter by distribution, release and architecture -- the hard gate, decided
   before the guest boots and therefore without a kernel.
2. Of what remains, keep entries whose `protocol` range covers
   `vmlord_display_protocol::CURRENT_VERSION`.
3. Take the greatest `version`.

No candidate is `NoPayloadForGuest`: the display goes `Degraded` with that cause
and the VM starts. An entry outside the protocol range is **not** a broken
release -- it is a payload built for a VMLord this is not, and a release may
legitimately carry one.

### Verification

Twice, on both sides of the share, because a 9p export is a filesystem the host
can rewrite between them:

* **The host, before exporting:** the archive against `archive_sha256`, the
  expansion limits, and `payload.json`/`sources.json` against the entry.
* **The guest, before applying:** `payload.json` against
  `payload_manifest_sha256` and every file in the tree against the digest
  `payload.json` records, before anything is copied to `/usr/src`.

## Inside the archive

```
payload.json          the manifest: version, target, protocol range, per-file digests
sources.json          provenance
licenses/
content/drm/          dkms.conf, Kbuild, the module sources, the compat header,
                      modprobe.d and the unit that unbinds simpledrm
content/services/     empty in this task; task #115 fills it
```

`content/services/` exists from the first version, empty. Adding it later would
mean a second manifest schema for the sake of a directory.

### The module

`vmlord_drm`, packaged for DKMS as `vmlord-display`, versioned as the payload
is -- so `/usr/src/vmlord-display-0.1.0`, and `dkms status` naming versions is
what makes a one-step rollback free.

What it is in this task: a platform device under its own name, one CRTC, one
connector with a synthesized EDID, a primary plane, GEM shmem buffers, atomic
modesetting, PRIME export, XRGB8888/ARGB8888 with `DRM_FORMAT_MOD_LINEAR` only,
and no `DRIVER_CURSOR_HOTSPOT`. It carries the four kernel version guards task
#111 measured: `remove`/`remove_new`, `hrtimer_setup`, `DRM_PLANE_NO_SCALING`
and `.date` below 6.14.

Two constraints from #111 that are not stylistic. The device must not be named
after `vkms` nor registered on the faux bus, or `61-mutter.rules` tags it
`mutter-device-ignore` on `ID_PATH` and no compositor will bind it. And
`simpledrm` is builtin, so it cannot be blacklisted -- it is unbound from
`simple-framebuffer` by the unit the payload ships.

A cursor plane, the mode list up to 2560x1440, hrtimer vblank and the behavior
of a failed output belong to task #114. Without a cursor plane mutter draws the
pointer into the primary plane, which is a working desktop, not a broken one.

## The host side

* `layout::display_payload_staging_directory(vm_directory)` -- the VM's
  `display-payload` child, beside `gpu-payload`. The cache root is shared:
  content-addressed storage has nothing to separate.
* `platform::display_staging::stage_for_vm` -- select, prepare the generation in
  the cache, stage it into that child. What is exported is the **generation**,
  never the staging root, which also holds ready markers and locks.
* The share is its own: `vmlord.display.payload`, mounted by the guest at
  `/opt/vmlord/display-payload`.
* The agent schema gains `AttachDisplayPayloadRequest`/`Response` and grows by a
  minor revision. It is **not** a new role inside `AttachGpuShares`: that would
  make a GPU attach failure a display failure, which is the lifecycle merge this
  task exists to avoid.

Staging runs on the start of a VM whose profile is `DesktopProfile::Gnome`,
before the agent is asked for the recipe. Every failure -- no entry for the
guest, a digest mismatch, no space -- is a `Degraded` display with a structured
cause and never a failed start.

## The guest side

### The recipe

Stages, in order, reported as a list and never as a verdict -- the same shape
and for the same reason as the GPU recipe: "the module built and no device
appeared" and "the headers would not install" are one word apart in a summary
and are different problems.

| Stage | What it does |
| --- | --- |
| `DISTRIBUTION` | What the guest is; a guest with no recipe is skipped with the reason |
| `PAYLOAD` | The mount, `payload.json`, and every file's digest -- before anything is copied |
| `BUILD_DEPENDENCIES` | `dkms`, `build-essential`, `linux-headers-$(uname -r)` from the guest's own apt |
| `MODULE_SOURCE` | `content/drm` copied to `/usr/src/vmlord-display-<version>`, because 9p is read-only and DKMS writes beside its sources |
| `MODULE_BUILD` | `dkms add`/`build`/`install`, idempotent against what is already registered |
| `MODULE_LOAD` | `modules-load.d`, the `simpledrm` unbind unit, `modprobe` |
| `DEVICE` | A `/dev/dri/card*` whose driver names itself `vmlord_drm` |
| `SERVICES` | Installs `content/services`; skipped with a reason while it is empty |
| `SERVICES_START` | Brings them up; skipped for the same reason |

Idempotence is by fact, not by a flag: the installed version equal to the
payload's, the module loaded and the device openable short-circuits the three
build stages, so every start after the first costs a few checks and needs no
network.

A kernel upgrade is handled in the same place. DKMS's `AUTOINSTALL=yes` rebuilds
without VMLord involved; when it did not, the recipe finds no loaded module and
runs the build stages again. A build that fails on the new kernel is a
`Degraded` display naming exactly that, with a VM that runs and is reachable
over SSH.

Every external program -- `apt-get`, `dkms`, `modprobe` -- runs through the
agent's existing budgeted helper in a process group of its own, as the GPU
recipe's do.

### What the guest reports

The installed version, the previous version DKMS still holds, and the version of
the loaded module, as facts on `VmDisplayFacts`. None of it is stored: an update
needs a running VM, so a stopped one has no question to answer.

## Update and rollback

An update is an explicit action on a running VM. The host compares the best
catalog entry with what the guest reported and offers it; nothing upgrades on a
start.

Progress splits at the natural boundary. The host half -- selection, download,
digest verification, staging -- reports through the existing `PayloadProgress`.
The guest half is one `UpdateDisplayPayload` request naming the target version,
answered by a stage report of the same shape as the recipe's. The guest half is
not streamed: that would be a second conversation on one socket for the sake of
an indicator, and a DKMS build is one long stage regardless.

**Health verification** ends the update: the module is loaded, its version is
the target, the device opens, and the services answer on the control channel
with a revision inside the entry's range. While `content/services` is empty that
last check is reported as skipped, visibly.

**Rollback** is one version and is triggered by a failed verification. The
previous `/usr/src` tree is untouched until verification succeeds and DKMS holds
both versions, so a rollback unloads the new module, loads the previous one,
removes the new DKMS version and brings the services back.

A successful rollback is **not** `Degraded`. The display works, on the previous
version, and the status says exactly that -- "the update failed; version X is
running" -- with the cause. `Degraded` is for an update that neither installed
the new version nor restored the old one.

Rollback is one step deep. Keeping two would be a version history, and there is
nothing in an MVP to build one from.

## Status

All of it lives in the display model of task #112 and touches no GPU type.

* `DisplayStage` gains `Payload`.
* `DisplayStatusCode` gains the payload's causes: no entry for this guest, a
  digest that did not match, build dependencies that could not be installed, a
  module that would not build, a module that would not load, no device after a
  load, an update that failed and rolled back, and an update that failed without
  a rollback.
* `VmDisplayStatus` carries the version that is running and the version that is
  available, when they differ.

Retryability follows what #112 established: a cause the guest can get past on
its own -- a download, an apt failure -- is retryable; a payload that does not
exist for this guest is not.

## Testing

* **The extraction** is proven by the GPU tests that already exist, unchanged
  except for where the code they cover now lives.
* **The shared crate** gets the cache, expansion-limit and staging tests, moved
  rather than rewritten.
* **The display catalog**: selection by version and protocol range, a digest
  mismatch, a document that fails validation failing the whole catalog, a file
  not named for its `payload_id`, an entry whose protocol range this build does
  not cover being passed over rather than failing.
* **The recipe** is tested the way the GPU recipe is: its decisions are
  functions of text -- `/etc/os-release`, `payload.json`, `dkms status`,
  `/proc/modules` -- so the short circuit on an installed version, the behavior
  after a kernel change, and the order of a rollback are all testable in WSL
  without a VM.
* **The host half of an update** -- stages, progress and the resulting status --
  against a stub agent, as `platform` already does.
* **The module compiles** because building the payload compiles it: the
  per-release Docker image installs that release's headers and builds it, so a
  module that does not compile on 22.04, 24.04 or 26.04 is an artifact that was
  never produced.

## Building an artifact

`payloads/display/ubuntu-<release>-amd64/` holds a `Dockerfile` with its base
image pinned by digest, in the shape the GPU payload established: the host needs
docker and no toolchain of its own. It installs that release's
`linux-headers-generic`, builds `vmlord_drm` from this repository's sources and
lays out `content/`.

```sh
payloads/display/ubuntu-24.04-amd64/prepare.sh --output target/display-payload
cargo run -p xtask -- display-payload pack \
    --input         target/display-payload/prepared \
    --archive       target/display-payload/payload.zip \
    --catalog-entry target/display-payload/catalog-entry.json
cargo dist --display-payload target/display-payload
```

`cargo dist --display-payload` re-reads the entry through its own validation,
hashes the archive against what the entry claims, and copies both files into
`display-payload/` beside `vmlord.exe`.

## Deliberately not in this task

* **A signed manifest.** The place is prepared: an entry already commits to its
  archive and its manifest by digest, so signing is signing the entry document.
  Verifying a signature, and the key material that implies, is its own task
  before a public stable release. Nothing here should be read as having done it.
* **That the module loads in a guest and GDM binds it.** That needs a VM: task
  #114 for the output itself, task #128 for the release matrix. This task proves
  delivery, versioning, idempotence and rollback.
* **The display services.** Task #115 fills `content/services/`; here that
  directory is empty and the stages that would use it report as skipped.
* **Anything about GPU.** The mechanism is shared; the lifecycle, the status and
  the shares are not.
