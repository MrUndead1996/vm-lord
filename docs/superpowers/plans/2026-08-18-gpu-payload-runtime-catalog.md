# GPU Payload Runtime Catalog Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the compiled-in GPU payload catalog with one assembled at runtime from `<executable directory>/gpu-payload/<payload_id>.{json,zip}`, and delete the network path that the catalog's URL fields existed to serve.

**Architecture:** The catalog stops being a build artifact. Each payload is a pair of files beside `vmlord.exe`: an entry document (`schema_version: 2`) and its archive, named by `payload_id`. `PayloadCatalog::from_release_directory` assembles the catalog from that directory; a missing directory yields an empty catalog, a present-but-broken file is an error. `prepare` takes the archive as a required path and never reaches the network, so `download.rs`, `ureq` and the `url` dependency go. The entry schema loses `archive_url`, `archive_size`, `vmlord_revision` and `builder_version`; the two that backed real mechanisms are replaced by measuring the archive instead of trusting its entry.

**Tech Stack:** Rust 2024 workspace; `serde`/`serde_json`, `sha2`, `zip`; `cargo check-windows` and `cargo test-windows` (Windows targets run from WSL through binfmt interop).

**Spec:** `docs/superpowers/specs/2026-08-18-gpu-payload-runtime-catalog-design.md`

## Execution notes

* Tasks 5 and 6 were committed together. `PayloadCatalog::embedded` reads a
  multi-entry catalog document, so deleting `from_json` leaves it without a
  reader; splitting the two would have left the tree uncompilable between
  commits, which the last constraint below forbids.
* `builder::tests::archive_members_are_lexically_sorted_with_fixed_metadata`
  and `builder::tests::prepared_paths_cannot_collide_on_windows` fail on this
  machine **before** any of this work, and still do. They build fixtures under
  the Windows temp directory, where NTFS is case-insensitive and permissions
  come back `0664` rather than `0644`. They compile only with the `builder`
  feature, which is why `cargo test-windows` alone is green and
  `cargo test-windows -p xtask` is not. Not this task's to fix.

## Global Constraints

* Every commit subject is prefixed `TASK-109: ` (AGENTS.md).
* Work happens on branch `task-109-gpu-payload-runtime-catalog`. Do not open a merge request without explicit approval.
* Test command for the crate: `cargo test-windows -p vmlord-gpu-payload`. For the others: `-p xtask`, `-p vmlord-platform`. Compile check: `cargo check-windows`.
* Never spell `gpu-payload` as a literal outside `release.rs` — `LOCAL_ARCHIVE_DIRECTORY` and the two path functions own that name.
* Catalog entry schema version after this work is `2`; `sources.json` and `payload.json` stay at `1`.
* Do not add dependencies. This plan only removes them (`ureq`, `url`).
* No backwards compatibility with schema 1 files: there are no users, and a stale pair beside an executable is a broken release, not a migration (see `mvp-no-back-compat-migrations`).
* Each task must end with the workspace compiling and its crate's tests passing. Nothing is left half-migrated across a commit boundary.

---

### Task 1: Take the network out of the crate

Deletes `download.rs`, the `ureq` dependency, and the fallback branch in `prepare`. `PayloadProgress` moves to its own module and loses the two variants that only a download can report.

**Files:**
- Create: `crates/gpu-payload/src/progress.rs`
- Delete: `crates/gpu-payload/src/download.rs`
- Modify: `crates/gpu-payload/src/lib.rs`, `crates/gpu-payload/src/cache.rs`, `crates/gpu-payload/Cargo.toml`, `crates/platform/src/gpu_staging.rs`
- Test: the existing `mod tests` in `crates/gpu-payload/src/cache.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `PrepareRequest { entry: &CatalogEntry, cache_root: &Path, archive: &Path, progress: &dyn Fn(PayloadProgress), cancel: &AtomicBool }`; `PayloadProgress::{Verifying { hashed: u64, total: u64 }, Extracting { files: u64, total: u64 }, Staging { files: u64, total: u64 }, Ready}` re-exported from `crate::progress`.

- [x] **Step 1: Rewrite the cache tests that drove the download branch**

In `crates/gpu-payload/src/cache.rs`, the tests that passed `local_archive: None` or asserted on `PayloadProgress::Connecting` are testing a branch that is about to not exist. Replace them with the same assertions expressed over the required archive. Rename `Fixture::archive_path` usage stays as is.

`warm_cache_is_verified_under_digest_lock_without_network_access` becomes:

```rust
    #[test]
    fn a_warm_cache_is_returned_without_reading_the_archive_again() {
        let fixture = Fixture::new("warm-hit");
        fixture.prepare_local().unwrap();
        // The archive is removed after the cache is warm: a hit must not need
        // it, and a miss would fail loudly on the missing file rather than
        // quietly re-preparing.
        fs::remove_file(&fixture.archive_path).unwrap();

        let ready = prepare(PrepareRequest {
            entry: &fixture.entry,
            cache_root: &fixture.cache_root(),
            archive: &fixture.archive_path,
            progress: &|_| {},
            cancel: &AtomicBool::new(false),
        })
        .expect("a warm cache entry must be returned without any source");

        assert_eq!(ready.payload_id(), "test");
        assert_no_operation_directories(&fixture);
    }
```

Add the case the removed fallback used to hide:

```rust
    #[test]
    fn an_archive_that_is_not_there_is_an_error_and_not_a_fallback() {
        let fixture = Fixture::new("absent-archive");
        let absent = fixture.temporary.path().join("not-here.zip");

        let result = prepare(PrepareRequest {
            entry: &fixture.entry,
            cache_root: &fixture.cache_root(),
            archive: &absent,
            progress: &|_| {},
            cancel: &AtomicBool::new(false),
        });

        assert!(matches!(result, Err(PayloadError::Io { .. })));
    }
```

Delete `a_missing_local_archive_falls_back_to_the_published_url` outright — the behaviour it names is being removed. In the remaining tests (`a_local_archive_is_prepared_without_reaching_the_network`, `a_local_archive_that_does_not_match_the_entry_fails_instead_of_downloading`, `a_truncated_local_archive_fails_on_its_length`, and every other `PrepareRequest` literal in the module) replace `local_archive: Some(&path)` with `archive: &path` and `local_archive: None` with `archive: &fixture.archive_path`, and drop any `PayloadProgress::Connecting` assertion and the `connected` flag that fed it.

- [x] **Step 2: Run the tests to watch them fail**

Run: `cargo test-windows -p vmlord-gpu-payload`
Expected: compile errors — `PrepareRequest` has no field `archive`, `local_archive` is missing from the literals.

- [x] **Step 3: Move `PayloadProgress` into `progress.rs`**

Create `crates/gpu-payload/src/progress.rs`:

```rust
//! What a payload preparation reports while it works.
//!
//! Every stage is local: bytes are hashed, expanded and staged from a file
//! this build ships. Nothing here describes a transfer, because there is no
//! longer one to describe.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadProgress {
    Verifying { hashed: u64, total: u64 },
    Extracting { files: u64, total: u64 },
    Staging { files: u64, total: u64 },
    Ready,
}
```

- [x] **Step 4: Delete the download module and its dependency**

```bash
git rm crates/gpu-payload/src/download.rs
```

In `crates/gpu-payload/src/lib.rs`, replace `mod download;` with `mod progress;` and `pub use download::PayloadProgress;` with `pub use progress::PayloadProgress;`.

In `crates/gpu-payload/Cargo.toml`, delete the `ureq` line. Leave `url` — Task 2 removes it with `archive_url`.

- [x] **Step 5: Make the archive required in `prepare`**

In `crates/gpu-payload/src/cache.rs`, drop `download::LockedArchive` from the `use crate::{...}` list, and replace the `local_archive` field with:

```rust
    /// The archive this release ships for the entry.
    ///
    /// Required, because there is no second source. A file that is not there
    /// is an error and not a reason to look elsewhere: the catalog only
    /// yields an entry when its archive is beside it, so a missing file here
    /// means the release changed under a running application.
    pub archive: &'a Path,
```

Replace the `match request.local_archive.filter(...)` block (cache.rs:94-113) with the single call:

```rust
    let ready = prepare_verified_archive(
        request.entry,
        request.archive,
        &root,
        request.progress,
        request.cancel,
    );
```

- [x] **Step 6: Fix the one out-of-crate caller**

In `crates/platform/src/gpu_staging.rs`, `local_archive: Some(&archive)` becomes `archive: &archive`.

- [x] **Step 7: Run the tests**

Run: `cargo test-windows -p vmlord-gpu-payload -p vmlord-platform`
Expected: PASS.

Run: `cargo check-windows`
Expected: no errors.

- [x] **Step 8: Commit**

```bash
git add -A
git commit -m "TASK-109: Delete the download path a payload never travelled

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 2: Drop `archive_url` from the entry

**Files:**
- Modify: `crates/gpu-payload/src/catalog.rs`, `crates/gpu-payload/src/builder.rs`, `crates/gpu-payload/Cargo.toml`, `crates/gpu-payload/catalog/catalog.json`, `crates/gpu-payload/tests/fixtures/recipe.json`, `payloads/ubuntu-26.04-amd64/payload.spec.json`
- Test: `mod tests` in `catalog.rs`, `cache.rs`, `staging.rs`, `archive.rs`, `manifest.rs`, `builder.rs`

**Interfaces:**
- Consumes: Task 1's `PrepareRequest`.
- Produces: `CatalogEntry` with no `archive_url` accessor; `validate_url` gone.

- [x] **Step 1: Delete the test that pins the removed rule, and the field from every literal**

In `crates/gpu-payload/src/builder.rs` delete `recipe_archive_url_must_be_immutable_https` (builder.rs:1077). In `crates/gpu-payload/src/catalog.rs` delete any test asserting on URL validation.

Remove the `"archive_url": ...` line from every JSON literal: `catalog.rs` (the `catalog()` and `entry_json` helpers), `cache.rs:900`, `staging.rs:1031`, `archive.rs:664`, `manifest.rs:367`, and from `crates/gpu-payload/tests/fixtures/recipe.json`, `crates/gpu-payload/catalog/catalog.json` and `payloads/ubuntu-26.04-amd64/payload.spec.json`.

- [x] **Step 2: Run the tests to watch them fail**

Run: `cargo test-windows -p vmlord-gpu-payload`
Expected: FAIL — `InvalidCatalog("missing field \`archive_url\`")` from the entry documents.

- [x] **Step 3: Remove the field**

In `crates/gpu-payload/src/catalog.rs` delete `archive_url` from `CatalogEntryDocument`, `CatalogEntry`, the `From` impl, the `archive_url()` accessor, the `validate_url(&self.archive_url)?` call in `validate`, the `validate_url` function itself, and `use url::Url;`.

In `crates/gpu-payload/src/builder.rs` delete the `archive_url` field from `PackRecipe` (builder.rs:61) and the `"archive_url": recipe.archive_url,` line from `catalog_entry` (builder.rs:386).

In `crates/gpu-payload/Cargo.toml` delete the `url` dependency.

- [x] **Step 4: Run the tests**

Run: `cargo test-windows -p vmlord-gpu-payload`
Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add -A
git commit -m "TASK-109: Stop claiming a URL nothing publishes

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 3: Measure the archive instead of trusting `archive_size`

The declared size backed two mechanisms. Both now take the length of the file on disk, which the digest already pins.

**Files:**
- Modify: `crates/gpu-payload/src/catalog.rs`, `crates/gpu-payload/src/cache.rs`, `crates/gpu-payload/src/archive.rs`, `crates/gpu-payload/src/builder.rs`, `crates/xtask/src/gpu_payload.rs`, `crates/gpu-payload/catalog/catalog.json`
- Test: `mod tests` in `archive.rs`, `cache.rs`, `staging.rs`, `manifest.rs`, `catalog.rs`, `crates/xtask/src/gpu_payload.rs`

**Interfaces:**
- Consumes: Task 2's `CatalogEntry`.
- Produces: `pub(crate) fn extract(entry: &CatalogEntry, archive: &Path, archive_length: u64, destination: &Path, progress: &dyn Fn(PayloadProgress), cancel: &AtomicBool) -> Result<(PayloadManifest, SourceManifest), PayloadError>`; `CatalogEntry` with no `archive_size()`.

- [x] **Step 1: Move the compressed-size limit onto `extract`'s argument in the tests**

The guard at `archive.rs:199-204` refuses an archive whose central directory claims more compressed bytes than the archive holds. It keeps its test lever by taking the length as an argument rather than reading it from the entry: the caller is the one holding the file, and one caller measuring beats an entry claiming.

In `crates/gpu-payload/src/archive.rs` change the test helpers so `entry(...)` no longer takes `archive_size_limit` and `extract_fixture` passes it to `extract` instead:

```rust
    fn extract_fixture(
        archive: &[u8],
        payload: &[u8],
        archive_length: u64,
        expanded_size_limit: u64,
        file_count_limit: u64,
        cancelled: bool,
    ) -> Result<(), PayloadError> {
        let temporary = TemporaryDirectory::new("extract");
        let archive_path = temporary.path().join("archive.zip");
        let destination = temporary.path().join("files");
        fs::write(&archive_path, archive).unwrap();
        fs::create_dir(&destination).unwrap();
        let entry = entry(archive, payload, expanded_size_limit, file_count_limit);
        extract(
            &entry,
            &archive_path,
            archive_length,
            &destination,
            &|_| {},
            &AtomicBool::new(cancelled),
        )
        .map(|_| ())
    }
```

Every other call site of `entry(...)` in that module drops its third argument. `compressed_and_expanded_limits_are_enforced` keeps both halves unchanged — it already passes `1` and `archive.len() as u64` as that position.

- [x] **Step 2: Add the cache test that pins what replaces the declared size**

In `crates/gpu-payload/src/cache.rs`:

```rust
    #[test]
    fn a_corrupt_archive_is_caught_by_its_digest_and_nothing_else() {
        let fixture = Fixture::new("corrupt");
        let corrupt = fixture.temporary.path().join("corrupt.zip");
        let mut bytes = fixture.archive.clone();
        // Same length, different content: with no declared size left, the
        // digest is the whole check, and it must be enough.
        *bytes.last_mut().unwrap() ^= 0xff;
        fs::write(&corrupt, &bytes).unwrap();

        let result = prepare(PrepareRequest {
            entry: &fixture.entry,
            cache_root: &fixture.cache_root(),
            archive: &corrupt,
            progress: &|_| {},
            cancel: &AtomicBool::new(false),
        });

        assert!(matches!(result, Err(PayloadError::DigestMismatch { .. })));
        assert_no_operation_directories(&fixture);
    }
```

Rename `a_truncated_local_archive_fails_on_its_length` to `a_truncated_archive_fails_on_its_digest` and change its expectation from `PayloadError::ArchiveSizeMismatch { .. }` to `PayloadError::DigestMismatch { .. }`.

Then remove `"archive_size": ...` from every JSON literal named in Task 2 Step 1, plus `crates/gpu-payload/catalog/catalog.json`.

- [x] **Step 3: Run the tests to watch them fail**

Run: `cargo test-windows -p vmlord-gpu-payload`
Expected: FAIL — `extract` takes five arguments, `archive_size` is a missing field.

- [x] **Step 4: Remove the field and measure instead**

In `crates/gpu-payload/src/catalog.rs` delete `archive_size` from `CatalogEntryDocument`, `CatalogEntry`, the `From` impl, the `archive_size()` accessor, and both conditions that mention it in `validate` (`self.archive_size == 0` and `self.expanded_size_limit < self.archive_size`).

In `crates/gpu-payload/src/archive.rs` give `extract` the new parameter and pass it down:

```rust
pub(crate) fn extract(
    entry: &CatalogEntry,
    archive: &Path,
    archive_length: u64,
    destination: &Path,
    progress: &dyn Fn(PayloadProgress),
    cancel: &AtomicBool,
) -> Result<(PayloadManifest, SourceManifest), PayloadError> {
```

`inspect_entries(&mut zip, entry)` becomes `inspect_entries(&mut zip, entry, archive_length)`, and inside it the limit argument at archive.rs:203 becomes `archive_length` instead of `entry.archive_size()`.

In `crates/gpu-payload/src/cache.rs`:

* in `prepare_verified_archive`, measure the source before copying and hand the length to both steps:

```rust
    let archive_length = require_regular_file(archive, "read release archive")?;
    let cached_archive = temporary.path().join("archive.zip");
    copy_and_flush(archive, &cached_archive, archive_length, cancel)?;
```

  and `archive::extract(entry, &cached_archive, archive_length, &files_directory, progress, cancel)?`;

* in `load_ready`, delete the `if archive_size != entry.archive_size()` block and use the measured size for the progress total and for `extract`'s callers:

```rust
    let archive_size = require_regular_file(&archive_path, "verify cached archive")?;
    progress(PayloadProgress::Verifying {
        hashed: 0,
        total: archive_size,
    });
```

In `crates/gpu-payload/src/builder.rs` delete `archive_size` from the emitted entry (builder.rs:244 and the `catalog_entry` parameter and `"archive_size": archive_size,` line). Keep the local `archive_size` binding at builder.rs:193 — `expanded_size_limit = expanded_bytes.max(archive_size)` still needs it.

In `crates/xtask/src/gpu_payload.rs` delete the size comparison from `stage_release_payload` (the `if size != entry.archive_size()` block); the digest check that follows it stays.

`PayloadError::ArchiveSizeMismatch` stays. Update its doc-adjacent use: it now only reports a source file that changed length while being copied.

- [x] **Step 5: Run the tests**

Run: `cargo test-windows -p vmlord-gpu-payload -p xtask`
Expected: PASS.

- [x] **Step 6: Commit**

```bash
git add -A
git commit -m "TASK-109: Measure the archive rather than believe its entry

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 4: Remove `vmlord_revision` and `builder_version` everywhere

They existed to be cross-checked against each other. With the entry silent, a self-reported revision inside a digest-pinned archive proves nothing the digest does not.

**Files:**
- Modify: `crates/gpu-payload/src/catalog.rs`, `crates/gpu-payload/src/manifest.rs`, `crates/gpu-payload/src/builder.rs`, `crates/gpu-payload/catalog/catalog.json`, `crates/gpu-payload/tests/fixtures/recipe.json`, `crates/gpu-payload/tests/fixtures/prepared/sources.json`, `payloads/ubuntu-26.04-amd64/payload.spec.json`, `payloads/ubuntu-26.04-amd64/prepare.py`, `payloads/ubuntu-26.04-amd64/prepare.sh`, `payloads/ubuntu-26.04-amd64/README.md`
- Test: `mod tests` in `catalog.rs`, `manifest.rs`, `builder.rs`, `cache.rs`, `staging.rs`, `archive.rs`

**Interfaces:**
- Consumes: Task 3's `CatalogEntry`.
- Produces: `CatalogEntry` with no `vmlord_revision()` or `builder_version()`; `sources.json` at `schema_version: 1` with fields `target`, `mesa_policy`, `sources`, `overlays` only.

- [x] **Step 1: Delete the tests that pin the cross-check**

Delete `catalog_provenance_requires_vmlord_revision_and_builder_version` (catalog.rs:484). In `crates/gpu-payload/src/manifest.rs` remove the `"vmlord_revision"` and `"builder"` cases from the mismatch-driving test at manifest.rs:520-522 and the two assertions at manifest.rs:564-566. In `crates/gpu-payload/src/builder.rs` remove the `"vmlord_revision"`/`"builder"` cases at builder.rs:881-884.

Remove both fields from every JSON literal in `catalog.rs`, `cache.rs`, `staging.rs`, `archive.rs`, `manifest.rs` (both the entry literals and the `sources.json` literals), and from `crates/gpu-payload/tests/fixtures/recipe.json`, `crates/gpu-payload/tests/fixtures/prepared/sources.json`, `crates/gpu-payload/catalog/catalog.json` and `payloads/ubuntu-26.04-amd64/payload.spec.json`.

- [x] **Step 2: Run the tests to watch them fail**

Run: `cargo test-windows -p vmlord-gpu-payload`
Expected: FAIL — missing fields in the entry and in `sources.json` (`deny_unknown_fields` makes the reverse fail too).

- [x] **Step 3: Remove the fields**

In `crates/gpu-payload/src/catalog.rs`: both fields from `CatalogEntryDocument`, `CatalogEntry`, the `From` impl, both accessors, and the four `validate` conditions that check the revision's 40 hex digits and the builder version's emptiness.

In `crates/gpu-payload/src/manifest.rs`: both fields from `SourceManifestDocument`, and the two comparisons at manifest.rs:179-180 from `parse_and_validate`.

In `crates/gpu-payload/src/builder.rs`: both fields from `PackRecipe` (builder.rs:64-65) and from `PreparedSources` (builder.rs:113-114), the two comparisons in `validate_prepared_provenance` (builder.rs:411-412), and both lines from `catalog_entry`.

In `payloads/ubuntu-26.04-amd64/prepare.py`: the `--revision` argument, its 40-hex validation, and both keys in the emitted `sources.json`.

In `payloads/ubuntu-26.04-amd64/prepare.sh`: the `revision="$(git -C "$repository" rev-parse HEAD)"` line, the dirty-tree warning that follows it, and the `--revision "$revision"` argument to `prepare.py`.

In `payloads/ubuntu-26.04-amd64/README.md`: delete the "Commit before building. `vmlord_revision` is this repository's `HEAD` …" paragraph.

- [x] **Step 4: Run the tests**

Run: `cargo test-windows -p vmlord-gpu-payload`
Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add -A
git commit -m "TASK-109: Drop provenance that only agreed with itself

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 5: One entry, one file, schema 2

The entry becomes a release artifact with its own version, and the crate reads it in one place instead of three.

**Files:**
- Modify: `crates/gpu-payload/src/catalog.rs`, `crates/gpu-payload/src/builder.rs`, `crates/gpu-payload/src/lib.rs`, `crates/xtask/src/gpu_payload.rs`
- Test: `mod tests` in `catalog.rs`, `cache.rs`, `staging.rs`, `archive.rs`, `manifest.rs`, `builder.rs`, `crates/xtask/src/gpu_payload.rs`

**Interfaces:**
- Consumes: Task 4's `CatalogEntry`.
- Produces: `impl CatalogEntry { pub fn from_json(bytes: &[u8]) -> Result<Self, PayloadError> }` reading a single entry document at `schema_version: 2`; `fn PayloadCatalog::from_entries(entries: Vec<CatalogEntry>) -> Result<PayloadCatalog, PayloadError>` (private to the crate); `PayloadCatalog::from_json` and `from_entry_json` no longer exist; `#[cfg(test)] pub(crate) fn test_entry(value: serde_json::Value) -> CatalogEntry` in `catalog.rs`.

- [x] **Step 1: Write the failing tests for the new reader**

In `crates/gpu-payload/src/catalog.rs`'s `mod tests`, replace the document-shaped helpers with entry-shaped ones and add:

```rust
    #[test]
    fn an_entry_is_a_document_of_its_own_at_schema_two() {
        let entry = CatalogEntry::from_json(entry_json("ubuntu", "26.04", "amd64", "7.0.0-14-generic").as_bytes())
            .expect("a schema 2 entry document must be read");
        assert_eq!(entry.payload_id(), "ubuntu-26.04-amd64-7.0.0-14-generic");
    }

    #[test]
    fn an_entry_at_another_schema_version_is_refused() {
        let mut value: serde_json::Value =
            serde_json::from_str(&entry_json("ubuntu", "26.04", "amd64", "k")).unwrap();
        value["schema_version"] = 1.into();
        assert!(matches!(
            CatalogEntry::from_json(&serde_json::to_vec(&value).unwrap()),
            Err(PayloadError::InvalidCatalog(_))
        ));
    }
```

`entry_json` gains `"schema_version":2,` at the front and no longer wraps in `{"entries": [...]}`. `catalog_with(&[String])` becomes a helper that maps each string through `CatalogEntry::from_json` and builds the catalog through a new private constructor, which is where the uniqueness checks that `from_json` used to run now live:

```rust
impl PayloadCatalog {
    /// The catalog a set of read entries forms, with the uniqueness a
    /// selection depends on checked once, here.
    fn from_entries(entries: Vec<CatalogEntry>) -> Result<Self, PayloadError> {
        let mut ids = HashSet::new();
        let mut targets = HashSet::new();
        for entry in &entries {
            if !ids.insert(entry.payload_id.clone()) || !targets.insert(entry.target.clone()) {
                return Err(PayloadError::InvalidCatalog(
                    "duplicate payload ID or target".into(),
                ));
            }
        }
        Ok(Self { entries })
    }
}
```

Add the shared test constructor so the other modules stop spelling the document out:

```rust
    #[cfg(test)]
    pub(crate) fn test_entry(mut value: serde_json::Value) -> CatalogEntry {
        value["schema_version"] = 2.into();
        CatalogEntry::from_json(&serde_json::to_vec(&value).unwrap())
            .expect("the test entry must be a valid entry document")
    }
```

placed in `catalog.rs` outside `mod tests` behind `#[cfg(test)]`, and re-exported from `lib.rs` as `#[cfg(test)] pub(crate) use catalog::test_entry;`.

- [x] **Step 2: Point every other test module at it**

In `cache.rs`, `staging.rs`, `archive.rs` and `manifest.rs`, replace each

```rust
        let entry = PayloadCatalog::from_json(&serde_json::to_vec(&catalog).unwrap())
            .unwrap()
            .entries()[0]
            .clone();
```

with `let entry = crate::test_entry(entry_value);`, where `entry_value` is the former `catalog["entries"][0]` object literal — that is, delete the `{"schema_version": 1, "entries": [ ... ]}` wrapper and keep the object inside it.

- [x] **Step 3: Run the tests to watch them fail**

Run: `cargo test-windows -p vmlord-gpu-payload`
Expected: FAIL — `CatalogEntry::from_json` and `test_entry` do not exist.

- [x] **Step 4: Implement the reader**

In `crates/gpu-payload/src/catalog.rs`:

* change `const CATALOG_SCHEMA_VERSION: u32 = 1;` to `const ENTRY_SCHEMA_VERSION: u32 = 2;`;
* delete `struct CatalogDocument`, `PayloadCatalog::from_json` and `PayloadCatalog::from_entry_json`;
* add `schema_version: u32` to `CatalogEntryDocument` (it is not carried onto `CatalogEntry` — a validated entry has no version left to disagree about);
* add:

```rust
impl CatalogEntry {
    /// Reads one entry document, as `cargo xtask gpu-payload pack` writes it
    /// and as a release carries it beside its archive.
    pub fn from_json(bytes: &[u8]) -> Result<Self, PayloadError> {
        let document: CatalogEntryDocument = serde_json::from_slice(bytes)
            .map_err(|error| PayloadError::InvalidCatalog(error.to_string()))?;
        if document.schema_version != ENTRY_SCHEMA_VERSION {
            return Err(PayloadError::InvalidCatalog(
                "unknown catalog entry schema version".into(),
            ));
        }
        let entry = Self::from(document);
        entry.validate()?;
        Ok(entry)
    }
}
```

In `crates/gpu-payload/src/lib.rs`, the `pub use catalog::{...}` list is unchanged apart from the `#[cfg(test)] pub(crate) use catalog::test_entry;` line.

In `crates/gpu-payload/src/builder.rs`, `catalog_entry` emits `"schema_version": 2,` as its first key, and the `pack` self-check that reads back what it wrote uses `CatalogEntry::from_json`.

In `crates/xtask/src/gpu_payload.rs`, `PayloadCatalog::from_entry_json(&entry_bytes)` becomes `CatalogEntry::from_json(&entry_bytes)`; adjust the `use` list.

- [x] **Step 5: Run the tests**

Run: `cargo test-windows -p vmlord-gpu-payload -p xtask`
Expected: PASS.

- [x] **Step 6: Commit**

```bash
git add -A
git commit -m "TASK-109: Give the catalog entry a schema of its own

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 6: Assemble the catalog from the release directory

**Files:**
- Modify: `crates/gpu-payload/src/catalog.rs`, `crates/gpu-payload/src/release.rs`, `crates/gpu-payload/src/lib.rs`, `crates/platform/src/gpu_staging.rs`
- Delete: `crates/gpu-payload/catalog/catalog.json` (and the now-empty `crates/gpu-payload/catalog/` directory)
- Test: `mod tests` in `catalog.rs`, `release.rs`, `crates/platform/src/gpu_staging.rs`

**Interfaces:**
- Consumes: Task 5's `CatalogEntry::from_json` and `PayloadCatalog::from_entries`.
- Produces: `PayloadCatalog::from_release_directory(directory: &Path) -> Result<PayloadCatalog, PayloadError>`, where `directory` is the executable's; `release::local_entry_path(directory: &Path, payload_id: &str) -> PathBuf`.

- [x] **Step 1: Write the failing tests**

In `crates/gpu-payload/src/release.rs`'s `mod tests`:

```rust
    #[test]
    fn a_release_keeps_each_entry_beside_its_archive() {
        assert_eq!(
            local_entry_path(Path::new("dist"), "ubuntu-26.04-amd64-7.0.0-28-v1"),
            PathBuf::from("dist")
                .join("gpu-payload")
                .join("ubuntu-26.04-amd64-7.0.0-28-v1.json")
        );
    }
```

In `crates/gpu-payload/src/catalog.rs`'s `mod tests`, add a temporary-directory helper matching the one in `cache.rs` (same `TemporaryDirectory` shape, label prefix `vmlord-gpu-payload-catalog-`) and a writer:

```rust
    /// Writes the pair a release carries for one entry.
    fn write_pair(directory: &Path, payload_id: &str, entry: &str) {
        let gpu_payload = directory.join("gpu-payload");
        fs::create_dir_all(&gpu_payload).unwrap();
        fs::write(gpu_payload.join(format!("{payload_id}.json")), entry).unwrap();
        fs::write(gpu_payload.join(format!("{payload_id}.zip")), b"archive").unwrap();
    }

    #[test]
    fn a_release_directory_is_read_as_the_catalog_it_holds() {
        let temporary = TemporaryDirectory::new("pair");
        write_pair(
            temporary.path(),
            "ubuntu-26.04-amd64-7.0.0-14-generic",
            &entry_json("ubuntu", "26.04", "amd64", "7.0.0-14-generic"),
        );

        let catalog = PayloadCatalog::from_release_directory(temporary.path()).unwrap();

        assert_eq!(
            catalog.select_for_guest(&ubuntu_2604()).unwrap().payload_id(),
            "ubuntu-26.04-amd64-7.0.0-14-generic"
        );
    }

    #[test]
    fn a_build_that_ships_no_payload_has_an_empty_catalog_and_not_an_error() {
        let temporary = TemporaryDirectory::new("empty");
        // Three shapes of "nothing": no directory at all, an empty one, and
        // one holding an archive no entry claims.
        let absent = PayloadCatalog::from_release_directory(&temporary.path().join("absent"));
        fs::create_dir(temporary.path().join("gpu-payload")).unwrap();
        let empty = PayloadCatalog::from_release_directory(temporary.path());
        fs::write(
            temporary.path().join("gpu-payload").join("stray.zip"),
            b"archive",
        )
        .unwrap();
        let stray = PayloadCatalog::from_release_directory(temporary.path());

        for catalog in [absent, empty, stray] {
            let catalog = catalog.expect("a release without a payload is a release without GPU");
            assert!(catalog.entries().is_empty());
            assert!(matches!(
                catalog.select_for_guest(&ubuntu_2604()),
                Err(PayloadError::NoPayloadForGuest { .. })
            ));
        }
    }

    #[test]
    fn an_entry_file_that_is_there_and_wrong_fails_the_catalog() {
        let temporary = TemporaryDirectory::new("broken");
        let valid = entry_json("ubuntu", "26.04", "amd64", "7.0.0-14-generic");

        // Not JSON.
        write_pair(temporary.path(), "a", "{not json");
        assert!(PayloadCatalog::from_release_directory(temporary.path()).is_err());
        fs::remove_dir_all(temporary.path().join("gpu-payload")).unwrap();

        // Named something other than its own payload ID.
        write_pair(temporary.path(), "wrong-name", &valid);
        assert!(PayloadCatalog::from_release_directory(temporary.path()).is_err());
        fs::remove_dir_all(temporary.path().join("gpu-payload")).unwrap();

        // An entry whose archive is not beside it.
        write_pair(
            temporary.path(),
            "ubuntu-26.04-amd64-7.0.0-14-generic",
            &valid,
        );
        fs::remove_file(
            temporary
                .path()
                .join("gpu-payload")
                .join("ubuntu-26.04-amd64-7.0.0-14-generic.zip"),
        )
        .unwrap();
        assert!(PayloadCatalog::from_release_directory(temporary.path()).is_err());
    }

    #[test]
    fn two_entries_for_one_guest_fail_rather_than_depend_on_directory_order() {
        let temporary = TemporaryDirectory::new("duplicate");
        let entry = entry_json("ubuntu", "26.04", "amd64", "7.0.0-14-generic");
        write_pair(temporary.path(), "ubuntu-26.04-amd64-7.0.0-14-generic", &entry);
        // Same target under a second name: the file name must match the ID, so
        // this one is rejected on its name -- and would be rejected on its
        // target if it were not.
        let mut second: serde_json::Value = serde_json::from_str(&entry).unwrap();
        second["payload_id"] = "second".into();
        write_pair(
            temporary.path(),
            "second",
            &serde_json::to_string(&second).unwrap(),
        );

        assert!(matches!(
            PayloadCatalog::from_release_directory(temporary.path()),
            Err(PayloadError::InvalidCatalog(_))
        ));
    }
```

Note `entry_json`'s `payload_id` is `"{distribution}-{release}-{architecture}-{kernel}"`, which is why the file names above read as they do.

- [x] **Step 2: Run the tests to watch them fail**

Run: `cargo test-windows -p vmlord-gpu-payload`
Expected: FAIL — `from_release_directory` and `local_entry_path` do not exist.

- [x] **Step 3: Implement the layout rule**

In `crates/gpu-payload/src/release.rs`:

```rust
/// The entry document for `payload_id` below `directory`.
///
/// The pair is named by the payload's own ID: one directory listing then says
/// which payloads a release carries, and an entry cannot describe an archive
/// other than the one beside it.
pub fn local_entry_path(directory: &Path, payload_id: &str) -> PathBuf {
    directory
        .join(LOCAL_ARCHIVE_DIRECTORY)
        .join(format!("{payload_id}.json"))
}

/// The directory a release keeps its payload pairs in.
pub(crate) fn local_payload_directory(directory: &Path) -> PathBuf {
    directory.join(LOCAL_ARCHIVE_DIRECTORY)
}
```

- [x] **Step 4: Implement the reader**

In `crates/gpu-payload/src/catalog.rs`:

```rust
impl PayloadCatalog {
    /// The catalog a release carries beside its executable.
    ///
    /// `directory` is the one holding the executable; the `gpu-payload` child
    /// is `release.rs`'s to name. A child that is not there, cannot be listed,
    /// or holds no entry is an empty catalog rather than an error: a build
    /// without a payload is a build without GPU support, and GPU support is
    /// best effort. A file that *is* there and is wrong fails, because that is
    /// a broken release and a silent absence is the worst way to learn it.
    pub fn from_release_directory(directory: &Path) -> Result<Self, PayloadError> {
        let payloads = crate::release::local_payload_directory(directory);
        let Ok(listing) = fs::read_dir(&payloads) else {
            return Self::from_entries(Vec::new());
        };
        let mut entries = Vec::new();
        for item in listing {
            let Ok(item) = item else {
                continue;
            };
            let path = item.path();
            if path.extension().and_then(OsStr::to_str) != Some("json") {
                continue;
            }
            let bytes = fs::read(&path)
                .map_err(|error| PayloadError::io("read GPU payload entry", path.clone(), error))?;
            let entry = CatalogEntry::from_json(&bytes)?;
            let name = path.file_stem().and_then(OsStr::to_str);
            if name != Some(entry.payload_id()) {
                return Err(PayloadError::InvalidCatalog(format!(
                    "{} does not name its payload ID {}",
                    path.display(),
                    entry.payload_id()
                )));
            }
            let archive = crate::local_archive_path(directory, entry.payload_id());
            if !archive.is_file() {
                return Err(PayloadError::InvalidCatalog(format!(
                    "payload {} has no archive at {}",
                    entry.payload_id(),
                    archive.display()
                )));
            }
            entries.push(entry);
        }
        Self::from_entries(entries)
    }
}
```

`use std::{ffi::OsStr, fs, path::Path};` joins the module's imports. `PayloadCatalog::embedded` is deleted.

- [x] **Step 5: Delete the compiled-in catalog**

```bash
git rm crates/gpu-payload/catalog/catalog.json
```

- [x] **Step 6: Point the staging service at the release directory**

In `crates/platform/src/gpu_staging.rs`, `let catalog = PayloadCatalog::embedded()?;` becomes:

```rust
    let catalog = PayloadCatalog::from_release_directory(request.executable_directory)?;
```

The existing test `a_guest_the_catalog_has_nothing_for_stages_nothing` keeps its meaning and gains a sharper one — its `executable_directory` is a temporary directory with no `gpu-payload` child, so it now exercises the empty-catalog rule end to end. Update its comment to say so.

- [x] **Step 7: Run the tests**

Run: `cargo test-windows -p vmlord-gpu-payload -p vmlord-platform`
Expected: PASS.

Run: `cargo check-windows`
Expected: no errors.

- [x] **Step 8: Commit**

```bash
git add -A
git commit -m "TASK-109: Assemble the catalog from the release directory

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 7: `cargo dist` ships the pair

**Files:**
- Modify: `crates/xtask/src/gpu_payload.rs`, `crates/xtask/src/main.rs`
- Test: `mod tests` in `crates/xtask/src/gpu_payload.rs`

**Interfaces:**
- Consumes: Task 6's `local_entry_path` and `local_archive_path`.
- Produces: `stage_release_payload(source: &Path, destination: &Path) -> Result<String, String>` writing two files.

- [x] **Step 1: Write the failing test**

In `crates/xtask/src/gpu_payload.rs`'s `mod tests`, extend the test that copies a valid pair (currently asserting on `gpu-payload/<id>.zip` around gpu_payload.rs:299):

```rust
        let payload_id = stage_release_payload(source.path(), destination.path()).unwrap();

        let archive = destination
            .path()
            .join("gpu-payload")
            .join(format!("{payload_id}.zip"));
        let entry = destination
            .path()
            .join("gpu-payload")
            .join(format!("{payload_id}.json"));
        assert!(archive.is_file(), "the archive must travel");
        assert!(entry.is_file(), "the entry must travel beside it");
        // The catalog the application will assemble must accept what dist
        // wrote, or the release ships a pair nothing can read.
        assert_eq!(
            PayloadCatalog::from_release_directory(destination.path())
                .unwrap()
                .entries()
                .len(),
            1
        );
```

- [x] **Step 2: Run the test to watch it fail**

Run: `cargo test-windows -p xtask`
Expected: FAIL — `<id>.json` is not there.

- [x] **Step 3: Copy both files**

In `stage_release_payload`, after the digest check, write the entry as well:

```rust
    let target = local_archive_path(destination, entry.payload_id());
    let directory = target
        .parent()
        .expect("a payload archive path always has a parent");
    fs::create_dir_all(directory)
        .map_err(|error| format!("cannot create {}: {error}", directory.display()))?;
    fs::write(&target, &archive)
        .map_err(|error| format!("cannot write {}: {error}", target.display()))?;
    let entry_target = local_entry_path(destination, entry.payload_id());
    fs::write(&entry_target, &entry_bytes)
        .map_err(|error| format!("cannot write {}: {error}", entry_target.display()))?;
    Ok(entry.payload_id().to_owned())
```

Update the function's doc comment: the entry travels now, and the paragraph explaining that a catalog beside the executable would be one an attacker can edit is replaced by the spec's trust paragraph — whoever can write there can replace `vmlord.exe`, so the boundary becomes visible rather than moving.

In `crates/xtask/src/main.rs`, the line printed after staging becomes:

```rust
        println!("dist: gpu-payload/{payload_id}.zip and gpu-payload/{payload_id}.json");
```

- [x] **Step 4: Run the tests**

Run: `cargo test-windows -p xtask`
Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add -A
git commit -m "TASK-109: Ship the entry beside the archive it describes

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 8: Documentation

**Files:**
- Modify: `ARCHITECTURE.md`, `payloads/ubuntu-26.04-amd64/README.md`

**Interfaces:**
- Consumes: every task above.
- Produces: nothing code depends on.

- [x] **Step 1: Rewrite the payload section of ARCHITECTURE.md**

Find the passage at ARCHITECTURE.md:1190 describing the local archive with `archive_url` as fallback. Replace the whole local-archive/URL discussion with the runtime catalog, keeping the document's voice:

* the release carries `gpu-payload/<payload_id>.json` and `.zip` beside `vmlord.exe`, named by the payload's ID;
* `PayloadCatalog::from_release_directory` reads the executable's directory; no directory and no entries both mean an empty catalog, so a build without a payload starts VMs without GPU support rather than failing;
* a present-but-wrong entry fails the catalog: unreadable JSON, a failed validation, a name that is not its `payload_id`, or a missing archive;
* `prepare` takes the archive as a required path and no longer has a network path at all;
* the trust model: the entry is no longer trusted for being compiled in; whoever can write into the installation directory can equally replace the executable, so the boundary is unchanged and now visible. Reading a payload from a user directory or from configuration stays refused — the directory comes from `current_exe` through `release.rs` and nowhere else;
* the archive is checked by `archive_sha256` alone; its length is measured, not claimed.

- [x] **Step 2: Rewrite the payload README**

In `payloads/ubuntu-26.04-amd64/README.md`:

* the `dist` paragraph (README.md:25-30) says that both files travel — the entry to `gpu-payload/<payload_id>.json` and the archive to `gpu-payload/<payload_id>.zip` — and that `dist` validates the entry and hashes the archive before copying;
* delete the "Before this can be published" section entirely: there is no URL and nothing to publish, and the catalog is no longer pasted into the crate;
* in "What the spec holds", the builder still reads provenance twice and refuses a disagreeing pair — that is about `sources`, `overlays` and `target`, which stay.

- [x] **Step 3: Verify the whole workspace**

Run: `cargo check-windows`
Expected: no errors.

Run: `cargo test-windows`
Expected: PASS across the workspace.

Confirm nothing references the removed names:

```bash
grep -rn "archive_url\|vmlord_revision\|builder_version\|embedded()\|from_entry_json\|ureq" crates payloads ARCHITECTURE.md docs/superpowers/plans
```

Expected: no hits outside the design document, which records history deliberately.

- [x] **Step 4: Commit**

```bash
git add -A
git commit -m "TASK-109: Record the runtime catalog and its trust model

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```
