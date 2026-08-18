# GPU Payload in the Release Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make an archive shipped beside `vmlord.exe` the primary source of a GPU payload, keep `archive_url` as the fallback, and give the host a service that turns a prepared generation into a VM's `gpu-payload` staging directory.

**Architecture:** `PrepareRequest` gains a `local_archive` field that `prepare` consults before it reaches for the network; one crate function owns the release layout so `cargo dist` and the running application cannot disagree about it; `cargo dist --gpu-payload <dir>` verifies a `pack` output pair and copies the archive into the distribution; a new `platform::gpu_staging` module chains catalog selection, `prepare` and `stage_payload` into `<vm>/gpu-payload`.

**Tech Stack:** Rust 2024, `vmlord-gpu-payload` (sha2, zip, ureq), `xtask`, `vmlord-platform` (Windows).

**Spec:** `docs/superpowers/specs/2026-08-18-gpu-payload-in-release-design.md`

## Global Constraints

* Commit subjects are `TASK-103: <comment>`; every commit ends with the trailer `Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>`.
* The embedded catalog `crates/gpu-payload/catalog/catalog.json` stays `{"schema_version":1,"entries":[]}`. Do not add an entry.
* Trust is not relaxed for a local archive: it goes through `prepare_verified_archive` exactly as a downloaded one does.
* `prepare_verified_archive` stays `pub(crate)`. Do not widen it.
* `catalog-entry.json` is a build-time input only; it must not be copied into the distribution.
* No field is added to `AppSettings`.
* `crates/gpu-payload` and `crates/xtask` build and test on Linux: use `cargo test -p vmlord-gpu-payload` and `cargo test -p xtask`. `crates/platform` is Windows-only: use `cargo test-windows` and `cargo check-windows` from WSL.
* Work happens on the branch `task-103-gpu-payload-in-release`, which already exists and already holds the spec commit.

---

### Task 1: The crate learns about local archives

**Files:**
- Create: `crates/gpu-payload/src/release.rs`
- Modify: `crates/gpu-payload/src/lib.rs`
- Modify: `crates/gpu-payload/src/cache.rs` (the `PrepareRequest` struct near line 21, the `prepare` function near line 62, and the test module's `PrepareRequest` literals near lines 983 and 1084)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `pub struct PrepareRequest<'a>` with the new field `pub local_archive: Option<&'a Path>` between `cache_root` and `progress`.
  - `pub const LOCAL_ARCHIVE_DIRECTORY: &str` = `"gpu-payload"`.
  - `pub fn local_archive_path(directory: &Path, payload_id: &str) -> PathBuf`.

- [ ] **Step 1: Add the field and fix the existing call sites so the crate compiles**

In `crates/gpu-payload/src/cache.rs`, change the struct:

```rust
pub struct PrepareRequest<'a> {
    pub entry: &'a CatalogEntry,
    pub cache_root: &'a Path,
    /// The archive this build ships beside its executable, when it has one.
    ///
    /// A path naming a regular file is prepared from without any network
    /// access at all. A path naming nothing falls back to `archive_url`: a
    /// build that shipped without its payload is not broken, only online. A
    /// file that is present and does not match the entry is an error --
    /// falling back would put a host on the network exactly when someone
    /// arranged for it not to be, and would hide a substituted release
    /// artifact behind a successful start.
    pub local_archive: Option<&'a Path>,
    pub progress: &'a dyn Fn(PayloadProgress),
    pub cancel: &'a AtomicBool,
}
```

Then add `local_archive: None,` to the two `PrepareRequest { .. }` literals in the test module of the same file (in `warm_cache_is_verified_under_digest_lock_without_network_access` and `digest_lock_contention_is_reported_before_cache_or_network_work`), so the existing tests keep compiling and keep asserting the download path.

- [ ] **Step 2: Run the crate's tests to confirm the field changed nothing yet**

Run: `cargo test -p vmlord-gpu-payload`
Expected: PASS.

- [ ] **Step 3: Write the four failing tests for the source choice**

Add to the test module at the end of `crates/gpu-payload/src/cache.rs` (inside `mod tests`, which already has `Fixture`, `PayloadProgress`, `fs`, `Ordering` and `AtomicBool` in scope):

```rust
    #[test]
    fn a_local_archive_is_prepared_without_reaching_the_network() {
        let fixture = Fixture::new("local-source");
        let connected = AtomicBool::new(false);
        let progress = |event| {
            if matches!(
                event,
                PayloadProgress::Connecting | PayloadProgress::Downloading { .. }
            ) {
                connected.store(true, Ordering::Relaxed);
            }
        };

        let ready = prepare(PrepareRequest {
            entry: &fixture.entry,
            cache_root: &fixture.cache_root(),
            local_archive: Some(&fixture.archive_path),
            progress: &progress,
            cancel: &AtomicBool::new(false),
        })
        .unwrap();

        assert_eq!(ready.generation(), fixture.entry.archive_sha256());
        assert!(!connected.load(Ordering::Relaxed));
    }

    #[test]
    fn a_local_archive_that_does_not_match_the_entry_fails_instead_of_downloading() {
        let fixture = Fixture::new("local-corrupt");
        let corrupt = fixture.temporary.path().join("corrupt.zip");
        let mut bytes = fixture.archive.clone();
        bytes[0] ^= 0xFF;
        fs::write(&corrupt, &bytes).unwrap();
        let connected = AtomicBool::new(false);
        let progress = |event| {
            if matches!(
                event,
                PayloadProgress::Connecting | PayloadProgress::Downloading { .. }
            ) {
                connected.store(true, Ordering::Relaxed);
            }
        };

        let result = prepare(PrepareRequest {
            entry: &fixture.entry,
            cache_root: &fixture.cache_root(),
            local_archive: Some(&corrupt),
            progress: &progress,
            cancel: &AtomicBool::new(false),
        });

        assert!(matches!(result, Err(PayloadError::DigestMismatch { .. })));
        assert!(!connected.load(Ordering::Relaxed));
    }

    #[test]
    fn a_truncated_local_archive_fails_on_its_length() {
        let fixture = Fixture::new("local-short");
        let short = fixture.temporary.path().join("short.zip");
        fs::write(&short, &fixture.archive[..fixture.archive.len() - 1]).unwrap();

        let result = prepare(PrepareRequest {
            entry: &fixture.entry,
            cache_root: &fixture.cache_root(),
            local_archive: Some(&short),
            progress: &|_| {},
            cancel: &AtomicBool::new(false),
        });

        assert!(matches!(
            result,
            Err(PayloadError::ArchiveSizeMismatch { .. })
        ));
    }

    #[test]
    fn a_missing_local_archive_falls_back_to_the_published_url() {
        let fixture = Fixture::new("local-absent");
        let absent = fixture.temporary.path().join("absent.zip");
        let connected = AtomicBool::new(false);
        let progress = |event| {
            if event == PayloadProgress::Connecting {
                connected.store(true, Ordering::Relaxed);
            }
        };

        // The fixture's `archive_url` is `https://offline.invalid/payload.zip`,
        // so the fallback is taken and then fails: that it was taken at all is
        // the fact under test.
        let result = prepare(PrepareRequest {
            entry: &fixture.entry,
            cache_root: &fixture.cache_root(),
            local_archive: Some(&absent),
            progress: &progress,
            cancel: &AtomicBool::new(false),
        });

        assert!(matches!(result, Err(PayloadError::Http(_))));
        assert!(connected.load(Ordering::Relaxed));
    }
```

- [ ] **Step 4: Run them and watch them fail**

Run: `cargo test -p vmlord-gpu-payload local_archive`
Expected: the four tests FAIL — the first three because `prepare` downloads instead of reading the file, the fourth passes only by accident, so check each failure message rather than the count.

- [ ] **Step 5: Teach `prepare` to choose its source**

In `crates/gpu-payload/src/cache.rs`, replace the tail of `prepare` (everything from `let mut locked = LockedArchive::acquire(...)` down to `ready`) with:

```rust
    let ready = match request.local_archive.filter(|path| path.is_file()) {
        Some(archive) => prepare_verified_archive(
            request.entry,
            archive,
            &root,
            request.progress,
            request.cancel,
        ),
        None => {
            let mut locked = LockedArchive::acquire(&root, request.entry)?;
            locked.download(request.progress, request.cancel)?;
            locked.verify(request.progress, request.cancel)?;
            let archive_path = locked.path().to_owned();
            drop(locked);
            prepare_verified_archive(
                request.entry,
                &archive_path,
                &root,
                request.progress,
                request.cancel,
            )
        }
    };
    drop(quarantines);
    ready
```

Nothing above this changes: the digest lock is still taken first and a warm cache entry is still verified and returned before either source is consulted.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p vmlord-gpu-payload`
Expected: PASS, all of them.

- [ ] **Step 7: Write the failing test for the release layout**

Create `crates/gpu-payload/src/release.rs`:

```rust
#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::local_archive_path;

    #[test]
    fn a_release_keeps_each_payload_under_its_own_id() {
        assert_eq!(
            local_archive_path(Path::new("dist"), "ubuntu-26.04-amd64-7.0.0-28-v1"),
            PathBuf::from("dist")
                .join("gpu-payload")
                .join("ubuntu-26.04-amd64-7.0.0-28-v1.zip")
        );
    }
}
```

- [ ] **Step 8: Run it and watch it fail**

Run: `cargo test -p vmlord-gpu-payload release`
Expected: FAIL — `release.rs` is not a module of the crate and `local_archive_path` does not exist.

- [ ] **Step 9: Write the layout**

Add above the test module in `crates/gpu-payload/src/release.rs`:

```rust
//! Where a release keeps the payload archives it ships.
//!
//! One rule, written once: `cargo dist` copies to it and the running
//! application reads from it, and a disagreement between the two would be a
//! release whose payload is invisible with nothing to say so.

use std::path::{Path, PathBuf};

/// The child of the executable's directory holding shipped archives.
pub const LOCAL_ARCHIVE_DIRECTORY: &str = "gpu-payload";

/// The archive for `payload_id` below `directory`.
///
/// `directory` is the one holding the executable. It is a parameter rather
/// than read from `current_exe` here so that this can be tested, and so that
/// the build tool -- which is placing files into a distribution rather than
/// running from one -- can use the same rule.
pub fn local_archive_path(directory: &Path, payload_id: &str) -> PathBuf {
    directory
        .join(LOCAL_ARCHIVE_DIRECTORY)
        .join(format!("{payload_id}.zip"))
}
```

In `crates/gpu-payload/src/lib.rs`, add `mod release;` after `mod manifest;` and add the re-export line:

```rust
pub use release::{LOCAL_ARCHIVE_DIRECTORY, local_archive_path};
```

- [ ] **Step 10: Run the crate's tests**

Run: `cargo test -p vmlord-gpu-payload`
Expected: PASS.

- [ ] **Step 11: Commit**

```bash
git add crates/gpu-payload/src/release.rs crates/gpu-payload/src/lib.rs crates/gpu-payload/src/cache.rs
git commit -m "$(cat <<'EOF'
TASK-103: Prepare a GPU payload from an archive beside the executable

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: One reading of a packed catalog entry

**Files:**
- Modify: `crates/gpu-payload/src/catalog.rs` (add an associated function to `PayloadCatalog`, near `from_json`)
- Modify: `crates/gpu-payload/src/builder.rs:227-235` (the wrap-and-validate that `pack` does today)

**Interfaces:**
- Consumes: nothing from Task 1.
- Produces: `pub fn PayloadCatalog::from_entry_json(bytes: &[u8]) -> Result<CatalogEntry, PayloadError>` — parses the single-entry document `cargo xtask gpu-payload pack` writes to `--catalog-entry` and returns it validated.

- [ ] **Step 1: Write the failing tests**

Add to the test module at the end of `crates/gpu-payload/src/catalog.rs` (which already has the `catalog()` helper, `Z` and `C`):

```rust
    #[test]
    fn a_packed_entry_is_read_through_the_same_validation_as_the_catalog() {
        let document: serde_json::Value = serde_json::from_str(&catalog()).unwrap();
        let entry = serde_json::to_vec(&document["entries"][0]).unwrap();

        assert_eq!(
            PayloadCatalog::from_entry_json(&entry).unwrap().payload_id(),
            "ubuntu-26.04-amd64-7.0.0-14-v1"
        );
    }

    #[test]
    fn a_packed_entry_that_fails_catalog_validation_is_refused() {
        let mut document: serde_json::Value = serde_json::from_str(&catalog()).unwrap();
        document["entries"][0]["archive_url"] = "http://downloads.example.test/payload.zip".into();
        let entry = serde_json::to_vec(&document["entries"][0]).unwrap();

        assert!(matches!(
            PayloadCatalog::from_entry_json(&entry),
            Err(PayloadError::InvalidCatalog(_))
        ));
    }

    #[test]
    fn a_whole_catalog_document_is_not_an_entry() {
        assert!(matches!(
            PayloadCatalog::from_entry_json(catalog().as_bytes()),
            Err(PayloadError::InvalidCatalog(_))
        ));
    }
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test -p vmlord-gpu-payload packed_entry`
Expected: FAIL — `from_entry_json` does not exist.

- [ ] **Step 3: Implement it**

In `crates/gpu-payload/src/catalog.rs`, add to `impl PayloadCatalog`, directly after `from_json`:

```rust
    /// Reads one entry as `cargo xtask gpu-payload pack` writes it.
    ///
    /// `pack` emits a bare entry object rather than a catalog document, and
    /// what makes an entry trustworthy is [`Self::from_json`]'s validation --
    /// so the entry is wrapped in the document it belongs to and read through
    /// exactly that. Both the builder and the release build use this, so the
    /// file has one reading rather than two that can drift apart.
    pub fn from_entry_json(bytes: &[u8]) -> Result<CatalogEntry, PayloadError> {
        let entry: serde_json::Value = serde_json::from_slice(bytes)
            .map_err(|error| PayloadError::InvalidCatalog(error.to_string()))?;
        let document = serde_json::to_vec(&serde_json::json!({
            "schema_version": CATALOG_SCHEMA_VERSION,
            "entries": [entry],
        }))
        .map_err(|error| PayloadError::InvalidCatalog(error.to_string()))?;
        Self::from_json(&document)?
            .entries
            .into_iter()
            .next()
            .ok_or_else(|| PayloadError::InvalidCatalog("empty catalog entry".into()))
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-gpu-payload packed_entry`
Expected: PASS. (`a_whole_catalog_document_is_not_an_entry` passes because a document wrapped as an entry has no `payload_id` and fails deserialization.)

- [ ] **Step 5: Make `pack` use it**

In `crates/gpu-payload/src/builder.rs`, replace these lines:

```rust
    let catalog_bytes = serde_json::to_vec(&serde_json::json!({
        "schema_version": 1,
        "entries": [entry]
    }))
    .map_err(|error| PayloadError::InvalidCatalog(error.to_string()))?;
    let catalog = PayloadCatalog::from_json(&catalog_bytes)?;
    let validated_entry = &catalog.entries()[0];
    PayloadManifest::parse_and_validate(&manifest_bytes, validated_entry)?;
    let sources_bytes = read_prepared_file(&files, "sources.json")?;
    SourceManifest::parse_and_validate(&sources_bytes, validated_entry)?;
```

with:

```rust
    // Validated from the exact bytes that are about to be written, so what
    // the file says and what was checked cannot differ.
    let validated_entry = PayloadCatalog::from_entry_json(&entry_bytes)?;
    PayloadManifest::parse_and_validate(&manifest_bytes, &validated_entry)?;
    let sources_bytes = read_prepared_file(&files, "sources.json")?;
    SourceManifest::parse_and_validate(&sources_bytes, &validated_entry)?;
```

If the compiler reports `entry` as now unused after the `catalog_entry` call, keep it: `entry_bytes` is serialized from it above.

- [ ] **Step 6: Run the crate's tests, including the builder's**

Run: `cargo test -p vmlord-gpu-payload --features builder`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/gpu-payload/src/catalog.rs crates/gpu-payload/src/builder.rs
git commit -m "$(cat <<'EOF'
TASK-103: Read a packed catalog entry in one place

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: `cargo dist --gpu-payload`

**Files:**
- Modify: `crates/gpu-payload/src/digest.rs:17` (widen one helper)
- Modify: `crates/xtask/src/main.rs` (the `dist` dispatch near line 35, the `dist` function, and `ARTIFACTS`' neighbourhood)
- Modify: `crates/xtask/src/gpu_payload.rs` (add the release staging and its tests)
- Modify: `crates/xtask/Cargo.toml` (one dev-dependency)
- Modify: `payloads/ubuntu-26.04-amd64/README.md` (the `## Building` section)

**Interfaces:**
- Consumes: `vmlord_gpu_payload::local_archive_path` (Task 1), `vmlord_gpu_payload::PayloadCatalog::from_entry_json` (Task 2).
- Produces: `pub(crate) fn stage_release_payload(source: &Path, destination: &Path) -> Result<String, String>` — verifies the `payload.zip` / `catalog-entry.json` pair in `source`, copies the archive under `destination`, returns the payload ID. And `pub(crate) fn parse_dist<I: IntoIterator<Item = String>>(arguments: I) -> Result<Vec<PathBuf>, String>`.

- [ ] **Step 1: Let the build tool hash an archive**

`Sha256Digest::hash_reader` is `pub(crate)` today and the build tool needs it.
In `crates/gpu-payload/src/digest.rs`, change its signature to:

```rust
    pub fn hash_reader(mut reader: impl Read) -> Result<Self, PayloadError> {
```

`from_bytes` stays `pub(crate)`: a digest built from bytes nobody hashed is not
a thing a caller outside the crate should be able to make.

Run: `cargo check -p vmlord-gpu-payload`
Expected: PASS.

- [ ] **Step 2: Give the tests a JSON reader**

The tests below read the payload ID out of the entry `pack` wrote. Add to
`crates/xtask/Cargo.toml`, after the `[dependencies]` section:

```toml
# Tests only: reading back the catalog entry `pack` wrote. The build tool
# itself reads it through `PayloadCatalog::from_entry_json`.
[dev-dependencies]
serde_json = "1.0"
```

- [ ] **Step 3: Write the failing tests for the staging**

Add to the test module of `crates/xtask/src/gpu_payload.rs` (which already has `TemporaryDirectory`, `fs`, `Path`, `PathBuf` and the `pack` fixtures in scope):

```rust
    /// Packs the crate's fixture into `directory`, as the recipe's `pack` step
    /// does, and answers with the payload ID the entry carries.
    fn packed_pair(directory: &Path) -> String {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../gpu-payload/tests/fixtures");
        pack(PackRequest {
            prepared_directory: &fixture.join("prepared"),
            recipe_path: &fixture.join("recipe.json"),
            archive_path: &directory.join("payload.zip"),
            catalog_entry_path: &directory.join("catalog-entry.json"),
        })
        .unwrap();
        let entry: serde_json::Value =
            serde_json::from_slice(&fs::read(directory.join("catalog-entry.json")).unwrap())
                .unwrap();
        entry["payload_id"].as_str().unwrap().to_owned()
    }

    #[test]
    fn a_packed_pair_is_copied_under_its_payload_id() {
        let temporary = TemporaryDirectory::new("stage-ok");
        let source = temporary.path().join("built");
        let destination = temporary.path().join("dist");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&destination).unwrap();
        let payload_id = packed_pair(&source);

        assert_eq!(
            super::stage_release_payload(&source, &destination).unwrap(),
            payload_id
        );
        assert_eq!(
            fs::read(
                destination
                    .join("gpu-payload")
                    .join(format!("{payload_id}.zip"))
            )
            .unwrap(),
            fs::read(source.join("payload.zip")).unwrap()
        );
    }

    #[test]
    fn a_pair_that_is_not_what_pack_produced_fails_the_build() {
        let temporary = TemporaryDirectory::new("stage-bad");
        let destination = temporary.path().join("dist");
        fs::create_dir_all(&destination).unwrap();

        // Each case is a separate source directory: a build tool that accepted
        // any of these would put bytes nobody verified into a release.
        for (label, damage) in [
            (
                "truncated",
                Box::new(|source: &Path| {
                    let archive = fs::read(source.join("payload.zip")).unwrap();
                    fs::write(source.join("payload.zip"), &archive[..archive.len() - 1]).unwrap();
                }) as Box<dyn Fn(&Path)>,
            ),
            (
                "flipped",
                Box::new(|source: &Path| {
                    let mut archive = fs::read(source.join("payload.zip")).unwrap();
                    archive[0] ^= 0xFF;
                    fs::write(source.join("payload.zip"), archive).unwrap();
                }),
            ),
            (
                "entry-invalid",
                Box::new(|source: &Path| {
                    fs::write(source.join("catalog-entry.json"), b"{}").unwrap();
                }),
            ),
            (
                "archive-missing",
                Box::new(|source: &Path| {
                    fs::remove_file(source.join("payload.zip")).unwrap();
                }),
            ),
            (
                "entry-missing",
                Box::new(|source: &Path| {
                    fs::remove_file(source.join("catalog-entry.json")).unwrap();
                }),
            ),
        ] {
            let source = temporary.path().join(label);
            fs::create_dir_all(&source).unwrap();
            packed_pair(&source);
            damage(&source);

            assert!(
                super::stage_release_payload(&source, &destination).is_err(),
                "accepted a {label} pair"
            );
        }
    }
```

- [ ] **Step 4: Run them and watch them fail**

Run: `cargo test -p xtask`
Expected: FAIL — `stage_release_payload` does not exist.

- [ ] **Step 5: Implement the staging**

At the top of `crates/xtask/src/gpu_payload.rs`, extend the imports:

```rust
use std::{
    fs,
    path::{Path, PathBuf},
};
use vmlord_gpu_payload::{
    PayloadCatalog, Sha256Digest,
    builder::{PackRequest, pack},
    local_archive_path,
};
```

and add below `run`:

```rust
/// Copies one packed payload into a distribution, refusing anything that is
/// not exactly what `pack` wrote.
///
/// `source` is the directory the recipe's `pack` step wrote `payload.zip` and
/// `catalog-entry.json` into. Only the archive travels: the catalog is
/// embedded in the application and trusted for being embedded, and a second
/// catalog sitting beside the executable would be one an attacker can edit.
///
/// Deeper checks -- `payload.json`, `sources.json`, expansion limits -- belong
/// to `prepare` on the machine that will use the payload. Repeating them here
/// would be a second opinion that can drift from the first.
pub(crate) fn stage_release_payload(source: &Path, destination: &Path) -> Result<String, String> {
    let entry_path = source.join("catalog-entry.json");
    let archive_path = source.join("payload.zip");
    let entry_bytes = fs::read(&entry_path)
        .map_err(|error| format!("cannot read {}: {error}", entry_path.display()))?;
    let entry = PayloadCatalog::from_entry_json(&entry_bytes)
        .map_err(|error| format!("{} is not a packed catalog entry: {error}", entry_path.display()))?;

    let archive = fs::read(&archive_path)
        .map_err(|error| format!("cannot read {}: {error}", archive_path.display()))?;
    let size = archive.len() as u64;
    if size != entry.archive_size() {
        return Err(format!(
            "{} is {size} bytes; its entry says {}",
            archive_path.display(),
            entry.archive_size()
        ));
    }
    let digest = Sha256Digest::hash_reader(archive.as_slice())
        .map_err(|error| format!("cannot hash {}: {error}", archive_path.display()))?;
    if digest != *entry.archive_sha256() {
        return Err(format!(
            "{} hashes to {digest}; its entry says {}",
            archive_path.display(),
            entry.archive_sha256()
        ));
    }

    let target = local_archive_path(destination, entry.payload_id());
    let directory = target
        .parent()
        .expect("a payload archive path always has a parent");
    fs::create_dir_all(directory)
        .map_err(|error| format!("cannot create {}: {error}", directory.display()))?;
    fs::write(&target, &archive)
        .map_err(|error| format!("cannot write {}: {error}", target.display()))?;
    Ok(entry.payload_id().to_owned())
}
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p xtask`
Expected: PASS.

- [ ] **Step 7: Write the failing tests for the argument**

Add to the same test module:

```rust
    #[test]
    fn dist_takes_any_number_of_payload_directories() {
        assert_eq!(super::parse_dist(Vec::new()).unwrap(), Vec::<PathBuf>::new());
        assert_eq!(
            super::parse_dist(
                ["--gpu-payload", "one", "--gpu-payload", "two"]
                    .into_iter()
                    .map(str::to_owned)
            )
            .unwrap(),
            vec![PathBuf::from("one"), PathBuf::from("two")]
        );
    }

    #[test]
    fn dist_rejects_an_unknown_or_incomplete_argument() {
        for arguments in [
            vec!["--gpu-payload"],
            vec!["--unknown", "value"],
            vec!["built"],
        ] {
            assert!(
                super::parse_dist(arguments.iter().map(|value| (*value).to_owned())).is_err(),
                "accepted {arguments:?}"
            );
        }
    }
```

- [ ] **Step 8: Run them and watch them fail**

Run: `cargo test -p xtask dist`
Expected: FAIL — `parse_dist` does not exist.

- [ ] **Step 9: Implement the parser**

Add to `crates/xtask/src/gpu_payload.rs`:

```rust
/// Reads `cargo dist`'s arguments: zero or more payload directories.
pub(crate) fn parse_dist<I: IntoIterator<Item = String>>(
    arguments: I,
) -> Result<Vec<PathBuf>, String> {
    let mut values = arguments.into_iter();
    let mut directories = Vec::new();
    while let Some(flag) = values.next() {
        if flag != "--gpu-payload" {
            return Err(format!("unknown argument `{flag}`"));
        }
        directories.push(PathBuf::from(
            values.next().ok_or("missing value for --gpu-payload")?,
        ));
    }
    Ok(directories)
}
```

- [ ] **Step 10: Run the tests to verify they pass**

Run: `cargo test -p xtask`
Expected: PASS.

- [ ] **Step 11: Wire it into `dist`**

In `crates/xtask/src/main.rs`, change the dispatch line:

```rust
        Some("dist") => gpu_payload::parse_dist(env::args().skip(2)).and_then(dist),
```

change the signature:

```rust
fn dist(gpu_payloads: Vec<PathBuf>) -> Result<(), String> {
```

and, after the `for (target, file) in ARTIFACTS` loop and before the final `println!`, add:

```rust
    if gpu_payloads.is_empty() {
        println!("dist: no GPU payload included; pass --gpu-payload <directory>");
    }
    for source in &gpu_payloads {
        let payload_id = gpu_payload::stage_release_payload(source, &destination)?;
        println!("dist: gpu-payload/{payload_id}.zip");
    }
```

- [ ] **Step 12: Check the workspace still builds**

Run: `cargo test -p xtask && cargo check -p xtask`
Expected: PASS.

- [ ] **Step 13: Document the argument in the recipe's README**

In `payloads/ubuntu-26.04-amd64/README.md`, after the code block in `## Building`, add:

```markdown
The two outputs travel together. `target/gpu-payload` is what `cargo dist`
wants:

```sh
cargo dist --gpu-payload target/gpu-payload
```

`dist` re-reads `catalog-entry.json`, checks `payload.zip` against the size and
digest it claims, and copies the archive to `gpu-payload/<payload_id>.zip`
beside `vmlord.exe`, which is where the application looks for it. The entry
itself does not travel: the catalog the application trusts is the one compiled
into it. Without the argument `dist` builds a release with no payload and says
so.
```

- [ ] **Step 14: Commit**

```bash
git add crates/gpu-payload/src/digest.rs crates/xtask/src/gpu_payload.rs crates/xtask/src/main.rs crates/xtask/Cargo.toml Cargo.lock payloads/ubuntu-26.04-amd64/README.md
git commit -m "$(cat <<'EOF'
TASK-103: Ship a verified GPU payload archive with the release

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: Filling a VM's staging directory

**Files:**
- Create: `crates/platform/src/gpu_staging.rs`
- Modify: `crates/platform/src/lib.rs` (the module list and the re-exports)
- Modify: `crates/platform/Cargo.toml` (one dependency)
- Read only: `crates/platform/src/gpu_exports.rs` — `ExportRoots::resolve`, `build_with_payload` and `GpuExports::manifest` are all `pub(crate)` already and need no change

**Interfaces:**
- Consumes: `vmlord_gpu_payload::{local_archive_path, prepare, stage_payload, PrepareRequest, PayloadCatalog, StagedGpuPayload, PayloadError, PayloadProgress, GuestTarget}` (Task 1), `crate::layout::gpu_payload_staging_directory`, `crate::gpu_exports::build_with_payload`.
- Produces: `pub struct StageGpuPayloadRequest<'a>` and `pub fn stage_for_vm(request: StageGpuPayloadRequest<'_>) -> Result<StagedGpuPayload, PayloadError>`; `pub(crate) fn prepare_staging_root(vm_directory: &Path) -> Result<PathBuf, PayloadError>`.

- [ ] **Step 1: Add the dependency**

In `crates/platform/Cargo.toml`, under `[dependencies]`, after the `vmlord-seed` block:

```toml
# The GPU payload boundary: catalog, verified cache and per-VM staging. The
# crate is portable and knows nothing of Windows; this layer is what gives it a
# VM directory to fill.
vmlord-gpu-payload = { path = "../gpu-payload" }
```

- [ ] **Step 2: Write the failing tests**

Create `crates/platform/src/gpu_staging.rs` with only its test module for now:

```rust
#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicBool, AtomicU64, Ordering},
    };

    use vmlord_core::{GpuShare, RepositoryError};
    use vmlord_gpu_payload::{GuestTarget, PayloadError};

    use super::{StageGpuPayloadRequest, prepare_staging_root, stage_for_vm};
    use crate::gpu_exports::{ExportRoots, build_with_payload};

    static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

    struct TemporaryDirectory(PathBuf);

    impl TemporaryDirectory {
        fn new(label: &str) -> Self {
            let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "vmlord-gpu-staging-{label}-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TemporaryDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_staging_root_is_the_child_the_payload_share_exports() {
        let temporary = TemporaryDirectory::new("root");
        let vm = temporary.path().join("dev-linux");
        fs::create_dir(&vm).unwrap();

        let root = prepare_staging_root(&vm).unwrap();

        assert_eq!(root, vm.join("gpu-payload"));
        assert!(root.join("generations").is_dir());
        assert!(root.join("ready").is_dir());

        // The same directory has to be the one the Plan9 export accepts:
        // staging that filled anything else would be invisible to the guest.
        let canonicalize = |path: &Path| {
            fs::canonicalize(path)
                .map_err(|error| RepositoryError::new(format!("{}: {error}", path.display())))
        };
        // No system roots: this test is about the per-VM child alone, and a
        // system directory that does not resolve leaves `ExportRoots` empty.
        let roots = ExportRoots::resolve(&temporary.path().join("no-system32"), &canonicalize);
        let exports = build_with_payload(&[], &roots, &vm, &canonicalize).unwrap();

        assert_eq!(exports.manifest().shares[0], GpuShare::payload());
    }

    #[test]
    fn an_unsupported_target_stages_nothing() {
        let temporary = TemporaryDirectory::new("unsupported");
        let vm = temporary.path().join("dev-linux");
        fs::create_dir(&vm).unwrap();

        let result = stage_for_vm(StageGpuPayloadRequest {
            executable_directory: temporary.path(),
            cache_root: &temporary.path().join("cache"),
            vm_directory: &vm,
            target: &GuestTarget::ubuntu_26_04_amd64("7.0.0-28-generic"),
            progress: &|_| {},
            cancel: &AtomicBool::new(false),
        });

        assert!(matches!(result, Err(PayloadError::UnsupportedTarget(_))));
        assert_eq!(fs::read_dir(&vm).unwrap().count(), 0);
    }
}
```

- [ ] **Step 3: Run them and watch them fail**

Run: `cargo test-windows -p vmlord-platform gpu_staging`
Expected: FAIL — the module is not declared and neither function exists.

- [ ] **Step 4: Write the module**

Above the test module in `crates/platform/src/gpu_staging.rs`:

```rust
//! Turning a catalog entry into the payload directory a VM exports.
//!
//! Three steps that belong together and nowhere else: pick the entry for the
//! target an agent reported, prepare that generation in the shared cache, and
//! stage it into the VM's own `gpu-payload` child -- the exact directory
//! `gpu_exports` canonicalizes and offers as `vmlord.gpu.payload`.
//!
//! Nothing in the running application calls this yet, for the reason
//! `gpu_exports` states: a start cannot know a VM's GPU mode until the task
//! that applies HCS assignment records one, and that task is this module's
//! caller. The allow below goes away with it.
#![allow(dead_code)]

use std::{path::Path, path::PathBuf, sync::atomic::AtomicBool};

use vmlord_gpu_payload::{
    GuestTarget, PayloadCatalog, PayloadError, PayloadProgress, PrepareRequest, StagedGpuPayload,
    ensure_staging_root, local_archive_path, prepare, stage_payload,
};

use crate::layout::gpu_payload_staging_directory;

/// Everything staging a payload for one VM needs.
pub struct StageGpuPayloadRequest<'a> {
    /// The directory holding the running executable; the shipped archive is
    /// found below it.
    pub executable_directory: &'a Path,
    /// The shared, content-addressed payload cache, common to every VM.
    pub cache_root: &'a Path,
    /// The VM's own directory. Its `gpu-payload` child is what gets filled.
    pub vm_directory: &'a Path,
    /// The exact guest tuple the agent reported.
    pub target: &'a GuestTarget,
    pub progress: &'a dyn Fn(PayloadProgress),
    pub cancel: &'a AtomicBool,
}

/// Creates the VM's staging root and answers with it.
pub(crate) fn prepare_staging_root(vm_directory: &Path) -> Result<PathBuf, PayloadError> {
    let root = gpu_payload_staging_directory(vm_directory);
    ensure_staging_root(&root)?;
    Ok(root)
}

/// Stages the payload for `target` into the VM's `gpu-payload` child.
///
/// A failure here is a failure of GPU support and not of the VM: assignment is
/// best effort by design, so the caller decides what a [`PayloadError`] means
/// for a start and nothing in this module touches lifecycle.
pub fn stage_for_vm(request: StageGpuPayloadRequest<'_>) -> Result<StagedGpuPayload, PayloadError> {
    let catalog = PayloadCatalog::embedded()?;
    let entry = catalog.select(request.target)?;
    let archive = local_archive_path(request.executable_directory, entry.payload_id());
    let ready = prepare(PrepareRequest {
        entry,
        cache_root: request.cache_root,
        local_archive: Some(&archive),
        progress: request.progress,
        cancel: request.cancel,
    })?;
    let root = prepare_staging_root(request.vm_directory)?;
    stage_payload(&ready, &root, request.progress, request.cancel)
}
```

In `crates/platform/src/lib.rs`, add `mod gpu_staging;` after `mod gpu_exports;`, and the re-export after `pub use gpu_discovery::discover_host_gpu;`:

```rust
pub use gpu_staging::{StageGpuPayloadRequest, stage_for_vm};
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-platform gpu_staging`
Expected: PASS.

- [ ] **Step 6: Check the whole workspace**

Run: `cargo check-windows && cargo test-windows`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/platform/src/gpu_staging.rs crates/platform/src/lib.rs crates/platform/Cargo.toml Cargo.lock
git commit -m "$(cat <<'EOF'
TASK-103: Stage a prepared GPU payload into a VM's export directory

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: Record it in ARCHITECTURE.md

**Files:**
- Modify: `ARCHITECTURE.md:1026-1044` (the `### GPU: guest payload` section)

**Interfaces:**
- Consumes: the behaviour built in Tasks 1-4.
- Produces: nothing code depends on.

- [ ] **Step 1: Rewrite the section's middle**

In `ARCHITECTURE.md`, in `### GPU: guest payload`, replace the sentence beginning "Its embedded schema-v1 production catalog is still empty" and the one after it with:

```markdown
A payload reaches a host as a file rather than as a download. `cargo dist
--gpu-payload <directory>` takes what `cargo xtask gpu-payload pack` wrote --
`payload.zip` beside `catalog-entry.json` -- re-reads the entry through
`PayloadCatalog::from_entry_json`, checks the archive against the size and
digest it claims, and copies it to `gpu-payload/<payload_id>.zip` beside the
executable. `local_archive_path` is the single statement of that layout, used
by the build tool placing the file and by the application reading it.
`PrepareRequest::local_archive` is what the application passes: a file that is
there is prepared from with no network access at all, a file that is not there
falls back to `archive_url`, and a file that is there and does not match its
entry is an error rather than a reason to go online. Verification does not
soften for a local archive -- the same digest, the same expansion limits, the
same `payload.json` and `sources.json` cross-check against the entry's
provenance. `archive_url` therefore keeps one meaning, where these bytes are
published, and serves as both the fallback and the way to replace a payload
between releases. The embedded schema-v1 production catalog is still empty, so
none of this selects anything in a built release yet: an entry needs an archive
that has been published with its digest, which is neither code nor a decision
any of these tasks makes. Release tooling creates sorted ZIP content and
deterministic provenance without making the archive digest self-referential.
```

- [ ] **Step 2: Add the staging service to the paragraph that follows**

Directly after "Ready content is materialized below a VM's exact `gpu-payload` child as `generations/<digest>` followed by `ready/<digest>.json`.", insert:

```markdown
`platform::gpu_staging` is what materializes it: given the executable's
directory, the shared cache root, a VM directory and the guest tuple an agent
reported, it selects the entry, prepares the generation and stages it into
`layout::gpu_payload_staging_directory` -- the same child `gpu_exports` will
canonicalize. It is called by nothing yet, for the reason `gpu_exports` is: a
start does not know a VM's GPU mode until assignment records one.
```

- [ ] **Step 3: Read the section back and check it does not contradict itself**

Run: `sed -n '1024,1060p' ARCHITECTURE.md`
Expected: one section that says the catalog is empty exactly once, and never says the archive is downloaded by default.

- [ ] **Step 4: Commit**

```bash
git add ARCHITECTURE.md
git commit -m "$(cat <<'EOF'
TASK-103: Record the shipped GPU payload in the architecture

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Final verification

- [ ] `cargo test -p vmlord-gpu-payload --features builder`
- [ ] `cargo test -p xtask`
- [ ] `cargo check-windows`
- [ ] `cargo test-windows`
- [ ] `git log --oneline main..HEAD` shows five `TASK-103:` implementation commits plus the spec commit (six in all).
- [ ] `git diff main -- crates/gpu-payload/catalog/catalog.json` is empty: the embedded catalog was not touched.
