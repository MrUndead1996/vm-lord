# GPU-PV Guest Payload Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build task 93's portable, versioned GPU payload boundary: validated catalogs and manifests, bounded host-side download, content-addressed cache readiness, safe archive extraction, deterministic provenance, per-VM staging, and a read-only Plan9 payload share contract.

**Architecture:** A new platform-neutral `vmlord-gpu-payload` crate owns payload identity, validation, download, extraction, cache, and staging. `xtask` uses an opt-in builder module from that crate to turn a prepared directory into a deterministic ZIP and complete catalog entry. Existing `core` and `platform` gain only the logical `GpuPayload` share and validation of one exact VMLord-managed staging directory; task 94 will put that role on the wire, while tasks 95 and 96 will supply the first production Ubuntu/DKMS/Mesa recipe and catalog entry.

**Tech Stack:** Rust 2024, `serde`/`serde_json`, `sha2` 0.11, `ureq` 3.4 with the existing Rustls/platform-verifier feature set, `url` 2, and `zip` 9 with default features disabled and only `deflate-flate2-zlib-rs` plus `time` enabled. The selected ZIP configuration has a pure-Rust compression backend and adds no system C dependency.

**Spec:** `docs/superpowers/specs/2026-08-14-gpu-payload-design.md`

## Global Constraints

* Work on branch `task-93-gpu-payload-design`; every implementation commit subject starts with `TASK-93: `.
* All new application and build-automation code is Rust. Do not invoke PowerShell, WMI, WSL, `tar.exe`, or another external unpacker.
* The guest never downloads GPU artifacts or APT packages. This task builds the transport boundary; tasks 95 and 96 add the production local APT/DKMS/Mesa recipe.
* Do not add a fabricated production catalog entry. The embedded catalog remains schema-valid and empty until tasks 95 and 96 produce a compile-tested and probe-tested Ubuntu 26.04 `amd64` payload.
* Runtime catalog entries use immutable HTTPS URLs without credentials, query strings, or fragments. Tests inject local fixture bytes without weakening production validation.
* Payload identity is the archive SHA-256. Mutable Git refs never appear as artifact identities.
* Cache hits rehash the archive, `payload.json`, and every declared prepared file before returning `ReadyGpuPayload`.
* ZIP extraction accepts regular files only. Reject absolute paths, `..`, duplicate paths, backslashes in entry names, symlinks, hard links, devices, explicit directory entries, undeclared files, excess file count, and compressed or expanded size-limit violations.
* A ready staging generation is `generations/<archive-sha256>` plus the unique marker `ready/<archive-sha256>.json`. There is no replaceable `current.json`.
* `GpuPayload` is a third logical GPU share. It never broadens the existing DriverStore or `System32\\lxss\\lib` roots and never exposes the whole cache.
* GPU failures remain best effort for VM lifecycle. This task adds no new `GpuStage` and does not map failures into UI state.
* Do not modify the agent protocol or guest installation code; tasks 94, 95, and 96 own those changes.
* Do not add a lifecycle caller or UI status mapping; task 98 owns orchestration after the transport and recipes exist.
* Keep the Linux agent statically linked. `vmlord-agent` must not depend on `vmlord-gpu-payload`, `zip`, or a system C library.
* Use `cargo test -p vmlord-gpu-payload` and `cargo test -p xtask` for portable work, then `cargo check-windows`, `cargo test-windows`, and `cargo agent` for final verification. Never spell out their target triples.

---

## File structure

* Create `crates/gpu-payload/Cargo.toml`: portable crate dependencies and the opt-in `builder` feature.
* Create `crates/gpu-payload/src/lib.rs`: public API and re-exports only.
* Create `crates/gpu-payload/src/digest.rs`: parsed SHA-256 value and streaming hashing.
* Create `crates/gpu-payload/src/error.rs`: one structured error type for catalog, preparation, cache, and staging failures.
* Create `crates/gpu-payload/src/catalog.rs`: catalog schema, exact target selection, URL and provenance validation.
* Create `crates/gpu-payload/src/manifest.rs`: `payload.json`, `sources.json`, ready-marker, and generated cache provenance types.
* Create `crates/gpu-payload/src/download.rs`: bounded HTTPS download and OS-file lock around one partial archive.
* Create `crates/gpu-payload/src/archive.rs`: safe ZIP inspection/extraction and prepared-file verification.
* Create `crates/gpu-payload/src/cache.rs`: verify-on-hit orchestration, quarantine, and atomic cache publication.
* Create `crates/gpu-payload/src/staging.rs`: per-VM immutable generation materialization and unique ready-marker publication.
* Create `crates/gpu-payload/src/builder.rs`: deterministic ZIP writer, compiled only with feature `builder`.
* Create `crates/gpu-payload/catalog/catalog.json`: embedded schema-v1 catalog with an empty `entries` array.
* Create `crates/gpu-payload/tests/fixtures/prepared/`: tiny source/license/content fixture used by the packer and runtime round-trip tests.
* Modify `Cargo.toml` and `Cargo.lock`: register the crate and dependencies.
* Create `crates/xtask/src/gpu_payload.rs`: `gpu-payload pack` CLI parsing and call into the builder.
* Modify `crates/xtask/src/main.rs`, `crates/xtask/Cargo.toml`, and `.cargo/config.toml`: expose `cargo gpu-payload`.
* Modify `crates/core/src/gpu.rs` and `crates/core/src/lib.rs`: add `GpuShareRole::GpuPayload` and its fixed share constructor.
* Modify `crates/platform/src/layout.rs`: name the per-VM payload staging directory.
* Modify `crates/platform/src/gpu_exports.rs`: validate and export that exact staging directory without weakening system-root checks.
* Modify `ARCHITECTURE.md`: document the completed task-93 boundary and downstream ownership.

---

### Task 1: Payload identity and catalog validation

**Files:**

* Create: `crates/gpu-payload/Cargo.toml`
* Create: `crates/gpu-payload/src/lib.rs`
* Create: `crates/gpu-payload/src/digest.rs`
* Create: `crates/gpu-payload/src/error.rs`
* Create: `crates/gpu-payload/src/catalog.rs`
* Create: `crates/gpu-payload/catalog/catalog.json`
* Modify: `Cargo.toml`
* Modify: `Cargo.lock`
* Test: `crates/gpu-payload/src/digest.rs`, `crates/gpu-payload/src/catalog.rs`

**Interfaces:**

* Produces `Sha256Digest: FromStr + Display + Serialize + Deserialize` with `as_hex(&self) -> &str`.
* Produces `GuestTarget { distribution, release, architecture, kernel_release, payload_abi }` and `GuestTarget::ubuntu_26_04_amd64(kernel_release: impl Into<String>) -> Self` for tests and later capability conversion.
* Produces `RendererCapability::{D3d12Gallium, DznVulkan}` and `MesaPolicy::{Distro, Bundled}`.
* Produces `PayloadError::{InvalidDigest, InvalidCatalog, UnsupportedTarget, InvalidManifest, AlreadyInProgress, ArchiveSizeMismatch, DigestMismatch, UnsafeArchive, LimitExceeded, Cancelled, Http, Io, Archive, ConflictingGeneration}` with `Display` and `std::error::Error` implementations.
* Produces `PayloadCatalog::from_json(bytes: &[u8]) -> Result<Self, PayloadError>`, `PayloadCatalog::embedded() -> Result<Self, PayloadError>`, `PayloadCatalog::entries(&self) -> &[CatalogEntry]`, and `PayloadCatalog::select(&self, target: &GuestTarget) -> Result<&CatalogEntry, PayloadError>`.
* Produces read-only `CatalogEntry` getters used by later tasks: `payload_id()`, `target()`, `archive_url()`, `archive_size()`, `expanded_size_limit()`, `file_count_limit()`, `archive_sha256()`, `payload_manifest_sha256()`, `required_renderers()`, `mesa_policy()`, `sources()`, and `licenses()`.

- [ ] **Step 1: Write failing digest and catalog tests**

Create the modules with test blocks first. Use a complete fixture entry rather than optional fields:

```rust
const ZERO_SHA256: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";
const WSL_COMMIT: &str = "14794180686c2fb6307fbe359c359bec765249f3";

#[test]
fn a_catalog_selects_only_the_exact_kernel_tuple() {
    let catalog = PayloadCatalog::from_json(test_catalog().as_bytes()).unwrap();
    let supported = GuestTarget::ubuntu_26_04_amd64("7.0.0-14-generic");
    let unsupported = GuestTarget::ubuntu_26_04_amd64("7.0.0-15-generic");

    assert_eq!(catalog.select(&supported).unwrap().payload_id(), "ubuntu-26.04-amd64-7.0.0-14-v1");
    assert!(matches!(catalog.select(&unsupported), Err(PayloadError::UnsupportedTarget(_))));
}

#[test]
fn production_urls_cannot_carry_mutable_or_secret_structure() {
    for url in [
        "http://downloads.example.test/payload.zip",
        "https://user:secret@downloads.example.test/payload.zip",
        "https://downloads.example.test/payload.zip?latest=1",
        "https://downloads.example.test/payload.zip#latest",
    ] {
        let error = PayloadCatalog::from_json(catalog_with_url(url).as_bytes()).unwrap_err();
        assert!(matches!(error, PayloadError::InvalidCatalog(_)), "{url}: {error}");
    }
}

#[test]
fn an_empty_embedded_catalog_is_valid_until_a_tested_recipe_is_published() {
    let catalog = PayloadCatalog::embedded().unwrap();
    assert!(catalog.entries().is_empty());
}
```

The fixture JSON must include schema version `1`, payload ABI `1`, both renderer capabilities, `MesaPolicy::Bundled`, exact `WSL_COMMIT`, non-empty SPDX/license paths, compressed size, expanded size, file-count limit, archive digest, and `payload.json` digest.

- [ ] **Step 2: Run the focused tests to verify they fail**

Run: `rtk cargo test -p vmlord-gpu-payload`

Expected: FAIL because the crate and types do not exist.

- [ ] **Step 3: Implement the minimal schema and validators**

Register `crates/gpu-payload` in both workspace member lists. Use these dependencies:

```toml
[dependencies]
log.workspace = true
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
sha2 = "0.11"
ureq = { version = "3.4", default-features = false, features = [
    "rustls",
    "platform-verifier",
    "win-system-proxy",
] }
url = "2"
zip = { version = "9.0", default-features = false, features = [
    "deflate-flate2-zlib-rs",
    "time",
] }

[features]
builder = []
```

Parse through private serde documents, then validate into public types with private fields. `CatalogEntry::validate` must enforce all of the following in one place:

```rust
const CATALOG_SCHEMA_VERSION: u32 = 1;
const PAYLOAD_ABI_VERSION: u32 = 1;

fn validate_url(value: &str) -> Result<url::Url, PayloadError> {
    let url = url::Url::parse(value)
        .map_err(|error| PayloadError::InvalidCatalog(format!("invalid archive URL: {error}")))?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(PayloadError::InvalidCatalog(
            "archive URL must be immutable HTTPS without credentials, query, or fragment".into(),
        ));
    }
    Ok(url)
}
```

Define the shared error vocabulary before other modules consume it:

```rust
pub enum PayloadError {
    InvalidDigest(String),
    InvalidCatalog(String),
    UnsupportedTarget(GuestTarget),
    InvalidManifest(String),
    AlreadyInProgress { path: PathBuf },
    ArchiveSizeMismatch { expected: u64, actual: u64 },
    DigestMismatch { subject: String, expected: Sha256Digest, actual: Sha256Digest },
    UnsafeArchive(String),
    LimitExceeded { subject: &'static str, limit: u64, actual: u64 },
    Cancelled,
    Http(String),
    Io { operation: &'static str, path: PathBuf, source: io::Error },
    Archive(String),
    ConflictingGeneration { path: PathBuf },
}
```

Also reject duplicate targets or payload IDs, zero byte/file limits, an expanded limit smaller than archive size, missing required renderers, mutable source refs, empty source/version/license fields, and unknown schema/ABI versions. Store SHA-256 as normalized lowercase hex backed by `[u8; 32]`; do not pass unchecked strings beyond `catalog.rs`.

- [ ] **Step 4: Run catalog tests to verify they pass**

Run: `rtk cargo test -p vmlord-gpu-payload`

Expected: PASS, including rejection of duplicate targets, malformed digests, incomplete provenance, and unsafe URLs.

- [ ] **Step 5: Commit the catalog boundary**

```bash
rtk git add Cargo.toml Cargo.lock crates/gpu-payload
rtk git commit -m "TASK-93: Add GPU payload catalog boundary"
```

---

### Task 2: Prepared-file manifests and provenance

**Files:**

* Create: `crates/gpu-payload/src/manifest.rs`
* Modify: `crates/gpu-payload/src/lib.rs`
* Modify: `crates/gpu-payload/src/error.rs`
* Test: `crates/gpu-payload/src/manifest.rs`

**Interfaces:**

* Consumes `CatalogEntry`, `GuestTarget`, `MesaPolicy`, and `Sha256Digest` from Task 1.
* Produces `PayloadManifest::parse_and_validate(bytes: &[u8], entry: &CatalogEntry) -> Result<Self, PayloadError>`, `files(&self) -> &[PreparedFile]`, and `PreparedFile::{path(), size(), sha256()}`.
* Produces `SourceManifest::parse_and_validate(bytes: &[u8], entry: &CatalogEntry) -> Result<Self, PayloadError>`.
* Produces `ReadyMarker::new(entry: &CatalogEntry) -> Self` and deterministic `to_json_bytes()`.
* Produces internal `cache_provenance(entry: &CatalogEntry, sources: &SourceManifest) -> Result<Vec<u8>, PayloadError>`.

- [ ] **Step 1: Write failing manifest validation tests**

```rust
#[test]
fn a_manifest_lists_every_other_file_in_strict_path_order() {
    let entry = catalog_entry();
    let manifest = PayloadManifest::parse_and_validate(valid_manifest(), &entry).unwrap();

    assert_eq!(
        manifest.files().iter().map(|file| file.path()).collect::<Vec<_>>(),
        ["content/dxgkrnl/dxgmodule.c", "licenses/GPL-2.0.txt", "sources.json"]
    );
}

#[test]
fn unsafe_duplicate_and_self_referential_paths_are_rejected() {
    for path in ["/absolute", "../escape", r"content\\escape", "payload.json", "a/../../b"] {
        let error = PayloadManifest::parse_and_validate(manifest_with_path(path), &catalog_entry())
            .unwrap_err();
        assert!(matches!(error, PayloadError::InvalidManifest(_)), "{path}: {error}");
    }
}

#[test]
fn generated_provenance_can_contain_the_archive_digest_without_changing_it() {
    let entry = catalog_entry();
    let sources = SourceManifest::parse_and_validate(valid_sources(), &entry).unwrap();
    let json = cache_provenance(&entry, &sources).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&json).unwrap();

    assert_eq!(value["archive_sha256"], entry.archive_sha256().as_hex());
    assert_eq!(value["payload_manifest_sha256"], entry.payload_manifest_sha256().as_hex());
}
```

Add cases for unsorted paths, duplicate paths, zero-size mismatch, missing `sources.json`, missing license text, source commit mismatch, overlay attributed to Microsoft, unknown manifest schema, and a manifest whose target or payload ID differs from the catalog.

- [ ] **Step 2: Run the manifest tests to verify they fail**

Run: `rtk cargo test -p vmlord-gpu-payload manifest::tests`

Expected: FAIL because `manifest` and its types do not exist.

- [ ] **Step 3: Implement canonical manifests**

Use exact schema-v1 shapes:

```rust
#[derive(Clone, Debug, Deserialize, Serialize)]
struct PayloadManifestDocument {
    schema_version: u32,
    payload_id: String,
    target: GuestTarget,
    files: Vec<PreparedFile>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PreparedFile {
    path: String,
    size: u64,
    sha256: Sha256Digest,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReadyMarker {
    schema_version: u32,
    payload_id: String,
    generation: Sha256Digest,
    payload_manifest_sha256: Sha256Digest,
}
```

Path validation accepts only non-empty UTF-8 `/`-separated relative paths whose components are ordinary ASCII/Unicode names other than `.` and `..`; reject `\\`, NUL, roots, prefixes, and duplicate normalized paths. Require strict lexical order so the same tree has one manifest representation. Serialize generated JSON with `serde_json::to_vec`, ending with one newline; never include a timestamp in deterministic metadata.

`sources.json` must separately list upstream inputs and VMLord overlays. Require the pinned WSL commit `14794180686c2fb6307fbe359c359bec765249f3` only in test fixtures, not as a hard-coded production policy. Validate that `d3dkmthk.h` carries `GPL-2.0 WITH Linux-syscall-note` and that every declared license path appears in `payload.json`.

- [ ] **Step 4: Run all portable tests**

Run: `rtk cargo test -p vmlord-gpu-payload`

Expected: PASS.

- [ ] **Step 5: Commit manifests and provenance**

```bash
rtk git add crates/gpu-payload/src
rtk git commit -m "TASK-93: Validate GPU payload manifests"
```

---

### Task 3: Bounded verified archive download

**Files:**

* Create: `crates/gpu-payload/src/download.rs`
* Modify: `crates/gpu-payload/src/digest.rs`
* Modify: `crates/gpu-payload/src/error.rs`
* Modify: `crates/gpu-payload/src/lib.rs`
* Test: `crates/gpu-payload/src/download.rs`

**Interfaces:**

* Consumes immutable URL, exact archive byte length, and `Sha256Digest` from `CatalogEntry`.
* Produces public `PayloadProgress::{Connecting, Downloading { downloaded, total }, Verifying { hashed, total }, Extracting { files, total }, Staging { files, total }, Ready}`.
* Produces internal `LockedArchive::acquire(cache_root: &Path, entry: &CatalogEntry) -> Result<Self, PayloadError>`, `download(&mut self, progress: &dyn Fn(PayloadProgress), cancel: &AtomicBool)`, and `verify(&mut self, ...)`.

- [ ] **Step 1: Write failing lock, size, cancellation, and digest tests**

Reuse the small in-process TCP fixture style from `crates/image/tests/download.rs`, but keep it inside this crate. The focused cases are:

```rust
#[test]
fn a_second_preparer_is_refused_while_the_digest_lock_is_held() {
    let root = temporary_directory("locked");
    let entry = entry_for_bytes(b"archive");
    let first = LockedArchive::acquire(&root, &entry).unwrap();

    let error = LockedArchive::acquire(&root, &entry).unwrap_err();
    assert!(matches!(error, PayloadError::AlreadyInProgress { .. }));
    drop(first);
}

#[test]
fn a_body_larger_than_the_catalog_limit_is_stopped_before_eof() {
    let server = fixture_server(b"one-byte-too-many");
    let entry = entry_with_size(server.url(), 16);
    let mut archive = LockedArchive::acquire(&temporary_directory("large"), &entry).unwrap();

    let error = archive.download(&ignore_progress, &AtomicBool::new(false)).unwrap_err();
    assert!(matches!(error, PayloadError::ArchiveSizeMismatch { .. }));
}

#[test]
fn a_digest_mismatch_truncates_the_partial_file_for_retry() {
    // Serve the expected byte length with different bytes, assert
    // DigestMismatch, then assert the `.part` file has length zero.
}
```

Also assert that cancellation is checked between 64 KiB reads, a short body is rejected, and logs/errors identify the payload ID rather than echoing a URL.

- [ ] **Step 2: Run download tests to verify they fail**

Run: `rtk cargo test -p vmlord-gpu-payload download::tests`

Expected: FAIL because `LockedArchive` does not exist.

- [ ] **Step 3: Implement the locked partial downloader**

Follow `crates/image/src/part.rs` deliberately: open one `<digest>.zip.part`, take `File::try_lock`, and keep that handle for writing and hashing so Windows mandatory locking is respected. Do not add resume logic in task 93; truncate a stale partial before a new request.

Stream with an exact byte bound:

```rust
while downloaded < expected_size {
    if cancel.load(Ordering::Relaxed) {
        return Err(PayloadError::Cancelled);
    }
    let remaining = (expected_size - downloaded).min(buffer.len() as u64) as usize;
    let read = body.read(&mut buffer[..remaining])?;
    if read == 0 {
        return Err(PayloadError::ArchiveSizeMismatch { expected: expected_size, actual: downloaded });
    }
    partial.write_all(&buffer[..read])?;
    downloaded += read as u64;
}
if body.read(&mut [0_u8; 1])? != 0 {
    return Err(PayloadError::ArchiveSizeMismatch { expected: expected_size, actual: expected_size + 1 });
}
```

Flush, rewind, and hash through the locked handle. Production calls use the already validated HTTPS URL. Tests construct an internal validated entry with a loopback HTTP URL; do not add a public switch that admits HTTP catalogs.

- [ ] **Step 4: Run download and catalog tests**

Run: `rtk cargo test -p vmlord-gpu-payload`

Expected: PASS with no test waiting for another lock.

- [ ] **Step 5: Commit the downloader**

```bash
rtk git add crates/gpu-payload/src
rtk git commit -m "TASK-93: Download verified GPU payload archives"
```

---

### Task 4: Safe extraction and content-addressed cache readiness

**Files:**

* Create: `crates/gpu-payload/src/archive.rs`
* Create: `crates/gpu-payload/src/cache.rs`
* Modify: `crates/gpu-payload/src/lib.rs`
* Modify: `crates/gpu-payload/src/error.rs`
* Test: `crates/gpu-payload/src/archive.rs`, `crates/gpu-payload/src/cache.rs`

**Interfaces:**

* Consumes `LockedArchive`, `PayloadManifest`, `SourceManifest`, catalog limits, progress callback, and cancellation flag.
* Produces `PrepareRequest<'a> { entry, cache_root, progress, cancel }` and `prepare(request: PrepareRequest<'_>) -> Result<ReadyGpuPayload, PayloadError>`.
* Produces internal `prepare_verified_archive(entry: &CatalogEntry, archive: &Path, cache_root: &Path, progress: &dyn Fn(PayloadProgress), cancel: &AtomicBool) -> Result<ReadyGpuPayload, PayloadError>` so the downloader and builder round-trip use the identical extractor without a test-only production switch.
* Produces opaque `ReadyGpuPayload` getters: `payload_id()`, `generation() -> &Sha256Digest`, `files_directory() -> &Path`, `manifest() -> &PayloadManifest`, and `provenance_path() -> &Path`.

- [ ] **Step 1: Write failing hostile-archive and cache-hit tests**

Use an in-memory `ZipWriter` only in tests to construct each hostile archive:

```rust
#[test]
fn traversal_symlink_duplicate_and_undeclared_entries_are_rejected() {
    for archive in [
        zip_with_name("../escape"),
        zip_with_name("/absolute"),
        zip_with_name(r"content\\windows-path"),
        zip_with_unix_mode("content/link", 0o120777),
        zip_with_duplicate("content/file"),
        zip_with_undeclared("content/extra"),
    ] {
        let error = extract_fixture(&archive).unwrap_err();
        assert!(matches!(error, PayloadError::UnsafeArchive(_) | PayloadError::InvalidManifest(_)));
    }
}

#[test]
fn a_corrupt_prepared_file_prevents_a_cache_hit_and_is_rebuilt() {
    let fixture = valid_fixture_server();
    let first = prepare(fixture.request()).unwrap();
    fs::write(first.files_directory().join("content/dxgkrnl/dxgmodule.c"), b"changed").unwrap();

    let second = prepare(fixture.request()).unwrap();

    assert_eq!(second.generation(), fixture.entry().archive_sha256());
    assert_eq!(fs::read(second.files_directory().join("content/dxgkrnl/dxgmodule.c")).unwrap(), fixture.original_source());
}
```

Add cases for an explicit directory entry, device mode, more entries than `file_count_limit`, expanded sizes over the catalog limit, a wrong `payload.json` digest, a file whose streamed size differs from its declaration, cancellation during extraction, and two preparers leaving only one final digest directory.

- [ ] **Step 2: Run archive/cache tests to verify they fail**

Run: `rtk cargo test -p vmlord-gpu-payload`

Expected: FAIL because extraction and `prepare` do not exist.

- [ ] **Step 3: Implement safe inspection, extraction, and atomic publication**

`archive.rs` must open `payload.json` first, cap it at 1 MiB, verify its catalog-pinned digest, and parse it. Iterate entries by index; do not call `ZipArchive::extract`. For every entry:

```rust
let raw_name = std::str::from_utf8(file.name_raw())
    .map_err(|_| PayloadError::UnsafeArchive("non-UTF-8 entry name".into()))?;
let enclosed = file.enclosed_name()
    .ok_or_else(|| PayloadError::UnsafeArchive(raw_name.to_owned()))?;
if file.is_dir() || raw_name.contains('\\') || !seen.insert(enclosed.clone()) {
    return Err(PayloadError::UnsafeArchive(raw_name.to_owned()));
}
if let Some(mode) = file.unix_mode()
    && mode & 0o170000 != 0
    && mode & 0o170000 != 0o100000
{
    return Err(PayloadError::UnsafeArchive(raw_name.to_owned()));
}
```

Before creating a destination, match its path, `size()`, and SHA-256 against one declared `PreparedFile`. Sum file counts, `compressed_size()`, and declared expanded sizes with checked arithmetic against catalog limits. Stream each file to `create_new(true)`, hash while writing, `sync_all`, and reject any byte beyond its declared size. At end, every declaration must have been consumed exactly once.

`cache.rs` uses this layout:

```text
<root>/gpu-payload/v1/
    <digest>.lock
    <digest>.zip.part
    <digest>.tmp-<pid>-<counter>/
    <digest>.corrupt-<pid>-<counter>/
    <digest>/
        archive.zip
        provenance.json
        files/...
```

On a hit, rehash `archive.zip`, `payload.json`, and every declared file. Rename a bad final directory to the unique quarantine name, rebuild under a unique temporary directory, generate deterministic `provenance.json`, flush files, and rename the whole temporary directory to `<digest>`. If the final name appeared first, verify and adopt it rather than replacing it. Remove only this operation's temporary/quarantine paths; never recursively remove the cache root.

- [ ] **Step 4: Run the complete portable crate tests**

Run: `rtk cargo test -p vmlord-gpu-payload`

Expected: PASS, including hostile archives, cold cache, warm cache, corrupt cache, cancellation, and concurrency.

- [ ] **Step 5: Commit cache readiness**

```bash
rtk git add crates/gpu-payload/src
rtk git commit -m "TASK-93: Prepare atomic GPU payload cache entries"
```

---

### Task 5: Immutable per-VM staging generations

**Files:**

* Create: `crates/gpu-payload/src/staging.rs`
* Modify: `crates/gpu-payload/src/lib.rs`
* Modify: `crates/gpu-payload/src/error.rs`
* Test: `crates/gpu-payload/src/staging.rs`

**Interfaces:**

* Consumes only a verified `ReadyGpuPayload` and an exact caller-supplied staging root.
* Produces `ensure_staging_root(path: &Path) -> Result<(), PayloadError>`.
* Produces `stage_payload(payload: &ReadyGpuPayload, staging_root: &Path, progress: &dyn Fn(PayloadProgress), cancel: &AtomicBool) -> Result<StagedGpuPayload, PayloadError>` and internal `stage_with(..., hard_link: &dyn Fn(&Path, &Path) -> io::Result<()>)` for deterministic copy-fallback tests.
* Produces `StagedGpuPayload::{payload_id(), generation(), generation_directory(), ready_marker_path()}`.

- [ ] **Step 1: Write failing staging tests**

```rust
#[test]
fn a_generation_becomes_selectable_only_after_its_unique_ready_marker() {
    let payload = ready_fixture();
    let staging = temporary_directory("stage");

    let staged = stage_payload(&payload, &staging, &ignore_progress, &AtomicBool::new(false)).unwrap();

    assert_eq!(staged.generation(), payload.generation());
    assert!(staged.generation_directory().join("payload.json").is_file());
    assert_eq!(
        staged.ready_marker_path(),
        staging.join("ready").join(format!("{}.json", payload.generation()))
    );
    assert!(staged.ready_marker_path().is_file());
    assert!(!staging.join("current.json").exists());
}

#[test]
fn a_failed_hard_link_falls_back_to_a_verified_copy() {
    let payload = ready_fixture();
    let staging = temporary_directory("copy-fallback");
    let staged = stage_with(&payload, &staging, &|_, _| Err(io::Error::from(io::ErrorKind::CrossesDevices)))
        .unwrap();

    assert_generation_matches(&payload, &staged);
}
```

Also test idempotent restaging of the same digest, rejection/repair of a corrupt existing generation, cancellation before marker publication, and cleanup that never removes a generation named by an active request.

- [ ] **Step 2: Run staging tests to verify they fail**

Run: `rtk cargo test -p vmlord-gpu-payload staging::tests`

Expected: FAIL because staging APIs do not exist.

- [ ] **Step 3: Implement generation and marker publication**

Create `generations/` and `ready/` below the exact staging root. Materialize under `.tmp-<digest>-<pid>-<counter>`, preserving only regular files and directories implied by manifest paths. Try `fs::hard_link` first; on failure copy, hash, and compare size/digest. Verify the complete generation, then rename it to `generations/<digest>`.

Serialize `ReadyMarker`, write it with `create_new(true)` to `ready/.<digest>.part-<pid>-<counter>`, `sync_all`, and rename to the unique final `ready/<digest>.json`. Never replace an existing generation or marker: verify and adopt a matching one, quarantine a corrupt one, and return an error if a conflicting marker names different identity.

- [ ] **Step 4: Run staging and cache tests**

Run: `rtk cargo test -p vmlord-gpu-payload`

Expected: PASS; no test creates `current.json` and cancellation leaves no ready marker.

- [ ] **Step 5: Commit staging**

```bash
rtk git add crates/gpu-payload/src
rtk git commit -m "TASK-93: Stage immutable GPU payload generations"
```

---

### Task 6: Deterministic release archive builder

**Files:**

* Create: `crates/gpu-payload/src/builder.rs`
* Create: `crates/gpu-payload/tests/fixtures/prepared/sources.json`
* Create: `crates/gpu-payload/tests/fixtures/prepared/licenses/GPL-2.0.txt`
* Create: `crates/gpu-payload/tests/fixtures/prepared/licenses/Linux-syscall-note.txt`
* Create: `crates/gpu-payload/tests/fixtures/prepared/content/dxgkrnl/dxgmodule.c`
* Modify: `crates/gpu-payload/src/lib.rs`
* Create: `crates/xtask/src/gpu_payload.rs`
* Modify: `crates/xtask/src/main.rs`
* Modify: `crates/xtask/Cargo.toml`
* Modify: `.cargo/config.toml`
* Test: `crates/gpu-payload/src/builder.rs`, `crates/xtask/src/gpu_payload.rs`

**Interfaces:**

* Produces feature-gated `builder::pack(request: PackRequest<'_>) -> Result<BuiltArtifact, PayloadError>`.
* `PackRequest` contains `prepared_directory`, `recipe_path`, `archive_path`, and `catalog_entry_path`.
* `BuiltArtifact` exposes archive byte length, expanded byte length, file count, archive digest, and `payload.json` digest.
* Produces CLI `cargo gpu-payload pack --recipe <json> --input <dir> --archive <zip> --catalog-entry <json>`.

- [ ] **Step 1: Write failing deterministic packer and CLI tests**

```rust
#[test]
fn identical_inputs_produce_identical_zip_bytes() {
    let fixture = prepared_fixture();
    let first = temporary_file("first.zip");
    let second = temporary_file("second.zip");

    let one = pack(fixture.request(&first)).unwrap();
    let two = pack(fixture.request(&second)).unwrap();

    assert_eq!(fs::read(first).unwrap(), fs::read(second).unwrap());
    assert_eq!(one.archive_sha256(), two.archive_sha256());
}

#[test]
fn changing_one_overlay_byte_changes_archive_identity() {
    let fixture = prepared_fixture();
    let before = pack(fixture.request(&temporary_file("before.zip"))).unwrap();
    fs::write(fixture.input().join("content/dxgkrnl/dxgmodule.c"), b"changed").unwrap();
    let after = pack(fixture.request(&temporary_file("after.zip"))).unwrap();

    assert_ne!(before.archive_sha256(), after.archive_sha256());
}

#[test]
fn pack_arguments_are_explicit_and_complete() {
    let command = parse(["pack", "--recipe", "recipe.json", "--input", "prepared", "--archive", "payload.zip", "--catalog-entry", "entry.json"]).unwrap();
    assert_eq!(command.archive, PathBuf::from("payload.zip"));
}
```

Add a round-trip test: pack the fixture, validate its emitted catalog entry, prepare the archive through the same runtime extraction/cache path without network, and compare every resulting file.
`prepared_fixture()` must copy the checked-in fixture to a unique temporary directory so the mutation test never edits repository files.

- [ ] **Step 2: Run builder and xtask tests to verify they fail**

Run: `rtk cargo test -p vmlord-gpu-payload --features builder builder::tests`

Expected: FAIL because `builder` does not exist.

Run: `rtk cargo test -p xtask gpu_payload::tests`

Expected: FAIL because the command is unknown.

- [ ] **Step 3: Implement sorted deterministic ZIP output**

The builder walks only regular files below the prepared directory, rejects reparse/symlink entries, inserts generated `payload.json`, sorts all archive paths lexically, and writes them in that order. Use one fixed timestamp, `0o644` permissions, and one compression configuration for every file:

```rust
let options = zip::write::SimpleFileOptions::default()
    .compression_method(zip::CompressionMethod::Deflated)
    .compression_level(Some(6))
    .last_modified_time(fixed_zip_time())
    .permissions(0o644);

for file in files_in_lexical_order {
    writer.start_file(&file.archive_path, options)?;
    io::copy(&mut File::open(&file.host_path)?, &mut writer)?;
}
writer.finish()?.sync_all()?;
```

The recipe supplies identity, exact target, immutable final URL, renderer capabilities, Mesa policy, upstream commits, overlays, and licenses. The builder computes all byte counts and digests, writes a complete standalone catalog-entry JSON, and validates that output through `PayloadCatalog` before returning.

`xtask` manually parses exactly the arguments shown above, refuses unknown/missing/repeated flags, and delegates all archive logic to the crate. Add the alias:

```toml
gpu-payload = ["run", "-p", "xtask", "--", "gpu-payload"]
```

Do not add a production recipe or upload command in task 93. Tasks 95 and 96 will prepare the real `content/apt`, `content/dxgkrnl`, and optional `content/mesa` tree, run this packer, execute compile/probe gates, publish the immutable asset, and only then add its emitted entry to `catalog/catalog.json`.

- [ ] **Step 4: Run deterministic and round-trip verification**

Run: `rtk cargo test -p vmlord-gpu-payload --features builder`

Expected: PASS, including byte-for-byte repeatability and runtime round trip.

Run: `rtk cargo test -p xtask`

Expected: PASS, including the existing `dist` test.

- [ ] **Step 5: Commit release tooling**

```bash
rtk git add .cargo/config.toml crates/gpu-payload crates/xtask
rtk git commit -m "TASK-93: Build deterministic GPU payload archives"
```

---

### Task 7: Add the read-only GPU payload share boundary

**Files:**

* Modify: `crates/core/src/gpu.rs`
* Modify: `crates/core/src/lib.rs`
* Modify: `crates/platform/src/layout.rs`
* Modify: `crates/platform/src/gpu_exports.rs`
* Test: `crates/core/src/gpu.rs`, `crates/platform/src/layout.rs`, `crates/platform/src/gpu_exports.rs`

**Interfaces:**

* Produces `GpuShareRole::GpuPayload`, `GpuShare::payload() -> GpuShare`, and `GPU_PAYLOAD_SHARE: &str = "vmlord.gpu.payload"`.
* Produces `layout::gpu_payload_staging_directory(vm_directory: &Path) -> PathBuf` returning `<vm-directory>/gpu-payload`.
* Extends `GpuExports::build(adapters: &[HostGpuAdapter], vm_directory: &Path) -> Option<GpuExports>` and test helper `build_with(adapters, roots, payload_root, canonicalize)`.
* Does not modify protobuf; task 94 converts the new core role into the wire schema.

- [ ] **Step 1: Write failing core and platform tests**

```rust
#[test]
fn the_payload_share_has_one_fixed_name_and_role() {
    let share = GpuShare::payload();
    assert_eq!(share.name, GPU_PAYLOAD_SHARE);
    assert_eq!(share.role, GpuShareRole::GpuPayload);
}

#[test]
fn the_payload_staging_directory_lives_inside_its_vm() {
    assert_eq!(
        gpu_payload_staging_directory(Path::new(r"D:\VMLord\dev-linux")),
        PathBuf::from(r"D:\VMLord\dev-linux\gpu-payload")
    );
}

#[test]
fn payload_wsl_and_driver_package_have_distinct_roles_and_order() {
    let exports = build_fixture_with_payload();
    let roles: Vec<_> = exports.manifest().shares.into_iter().map(|share| share.role).collect();
    assert!(matches!(roles.as_slice(), [GpuShareRole::GpuPayload, GpuShareRole::WslLib, GpuShareRole::DriverPackage { .. }]));
}

#[test]
fn a_payload_directory_reparsed_outside_its_vm_is_dropped() {
    let exports = build_fixture_with_payload_mapping_to(r"D:\attacker\payload");
    assert!(!exports.manifest().shares.iter().any(|share| share.role == GpuShareRole::GpuPayload));
}
```

Also cover a missing staging directory, a sibling-prefix path such as `dev-linux-evil`, case-insensitive Windows components, deduplication, and grant refusal dropping only the payload share.

- [ ] **Step 2: Run focused tests to verify they fail**

Run: `rtk cargo test -p vmlord-core gpu::tests::the_payload_share_has_one_fixed_name_and_role`

Expected: FAIL because `GpuPayload` does not exist.

Run: `rtk cargo test-windows -p vmlord-platform gpu_exports::tests`

Expected: FAIL because builders do not accept a VM staging root.

- [ ] **Step 3: Implement the narrow third root**

Add the core constructor:

```rust
pub const GPU_PAYLOAD_SHARE: &str = "vmlord.gpu.payload";

pub fn payload() -> Self {
    Self {
        name: GPU_PAYLOAD_SHARE.to_owned(),
        role: GpuShareRole::GpuPayload,
    }
}
```

In `gpu_exports`, retain the existing `System32` root resolution unchanged. Separately canonicalize the trusted `vm_directory` and its exact `layout::gpu_payload_staging_directory`. Include it only when the staging result is within the canonical VM directory component-by-component and the caller-supplied path is that direct child. Export the canonical staging path, never the unresolved input, and run `HcsGrantVmAccess` only after this validation.

Put `GpuPayload` first in manifest mount order, followed by WSL libraries and driver packages. Update the module documentation from “two roots and nothing else” to “two system roots plus one exact per-VM staging directory”; do not admit an arbitrary cache or storage-root descendant.

- [ ] **Step 4: Run core and Windows platform tests**

Run: `rtk cargo test -p vmlord-core gpu::tests`

Expected: PASS.

Run: `rtk cargo test-windows -p vmlord-platform`

Expected: PASS; existing DriverStore and WSL escape tests remain unchanged.

- [ ] **Step 5: Commit the share contract**

```bash
rtk git add crates/core/src/gpu.rs crates/core/src/lib.rs crates/platform/src/layout.rs crates/platform/src/gpu_exports.rs
rtk git commit -m "TASK-93: Add GPU payload Plan9 export boundary"
```

---

### Task 8: Document and verify the task-93 foundation

**Files:**

* Modify: `ARCHITECTURE.md`
* Modify: `README.md`
* Verify: the whole workspace and final change set

**Interfaces:**

* Consumes every task-93 interface above.
* Produces architecture documentation that explicitly leaves protobuf/mounting to task 94, production package/driver/Mesa recipes to tasks 95 and 96, and lifecycle/UI orchestration to task 98.

- [ ] **Step 1: Update architecture and command documentation**

Add a `GPU: guest payload` subsection after the existing Plan9 export section. Describe:

* why `vmlord-gpu-payload` is portable and separate from `core`/`platform`;
* exact target selection and the intentionally empty production catalog at task-93 completion;
* verify-on-hit content-addressed cache and opaque `ReadyGpuPayload` constructor boundary;
* deterministic ZIP/provenance and the non-self-referential digest arrangement;
* per-VM `generations/<digest>` plus `ready/<digest>.json` staging;
* the third exact Plan9 root and downstream task ownership.

Add `cargo gpu-payload pack ...` to the README command table as release tooling, not an end-user command.

- [ ] **Step 2: Verify portable crates and deterministic tooling**

Run: `rtk cargo test -p vmlord-gpu-payload --features builder`

Expected: exit status 0; all catalog, hostile archive, cache, staging, determinism, and round-trip tests pass.

Run: `rtk cargo test -p xtask`

Expected: exit status 0; `dist` and `gpu-payload` argument tests pass.

- [ ] **Step 3: Verify Windows application and Linux agent constraints**

Run: `rtk cargo check-windows`

Expected: exit status 0 with no compile errors.

Run: `rtk cargo test-windows`

Expected: exit status 0 with no non-ignored test failures.

Run: `rtk cargo agent`

Expected: exit status 0 using `x86_64-unknown-linux-musl` and `rust-lld`.

Run: `rtk cargo tree -p vmlord-agent`

Expected: output contains neither `vmlord-gpu-payload` nor `zip`.

- [ ] **Step 4: Inspect scope, provenance, and repository state**

Run: `rtk rg -n "appsandbox|AppSandbox" crates/gpu-payload crates/xtask/src/gpu_payload.rs`

Expected: no matches in payload source or fixtures.

Run: `rtk rg -n 'T[B]D|T[O]DO|F[I]XME|current\\.json' crates/gpu-payload ARCHITECTURE.md README.md`

Expected: no matches introduced by task 93.

Run: `rtk git diff --check && rtk git status --short`

Expected: no whitespace errors and only intended task-93 documentation changes remain uncommitted.

- [ ] **Step 5: Commit documentation**

```bash
rtk git add ARCHITECTURE.md README.md
rtk git commit -m "TASK-93: Document GPU payload foundation"
```

---

## Downstream handoff

Task 93 is complete when the generic builder, validated empty embedded catalog,
runtime cache/staging APIs, and Plan9 share boundary pass their fixture-based
tests. It does not claim that a real Ubuntu target is GPU-ready.

Task 94 puts `GpuPayload` on the wire and mounts it through the guest allowlist.
Task 95 must prepare the pinned `dxgkrnl` source/overlay and exact local APT
closure for a tested Ubuntu 26.04 `amd64` kernel tuple. Task 96 must decide
`MesaPolicy` from real D3D12/dzn probes and add bundled upstream Mesa when the
distro closure is insufficient. Together they run `cargo gpu-payload pack`,
publish the immutable archive, and add the generated entry to the embedded
catalog only after compile and probe gates pass. Task 98 connects the prepared
payload and guest recipes to VM start/reconnect and maps their facts through the
existing GPU status model. Task 99 supplies the cold-host, warm-host,
network-disabled end-to-end acceptance tests from the design.
