# Versioned display payload implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the display stack a versioned, verified, guest-side payload of its own — VMLord's own DKMS'd DRM module today, task #115's display services later — that installs itself idempotently, survives kernel upgrades, updates only when asked, and steps back one version when an update does not verify.

**Architecture:** The delivery mechanism GPU already has (content-addressed cache, ZIP expansion under limits, per-VM staging, a release directory of `<payload_id>.json`/`.zip` pairs) is extracted into a new `vmlord-payload` crate, generic over the entry type. `vmlord-gpu-payload` keeps its own entry, manifest and selection on top of it; `vmlord-display-payload` is a new, equally thin crate beside it. The display half then gets its own Plan9 share, its own agent messages, its own guest recipe and its own statuses inside the display model of task #112 — nothing about display lifecycle is shared with GPU.

**Tech Stack:** Rust 2024 (workspace), `serde`/`serde_json`, `sha2`, `zip`; Protobuf (`prost`) for the agent schema; C against the Linux DRM/KMS API packaged as DKMS; Docker (pinned base images) for building the payload; `cargo check-windows` / `cargo test-windows` for the Windows halves from WSL.

**Spec:** `docs/superpowers/specs/2026-08-21-display-payload-design.md`

## Global Constraints

- Task branch: `task-113-display-payload`. Commit subjects are `TASK-113: <comment>`, imperative mood, and every commit ends with `Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>`.
- No back-compatibility migrations. There are no users; old VMs are recreated, not migrated. Schema versions may be set to `1` and changed freely inside this task.
- Payload versions are semver; the DKMS package version equals the payload version.
- Target guests: Ubuntu 22.04, 24.04, 26.04 amd64. First proven target is 24.04.
- `kernel_release` is recorded as `proven_on` and is **never** matched on when selecting.
- Display protocol revision this build speaks: `vmlord_display_protocol::CURRENT_VERSION` (currently `1.0`).
- The module is named `vmlord_drm`; the DKMS package is `vmlord-display`; the guest mounts the payload at `/opt/vmlord/display-payload`; the share is `vmlord.display.payload`.
- The module must not be registered on the faux bus and must not carry `vkms` in its name or `ID_PATH` (`61-mutter.rules` would tag it `mutter-device-ignore`), and must not set `DRIVER_CURSOR_HOTSPOT`.
- Every failure of the display payload is a `Degraded` display with a `DisplayStage` and a `DisplayStatusCode`. None of them fails a VM start, the agent session, or GPU.
- `unsafe` is denied workspace-wide in Rust; the kernel module is C and lives outside the Rust workspace.
- Portable crates (`vmlord-payload`, `vmlord-display-payload`) contain no Windows API, no Linux syscalls, no network and no compiled-in catalog.
- Commands: `cargo test -p <crate>` for portable crates in WSL, `cargo test -p vmlord-agent` for the guest agent, `cargo test-windows -p <crate>` for Windows crates, `cargo check-windows` before every commit that touches Windows code.

## File structure

**Created:**

| Path | Responsibility |
| --- | --- |
| `crates/payload/Cargo.toml`, `src/lib.rs` | The shared payload crate `vmlord-payload` |
| `crates/payload/src/{digest,error,progress,prepared,archive,cache,staging,release,catalog,entry}.rs` | The mechanism, generic over `PayloadEntry` |
| `crates/display-payload/Cargo.toml`, `src/lib.rs` | `vmlord-display-payload` |
| `crates/display-payload/src/{catalog,version,protocol,manifest,builder}.rs` | Display entry, selection, manifest, packing |
| `crates/platform/src/display_staging.rs` | Selecting, preparing and staging a display payload for one VM |
| `crates/platform/src/display_exports.rs` | The one display share a VM is offered |
| `crates/platform/src/display_update.rs` | The host half of an explicit update: stage, ask, verify, roll back |
| `crates/agent/src/display_recipe.rs` | The guest recipe's decisions — functions of text |
| `crates/agent/src/display_kernel.rs` | The guest recipe's effects — apt, DKMS, modprobe, the device |
| `payloads/display/module/` | The `vmlord_drm` sources, `dkms.conf`, `Kbuild`, modprobe.d, the systemd unit |
| `payloads/display/ubuntu-{22.04,24.04,26.04}-amd64/{Dockerfile,prepare.sh,payload.spec.json}` | Building one release's artifact |
| `crates/xtask/src/display_payload.rs` | `display-payload pack` and `cargo dist --display-payload` |

**Modified:**

| Path | Change |
| --- | --- |
| `crates/gpu-payload/src/*` | Everything shared moves out; entry, manifest, selection and builder stay |
| `crates/core/src/display.rs` | `DisplayStage::Payload`, payload status codes, payload facts, versions on the status |
| `crates/core/src/lib.rs` | Re-exports; `VmRepository::update_display_payload` |
| `crates/platform/src/{layout,start,agent_session,hcs_config}.rs` | Staging root, the display share, the new agent requests, a Plan9 export list that is not GPU's |
| `crates/agent-protocol/proto/vmlord/agent/v1/agent.proto` | Display messages; schema revision `1.5` |
| `crates/agent/src/session.rs`, `main.rs` | Serving the display requests |
| `crates/app/src/display.rs`, `lib.rs` | Deriving the payload half of the status; the update action |
| `ARCHITECTURE.md` | The display payload, its recipe and its update |
| `Cargo.toml`, `.cargo/config.toml` | The two new crates and the `display-payload` alias |

---

## Task 1: Extract the shared payload primitives

**Files:**
- Create: `crates/payload/Cargo.toml`, `crates/payload/src/lib.rs`, `crates/payload/src/digest.rs`, `crates/payload/src/error.rs`, `crates/payload/src/progress.rs`, `crates/payload/src/prepared.rs`
- Modify: `Cargo.toml`, `crates/gpu-payload/Cargo.toml`, `crates/gpu-payload/src/lib.rs`
- Delete: `crates/gpu-payload/src/digest.rs`, `crates/gpu-payload/src/error.rs`, `crates/gpu-payload/src/progress.rs`

**Interfaces:**
- Produces: `vmlord_payload::{Sha256Digest, Sha256Hasher, PayloadError, PayloadProgress, PreparedFile}`; `PayloadError::io(operation, path, source)` stays `pub(crate)`-equivalent as `pub fn io(...)` because two crates now build errors.
- Consumes: nothing.

`PayloadError::UnsupportedTarget(GuestTarget)` cannot move as it stands — `GuestTarget` is a GPU type. It becomes `UnsupportedTarget(String)` carrying the target's debug form, and every message that says "GPU payload" says "payload". `PreparedFile` moves out of `manifest.rs` because both payloads' manifests list files the same way.

- [ ] **Step 1: Create the crate and register it**

`crates/payload/Cargo.toml`:

```toml
[package]
name = "vmlord-payload"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
log.workspace = true
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
sha2 = "0.11"
zip = { version = "9.0.0-pre3", default-features = false, features = ["deflate-flate2-zlib-rs", "time"] }

[lints]
workspace = true
```

Add `"crates/payload"` to both `members` and `default-members` in the workspace `Cargo.toml`.

- [ ] **Step 2: Move `digest.rs`, `progress.rs` and `PreparedFile` verbatim**

```bash
git mv crates/gpu-payload/src/digest.rs crates/payload/src/digest.rs
git mv crates/gpu-payload/src/progress.rs crates/payload/src/progress.rs
```

`crates/payload/src/prepared.rs` takes `PreparedFile` and the `validate_path` helper out of `crates/gpu-payload/src/manifest.rs` unchanged, including its rejection of `\`, `\0`, absolute paths, empty components, `.`, `..` and `payload.json` itself.

- [ ] **Step 3: Move the error type, generalizing two variants**

`crates/payload/src/error.rs` is `crates/gpu-payload/src/error.rs` with:

```rust
    /// A target no entry in the catalog has, in the form its own crate prints
    /// it: the shared error cannot name a GPU tuple or a display one.
    UnsupportedTarget(String),
```

and every "GPU payload" in a message replaced by "payload". `PayloadError::io` becomes `pub`.

- [ ] **Step 4: Write the failing test that the shared crate stands alone**

`crates/payload/src/lib.rs`:

```rust
//! What every VMLord payload is made of, whatever it carries.
//!
//! A payload is an archive a release ships, verified by digest, expanded under
//! limits, cached by content and staged into one VM's directory. None of that
//! knows whether the files inside are a GPU stack or a display one, so none of
//! it lives in a crate that does.

mod digest;
mod error;
mod prepared;
mod progress;

pub use digest::{Sha256Digest, Sha256Hasher};
pub use error::PayloadError;
pub use prepared::PreparedFile;
pub use progress::PayloadProgress;

#[cfg(test)]
mod tests {
    use super::{PayloadError, Sha256Digest};

    #[test]
    fn an_unsupported_target_is_named_by_whoever_had_one() {
        let error = PayloadError::UnsupportedTarget("ubuntu 24.04 amd64".into());
        assert!(error.to_string().contains("ubuntu 24.04 amd64"));
    }

    #[test]
    fn a_digest_round_trips_through_its_hex() {
        let digest = Sha256Digest::hash_reader(b"payload".as_slice()).unwrap();
        assert_eq!(digest.as_hex().parse::<Sha256Digest>().unwrap(), digest);
    }
}
```

- [ ] **Step 5: Run the tests and watch them fail**

Run: `cargo test -p vmlord-payload`
Expected: FAIL — the modules do not compile yet (missing `PreparedFile`, unresolved imports in the moved files).

- [ ] **Step 6: Make the moved modules compile**

Fix the imports in the moved files (`use crate::{PayloadError, Sha256Digest};`), and delete `crates/gpu-payload/src/{digest,error,progress}.rs`.

- [ ] **Step 7: Point `vmlord-gpu-payload` at the shared crate**

Add `vmlord-payload = { path = "../payload" }` to `crates/gpu-payload/Cargo.toml`, and in `crates/gpu-payload/src/lib.rs` replace the removed modules with re-exports so no downstream import changes:

```rust
pub use vmlord_payload::{PayloadError, PayloadProgress, PreparedFile, Sha256Digest};
```

Replace the two `PayloadError::UnsupportedTarget(target.clone())` construction sites in `catalog.rs` with `PayloadError::UnsupportedTarget(format!("{target:?}"))`, and fix the matching test to assert on the message rather than the payload.

- [ ] **Step 8: Run every affected test**

Run: `cargo test -p vmlord-payload -p vmlord-gpu-payload`
Expected: PASS — the GPU suite is unchanged and green, which is the whole proof that this step moved code and not behavior.

- [ ] **Step 9: Commit**

```bash
git add Cargo.toml crates/payload crates/gpu-payload
git commit -m "$(cat <<'EOF'
TASK-113: Extract the payload primitives shared by every payload

A digest, a progress report, a prepared file and an error are true of any
payload VMLord ships, so they stop living in the crate that happens to have
had them first. `UnsupportedTarget` carries a string now: the shared error
cannot name a GPU tuple.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Move archive expansion into the shared crate

**Files:**
- Create: `crates/payload/src/archive.rs`
- Modify: `crates/payload/src/lib.rs`, `crates/gpu-payload/src/cache.rs`
- Delete: `crates/gpu-payload/src/archive.rs`

**Interfaces:**
- Consumes: `vmlord_payload::{PayloadError, PayloadProgress}` (Task 1).
- Produces: `vmlord_payload::archive::extract(archive: &Path, destination: &Path, limits: ExpansionLimits, progress: &dyn Fn(PayloadProgress), cancel: &AtomicBool) -> Result<u64, PayloadError>` and `pub struct ExpansionLimits { pub expanded_size: u64, pub file_count: u64, pub archive_length: u64 }`.

The existing `extract` takes a `&CatalogEntry` only to read three numbers off it. `ExpansionLimits` is those three numbers, which is what makes the function shared.

- [ ] **Step 1: Write the failing test**

`crates/payload/src/archive.rs`, in its `mod tests`:

```rust
#[test]
fn an_archive_that_would_outgrow_its_limit_is_refused() {
    let temporary = TemporaryDirectory::new("expansion-limit");
    let archive = temporary.path().join("payload.zip");
    write_archive(&archive, &[("content/big", &vec![b'x'; 4096])]);

    let error = extract(
        &archive,
        &temporary.path().join("files"),
        ExpansionLimits { expanded_size: 1024, file_count: 8, archive_length: 8192 },
        &|_| {},
        &AtomicBool::new(false),
    )
    .expect_err("4096 bytes cannot come out of a 1024-byte budget");

    assert!(matches!(
        error,
        PayloadError::LimitExceeded { subject: "expanded size", .. }
    ));
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p vmlord-payload archive`
Expected: FAIL — `crates/payload/src/archive.rs` does not exist.

- [ ] **Step 3: Move the module and cut the entry out of it**

```bash
git mv crates/gpu-payload/src/archive.rs crates/payload/src/archive.rs
```

Replace the `entry: &CatalogEntry` parameter with `limits: ExpansionLimits`, and the three reads (`entry.expanded_size_limit()`, `entry.file_count_limit()`, the measured archive length) with the struct's fields. Everything else — the path rules, the per-member compressed-size cap, the cancellation checks, the `PayloadProgress::Extracting` reports — moves untouched, along with its existing tests.

Export it from `crates/payload/src/lib.rs`:

```rust
pub mod archive;
pub use archive::ExpansionLimits;
```

- [ ] **Step 4: Update the one caller**

In `crates/gpu-payload/src/cache.rs`, build the limits at the call site:

```rust
let expanded = archive::extract(
    &cached_archive,
    &files_directory,
    ExpansionLimits {
        expanded_size: entry.expanded_size_limit(),
        file_count: entry.file_count_limit(),
        archive_length,
    },
    request_progress,
    cancel,
)?;
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p vmlord-payload -p vmlord-gpu-payload`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/payload crates/gpu-payload
git commit -m "$(cat <<'EOF'
TASK-113: Share the archive expansion and its limits

Expansion never needed a catalog entry, only the three numbers it reads off
one. `ExpansionLimits` is those numbers, and with them the path rules and the
size caps are one implementation for every payload rather than one per kind.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Define `PayloadEntry` and share the cache

**Files:**
- Create: `crates/payload/src/entry.rs`, `crates/payload/src/cache.rs`
- Modify: `crates/payload/src/lib.rs`, `crates/gpu-payload/src/lib.rs`, `crates/gpu-payload/src/catalog.rs`, `crates/gpu-payload/src/manifest.rs`
- Delete: `crates/gpu-payload/src/cache.rs`

**Interfaces:**
- Consumes: `vmlord_payload::{archive, PayloadError, PayloadProgress, PreparedFile, Sha256Digest}`.
- Produces:

```rust
pub trait PayloadEntry: serde::Serialize + Sized {
    /// The parsed `payload.json` of this kind of payload.
    type Manifest: PayloadFiles;
    /// The parsed `sources.json`, kept only to be written into the cache's
    /// provenance record.
    type Sources: serde::Serialize;

    /// The cache namespace and the release subdirectory. Two payload kinds
    /// must not share a cache directory even when their digests could not
    /// collide: a quarantine of one must not touch the other.
    const NAMESPACE: &'static str;

    fn from_json(bytes: &[u8]) -> Result<Self, PayloadError>;
    fn payload_id(&self) -> &str;
    fn archive_sha256(&self) -> &Sha256Digest;
    fn payload_manifest_sha256(&self) -> &Sha256Digest;
    fn expanded_size_limit(&self) -> u64;
    fn file_count_limit(&self) -> u64;
    fn parse_manifest(&self, bytes: &[u8]) -> Result<Self::Manifest, PayloadError>;
    fn parse_sources(&self, bytes: &[u8]) -> Result<Self::Sources, PayloadError>;
}

pub trait PayloadFiles {
    fn files(&self) -> &[PreparedFile];
}

pub struct ReadyPayload<E: PayloadEntry> { /* payload_id, generation, payload_manifest_sha256, files_directory, manifest: E::Manifest, provenance_path */ }
pub fn prepare<E: PayloadEntry>(request: PrepareRequest<'_, E>) -> Result<ReadyPayload<E>, PayloadError>;
pub struct PrepareRequest<'a, E: PayloadEntry> { pub entry: &'a E, pub cache_root: &'a Path, pub archive: &'a Path, pub progress: &'a dyn Fn(PayloadProgress), pub cancel: &'a AtomicBool }
pub fn cache_provenance<E: PayloadEntry>(entry: &E, sources: &E::Sources) -> Result<Vec<u8>, PayloadError>;
```

- [ ] **Step 1: Write the failing test for the seam**

`crates/payload/src/cache.rs`, in `mod tests`, a payload kind that exists only for the test — this is what proves the trait is the whole contract:

```rust
#[test]
fn any_payload_kind_can_be_prepared_through_the_trait() {
    let temporary = TemporaryDirectory::new("trait-cache");
    let archive = temporary.path().join("payload.zip");
    let entry = TestEntry::packing(&archive, &[("content/marker", b"one")]);

    let ready = prepare(PrepareRequest {
        entry: &entry,
        cache_root: temporary.path(),
        archive: &archive,
        progress: &|_| {},
        cancel: &AtomicBool::new(false),
    })
    .expect("a valid archive must prepare");

    assert_eq!(ready.manifest().files().len(), 2, "the marker and sources.json");
    assert!(ready.files_directory().join("content/marker").is_file());
}

#[test]
fn a_cache_hit_is_rehashed_rather_than_trusted() {
    let temporary = TemporaryDirectory::new("tampered-hit");
    let archive = temporary.path().join("payload.zip");
    let entry = TestEntry::packing(&archive, &[("content/marker", b"one")]);
    let first = prepare(request(&entry, temporary.path(), &archive)).unwrap();
    fs::write(first.files_directory().join("content/marker"), b"two").unwrap();

    let second = prepare(request(&entry, temporary.path(), &archive))
        .expect("a tampered generation is rebuilt, not returned");

    assert_eq!(fs::read(second.files_directory().join("content/marker")).unwrap(), b"one");
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p vmlord-payload cache`
Expected: FAIL — no `cache` module, no `PayloadEntry`.

- [ ] **Step 3: Write the trait**

`crates/payload/src/entry.rs` holds exactly the trait definitions from the Interfaces block above, each with a doc comment saying why the method is on the trait rather than in the shared body: the shared half must be able to identify a payload, verify it, expand it under limits and record what it prepared, and everything else — what a target is, what provenance means, which renderers a payload requires — is the kind's own business.

- [ ] **Step 4: Move the cache and make it generic**

```bash
git mv crates/gpu-payload/src/cache.rs crates/payload/src/cache.rs
```

Substitutions, and nothing else:

- `CatalogEntry` → `E`, with `E: PayloadEntry` on every function.
- `ReadyGpuPayload` → `ReadyPayload<E>`, its `manifest` field typed `E::Manifest`.
- `root = request.cache_root.join("gpu-payload").join("v1")` → `.join(E::NAMESPACE).join("v1")`.
- `PayloadManifest::parse_and_validate(&bytes, entry)` → `entry.parse_manifest(&bytes)`.
- `SourceManifest::parse_and_validate(&bytes, entry)` → `entry.parse_sources(&bytes)`.
- `manifest::cache_provenance` moves here as the generic function in the Interfaces block, serializing `{schema_version, payload_id, archive_sha256, payload_manifest_sha256, catalog_entry: &E, sources: &E::Sources}`.
- `ReadyMarker` moves here too, built from a `&ReadyPayload<E>` or from an `&E`.

The `DigestLock`, `OperationPath`, quarantine, partial-file and atomic-publication code moves untouched.

- [ ] **Step 5: Implement the trait for the GPU entry**

In `crates/gpu-payload/src/catalog.rs`:

```rust
impl PayloadEntry for CatalogEntry {
    type Manifest = PayloadManifest;
    type Sources = SourceManifest;
    const NAMESPACE: &'static str = "gpu-payload";

    fn from_json(bytes: &[u8]) -> Result<Self, PayloadError> { Self::from_json(bytes) }
    fn payload_id(&self) -> &str { self.payload_id() }
    fn archive_sha256(&self) -> &Sha256Digest { self.archive_sha256() }
    fn payload_manifest_sha256(&self) -> &Sha256Digest { self.payload_manifest_sha256() }
    fn expanded_size_limit(&self) -> u64 { self.expanded_size_limit() }
    fn file_count_limit(&self) -> u64 { self.file_count_limit() }
    fn parse_manifest(&self, bytes: &[u8]) -> Result<Self::Manifest, PayloadError> {
        PayloadManifest::parse_and_validate(bytes, self)
    }
    fn parse_sources(&self, bytes: &[u8]) -> Result<Self::Sources, PayloadError> {
        SourceManifest::parse_and_validate(bytes, self)
    }
}
```

`SourceManifest` needs `Serialize`; derive it on the document it wraps and implement `Serialize` for the wrapper by delegating to `self.document`.

In `crates/gpu-payload/src/lib.rs`:

```rust
pub type ReadyGpuPayload = vmlord_payload::ReadyPayload<CatalogEntry>;
pub use vmlord_payload::{PrepareRequest, prepare};
```

- [ ] **Step 6: Run every test**

Run: `cargo test -p vmlord-payload -p vmlord-gpu-payload`
Expected: PASS. The GPU cache tests moved with the code; the two new tests exercise the trait through a second, fake payload kind.

- [ ] **Step 7: Check the Windows build still links**

Run: `cargo check-windows`
Expected: no errors — `platform` uses `prepare`/`PrepareRequest` by the names it always did.

- [ ] **Step 8: Commit**

```bash
git add crates/payload crates/gpu-payload
git commit -m "$(cat <<'EOF'
TASK-113: Make the payload cache generic over what it carries

`PayloadEntry` is the whole contract between the mechanism and a payload
kind: identify, verify, expand under limits, parse the two documents at the
root. The cache is now written once against that trait, and the test that
proves it is a second payload kind that exists only in the test.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Share staging and the release layout

**Files:**
- Create: `crates/payload/src/staging.rs`, `crates/payload/src/release.rs`
- Modify: `crates/payload/src/lib.rs`, `crates/gpu-payload/src/lib.rs`, `crates/gpu-payload/src/release.rs` (reduced to constants)
- Delete: `crates/gpu-payload/src/staging.rs`

**Interfaces:**
- Consumes: `ReadyPayload<E>`, `PayloadEntry` (Task 3).
- Produces: `vmlord_payload::{ensure_staging_root, stage_payload, StagedPayload}`, where `stage_payload<E: PayloadEntry>(ready: &ReadyPayload<E>, root: &Path, progress: &dyn Fn(PayloadProgress), cancel: &AtomicBool) -> Result<StagedPayload, PayloadError>`; and `vmlord_payload::release::{archive_path, entry_path, payload_directory}` each taking `directory: &Path, subdirectory: &str, payload_id: &str`.

`StagedPayload` is not generic: what staging produced is a directory, a payload ID and a generation digest, none of which depends on the kind.

- [ ] **Step 1: Write the failing test for a parameterized release layout**

`crates/payload/src/release.rs`:

```rust
#[test]
fn each_payload_kind_keeps_its_pair_in_its_own_directory() {
    assert_eq!(
        archive_path(Path::new("dist"), "display-payload", "display-ubuntu-24.04-amd64-0.1.0"),
        PathBuf::from("dist").join("display-payload").join("display-ubuntu-24.04-amd64-0.1.0.zip")
    );
    assert_eq!(
        entry_path(Path::new("dist"), "gpu-payload", "ubuntu-26.04-amd64-7.0.0-28-v2"),
        PathBuf::from("dist").join("gpu-payload").join("ubuntu-26.04-amd64-7.0.0-28-v2.json")
    );
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p vmlord-payload release`
Expected: FAIL — no `release` module in the shared crate.

- [ ] **Step 3: Move both modules**

```bash
git mv crates/gpu-payload/src/staging.rs crates/payload/src/staging.rs
```

`crates/payload/src/release.rs` is the moved layout with the subdirectory as a parameter. `crates/gpu-payload/src/release.rs` shrinks to the GPU's own names, so its callers keep the signatures they have:

```rust
use std::path::{Path, PathBuf};

/// The child of the executable's directory holding shipped GPU archives.
pub const LOCAL_ARCHIVE_DIRECTORY: &str = "gpu-payload";

pub fn local_archive_path(directory: &Path, payload_id: &str) -> PathBuf {
    vmlord_payload::release::archive_path(directory, LOCAL_ARCHIVE_DIRECTORY, payload_id)
}

pub fn local_entry_path(directory: &Path, payload_id: &str) -> PathBuf {
    vmlord_payload::release::entry_path(directory, LOCAL_ARCHIVE_DIRECTORY, payload_id)
}
```

In `staging.rs`, `StagedGpuPayload` becomes `StagedPayload` and `stage_payload`/`stage_with` become generic over `E: PayloadEntry` only where they touch `ReadyPayload<E>`. `crates/gpu-payload/src/lib.rs` keeps the old name alive: `pub type StagedGpuPayload = vmlord_payload::StagedPayload;`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p vmlord-payload -p vmlord-gpu-payload`
Expected: PASS, including the moved staging tests for the ready marker, the canonicalization guard and the atomic swap.

- [ ] **Step 5: Check the Windows build**

Run: `cargo check-windows`
Expected: no errors.

- [ ] **Step 6: Commit**

```bash
git add crates/payload crates/gpu-payload
git commit -m "$(cat <<'EOF'
TASK-113: Share staging and the release layout

Where a release keeps its pairs and how a generation is published into a VM's
directory are rules about payloads, not about GPUs. The subdirectory becomes a
parameter, which is the only thing that was ever GPU-specific about them.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Share reading a release directory

**Files:**
- Create: `crates/payload/src/catalog.rs`
- Modify: `crates/payload/src/lib.rs`, `crates/gpu-payload/src/catalog.rs`

**Interfaces:**
- Consumes: `PayloadEntry` (Task 3), `release` (Task 4).
- Produces: `vmlord_payload::catalog::read_release_directory<E: PayloadEntry>(directory: &Path, subdirectory: &str) -> Result<Vec<E>, PayloadError>`.

Selection stays in each crate. What is shared is the four rules a release directory obeys: a missing or unlistable directory is an empty catalog; a `*.json` must parse and validate; its file stem must equal its `payload_id`; its archive must be beside it. Duplicate detection stays with selection, because "duplicate" means something different per kind — GPU has one entry per exact target, display has one entry per target **and version**.

- [ ] **Step 1: Write the failing test**

`crates/payload/src/catalog.rs`:

```rust
#[test]
fn a_release_without_this_kind_of_payload_reads_as_an_empty_list() {
    let temporary = TemporaryDirectory::new("absent");
    let entries: Vec<TestEntry> =
        read_release_directory(temporary.path(), "display-payload").expect("nothing is not an error");
    assert!(entries.is_empty());
}

#[test]
fn an_entry_file_that_is_there_and_wrong_fails_the_read() {
    let temporary = TemporaryDirectory::new("misnamed");
    write_pair(temporary.path(), "display-payload", "not-its-id", &TestEntry::json("real-id"));

    let error = read_release_directory::<TestEntry>(temporary.path(), "display-payload")
        .expect_err("a file that does not name its payload is a broken release");

    assert!(matches!(error, PayloadError::InvalidCatalog(_)));
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p vmlord-payload catalog`
Expected: FAIL — no `catalog` module in the shared crate.

- [ ] **Step 3: Implement it by lifting the body of `from_release_directory`**

The body of `PayloadCatalog::from_release_directory` moves verbatim, with `CatalogEntry::from_json` becoming `E::from_json` and the archive path coming from `release::archive_path(directory, subdirectory, ...)`.

- [ ] **Step 4: Reduce the GPU catalog to its own rules**

```rust
    pub fn from_release_directory(directory: &Path) -> Result<Self, PayloadError> {
        Self::from_entries(vmlord_payload::catalog::read_release_directory(
            directory,
            crate::LOCAL_ARCHIVE_DIRECTORY,
        )?)
    }
```

`from_entries` keeps the GPU uniqueness rule (one entry per payload ID and per exact target) and every existing selection test.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p vmlord-payload -p vmlord-gpu-payload`
Expected: PASS — including `a_build_that_ships_no_payload_has_an_empty_catalog_and_not_an_error` and `an_entry_file_that_is_there_and_wrong_fails_the_catalog`, unchanged.

- [ ] **Step 6: Check the Windows build**

Run: `cargo check-windows`
Expected: no errors.

- [ ] **Step 7: Commit**

```bash
git add crates/payload crates/gpu-payload
git commit -m "$(cat <<'EOF'
TASK-113: Share the rules a release directory obeys

Missing is empty, present and wrong fails, a file must name its payload and
its archive must be beside it: four rules about releases, now written once.
Selection stays per kind, because a duplicate means something different to a
catalog whose entries differ by kernel and one whose entries differ by version.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: The display catalog entry

**Files:**
- Create: `crates/display-payload/Cargo.toml`, `crates/display-payload/src/lib.rs`, `crates/display-payload/src/version.rs`, `crates/display-payload/src/protocol.rs`, `crates/display-payload/src/catalog.rs`
- Modify: `Cargo.toml`

**Interfaces:**
- Consumes: `vmlord_payload::{PayloadEntry, PayloadError, Sha256Digest}`.
- Produces: `vmlord_display_payload::{DisplayCatalogEntry, DisplayTarget, GuestSelector, PayloadVersion, ProtocolRange}`; `DisplayCatalogEntry::from_json(&[u8])`, `.payload_id()`, `.version() -> &PayloadVersion`, `.target() -> &DisplayTarget`, `.proven_on() -> &str`, `.protocol() -> &ProtocolRange`.

`PayloadVersion` is a three-number semver with `Ord`, parsed from `"0.1.0"`; no pre-release and no build metadata, because a payload that ships is a payload that is released. `ProtocolRange { major, min_minor, max_minor }` mirrors how `vmlord-display-protocol` negotiates: a differing major cannot be negotiated at all, minors negotiate down.

- [ ] **Step 1: Create the crate**

`crates/display-payload/Cargo.toml`:

```toml
[package]
name = "vmlord-display-payload"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
log.workspace = true
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
vmlord-payload = { path = "../payload" }

[features]
builder = []

[lints]
workspace = true
```

Register it in `members` and `default-members`.

- [ ] **Step 2: Write the failing tests for the version and the range**

`crates/display-payload/src/version.rs`:

```rust
#[test]
fn versions_order_by_number_and_not_by_text() {
    assert!("0.10.0".parse::<PayloadVersion>().unwrap() > "0.9.9".parse().unwrap());
    assert_eq!("1.2.3".parse::<PayloadVersion>().unwrap().to_string(), "1.2.3");
}

#[test]
fn a_version_that_is_not_three_numbers_is_refused() {
    for text in ["1.2", "1.2.3.4", "1.2.x", "v1.2.3", "1.2.3-rc1", ""] {
        assert!(text.parse::<PayloadVersion>().is_err(), "accepted {text}");
    }
}
```

`crates/display-payload/src/protocol.rs`:

```rust
#[test]
fn a_range_covers_only_its_own_major() {
    let range = ProtocolRange { major: 1, min_minor: 0, max_minor: 2 };
    assert!(range.covers(1, 0) && range.covers(1, 2));
    assert!(!range.covers(1, 3), "a payload cannot promise a minor it has never seen");
    assert!(!range.covers(2, 0), "a differing major is not negotiable");
    assert!(!range.covers(0, 9));
}

#[test]
fn a_range_whose_bounds_are_inverted_is_invalid() {
    assert!(!ProtocolRange { major: 1, min_minor: 3, max_minor: 1 }.is_valid());
}
```

- [ ] **Step 3: Run them and watch them fail**

Run: `cargo test -p vmlord-display-payload`
Expected: FAIL — the crate has no modules yet.

- [ ] **Step 4: Implement the two value types**

```rust
/// A payload's own version, independent of VMLord's.
///
/// Three numbers and nothing else: a payload that reaches a release is
/// released, so there is no pre-release to order and no build metadata to
/// ignore. `Ord` is what "the newest version wins" is decided by, and it reads
/// the numbers rather than the text -- `0.10.0` is newer than `0.9.9`, which
/// sorting strings gets wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PayloadVersion {
    major: u32,
    minor: u32,
    patch: u32,
}
```

with `FromStr`, `Display`, and serde through those two. `ProtocolRange` derives `Deserialize`/`Serialize` over its three fields and carries `covers(major, minor)` and `is_valid()`.

- [ ] **Step 5: Write the failing test for the entry**

`crates/display-payload/src/catalog.rs`:

```rust
const ZERO: &str = "0000000000000000000000000000000000000000000000000000000000000000";

fn entry_json(release: &str, version: &str) -> String {
    format!(
        r#"{{"schema_version":1,"payload_id":"display-ubuntu-{release}-amd64-{version}",
        "version":"{version}","target":{{"distribution":"ubuntu","release":"{release}",
        "architecture":"amd64","payload_abi":1}},"proven_on":"6.8.0-137-generic",
        "protocol":{{"major":1,"min_minor":0,"max_minor":0}},"archive_sha256":"{ZERO}",
        "payload_manifest_sha256":"{ZERO}","expanded_size_limit":33554432,"file_count_limit":512,
        "sources":[{{"url":"https://vmlord.invalid/display","commit":"{}","version":"{version}"}}],
        "licenses":[{{"spdx":"GPL-2.0","path":"licenses/GPL-2.0.txt"}}]}}"#,
        "a".repeat(40)
    )
}

#[test]
fn an_entry_carries_its_version_its_proof_and_its_protocol_range() {
    let entry = DisplayCatalogEntry::from_json(entry_json("24.04", "0.1.0").as_bytes()).unwrap();

    assert_eq!(entry.payload_id(), "display-ubuntu-24.04-amd64-0.1.0");
    assert_eq!(entry.version().to_string(), "0.1.0");
    assert_eq!(entry.proven_on(), "6.8.0-137-generic", "a proof, never a selector");
    assert!(entry.protocol().covers(1, 0));
}

#[test]
fn an_entry_whose_id_does_not_end_in_its_version_is_refused() {
    let document = entry_json("24.04", "0.1.0").replace("amd64-0.1.0", "amd64-0.2.0");
    assert!(matches!(
        DisplayCatalogEntry::from_json(document.as_bytes()),
        Err(PayloadError::InvalidCatalog(_))
    ));
}

#[test]
fn an_entry_missing_any_required_field_is_refused() {
    for field in ["payload_id", "version", "proven_on"] {
        let mut document: serde_json::Value =
            serde_json::from_str(&entry_json("24.04", "0.1.0")).unwrap();
        document[field] = "".into();
        assert!(
            DisplayCatalogEntry::from_json(&serde_json::to_vec(&document).unwrap()).is_err(),
            "accepted an empty {field}"
        );
    }
}

#[test]
fn an_entry_at_another_schema_version_is_refused() {
    let mut document: serde_json::Value =
        serde_json::from_str(&entry_json("24.04", "0.1.0")).unwrap();
    document["schema_version"] = 2.into();
    assert!(DisplayCatalogEntry::from_json(&serde_json::to_vec(&document).unwrap()).is_err());
}
```

- [ ] **Step 6: Run them and watch them fail**

Run: `cargo test -p vmlord-display-payload catalog`
Expected: FAIL — `DisplayCatalogEntry` does not exist.

- [ ] **Step 7: Implement the entry**

Follow `vmlord-gpu-payload`'s shape exactly: a private `DisplayCatalogEntryDocument` with `#[derive(Deserialize)]`, a public `DisplayCatalogEntry` that can only be built through `from_json` (the compile-fail doctest that proves it comes with the pattern), and a `validate` that refuses an empty id, an empty target dimension, a `payload_abi` other than `1`, zero limits, a `proven_on` that is empty, an invalid `ProtocolRange`, an id whose suffix is not its version, a source list that is empty or carries a non-40-hex commit, and an empty license list.

- [ ] **Step 8: Run the tests**

Run: `cargo test -p vmlord-display-payload`
Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add Cargo.toml crates/display-payload
git commit -m "$(cat <<'EOF'
TASK-113: Add the display payload catalog entry

A version of its own, a kernel recorded as proof rather than as a condition,
and the range of display protocol revisions the payload's services speak --
the three things that make a display entry not a GPU entry.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: Display selection

**Files:**
- Modify: `crates/display-payload/src/catalog.rs`, `crates/display-payload/src/lib.rs`

**Interfaces:**
- Consumes: `DisplayCatalogEntry` (Task 6), `vmlord_payload::catalog::read_release_directory` (Task 5).
- Produces: `DisplayPayloadCatalog::{from_release_directory, from_entries, entries, select_for_guest}`; `select_for_guest(&self, guest: &GuestSelector<'_>, protocol: ProtocolVersionParts) -> Result<&DisplayCatalogEntry, PayloadError>` where `ProtocolVersionParts { major: u32, minor: u32 }` is passed in rather than read from `vmlord-display-protocol` — this crate stays free of that dependency, and `platform` supplies the build's own revision.
- Produces: `pub const LOCAL_ARCHIVE_DIRECTORY: &str = "display-payload";`

- [ ] **Step 1: Write the failing tests**

```rust
fn ubuntu_2404() -> GuestSelector<'static> {
    GuestSelector { distribution: "ubuntu", release: "24.04", architecture: "amd64" }
}

const SPEAKS_1_0: ProtocolVersionParts = ProtocolVersionParts { major: 1, minor: 0 };

#[test]
fn the_newest_version_for_the_guest_wins() {
    let catalog = catalog_with(&[entry_json("24.04", "0.1.0"), entry_json("24.04", "0.10.0")]);

    assert_eq!(
        catalog.select_for_guest(&ubuntu_2404(), SPEAKS_1_0).unwrap().version().to_string(),
        "0.10.0"
    );
}

#[test]
fn a_payload_this_build_cannot_speak_to_is_passed_over_and_not_an_error() {
    let future = entry_json("24.04", "0.2.0")
        .replace(r#""protocol":{"major":2,"min_minor":0,"max_minor":0}"#, r#""protocol":{"major":2,"min_minor":0,"max_minor":0}"#);
    let catalog = catalog_with(&[entry_json("24.04", "0.1.0"), future]);

    assert_eq!(
        catalog.select_for_guest(&ubuntu_2404(), SPEAKS_1_0).unwrap().version().to_string(),
        "0.1.0",
        "a payload built for a VMLord this is not is a candidate that does not apply"
    );
}

#[test]
fn a_guest_with_no_entry_is_told_which_guest_had_none() {
    let catalog = catalog_with(&[entry_json("24.04", "0.1.0")]);
    let error = catalog
        .select_for_guest(&GuestSelector { release: "22.04", ..ubuntu_2404() }, SPEAKS_1_0)
        .expect_err("no entry matches this release");

    assert!(matches!(error, PayloadError::NoPayloadForGuest { .. }));
    assert!(error.to_string().contains("22.04"));
}

#[test]
fn several_versions_for_one_guest_are_a_catalog_and_not_a_conflict() {
    assert!(
        DisplayPayloadCatalog::from_entries(vec![
            parse(&entry_json("24.04", "0.1.0")),
            parse(&entry_json("24.04", "0.2.0")),
        ])
        .is_ok(),
        "holding two versions at once is what an update is made of"
    );
}

#[test]
fn one_version_twice_for_one_guest_is_a_broken_release() {
    let mut second: serde_json::Value = serde_json::from_str(&entry_json("24.04", "0.1.0")).unwrap();
    second["payload_id"] = "display-ubuntu-24.04-amd64-0.1.0-again".into();

    assert!(
        DisplayPayloadCatalog::from_entries(vec![
            parse(&entry_json("24.04", "0.1.0")),
            parse(&serde_json::to_string(&second).unwrap()),
        ])
        .is_err(),
        "selection must not depend on the order a directory listed two identical candidates"
    );
}
```

(The second test's `future` entry needs the protocol object of `entry_json` replaced with a major of `2`; write it with `serde_json` rather than a string replace if that reads better.)

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test -p vmlord-display-payload catalog`
Expected: FAIL — `DisplayPayloadCatalog` does not exist.

- [ ] **Step 3: Implement selection**

```rust
    /// The best entry for a guest this build can actually talk to.
    ///
    /// The triple is the hard gate and is decided before the guest has booted,
    /// which is why no kernel appears in it. Of what is left, an entry whose
    /// protocol range this build is outside of is passed over rather than
    /// failed: a payload may legitimately be built for a newer or older
    /// VMLord, and a release carrying one is not broken. The greatest version
    /// wins, because a payload is only published when it is meant to be used.
    pub fn select_for_guest(
        &self,
        guest: &GuestSelector<'_>,
        protocol: ProtocolVersionParts,
    ) -> Result<&DisplayCatalogEntry, PayloadError> {
        self.entries
            .iter()
            .filter(|entry| entry.target().matches(guest))
            .filter(|entry| entry.protocol().covers(protocol.major, protocol.minor))
            .max_by(|left, right| left.version().cmp(right.version()))
            .ok_or_else(|| PayloadError::NoPayloadForGuest {
                distribution: guest.distribution.to_owned(),
                release: guest.release.to_owned(),
                architecture: guest.architecture.to_owned(),
            })
    }
```

`from_entries` rejects a duplicate `payload_id` and a duplicate (target, version) pair, and nothing else.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p vmlord-display-payload`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/display-payload
git commit -m "$(cat <<'EOF'
TASK-113: Select a display payload by version and by what it speaks

Several versions for one guest is the ordinary state of a catalog that can be
updated, so it is the same version twice that is the broken release. An entry
outside this build's protocol range is passed over rather than failed: a
payload is allowed to be newer than the application that reads its catalog.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: The display manifest, and the entry as a payload

**Files:**
- Create: `crates/display-payload/src/manifest.rs`
- Modify: `crates/display-payload/src/catalog.rs`, `crates/display-payload/src/lib.rs`

**Interfaces:**
- Consumes: `vmlord_payload::{PayloadEntry, PayloadFiles, PreparedFile, prepare, PrepareRequest, stage_payload, ensure_staging_root}`.
- Produces: `DisplayManifest` (implements `PayloadFiles`), `DisplaySources`, `impl PayloadEntry for DisplayCatalogEntry` with `NAMESPACE = "display-payload"`, and the crate-level aliases `pub type ReadyDisplayPayload = vmlord_payload::ReadyPayload<DisplayCatalogEntry>;`.

The display `payload.json` states `schema_version`, `payload_id`, `version`, the target, and the sorted, unique, non-empty file list. The cross-check is the entry's identity plus two structural facts: `sources.json` must be declared, and so must every license path the entry claims. One display-only rule beyond GPU's: the manifest must declare at least one file under `content/drm/`, because a display payload with no module is a payload that cannot do the one thing it exists for.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_manifest_must_match_the_entry_that_claims_it() {
    let entry = entry();
    let document = manifest_json("display-ubuntu-24.04-amd64-0.1.0", "0.1.0", &[
        "content/drm/Kbuild", "content/drm/vmlord_drm.c", "licenses/GPL-2.0.txt", "sources.json",
    ]);

    assert!(DisplayManifest::parse_and_validate(document.as_bytes(), &entry).is_ok());

    let wrong_version = manifest_json("display-ubuntu-24.04-amd64-0.1.0", "0.2.0", &[
        "content/drm/Kbuild", "licenses/GPL-2.0.txt", "sources.json",
    ]);
    assert!(matches!(
        DisplayManifest::parse_and_validate(wrong_version.as_bytes(), &entry),
        Err(PayloadError::InvalidManifest(_))
    ));
}

#[test]
fn a_payload_with_no_module_is_not_a_display_payload() {
    let document = manifest_json("display-ubuntu-24.04-amd64-0.1.0", "0.1.0", &[
        "content/services/README", "licenses/GPL-2.0.txt", "sources.json",
    ]);

    let error = DisplayManifest::parse_and_validate(document.as_bytes(), &entry())
        .expect_err("nothing under content/drm means nothing to build");

    assert!(error.to_string().contains("content/drm"));
}

#[test]
fn file_paths_must_be_sorted_unique_and_declared_once() {
    for files in [
        vec!["sources.json", "content/drm/Kbuild", "licenses/GPL-2.0.txt"],
        vec!["content/drm/Kbuild", "content/drm/Kbuild", "licenses/GPL-2.0.txt", "sources.json"],
    ] {
        let document = manifest_json("display-ubuntu-24.04-amd64-0.1.0", "0.1.0", &files);
        assert!(DisplayManifest::parse_and_validate(document.as_bytes(), &entry()).is_err());
    }
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test -p vmlord-display-payload manifest`
Expected: FAIL — no `manifest` module.

- [ ] **Step 3: Implement the manifest and the trait**

`DisplayManifest::parse_and_validate` follows `PayloadManifest`'s body: parse with `deny_unknown_fields`, check `schema_version == 1`, `payload_id` and `version` and target against the entry, then walk the files enforcing `vmlord_payload::PreparedFile`'s path rules, sortedness, uniqueness, non-zero size, the presence of `sources.json`, every claimed license path, and at least one `content/drm/` member.

`DisplaySources` parses `sources.json` and cross-checks it against the entry's `sources` the way `SourceManifest` does for GPU: same URLs, same commits, same versions, nothing extra.

Then the trait:

```rust
impl PayloadEntry for DisplayCatalogEntry {
    type Manifest = DisplayManifest;
    type Sources = DisplaySources;
    const NAMESPACE: &'static str = "display-payload";
    /* the eight accessors, parse_manifest, parse_sources */
}
```

- [ ] **Step 4: Write the failing end-to-end test through the shared mechanism**

`crates/display-payload/tests/prepare.rs` builds a real ZIP in a temporary directory from fixtures, prepares it and stages it:

```rust
#[test]
fn a_packed_display_payload_prepares_and_stages() {
    let fixture = DisplayPayloadFixture::build("0.1.0");

    let ready = vmlord_payload::prepare(vmlord_payload::PrepareRequest {
        entry: fixture.entry(),
        cache_root: fixture.cache_root(),
        archive: fixture.archive(),
        progress: &|_| {},
        cancel: &AtomicBool::new(false),
    })
    .expect("the fixture is a valid payload");

    assert!(ready.files_directory().join("content/drm/dkms.conf").is_file());

    vmlord_payload::ensure_staging_root(fixture.staging_root()).unwrap();
    let staged = vmlord_payload::stage_payload(&ready, fixture.staging_root(), &|_| {}, &AtomicBool::new(false))
        .expect("a ready payload stages");

    assert!(staged.generation_directory().join("payload.json").is_file());
}
```

- [ ] **Step 5: Run it and watch it fail, then pass**

Run: `cargo test -p vmlord-display-payload --test prepare`
Expected: FAIL first (no fixture helper), then PASS once the fixture builder from Task 9's `builder` module is used to produce the archive.

- [ ] **Step 6: Commit**

```bash
git add crates/display-payload
git commit -m "$(cat <<'EOF'
TASK-113: Verify a display payload's manifest and prepare one

The entry becomes a `PayloadEntry`, so the shared cache and staging carry a
display payload with no code of their own. One rule beyond GPU's: a manifest
that declares nothing under `content/drm` is not a display payload, and the
check belongs where every other structural claim is checked.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: Packing a display payload

**Files:**
- Create: `crates/display-payload/src/builder.rs`, `crates/xtask/src/display_payload.rs`
- Modify: `crates/display-payload/src/lib.rs`, `crates/xtask/src/main.rs`, `crates/xtask/src/gpu_payload.rs` (dist argument parsing), `.cargo/config.toml`

**Interfaces:**
- Consumes: `DisplayCatalogEntry`, `DisplayManifest` (Tasks 6–8).
- Produces: `vmlord_display_payload::builder::{PackRequest, pack, BuiltDisplayPayload}` behind the `builder` feature; `xtask` subcommand `display-payload pack --recipe --input --archive --catalog-entry`; `cargo dist --display-payload <directory>`.

`pack` takes a prepared directory and a recipe describing what cannot be derived from the files — version, target, `proven_on`, the protocol range, sources and licenses — writes `payload.json` into the archive, sorts the ZIP's members, and emits the catalog entry with the two digests filled in. The archive digest is not self-referential: `payload.json` is written first, the entry is written after the archive is closed and hashed.

- [ ] **Step 1: Write the failing test**

`crates/display-payload/src/builder.rs`:

```rust
#[test]
fn packing_produces_an_entry_that_describes_its_own_archive() {
    let temporary = TemporaryDirectory::new("pack");
    prepared_tree(temporary.path().join("prepared"));
    write_recipe(temporary.path().join("recipe.json"));

    pack(PackRequest {
        prepared_directory: &temporary.path().join("prepared"),
        recipe_path: &temporary.path().join("recipe.json"),
        archive_path: &temporary.path().join("payload.zip"),
        catalog_entry_path: &temporary.path().join("catalog-entry.json"),
    })
    .expect("a well-formed tree packs");

    let entry = DisplayCatalogEntry::from_json(
        &fs::read(temporary.path().join("catalog-entry.json")).unwrap(),
    )
    .expect("pack writes an entry its own reader accepts");
    let digest = Sha256Digest::hash_reader(
        File::open(temporary.path().join("payload.zip")).unwrap(),
    )
    .unwrap();

    assert_eq!(entry.archive_sha256(), &digest);
    assert_eq!(entry.payload_id(), "display-ubuntu-24.04-amd64-0.1.0");
}

#[test]
fn packing_a_tree_with_no_module_is_refused() {
    let temporary = TemporaryDirectory::new("pack-empty");
    fs::create_dir_all(temporary.path().join("prepared/content/services")).unwrap();
    fs::write(temporary.path().join("prepared/content/services/README"), b"later").unwrap();
    write_recipe(temporary.path().join("recipe.json"));

    assert!(pack(/* the same request */).is_err());
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p vmlord-display-payload --features builder builder`
Expected: FAIL — no `builder` module.

- [ ] **Step 3: Implement `pack`**

Mirror `crates/gpu-payload/src/builder.rs`: collect the files in sorted order, hash each, compose the manifest document, write the ZIP with deterministic order and fixed timestamps, hash the archive, and write the entry. The recipe is:

```json
{
  "schema_version": 1,
  "version": "0.1.0",
  "target": { "distribution": "ubuntu", "release": "24.04", "architecture": "amd64", "payload_abi": 1 },
  "proven_on": "6.8.0-137-generic",
  "protocol": { "major": 1, "min_minor": 0, "max_minor": 0 },
  "expanded_size_limit": 33554432,
  "file_count_limit": 512,
  "sources": [ { "url": "…", "commit": "…", "version": "0.1.0" } ],
  "licenses": [ { "spdx": "GPL-2.0", "path": "licenses/GPL-2.0.txt" } ]
}
```

and `payload_id` is derived, never given: `display-<distribution>-<release>-<architecture>-<version>`.

- [ ] **Step 4: Add the xtask subcommand and the dist flag**

`crates/xtask/src/display_payload.rs` copies the shape of `gpu_payload.rs`'s `parse`/`run`/`stage_release_payload`, with `--display-payload` in the `dist` parser and `LOCAL_ARCHIVE_DIRECTORY = "display-payload"` as its destination. `dist`'s argument parser accepts both flags and returns which kind each directory is:

```rust
pub(crate) enum DistPayload {
    Gpu(PathBuf),
    Display(PathBuf),
}
```

Add the alias to `.cargo/config.toml`:

```toml
display-payload = ["run", "-p", "xtask", "--", "display-payload", "pack"]
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p vmlord-display-payload --features builder && cargo test -p xtask`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/display-payload crates/xtask .cargo/config.toml
git commit -m "$(cat <<'EOF'
TASK-113: Pack and ship a display payload

`pack` writes the archive and then describes it, so an entry can never claim a
digest of something that includes the claim. `cargo dist --display-payload`
puts the pair beside the executable under the payload's own ID, the way the
GPU pair already travels.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 10: The `vmlord_drm` module sources

**Files:**
- Create: `payloads/display/module/dkms.conf.in`, `payloads/display/module/Kbuild`, `payloads/display/module/vmlord_drm.c`, `payloads/display/module/vmlord_compat.h`, `payloads/display/module/vmlord-display.conf` (modprobe.d), `payloads/display/module/vmlord-display-unbind-simpledrm.service`, `payloads/display/module/README.md`

**Interfaces:**
- Produces: the tree that becomes `content/drm/` inside a payload. `dkms.conf.in` carries `@VERSION@`, substituted at prepare time so the DKMS package version is the payload version.

This is the minimal module of the spec: one CRTC, one connector with a synthesized EDID, one primary plane, GEM shmem, atomic modesetting, PRIME export, linear XRGB8888/ARGB8888, no cursor plane, no `DRIVER_CURSOR_HOTSPOT`. Its correctness gate in this task is that it compiles against 22.04, 24.04 and 26.04 headers (Task 11) and loads far enough to create `/dev/dri/card*`. Everything about how it behaves as a desktop output is task #114.

- [ ] **Step 1: Write `dkms.conf.in` and `Kbuild`**

`dkms.conf.in`:

```
PACKAGE_NAME="vmlord-display"
PACKAGE_VERSION="@VERSION@"
BUILT_MODULE_NAME[0]="vmlord_drm"
DEST_MODULE_LOCATION[0]="/updates"
AUTOINSTALL="yes"
```

`AUTOINSTALL="yes"` is the whole of VMLord's answer to Ubuntu's unattended kernel upgrades: DKMS rebuilds the module for the new kernel with nothing on the host involved.

`Kbuild`:

```
obj-m += vmlord_drm.o
ccflags-y += -I$(src)
```

- [ ] **Step 2: Write `vmlord_compat.h`**

Exactly the four guards task #111 measured, each with the comment saying which kernel moved what:

```c
#ifndef VMLORD_COMPAT_H
#define VMLORD_COMPAT_H

#include <linux/version.h>
#include <drm/drm_plane.h>

/* Renamed from DRM_PLANE_HELPER_NO_SCALING in 6.1. */
#ifndef DRM_PLANE_NO_SCALING
#define DRM_PLANE_NO_SCALING DRM_PLANE_HELPER_NO_SCALING
#endif

/* hrtimer_setup() replaced hrtimer_init plus a function assignment in 6.15. */
#if LINUX_VERSION_CODE >= KERNEL_VERSION(6, 15, 0)
#define VMLORD_TIMER_SETUP(timer, fn) \
	hrtimer_setup((timer), (fn), CLOCK_MONOTONIC, HRTIMER_MODE_REL)
#else
#define VMLORD_TIMER_SETUP(timer, fn) do { \
		hrtimer_init((timer), CLOCK_MONOTONIC, HRTIMER_MODE_REL); \
		(timer)->function = (fn); \
	} while (0)
#endif

#endif /* VMLORD_COMPAT_H */
```

The `remove`/`remove_new` guard and the `.date` guard live at their own definition sites in `vmlord_drm.c`, because both are struct initializers and a macro around a field name reads worse than the `#if` does.

- [ ] **Step 3: Write `vmlord_drm.c`**

A single file, in this order:

1. `MODULE_LICENSE("GPL")`, `MODULE_DESCRIPTION("VMLord virtual display")`, and the module parameters `width` and `height`, defaulting to 1920x1080.
2. `struct vmlord_device { struct drm_device drm; struct drm_crtc crtc; struct drm_encoder encoder; struct drm_connector connector; struct drm_plane primary; };`
3. The connector: `vmlord_connector_get_modes` adds one mode from the module parameters through `drm_mode_duplicate`/`drm_add_modes_noedid` and sets the connector's physical size; the connector's status is always `connector_status_connected`.
4. The primary plane: `drm_atomic_helper` plane functions with `drm_gem_shmem` framebuffer helpers, the format list `{DRM_FORMAT_XRGB8888, DRM_FORMAT_ARGB8888}` and the modifier list `{DRM_FORMAT_MOD_LINEAR, DRM_FORMAT_MOD_INVALID}`, and `DRM_PLANE_NO_SCALING` for both scaling bounds.
5. The CRTC: `drm_crtc_helper_funcs` with `atomic_check` delegating to `drm_atomic_helper_check_crtc_primary_plane` and `atomic_enable`/`atomic_disable` arming and disarming vblank.
6. `drm_driver`:

```c
static struct drm_driver vmlord_drm_driver = {
	.driver_features = DRIVER_GEM | DRIVER_MODESET | DRIVER_ATOMIC,
	/* Deliberately not DRIVER_CURSOR_HOTSPOT: mutter hides the cursor plane
	 * of drivers that declare it unless they are on its allowlist. */
	.fops = &vmlord_drm_fops,
	.name = "vmlord_drm",
	.desc = "VMLord virtual display",
#if LINUX_VERSION_CODE < KERNEL_VERSION(6, 14, 0)
	/* drm_version() copies drm_driver::date unconditionally until 6.14
	 * removed the field. Left NULL it WARNs and hands userspace a NULL
	 * string, which segfaults drm_info -- and nothing catches it at build
	 * time. */
	.date = "20260821",
#endif
	DRM_GEM_SHMEM_DRIVER_OPS,
};
```

7. The platform driver, registered under the name `vmlord_drm` on the platform bus — **not** the faux bus, and with no `vkms` anywhere in the name:

```c
static struct platform_driver vmlord_platform_driver = {
	.driver = { .name = "vmlord_drm" },
	.probe = vmlord_probe,
#if LINUX_VERSION_CODE >= KERNEL_VERSION(6, 11, 0)
	.remove = vmlord_remove,
#elif LINUX_VERSION_CODE >= KERNEL_VERSION(6, 1, 0)
	.remove_new = vmlord_remove,
#else
	.remove = vmlord_remove_int,
#endif
};
```

with `vmlord_remove_int` a thin `int`-returning wrapper compiled only below 6.1.

8. `module_init`/`module_exit` that register the driver and create the single platform device.

- [ ] **Step 4: Write the two system files**

`vmlord-display.conf` for `/etc/modprobe.d`:

```
options vmlord_drm width=1920 height=1080
```

`vmlord-display-unbind-simpledrm.service`:

```ini
[Unit]
Description=Release simple-framebuffer so the VMLord display can take the console
DefaultDependencies=no
Before=vmlord-display.service display-manager.service
ConditionPathExists=/sys/bus/platform/drivers/simple-framebuffer/simple-framebuffer.0

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/bin/sh -c 'echo simple-framebuffer.0 > /sys/bus/platform/drivers/simple-framebuffer/unbind'

[Install]
WantedBy=multi-user.target
```

`simpledrm` is builtin (`CONFIG_DRM_SIMPLEDRM=y`), so blacklisting it is a no-op — unbinding is the only thing that works.

- [ ] **Step 5: Write the README**

`payloads/display/module/README.md` states what the module is, what task #111 proved, what #114 will add (cursor plane, mode list to 2560x1440, hrtimer vblank, degraded behavior), and that the only build that matters is the one in the per-release container.

- [ ] **Step 6: Commit**

```bash
git add payloads/display/module
git commit -m "$(cat <<'EOF'
TASK-113: Add the vmlord_drm module sources

VMLord's own minimal DRM output, in the shape task #111 proved: one CRTC, one
connector, a primary plane, GEM shmem, atomic modesetting, linear XRGB8888.
Not on the faux bus and with no `vkms` in its name -- 61-mutter.rules matches
on ID_PATH -- and without DRIVER_CURSOR_HOTSPOT, which mutter reads as a
reason to hide a cursor plane. The four kernel guards are the ones #111
measured, each beside what moved.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 11: Build a payload per Ubuntu release

**Files:**
- Create: `payloads/display/ubuntu-24.04-amd64/{Dockerfile,prepare.sh,payload.spec.json,README.md}`, then the same three for `ubuntu-22.04-amd64` and `ubuntu-26.04-amd64`
- Create: `payloads/display/licenses/GPL-2.0.txt`

**Interfaces:**
- Consumes: `payloads/display/module/` (Task 10), `xtask display-payload pack` (Task 9).
- Produces: `target/display-payload/prepared/` — `content/drm/`, `content/services/`, `licenses/`, `sources.json` — and, through `pack`, `payload.zip` plus `catalog-entry.json`.

The container is where "the module compiles on this release" is decided, and it decides it before an artifact exists rather than inside a guest.

- [ ] **Step 1: Write the Dockerfile for 24.04**

```dockerfile
# syntax=docker/dockerfile:1.7@sha256:a57df69d0ea827fb7266491f2813635de6f17269be881f696fbfdf2d83dda33e
#
# The frontend and the base image are pinned by digest because they are part of
# the toolchain that produces a payload, and a tag is something upstream moves.

ARG BASE=ubuntu:24.04
FROM ${BASE} AS toolchain
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential linux-headers-generic dkms jq \
    && rm -rf /var/lib/apt/lists/*

FROM toolchain AS build
ARG VERSION
COPY module /src/module
# The build is the test: a module that does not compile against this release's
# headers is an artifact that is never produced.
RUN set -eu; \
    kernel="$(ls /lib/modules | head -n1)"; \
    make -C "/lib/modules/$kernel/build" M=/src/module modules; \
    echo "$kernel" > /src/proven_on

FROM build AS layout
ARG VERSION
COPY licenses /src/licenses
RUN set -eu; \
    mkdir -p /out/content/drm /out/content/services /out/licenses; \
    make -C "/lib/modules/$(cat /src/proven_on)/build" M=/src/module clean; \
    cp /src/module/Kbuild /src/module/*.c /src/module/*.h /out/content/drm/; \
    cp /src/module/vmlord-display.conf /out/content/drm/; \
    cp /src/module/vmlord-display-unbind-simpledrm.service /out/content/drm/; \
    sed "s/@VERSION@/${VERSION}/" /src/module/dkms.conf.in > /out/content/drm/dkms.conf; \
    cp /src/licenses/GPL-2.0.txt /out/licenses/; \
    printf '{"schema_version":1,"sources":[{"url":"https://vmlord.invalid/display","commit":"%s","version":"%s"}]}\n' \
        "${COMMIT}" "${VERSION}" > /out/sources.json

FROM scratch AS output
COPY --from=layout /out /
```

`content/services/` is created empty and stays empty until task #115; `sources.json` records this repository's commit, which is what a VMLord-authored payload's provenance is.

- [ ] **Step 2: Write `prepare.sh`**

A wrapper over one `docker build`, reading `VERSION` and `COMMIT` out of `payload.spec.json` and writing the output tree with `--output type=local,dest=<output>/prepared`. It fails loudly when `docker`, `bash` 4 or the spec is missing, exactly as the GPU one does.

`payload.spec.json`:

```json
{
  "schema_version": 1,
  "version": "0.1.0",
  "target": { "distribution": "ubuntu", "release": "24.04", "architecture": "amd64", "payload_abi": 1 },
  "protocol": { "major": 1, "min_minor": 0, "max_minor": 0 },
  "expanded_size_limit": 33554432,
  "file_count_limit": 512
}
```

`proven_on` is not in the spec: it is whatever kernel the container's `linux-headers-generic` resolved to, which `prepare.sh` reads out of the built tree and passes into the recipe it writes for `pack`.

- [ ] **Step 3: Build 24.04 and confirm the module compiles**

Run: `payloads/display/ubuntu-24.04-amd64/prepare.sh --output target/display-payload`
Expected: the build succeeds and `target/display-payload/prepared/content/drm/dkms.conf` names version `0.1.0`. A compile error here is the intended failure mode of this task — fix `vmlord_drm.c` until it builds.

- [ ] **Step 4: Pack it and read the entry back**

```bash
cargo run -p xtask -- display-payload pack \
    --recipe        target/display-payload/recipe.json \
    --input         target/display-payload/prepared \
    --archive       target/display-payload/payload.zip \
    --catalog-entry target/display-payload/catalog-entry.json
```

Expected: both files written; `catalog-entry.json` has `"payload_id": "display-ubuntu-24.04-amd64-0.1.0"`.

- [ ] **Step 5: Repeat for 22.04 and 26.04**

Copy the directory, change `BASE` and the target release, and build both.
Expected: both compile. 22.04's kernel is 5.15 and 26.04's is 7.x, so this is where the four version guards earn their place — a failure here means a fifth guard is missing, and it belongs in `vmlord_compat.h` beside the others.

- [ ] **Step 6: Commit**

```bash
git add payloads/display
git commit -m "$(cat <<'EOF'
TASK-113: Build a display payload for each supported Ubuntu

Three pinned images, three builds, one version. The container build is the
proof that the module compiles on 22.04, 24.04 and 26.04: a module that does
not is an artifact that never exists, rather than a surprise inside a guest.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 12: Stage a display payload for one VM

**Files:**
- Create: `crates/platform/src/display_staging.rs`
- Modify: `crates/platform/src/layout.rs`, `crates/platform/src/lib.rs`, `crates/platform/Cargo.toml`

**Interfaces:**
- Consumes: `vmlord_display_payload::{DisplayPayloadCatalog, GuestSelector, ProtocolVersionParts, LOCAL_ARCHIVE_DIRECTORY}`, `vmlord_payload::{prepare, PrepareRequest, stage_payload, ensure_staging_root, StagedPayload, PayloadProgress}`, `vmlord_display_protocol::CURRENT_VERSION`.
- Produces: `layout::display_payload_staging_directory(vm_directory) -> PathBuf`; `platform::display_staging::{StageDisplayPayloadRequest, stage_for_vm}` returning `Result<StagedPayload, PayloadError>`.

- [ ] **Step 1: Write the failing test**

`crates/platform/src/display_staging.rs`:

```rust
#[test]
fn a_display_payload_is_staged_into_the_vms_own_child() {
    let temporary = TemporaryDirectory::new("display-staging");
    let release = fixture::release_directory(temporary.path(), "0.1.0");

    let staged = stage_for_vm(StageDisplayPayloadRequest {
        executable_directory: &release,
        cache_root: &temporary.path().join("cache"),
        vm_directory: &temporary.path().join("vm"),
        guest: GuestSelector { distribution: "ubuntu", release: "24.04", architecture: "amd64" },
        progress: &|_| {},
        cancel: &AtomicBool::new(false),
    })
    .expect("the release carries a payload for this guest");

    assert!(
        staged.generation_directory().starts_with(temporary.path().join("vm").join("display-payload")),
        "a generation lives under the VM's own display-payload child and nowhere else"
    );
}

#[test]
fn a_release_with_no_display_payload_says_which_guest_had_none() {
    let temporary = TemporaryDirectory::new("display-staging-empty");

    let error = stage_for_vm(/* the same request, against an empty release directory */)
        .expect_err("an empty release has no payload");

    assert!(matches!(error, PayloadError::NoPayloadForGuest { .. }));
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test-windows -p vmlord-platform display_staging`
Expected: FAIL — the module does not exist.

- [ ] **Step 3: Implement it**

```rust
/// Turning a catalog entry into the payload directory a VM exports.
///
/// The display twin of `gpu_staging`, and separate from it on purpose: the two
/// select differently, fail differently and mean different things to a VM.
pub fn stage_for_vm(request: StageDisplayPayloadRequest<'_>) -> Result<StagedPayload, PayloadError> {
    let catalog = DisplayPayloadCatalog::from_release_directory(request.executable_directory)?;
    let speaks = ProtocolVersionParts {
        major: vmlord_display_protocol::CURRENT_VERSION.major,
        minor: vmlord_display_protocol::CURRENT_VERSION.minor,
    };
    let entry = catalog.select_for_guest(&request.guest, speaks)?;
    let archive = vmlord_payload::release::archive_path(
        request.executable_directory,
        vmlord_display_payload::LOCAL_ARCHIVE_DIRECTORY,
        entry.payload_id(),
    );
    let ready = vmlord_payload::prepare(PrepareRequest {
        entry,
        cache_root: request.cache_root,
        archive: &archive,
        progress: request.progress,
        cancel: request.cancel,
    })?;
    let root = layout::display_payload_staging_directory(request.vm_directory);
    vmlord_payload::ensure_staging_root(&root)?;
    vmlord_payload::stage_payload(&ready, &root, request.progress, request.cancel)
}
```

and in `layout.rs`:

```rust
/// The exact per-VM directory that may hold staged display payload generations.
///
/// Beside `gpu-payload` and never inside it: the two are exported as different
/// shares, and a cleanup that removes one must not reach the other.
pub(crate) fn display_payload_staging_directory(vm_directory: &Path) -> PathBuf {
    vm_directory.join("display-payload")
}
```

The cache root is the one `gpu_payload_cache_root` already returns; rename it to `payload_cache_root` since it now serves both, and the namespacing inside it is `PayloadEntry::NAMESPACE`'s job.

- [ ] **Step 4: Run the tests**

Run: `cargo test-windows -p vmlord-platform display_staging`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/platform Cargo.toml
git commit -m "$(cat <<'EOF'
TASK-113: Stage a display payload into the VM that will mount it

The display twin of `gpu_staging`, deliberately not a mode of it: the two
catalogs select on different things and their failures mean different things
to a VM. The one cache root is shared, because a generation is addressed by
its content and the namespace inside it belongs to the payload kind.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 13: Export the display share

**Files:**
- Create: `crates/platform/src/display_exports.rs`
- Modify: `crates/core/src/display.rs`, `crates/core/src/lib.rs`, `crates/platform/src/hcs_config.rs`, `crates/platform/src/start.rs`

**Interfaces:**
- Consumes: `layout::display_payload_staging_directory` (Task 12).
- Produces: `vmlord_core::{DISPLAY_PAYLOAD_SHARE, DisplayShare}` where `DISPLAY_PAYLOAD_SHARE = "vmlord.display.payload"`; `platform::display_exports::build(vm_directory, payload: Option<&Path>, canonicalize) -> Option<Plan9Export>`; `hcs_config::apply_plan9_shares(document, exports: &[Plan9Export])`.

`apply_plan9_shares` currently takes `&GpuExports`. It becomes a slice of a small `Plan9Export { name: String, host_path: PathBuf }`, and `start.rs` composes the GPU list and the display export into it. That is the one place the two kinds of share meet, and they meet as paths in a configuration document rather than as a shared type.

- [ ] **Step 1: Write the failing test for the containment rule**

`crates/platform/src/display_exports.rs`:

```rust
#[test]
fn only_a_generation_inside_this_vms_staging_root_is_exported() {
    let temporary = TemporaryDirectory::new("display-export");
    let vm = temporary.path().join("vm");
    let staging = vm.join("display-payload");
    let generation = staging.join("generations").join("abc");
    fs::create_dir_all(&generation).unwrap();
    let elsewhere = temporary.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();

    assert!(build(&vm, Some(&generation), &canonicalize).is_some());
    assert!(build(&vm, Some(&staging), &canonicalize).is_none(), "the root holds markers, not a payload");
    assert!(build(&vm, Some(&elsewhere), &canonicalize).is_none(), "outside this VM is outside");
    assert!(build(&vm, None, &canonicalize).is_none());
}

#[test]
fn the_share_is_named_for_the_display_and_not_for_the_gpu() {
    assert_eq!(vmlord_core::DISPLAY_PAYLOAD_SHARE, "vmlord.display.payload");
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test-windows -p vmlord-platform display_exports`
Expected: FAIL.

- [ ] **Step 3: Implement the export and the core type**

`build` reuses `gpu_exports::is_within` — lift that helper into a small `paths` module both use, since it is a path comparison and not a GPU rule. The exported path must canonicalize to something strictly inside `display_payload_staging_directory(vm)` and must not be that root itself.

In `crates/core/src/display.rs`:

```rust
/// The name the display payload share is offered and mounted under.
pub const DISPLAY_PAYLOAD_SHARE: &str = "vmlord.display.payload";

/// The one share a VM's display is offered.
///
/// Its own type and not a `GpuShare` with another role: a GPU share manifest
/// that failed to attach must not be able to take the display with it, and a
/// role added to the GPU enum would be exactly that coupling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayShare {
    pub name: String,
}
```

- [ ] **Step 4: Generalize the Plan9 application**

In `hcs_config.rs`, change the parameter to `exports: &[Plan9Export]` and build the `Plan9Share` list from it. In `start.rs`, `with_gpu_shares` becomes `with_plan9_shares`, taking the GPU exports and the display export and concatenating them; the log message on failure names which kind was lost.

- [ ] **Step 5: Run the tests**

Run: `cargo test-windows -p vmlord-core -p vmlord-platform`
Expected: PASS, including the existing GPU export and HCS configuration tests.

- [ ] **Step 6: Commit**

```bash
git add crates/core crates/platform
git commit -m "$(cat <<'EOF'
TASK-113: Offer the display payload as a share of its own

`vmlord.display.payload` is exported beside the GPU shares and never among
them: what the two have in common is that HCS writes them into one Plan9
device, so that -- a list of names and paths -- is what they now share.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 14: The display messages in the agent schema

**Files:**
- Modify: `crates/agent-protocol/proto/vmlord/agent/v1/agent.proto`, `crates/agent-protocol/src/handshake.rs`, `crates/agent-protocol/proto/agent.descriptor.bin` (regenerated)

**Interfaces:**
- Produces: `Capability::Display`; `AttachDisplayPayloadRequest { DisplayShare share }` / `AttachDisplayPayloadResponse { DisplayMount mount }`; `ApplyDisplayRecipeRequest {}` / `ApplyDisplayRecipeResponse { repeated DisplayRecipeStage stages, DisplayPayloadVersions versions }`; `UpdateDisplayPayloadRequest { string target_version }` / `UpdateDisplayPayloadResponse { repeated DisplayRecipeStage stages, DisplayPayloadVersions versions, DisplayUpdateOutcome outcome }`; enums `DisplayRecipeStep`, `DisplayRecipeStageState`, `DisplayMountState`, `DisplayUpdateOutcome`; schema revision `1.5`.

`DisplayPayloadVersions { string installed, string previous, string loaded }` is what the host reads to know whether an update is available and what a rollback would land on. Empty strings mean "not present", because a proto3 scalar has no absence and a sentinel that is a version would be worse.

- [ ] **Step 1: Write the messages**

```proto
// What the guest is told about the one display share it is offered.
//
// Its own request and not a role inside AttachGpuShares: a GPU attach that
// fails must not be able to take the display down with it.
message AttachDisplayPayloadRequest {
  DisplayShare share = 1;
}

message DisplayShare {
  string name = 1;
}

message AttachDisplayPayloadResponse {
  DisplayMount mount = 1;
}

message DisplayMount {
  string name = 1;
  string mount_point = 2;
  DisplayMountState state = 3;
  string message = 4;
}

enum DisplayMountState {
  DISPLAY_MOUNT_STATE_UNSPECIFIED = 0;
  DISPLAY_MOUNT_STATE_MOUNTED = 1;
  DISPLAY_MOUNT_STATE_ALREADY_MOUNTED = 2;
  DISPLAY_MOUNT_STATE_FAILED = 3;
  DISPLAY_MOUNT_STATE_REFUSED = 4;
}

// Everything the guest needs in order to decide is in the guest or in the
// payload it mounted one message earlier, so this request carries nothing.
message ApplyDisplayRecipeRequest {}

message ApplyDisplayRecipeResponse {
  repeated DisplayRecipeStage stages = 1;
  DisplayPayloadVersions versions = 2;
}

message UpdateDisplayPayloadRequest {
  // The version the mounted payload is expected to carry. The guest refuses
  // rather than installs when the mount says something else: an update to a
  // version nobody asked for is worse than an update that did not happen.
  string target_version = 1;
}

message UpdateDisplayPayloadResponse {
  repeated DisplayRecipeStage stages = 1;
  DisplayPayloadVersions versions = 2;
  DisplayUpdateOutcome outcome = 3;
}

enum DisplayUpdateOutcome {
  DISPLAY_UPDATE_OUTCOME_UNSPECIFIED = 0;
  DISPLAY_UPDATE_OUTCOME_UPDATED = 1;
  DISPLAY_UPDATE_OUTCOME_ROLLED_BACK = 2;
  DISPLAY_UPDATE_OUTCOME_FAILED = 3;
}

// An empty string is "not present": proto3 scalars have no absence, and a
// sentinel that looks like a version would be read as one.
message DisplayPayloadVersions {
  string installed = 1;
  string previous = 2;
  string loaded = 3;
}

message DisplayRecipeStage {
  DisplayRecipeStep step = 1;
  DisplayRecipeStageState state = 2;
  string message = 3;
}

enum DisplayRecipeStep {
  DISPLAY_RECIPE_STEP_UNSPECIFIED = 0;
  DISPLAY_RECIPE_STEP_DISTRIBUTION = 1;
  DISPLAY_RECIPE_STEP_PAYLOAD = 2;
  DISPLAY_RECIPE_STEP_BUILD_DEPENDENCIES = 3;
  DISPLAY_RECIPE_STEP_MODULE_SOURCE = 4;
  DISPLAY_RECIPE_STEP_MODULE_BUILD = 5;
  DISPLAY_RECIPE_STEP_MODULE_LOAD = 6;
  DISPLAY_RECIPE_STEP_DEVICE = 7;
  DISPLAY_RECIPE_STEP_SERVICES = 8;
  DISPLAY_RECIPE_STEP_SERVICES_START = 9;
}

enum DisplayRecipeStageState {
  DISPLAY_RECIPE_STAGE_STATE_UNSPECIFIED = 0;
  DISPLAY_RECIPE_STAGE_STATE_OK = 1;
  DISPLAY_RECIPE_STAGE_STATE_SKIPPED = 2;
  DISPLAY_RECIPE_STAGE_STATE_FAILED = 3;
}
```

Add the three requests and three responses to the `Request.kind` and `Response.kind` oneofs with new field numbers, and `CAPABILITY_DISPLAY` to `Capability`.

- [ ] **Step 2: Raise the schema revision**

In `crates/agent-protocol/src/handshake.rs`, `CURRENT_VERSION` becomes `{ major: 1, minor: 5 }`, with the comment stating what 1.5 added.

- [ ] **Step 3: Write the failing test**

`crates/agent-protocol/tests/` gains:

```rust
#[test]
fn a_display_capable_agent_and_host_settle_on_the_display_revision() {
    let settled = negotiate_version(
        ProtocolVersion { major: 1, minor: 5 },
        ProtocolVersion { major: 1, minor: 4 },
    )
    .expect("one major, so they negotiate");

    assert_eq!(settled.minor, 4, "the older side never sees a message it has no field for");
}

#[test]
fn the_display_capability_is_its_own() {
    assert_ne!(i32::from(Capability::Display), i32::from(Capability::Gpu));
}
```

- [ ] **Step 4: Run and regenerate**

Run: `cargo test -p vmlord-agent-protocol`
Expected: PASS, and `proto/agent.descriptor.bin` is regenerated by the build script — commit the changed binary so a wire-format change is visible in a diff.

- [ ] **Step 5: Commit**

```bash
git add crates/agent-protocol
git commit -m "$(cat <<'EOF'
TASK-113: Add the display payload messages to the agent schema

Three requests of their own -- attach, apply, update -- rather than roles and
fields inside the GPU ones, so that the two stacks cannot fail together. The
schema gains messages and enum values only, so this is revision 1.5.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 15: The guest recipe's decisions

**Files:**
- Create: `crates/agent/src/display_recipe.rs`
- Modify: `crates/agent/src/main.rs`

**Interfaces:**
- Consumes: `vmlord_agent_protocol::v1::{DisplayRecipeStage, DisplayRecipeStageState, DisplayRecipeStep, DisplayPayloadVersions}` (Task 14).
- Produces: `display_recipe::{STEPS, Report, PayloadFacts, read_payload_facts, InstalledVersions, dkms_versions, module_is_loaded, needs_build, DkmsPackage}`.

Everything here is a function of text — `/etc/os-release`, the payload's `payload.json`, `dkms status`, `/proc/modules`, `/sys/class/drm` — which is what makes the recipe's decisions testable in WSL on a machine that is neither Ubuntu nor a Hyper-V guest. The effects live in Task 16.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn dkms_status_yields_every_installed_version_of_our_package() {
    let status = "\
vmlord-display/0.1.0, 6.8.0-137-generic, x86_64: installed
vmlord-display/0.2.0, 6.8.0-137-generic, x86_64: installed
other-module/1.0, 6.8.0-137-generic, x86_64: installed";

    let versions = dkms_versions(status, "vmlord-display");

    assert_eq!(versions, vec!["0.1.0".to_owned(), "0.2.0".to_owned()]);
}

#[test]
fn a_guest_that_already_runs_the_payloads_version_needs_no_build() {
    let installed = InstalledVersions { versions: vec!["0.1.0".into()], loaded: Some("0.1.0".into()) };

    assert!(!needs_build(&installed, "0.1.0", true), "same version, loaded, device present");
    assert!(needs_build(&installed, "0.2.0", true), "a newer payload is a build");
    assert!(
        needs_build(&installed, "0.1.0", false),
        "a kernel upgrade that left the module unbuilt shows up as no device"
    );
}

#[test]
fn a_payload_json_says_which_version_is_mounted() {
    let facts = read_payload_facts(
        br#"{"schema_version":1,"payload_id":"display-ubuntu-24.04-amd64-0.1.0","version":"0.1.0",
             "target":{"distribution":"ubuntu","release":"24.04","architecture":"amd64","payload_abi":1},
             "files":[]}"#,
    )
    .expect("a payload.json parses");

    assert_eq!(facts.version, "0.1.0");
    assert_eq!(facts.package, "vmlord-display");
}

#[test]
fn a_report_names_every_step_even_the_ones_that_never_ran() {
    let mut report = Report::new();
    report.ok(DisplayRecipeStep::Distribution, "ubuntu 24.04 amd64");
    report.failed(DisplayRecipeStep::ModuleBuild, "dkms build failed");

    let stages = report.finish("the recipe stopped before this stage");

    assert_eq!(stages.len(), STEPS.len());
    assert_eq!(stages[0].step(), DisplayRecipeStep::Distribution);
    assert!(stages.iter().all(|stage| !stage.message.is_empty()));
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test -p vmlord-agent display_recipe`
Expected: FAIL — no module.

- [ ] **Step 3: Implement**

`Report` is `gpu_recipe::Report` with the display step enum — same "first answer wins", same `finish` filling in the untouched steps in `STEPS` order. `dkms_versions` parses `name/version, kernel, arch: state` lines and keeps the versions of the package named. `needs_build(installed, wanted, device_present)` is true unless the wanted version is installed **and** loaded **and** the device is present.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p vmlord-agent display_recipe`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/agent
git commit -m "$(cat <<'EOF'
TASK-113: Decide the display recipe from text alone

What is installed, what is loaded, what the payload carries and whether
anything needs building are all readable out of files and command output, so
they are decided in a module that needs neither Ubuntu nor a Hyper-V guest to
be tested.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 16: The guest recipe's effects

**Files:**
- Create: `crates/agent/src/display_kernel.rs`
- Modify: `crates/agent/src/main.rs`

**Interfaces:**
- Consumes: `display_recipe` (Task 15), `command::run` (existing), `gpu_kernel::{copy_tree, write_if_different, read}` — lift those three into a small `guest_files` module, since they are file operations rather than GPU ones.
- Produces: `display_kernel::{apply, update, versions, device_is_usable, PAYLOAD_MOUNT}`, where `apply(stopping: &AtomicBool) -> (Vec<DisplayRecipeStage>, DisplayPayloadVersions)` and `update(target_version: &str, stopping: &AtomicBool) -> (Vec<DisplayRecipeStage>, DisplayPayloadVersions, DisplayUpdateOutcome)`.

Budgets are the GPU recipe's: 300 s for apt, 900 s for a build, 30 s for everything else, each in a process group of its own, with the shutdown flag checked between stages.

- [ ] **Step 1: Write the failing tests for what can be tested without a guest**

```rust
#[test]
fn the_source_tree_is_versioned_so_two_versions_can_coexist() {
    assert_eq!(
        source_directory("0.2.0"),
        PathBuf::from("/usr/src/vmlord-display-0.2.0"),
        "a rollback is only free while the previous tree is still there"
    );
}

#[test]
fn the_recipe_stops_when_the_guest_is_going_down() {
    let stopping = AtomicBool::new(true);

    let (stages, _) = apply(&stopping);

    assert!(
        stages.iter().all(|stage| stage.state() != DisplayRecipeStageState::Ok),
        "systemd is holding the guest open for this process to exit"
    );
}

#[test]
fn an_update_to_a_version_the_mount_does_not_carry_is_refused() {
    // The mount is whatever the host staged; a guest asked for something else
    // must not install what it happens to have.
    let (stages, _, outcome) = update("9.9.9", &AtomicBool::new(false));

    assert_eq!(outcome, DisplayUpdateOutcome::Failed);
    assert!(
        stages.iter().any(|stage| stage.step() == DisplayRecipeStep::Payload
            && stage.state() == DisplayRecipeStageState::Failed)
    );
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test -p vmlord-agent display_kernel`
Expected: FAIL.

- [ ] **Step 3: Implement `apply`**

The stages, in `STEPS` order:

- `DISTRIBUTION` — `guest_facts()`; a distribution with no recipe is skipped with the reason and ends the run.
- `PAYLOAD` — `/opt/vmlord/display-payload/payload.json` is read, parsed, and **every file it declares is hashed and compared** before anything is copied. A mismatch fails the stage: the mount is a filesystem the host can rewrite between its own verification and this one.
- Short circuit — if `!needs_build(...)`, the three build stages are skipped with "version X is installed, loaded and answering".
- `BUILD_DEPENDENCIES` — `apt-get install -y dkms build-essential linux-headers-$(uname -r)`, only when they are not all present.
- `MODULE_SOURCE` — `copy_tree("/opt/vmlord/display-payload/content/drm", "/usr/src/vmlord-display-<version>")`. Already-identical is `SKIPPED`, not `OK`.
- `MODULE_BUILD` — `dkms status`, then `dkms add`/`build`/`install` for this version and the running kernel; a failure carries the tail of `make.log`.
- `MODULE_LOAD` — write `/etc/modules-load.d/vmlord-display.conf`, install and enable `vmlord-display-unbind-simpledrm.service` and the modprobe.d file from the payload, then `modprobe vmlord_drm`.
- `DEVICE` — a `/dev/dri/card*` whose `/sys/class/drm/card*/device/driver` link resolves to `vmlord_drm`.
- `SERVICES`, `SERVICES_START` — skipped with "this payload carries no display services" while `content/services/` is empty.

- [ ] **Step 4: Implement `update`**

1. `PAYLOAD` — as above, and additionally the mounted `version` must equal `target_version`, or the update fails here having changed nothing.
2. Record what is installed and loaded now — that is what a rollback returns to.
3. `MODULE_SOURCE`, `MODULE_BUILD` for the new version. The previous `/usr/src` tree is **not** removed.
4. `MODULE_LOAD` — `modprobe -r vmlord_drm` then `modprobe vmlord_drm`.
5. `DEVICE` plus the version of the loaded module: this is the health verification, and while `content/services/` is empty the services half of it is a skipped stage rather than a pass.
6. On any failure from step 3 on: roll back — `modprobe -r`, `dkms remove` the new version, `modprobe` again, and re-check the device. Outcome `RolledBack` when the old version is running again, `Failed` when it is not.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p vmlord-agent && cargo agent`
Expected: PASS, and the musl build succeeds.

- [ ] **Step 6: Commit**

```bash
git add crates/agent
git commit -m "$(cat <<'EOF'
TASK-113: Install, rebuild and update the display module in the guest

Installation reconciles: the same version installed, loaded and answering
costs a few checks and no network, and a kernel upgrade that DKMS did not
carry shows up as a missing device and is rebuilt. An update is only ever
explicit, verifies what it installed, and leaves the previous source tree in
place until it has -- which is what makes a rollback a modprobe and a
`dkms remove` rather than a download.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 17: Serve the display requests in the agent

**Files:**
- Modify: `crates/agent/src/session.rs`, `crates/agent/src/main.rs`

**Interfaces:**
- Consumes: `display_kernel::{apply, update}` (Task 16).
- Produces: the agent answers `AttachDisplayPayload`, `ApplyDisplayRecipe` and `UpdateDisplayPayload`, and advertises `Capability::Display` in its hello.

- [ ] **Step 1: Write the failing test**

`crates/agent/src/session.rs` tests, in the style of the GPU ones:

```rust
#[test]
fn the_agent_advertises_the_display_capability() {
    let hello = hello_response();

    assert!(hello.capabilities.contains(&i32::from(Capability::Display)));
}

#[test]
fn a_display_payload_share_is_mounted_at_the_path_the_agent_chooses() {
    // The host names the share; where it lands in the guest is the guest's
    // business, and the response says where that was.
    let response = handle(request::Kind::AttachDisplayPayload(AttachDisplayPayloadRequest {
        share: Some(DisplayShare { name: "vmlord.display.payload".into() }),
    }));

    let Some(response::Kind::AttachDisplayPayload(attached)) = response.kind else {
        panic!("the agent must answer an attach with an attach");
    };
    assert_eq!(attached.mount.unwrap().mount_point, "/opt/vmlord/display-payload");
}

#[test]
fn a_share_by_another_name_is_refused_rather_than_mounted() {
    let response = handle(request::Kind::AttachDisplayPayload(AttachDisplayPayloadRequest {
        share: Some(DisplayShare { name: "vmlord.gpu.payload".into() }),
    }));

    let Some(response::Kind::AttachDisplayPayload(attached)) = response.kind else { panic!() };
    assert_eq!(attached.mount.unwrap().state(), DisplayMountState::Refused);
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p vmlord-agent session`
Expected: FAIL.

- [ ] **Step 3: Implement the three handlers**

Mount with the same 9p options the GPU mounts use (`trans=virtio`, `version=9p2000.L`, read-only, `aname=` the share's name), report `AlreadyMounted` when `/proc/self/mountinfo` already has it, and refuse any share whose name is not `vmlord.display.payload` — the guest must not mount whatever it is handed. `ApplyDisplayRecipe` and `UpdateDisplayPayload` run inline in the session the way the GPU recipe does, with the shutdown flag passed in.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p vmlord-agent && cargo agent`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/agent
git commit -m "$(cat <<'EOF'
TASK-113: Answer the display payload requests in the guest agent

Mount, apply, update -- inline in the session, as the GPU recipe already runs,
and refusing a share by any other name: a guest that mounts whatever it is
handed is a guest with no boundary at all.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 18: Ask the guest, and record what it said

**Files:**
- Modify: `crates/platform/src/agent_session.rs`, `crates/platform/src/start.rs`

**Interfaces:**
- Consumes: `display_staging::stage_for_vm` (Task 12), `display_exports::build` (Task 13), the schema (Task 14).
- Produces: a session that, for a VM whose profile wants a desktop, offers the display share, asks for the display recipe, and turns the answer into `VmDisplayFacts` — including `DisplayPayloadFacts` (Task 19).

- [ ] **Step 1: Write the failing test against the stub agent**

```rust
#[test]
fn a_desktop_vm_is_offered_its_display_payload_and_asked_to_apply_it() {
    let agent = StubAgent::speaking(&[Capability::Gpu, Capability::Display]);

    run_session(&agent, mapping_with(DesktopProfile::Gnome));

    assert!(agent.saw_request(|kind| matches!(kind, request::Kind::AttachDisplayPayload(_))));
    assert!(agent.saw_request(|kind| matches!(kind, request::Kind::ApplyDisplayRecipe(_))));
}

#[test]
fn a_headless_vm_is_asked_nothing_about_a_display() {
    let agent = StubAgent::speaking(&[Capability::Gpu, Capability::Display]);

    run_session(&agent, mapping_with(DesktopProfile::Headless));

    assert!(!agent.saw_request(|kind| matches!(kind, request::Kind::AttachDisplayPayload(_))));
}

#[test]
fn an_agent_that_does_not_speak_display_is_not_sent_display_requests() {
    let agent = StubAgent::speaking(&[Capability::Gpu]);

    run_session(&agent, mapping_with(DesktopProfile::Gnome));

    assert!(!agent.saw_request(|kind| matches!(kind, request::Kind::AttachDisplayPayload(_))));
}

#[test]
fn a_failed_display_recipe_leaves_the_session_and_the_gpu_alone() {
    let agent = StubAgent::speaking(&[Capability::Gpu, Capability::Display])
        .answering_display_recipe_with_failure("dkms build failed");

    let outcome = run_session(&agent, mapping_with(DesktopProfile::Gnome));

    assert!(outcome.session_completed, "a display that will not build is not a session that ends");
    assert!(agent.saw_request(|kind| matches!(kind, request::Kind::ProbeGpu(_))));
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test-windows -p vmlord-platform agent_session`
Expected: FAIL.

- [ ] **Step 3: Implement**

Add `attach_display_payload` and `apply_display_recipe` beside the GPU functions, gated on `Capability::Display` and on the VM's stored `DesktopProfile::wants_desktop()`. Sequence them after the GPU requests of the same session: the display recipe builds a module, and a session that spends its first minutes on it delays the GPU answer the UI is already waiting for. Report the stages at the volume the GPU stages are reported at, and turn the answer into facts through the sink the display status reads.

In `start.rs`, stage the display payload before the session for a VM that wants a desktop, and hand the export to `with_plan9_shares`. A staging failure logs and records the cause; it never fails the start.

- [ ] **Step 4: Run the tests**

Run: `cargo test-windows -p vmlord-platform`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/platform
git commit -m "$(cat <<'EOF'
TASK-113: Offer and apply the display payload in the agent session

After the GPU requests of the same session, because a module build is minutes
long and the GPU answer is already being waited on. A display that will not
build ends nothing: not the session, not the probe that follows it, and not
the VM.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 19: The payload half of the display model

**Files:**
- Modify: `crates/core/src/display.rs`, `crates/core/src/lib.rs`

**Interfaces:**
- Produces: `DisplayStage::Payload`; the payload codes on `DisplayStatusCode`; `DisplayPayloadFacts { installed: Option<String>, previous: Option<String>, loaded: Option<String>, available: Option<String> }` on `VmDisplayFacts`; `VmDisplayStatus::{running_version, available_version}`.

New codes, each with its serialized name, because these are what logs, tests and troubleshooting are indexed by:

| Variant | Serialized | Retryable |
| --- | --- | --- |
| `PayloadMissing` | `display-payload-missing` | no |
| `PayloadInvalid` | `display-payload-invalid` | no |
| `PayloadDependenciesFailed` | `display-payload-dependencies-failed` | yes |
| `PayloadBuildFailed` | `display-payload-build-failed` | yes |
| `PayloadModuleNotLoaded` | `display-payload-module-not-loaded` | yes |
| `PayloadNoDevice` | `display-payload-no-device` | yes |
| `PayloadUpdateRolledBack` | `display-payload-update-rolled-back` | yes |
| `PayloadUpdateFailed` | `display-payload-update-failed` | yes |

`PayloadMissing` is not retryable: a release that carries no payload for this guest will carry none on the next start either.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_release_with_no_payload_for_this_guest_is_not_worth_retrying() {
    assert!(!DisplayStatusCode::PayloadMissing.is_retryable());
    assert!(!DisplayStatusCode::PayloadInvalid.is_retryable());
}

#[test]
fn a_build_that_failed_is_worth_another_attempt() {
    for code in [
        DisplayStatusCode::PayloadDependenciesFailed,
        DisplayStatusCode::PayloadBuildFailed,
        DisplayStatusCode::PayloadModuleNotLoaded,
        DisplayStatusCode::PayloadNoDevice,
    ] {
        assert!(code.is_retryable(), "{code} should be retryable");
    }
}

#[test]
fn every_code_serializes_as_the_string_it_logs() {
    let code = DisplayStatusCode::PayloadUpdateRolledBack;
    assert_eq!(code.as_str(), "display-payload-update-rolled-back");
    assert_eq!(
        serde_json::to_string(&code).unwrap(),
        "\"display-payload-update-rolled-back\""
    );
}

#[test]
fn a_stage_of_its_own_because_a_payload_is_not_a_provisioning() {
    assert_eq!(DisplayStage::Payload.as_str(), "payload");
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test -p vmlord-core display`
Expected: FAIL.

- [ ] **Step 3: Implement**

Add the variants, extend `as_str`, `is_retryable` and the `Serialize` renames, add `DisplayStage::Payload`, add `DisplayPayloadFacts` to `VmDisplayFacts`, and the two version fields to `VmDisplayStatus`. Keep the doc comments in the register the module already uses: what the variant means, and why it is not the neighbouring one.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p vmlord-core`
Expected: PASS — including the #112 tests, unchanged.

- [ ] **Step 5: Commit**

```bash
git add crates/core
git commit -m "$(cat <<'EOF'
TASK-113: Give the display payload its own stage and its own causes

A payload that will not build and a desktop that will not install are both a
degraded display and are not the same problem, so they get different codes and
a stage that says which half was speaking. A release with no payload for this
guest is the one cause here that a retry cannot get past.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 20: Derive the payload half of the status

**Files:**
- Modify: `crates/app/src/display.rs`

**Interfaces:**
- Consumes: Task 19's types.
- Produces: `derive_status` reading `DisplayPayloadFacts`, and `VmDisplayStatus` carrying the running and available versions.

Order of reading, and it matters: the profile first (a headless VM has nothing to say), then the VM's state, then provisioning (a desktop that never installed is not a payload problem), then the payload, then the guest's services. A payload failure on a VM whose desktop never installed must read as the desktop's failure, because that is the one a person can act on.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_payload_that_would_not_build_is_degraded_and_says_so() {
    let facts = VmDisplayFacts {
        payload: DisplayPayloadFacts {
            installed: None,
            previous: None,
            loaded: None,
            available: Some("0.1.0".into()),
        },
        failure: Some(DisplayFailure::new(
            DisplayStage::Payload,
            DisplayStatusCode::PayloadBuildFailed,
            "dkms build failed for kernel 6.8.0-137-generic",
        )),
        ..VmDisplayFacts::default()
    };

    let status = derive_status(
        DesktopProfile::Gnome,
        &DisplayProvisioning::Ready,
        VmState::Running,
        &facts,
        SystemTime::UNIX_EPOCH,
    );

    assert_eq!(status.state, DisplayState::Degraded);
    assert_eq!(status.stage, DisplayStage::Payload);
    assert_eq!(status.code, DisplayStatusCode::PayloadBuildFailed);
    assert!(status.can_retry);
}

#[test]
fn a_newer_payload_in_the_release_is_offered_and_not_applied() {
    let facts = VmDisplayFacts {
        payload: DisplayPayloadFacts {
            installed: Some("0.1.0".into()),
            previous: None,
            loaded: Some("0.1.0".into()),
            available: Some("0.2.0".into()),
        },
        guest: Some(GuestDisplayReport::Ready(GuestDisplayDetail::default())),
        ..VmDisplayFacts::default()
    };

    let status = derive_status(/* Gnome, Ready, Running, &facts, now */);

    assert_eq!(status.state, DisplayState::Ready, "an offer is not a degradation");
    assert_eq!(status.running_version.as_deref(), Some("0.1.0"));
    assert_eq!(status.available_version.as_deref(), Some("0.2.0"));
}

#[test]
fn a_desktop_that_never_installed_reads_as_the_desktop_and_not_the_payload() {
    let facts = VmDisplayFacts { payload: DisplayPayloadFacts::default(), ..VmDisplayFacts::default() };

    let status = derive_status(
        DesktopProfile::Gnome,
        &DisplayProvisioning::Degraded(DisplayFailure::new(
            DisplayStage::Provisioning,
            DisplayStatusCode::PackageDownloadFailed,
            "could not reach archive.ubuntu.com",
        )),
        VmState::Running,
        &facts,
        SystemTime::UNIX_EPOCH,
    );

    assert_eq!(status.code, DisplayStatusCode::PackageDownloadFailed);
}

#[test]
fn a_rolled_back_update_is_a_working_display() {
    let facts = VmDisplayFacts {
        payload: DisplayPayloadFacts {
            installed: Some("0.1.0".into()),
            previous: None,
            loaded: Some("0.1.0".into()),
            available: Some("0.2.0".into()),
        },
        guest: Some(GuestDisplayReport::Ready(GuestDisplayDetail::default())),
        failure: Some(DisplayFailure::new(
            DisplayStage::Payload,
            DisplayStatusCode::PayloadUpdateRolledBack,
            "0.2.0 did not verify; 0.1.0 is running",
        )),
        ..VmDisplayFacts::default()
    };

    let status = derive_status(/* Gnome, Ready, Running, &facts, now */);

    assert_eq!(status.state, DisplayState::Ready, "the display works; the update did not");
    assert_eq!(status.code, DisplayStatusCode::PayloadUpdateRolledBack);
    assert!(status.message.contains("0.1.0"));
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test -p vmlord-app display`
Expected: FAIL.

- [ ] **Step 3: Implement**

Insert the payload reading between provisioning and the guest report, with the rolled-back case as the one payload failure that does not degrade the state. `VmDisplayFacts` gains a `failure: Option<DisplayFailure>` for what the payload half reported, since the guest report's `Failed` variant is about services rather than about a module.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p vmlord-app && cargo check-windows`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/app
git commit -m "$(cat <<'EOF'
TASK-113: Read the payload into the display status

Between the provisioning and the guest, so a desktop that never installed
reads as a desktop problem rather than as a payload one. A newer version in
the release is an offer beside a working display, and an update that rolled
back is a working display that says what happened -- neither is Degraded.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 21: The explicit update, end to end

**Files:**
- Create: `crates/platform/src/display_update.rs`
- Modify: `crates/core/src/lib.rs`, `crates/platform/src/repository.rs`, `crates/app/src/lib.rs`, `crates/legacy-backend/src/windows.rs`

**Interfaces:**
- Consumes: `display_staging::stage_for_vm` (Task 12), `UpdateDisplayPayload` (Task 14), the codes of Task 19.
- Produces: `VmRepository::update_display_payload(&mut self, name: &str) -> Result<(), RepositoryError>` with a default implementation that refuses (the legacy backend cannot do it); `platform::display_update::run`; `WorkspaceApp::update_display_payload(&mut self, name: &str)`.

Progress is the host's `PayloadProgress` while it selects, verifies and stages, and one long guest stage after that. The guest's answer carries the outcome, and the host turns it into a status: `Updated` clears the payload failure, `RolledBack` records `PayloadUpdateRolledBack` with the version that is running, `Failed` records `PayloadUpdateFailed` and degrades.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn an_update_stages_the_new_version_before_it_asks_the_guest() {
    let host = TestHost::with_release(&["0.1.0", "0.2.0"]).with_guest_running("0.1.0");

    run(&host.request("vm")).expect("0.2.0 is available");

    assert!(host.staged_generations().iter().any(|version| version == "0.2.0"));
    assert_eq!(host.guest_requests().last().unwrap().target_version, "0.2.0");
}

#[test]
fn a_stopped_vm_cannot_be_updated() {
    let host = TestHost::with_release(&["0.1.0", "0.2.0"]).with_vm_stopped();

    let error = run(&host.request("vm")).expect_err("there is nobody to ask");

    assert!(error.to_string().contains("running"));
}

#[test]
fn an_update_that_did_not_verify_is_recorded_as_rolled_back() {
    let host = TestHost::with_release(&["0.1.0", "0.2.0"])
        .with_guest_running("0.1.0")
        .answering_update(DisplayUpdateOutcome::RolledBack);

    let outcome = run(&host.request("vm")).expect("a rollback is an answer, not an error");

    assert_eq!(outcome.code, DisplayStatusCode::PayloadUpdateRolledBack);
    assert_eq!(outcome.running_version.as_deref(), Some("0.1.0"));
}

#[test]
fn an_update_that_left_nothing_running_degrades_the_display() {
    let host = TestHost::with_release(&["0.1.0", "0.2.0"])
        .with_guest_running("0.1.0")
        .answering_update(DisplayUpdateOutcome::Failed);

    let outcome = run(&host.request("vm")).expect("the guest answered");

    assert_eq!(outcome.code, DisplayStatusCode::PayloadUpdateFailed);
}

#[test]
fn an_update_with_nothing_newer_is_refused_before_anything_is_staged() {
    let host = TestHost::with_release(&["0.1.0"]).with_guest_running("0.1.0");

    assert!(run(&host.request("vm")).is_err());
    assert!(host.staged_generations().is_empty());
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test-windows -p vmlord-platform display_update`
Expected: FAIL.

- [ ] **Step 3: Implement**

`run` selects the best entry, compares its version with what the guest reported, refuses when there is nothing newer or the VM is not running, stages the generation (progress reported through the existing publisher), re-exports the share for the running VM, sends `UpdateDisplayPayloadRequest { target_version }`, and maps the response's outcome onto a status. The VM's stored `DisplayProvisioning` is not touched: a payload update is not a desktop installation.

- [ ] **Step 4: Wire the application layer**

`WorkspaceApp::update_display_payload` logs the action through `VmAction`, calls the repository, and refreshes. The legacy backend's implementation returns the same "not supported by this backend" refusal `open_display` does.

- [ ] **Step 5: Run everything**

Run: `cargo test-windows -p vmlord-platform -p vmlord-app -p vmlord-core && cargo check-windows`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/platform crates/core crates/app crates/legacy-backend
git commit -m "$(cat <<'EOF'
TASK-113: Update a display payload on request, and step back when it fails

Nothing upgrades on a start: a newer version in the release is an offer, and
moving to it is an action with progress that ends in a verification. What the
guest answers is what the status becomes -- updated, rolled back onto the
version that was already working, or degraded because neither version is.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 22: Record the display payload in the architecture

**Files:**
- Modify: `ARCHITECTURE.md`
- Modify: `docs/superpowers/specs/2026-08-21-display-payload-design.md` (only if the implementation departed from it)

**Interfaces:**
- Consumes: everything above.

- [ ] **Step 1: Write the sections**

After `### Desktop profile and display provisioning`, add, in the register the surrounding document uses — what was decided and why, not a list of types:

- **Display: the guest payload** — what the artifact is, the entry's version/`proven_on`/protocol range, selection (triple hard, protocol range as a filter, greatest version), the two verifications, the share and its mount point, and why the share and the agent messages are the display's own rather than roles inside GPU's.
- **Display: the guest's recipe** — the stages, the short circuit, what a kernel upgrade does, what a failure means (`Degraded`, never a failed start), and that the services stages are skipped until #115.
- **Display: updating and rolling back** — installation is automatic and idempotent, versions never are; where progress comes from on each side; what health verification checks; the one-step rollback and why a successful rollback is not `Degraded`.
- **The payload crates** — `vmlord-payload` as the shared mechanism with `PayloadEntry` as its whole contract, and the two thin crates above it.

Add the display payload pair to the release layout paragraph beside the GPU pair.

- [ ] **Step 2: Verify the whole workspace**

Run: `cargo test -p vmlord-payload -p vmlord-display-payload -p vmlord-gpu-payload -p vmlord-core -p vmlord-app -p vmlord-agent-protocol && cargo test -p vmlord-agent && cargo test-windows && cargo check-windows && cargo agent`
Expected: all green. Record the actual output in the commit's body if anything is skipped.

- [ ] **Step 3: Commit**

```bash
git add ARCHITECTURE.md docs/superpowers/specs
git commit -m "$(cat <<'EOF'
TASK-113: Record the display payload in the architecture

What the artifact is, how it is chosen and verified, what the guest does with
it, and why an update is the only thing here that is not automatic.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Self-review

**Spec coverage.** Whole guest side in one artifact — Tasks 8, 10, 11 (`content/services/` present and empty). Own semver and protocol range — Tasks 6, 7. Extraction into `vmlord-payload` — Tasks 1–5. VMLord's own module — Task 10. Automatic idempotent install, explicit updates — Tasks 16, 21. Health verification and one-step rollback — Tasks 16, 21. Degraded and never a failed start — Tasks 12, 18, 19, 20. `proven_on` never a selector — Tasks 6, 7. Signed manifest prepared but not implemented — no task, deliberately, and Task 22 keeps that statement in the documentation. Two-sided verification — Task 8 (host) and Task 16 (guest). Three release artifacts — Task 11. Own share and own agent messages — Tasks 13, 14, 17, 18. Statuses inside the #112 model — Tasks 19, 20. Testing strategy — the tests are in the tasks; the container build is the module's compile gate in Task 11.

**Placeholders.** None: every step names its files, its command and its expected result, and every code step carries the code.

**Type consistency.** `PayloadEntry`/`PayloadFiles`/`ReadyPayload<E>`/`StagedPayload` are defined in Tasks 3–4 and used under those names in Tasks 8, 12; `DisplayCatalogEntry`, `PayloadVersion`, `ProtocolRange`, `ProtocolVersionParts`, `GuestSelector` are defined in Tasks 6–7 and used in Tasks 8, 9, 12; `DisplayPayloadVersions`, `DisplayRecipeStep`, `DisplayUpdateOutcome` are defined in Task 14 and used in Tasks 15–18, 21; `DisplayPayloadFacts` and the payload status codes are defined in Task 19 and used in Tasks 18, 20, 21. `display_payload_staging_directory` (Task 12) is the name used in Task 13.

**One addition to the spec found during review:** the display catalog's uniqueness rule — one entry per payload ID and per (target, version), rather than GPU's one entry per target — is stated in Task 7 and belongs in the spec's *Selection* section. Add it there when Task 7 lands.
