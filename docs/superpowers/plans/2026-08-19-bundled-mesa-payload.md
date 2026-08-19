# Bundled Mesa payload Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship a GPU payload whose `content/mesa` carries a Mesa built with
`-Dvulkan-drivers=microsoft-experimental`, so a guest gets hardware Vulkan through dozen
instead of llvmpipe.

**Architecture:** The payload's source records become a union tagged by `kind`, so a
compiled tree can state its provenance honestly instead of pretending its binaries are
upstream files. The whole `prepare` step moves into a multi-stage BuildKit image that
fetches pinned checkouts, compiles Mesa with the network switched off, proves the result
loads against a clean Ubuntu, and lays out the prepared tree. `prepare.sh` shrinks to a
`docker build` wrapper; `pack` stays outside in Rust.

**Tech Stack:** Rust (serde, serde_json, zip), Python 3 (`prepare.py`), Bash, Docker
BuildKit, Meson/Ninja, Mesa.

**Spec:** `docs/superpowers/specs/2026-08-19-bundled-mesa-payload-design.md`

## Global Constraints

- Mesa's install prefix is `/opt/vmlord/wsl-mesa` with libdir `lib/x86_64-linux-gnu`.
  This is fixed by `MESA_PREFIX` (crates/agent/src/gpu_kernel.rs:52) and must not be
  parameterised.
- Meson configuration, verbatim: `-Dprefix=/opt/vmlord/wsl-mesa
  -Dlibdir=lib/x86_64-linux-gnu -Dgallium-drivers=d3d12,softpipe
  -Dvulkan-drivers=microsoft-experimental -Dllvm=disabled -Dglvnd=enabled
  -Dglvnd-vendor-name=mesa -Dplatforms=x11,wayland -Dbuildtype=release -Db_ndebug=true`
- `recipe.json` and `sources.json` move to `schema_version: 2`. Version 1 is refused, not
  migrated — this project rebuilds payloads rather than migrating them.
- The catalog entry keeps `schema_version: 2` and its existing field set. Nothing in this
  plan changes the format `CatalogEntry::from_json` reads.
- The prepared tree may contain no symlink and no empty file: `collect_files`
  (crates/gpu-payload/src/builder.rs:462) rejects both.
- The ICD file must be named `dzn_icd.x86_64.json` and sit in
  `<prefix>/share/vulkan/icd.d` — `icd_documents` (crates/agent/src/gpu_recipe.rs:297)
  looks for exactly that.
- Payload identity: `payload_id` becomes `ubuntu-26.04-amd64-7.0.0-28-v2`, `mesa_policy`
  becomes `bundled`, `required_renderers` becomes `["d3d12-gallium", "dzn-vulkan"]`.
- Run `cargo` directly, with no `timeout` prefix in front of it.
- Commit messages start with `TASK-108: ` and end with the project's
  `Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>` trailer.

---

## File Structure

**Rust — the provenance schema:**

- `crates/gpu-payload/src/manifest.rs` — `SourceRecord` becomes an untagged union of
  `CheckoutRecord` and `BuiltRecord`, each carrying a literal `kind`. Validation branches
  per kind, and the catalog comparison flattens `inputs` into the row list.
- `crates/gpu-payload/src/builder.rs` — `PackRecipe`/`PreparedSources` gain the same
  union, `catalog_entry` flattens `inputs`, and `validate_prepared_provenance` verifies a
  `built` record's digest against the tree it just hashed.
- `crates/gpu-payload/tests/fixtures/recipe.json`,
  `crates/gpu-payload/tests/fixtures/prepared/sources.json` — schema version 2, and a
  `built` record covering a fixture tree.
- `crates/gpu-payload/tests/fixtures/prepared/content/mesa/lib/x86_64-linux-gnu/marker.so`
  — a stand-in file so the fixture's `built` record has an output to digest.

**Payload — the build:**

- `payloads/ubuntu-26.04-amd64/Dockerfile` (new) — the five stages.
- `payloads/ubuntu-26.04-amd64/mesa/build.sh` (new) — configure, compile, install, trim.
- `payloads/ubuntu-26.04-amd64/mesa/closure.sh` (new) — the `ldd` gate.
- `payloads/ubuntu-26.04-amd64/prepare.sh` — becomes a `docker build` wrapper.
- `payloads/ubuntu-26.04-amd64/prepare.py` — `--mesa`, the `built` record, version 2.
- `payloads/ubuntu-26.04-amd64/payload.spec.json` — bundled identity, the Mesa source,
  the new licenses.
- `payloads/ubuntu-26.04-amd64/licenses/MIT-Mesa.txt`,
  `licenses/MIT-DirectX-Headers.txt` (new, plus one per further input the build reports).
- `payloads/ubuntu-26.04-amd64/README.md` — rewritten.

---

### Task 1: A source record that can say it was built

**Files:**
- Modify: `crates/gpu-payload/src/manifest.rs:129-235` (the `SourceRecord` struct and
  `SourceManifest::parse_and_validate`)
- Test: `crates/gpu-payload/src/manifest.rs` (the `mod tests` at line 340)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: the JSON shape every later task writes and reads. A source record is one of

  ```json
  { "kind": "checkout", "url": "...", "commit": "<40 hex>", "version": "...",
    "paths": ["a", "b"], "licenses": [{"path": "a", "spdx": "GPL-2.0"}], "sha256": "<64 hex>" }
  ```

  ```json
  { "kind": "built", "url": "...", "commit": "<40 hex>", "version": "...",
    "output": "content/mesa", "licenses": ["MIT"],
    "inputs": [{"url": "...", "commit": "<40 hex>", "version": "..."}],
    "sha256": "<64 hex>" }
  ```

  and `sources.json` carries `"schema_version": 2`.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `crates/gpu-payload/src/manifest.rs`. Note that `entry()` in that
module currently declares one source and one license; these tests build their own
documents on top of it.

```rust
    fn built_sources() -> Value {
        json!({
            "schema_version": 2,
            "target": {
                "distribution": "ubuntu",
                "release": "26.04",
                "architecture": "amd64",
                "kernel_release": "k",
                "payload_abi": 1
            },
            "mesa_policy": "bundled",
            "sources": [{
                "kind": "built",
                "url": "https://gitlab.freedesktop.org/mesa/mesa",
                "commit": COMMIT,
                "version": "26.1.2",
                "output": "content/mesa",
                "licenses": ["GPL-2.0"],
                "inputs": [{
                    "url": "https://github.com/microsoft/DirectX-Headers",
                    "commit": COMMIT,
                    "version": "v1.615.0"
                }],
                "sha256": ZERO
            }],
            "overlays": []
        })
    }

    /// A built record's inputs are sources in their own right: they are in the
    /// binaries, and the catalog is where a person looks for what is in a payload.
    fn built_entry() -> CatalogEntry {
        CatalogEntry::from_json(
            &serde_json::to_vec(&json!({
                "schema_version": 2,
                "payload_id": "p",
                "target": {
                    "distribution": "ubuntu",
                    "release": "26.04",
                    "architecture": "amd64",
                    "kernel_release": "k",
                    "payload_abi": 1
                },
                "expanded_size_limit": 2,
                "file_count_limit": 4,
                "archive_sha256": ZERO,
                "payload_manifest_sha256": ZERO,
                "required_renderers": ["d3d12-gallium", "dzn-vulkan"],
                "mesa_policy": "bundled",
                "sources": [
                    {
                        "url": "https://gitlab.freedesktop.org/mesa/mesa",
                        "commit": COMMIT,
                        "version": "26.1.2"
                    },
                    {
                        "url": "https://github.com/microsoft/DirectX-Headers",
                        "commit": COMMIT,
                        "version": "v1.615.0"
                    }
                ],
                "licenses": [{"spdx": "GPL-2.0", "path": "licenses/GPL-2.0.txt"}]
            }))
            .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn a_built_source_contributes_itself_and_its_inputs_to_the_catalog() {
        let entry = built_entry();
        let document = serde_json::to_vec(&built_sources()).unwrap();

        SourceManifest::parse_and_validate(&document, &entry)
            .expect("a built record and its inputs are the catalog's two source rows");
    }

    #[test]
    fn a_built_source_that_hides_an_input_from_the_catalog_is_refused() {
        let entry = built_entry();
        let mut document = built_sources();
        document["sources"][0]["inputs"] = json!([]);

        let error = SourceManifest::parse_and_validate(
            &serde_json::to_vec(&document).unwrap(),
            &entry,
        )
        .unwrap_err();

        assert!(matches!(error, PayloadError::InvalidManifest(_)));
    }

    #[test]
    fn a_built_source_must_declare_licences_the_catalog_knows() {
        let entry = built_entry();
        let mut document = built_sources();
        document["sources"][0]["licenses"] = json!(["MIT"]);

        let error = SourceManifest::parse_and_validate(
            &serde_json::to_vec(&document).unwrap(),
            &entry,
        )
        .unwrap_err();

        assert!(matches!(error, PayloadError::InvalidManifest(_)));
    }

    #[test]
    fn a_built_source_needs_an_output_a_licence_and_a_digest() {
        let entry = built_entry();
        for (field, value) in [
            ("output", json!("")),
            ("licenses", json!([])),
            ("output", json!("../escape")),
        ] {
            let mut document = built_sources();
            document["sources"][0][field] = value;

            let error = SourceManifest::parse_and_validate(
                &serde_json::to_vec(&document).unwrap(),
                &entry,
            )
            .unwrap_err();

            assert!(matches!(error, PayloadError::InvalidManifest(_)));
        }
    }

    #[test]
    fn a_sources_document_at_version_one_is_no_longer_understood() {
        let entry = entry();
        let mut document = sources();
        document["schema_version"] = json!(1);

        let error = SourceManifest::parse_and_validate(
            &serde_json::to_vec(&document).unwrap(),
            &entry,
        )
        .unwrap_err();

        assert!(matches!(error, PayloadError::InvalidManifest(_)));
    }
```

Then update the two existing helpers so the rest of the module still describes a valid
document: in `fn sources()` change `"schema_version": 1` to `"schema_version": 2` and add
`"kind": "checkout",` as the first field of the single source object.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-gpu-payload --lib manifest`
Expected: FAIL — `built_sources` is rejected because `SourceRecord` has no `kind` field
and `deny_unknown_fields` refuses it, and the version-1 test fails because version 1 is
still accepted.

- [ ] **Step 3: Replace `SourceRecord` with the union**

In `crates/gpu-payload/src/manifest.rs`, replace the `SourceRecord` struct (line 137) with:

```rust
/// One upstream a payload owes something to.
///
/// Untagged rather than `#[serde(tag = "kind")]` on purpose: serde does not honour
/// `deny_unknown_fields` on an internally tagged enum, and refusing a field nobody
/// meant to write is worth a less specific error message when both variants fail.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
enum SourceRecord {
    Checkout(CheckoutRecord),
    Built(BuiltRecord),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum CheckoutKind {
    Checkout,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum BuiltKind {
    Built,
}

/// Upstream files that travelled into the payload byte for byte.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CheckoutRecord {
    kind: CheckoutKind,
    url: String,
    commit: String,
    version: String,
    paths: Vec<String>,
    licenses: Vec<SourceLicenseRecord>,
    sha256: Sha256Digest,
}

/// A tree that was compiled, whose members correspond to no upstream file.
///
/// `output` is a path in the payload rather than upstream, `licenses` are bare SPDX
/// identifiers because attributing one shared object to one upstream file is not
/// meaningful, and `sha256` covers what shipped -- which makes it the one digest here
/// that can be checked rather than believed.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BuiltRecord {
    kind: BuiltKind,
    url: String,
    commit: String,
    version: String,
    output: String,
    licenses: Vec<String>,
    inputs: Vec<SourceInputRecord>,
    sha256: Sha256Digest,
}

/// An upstream that ended up inside a built tree's binaries.
///
/// No digest of its own: its bytes are not separable from the output's. The commit is
/// what makes it auditable, and it reaches the catalog as an ordinary source row.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SourceInputRecord {
    url: String,
    commit: String,
    version: String,
}
```

- [ ] **Step 4: Branch the validation per kind**

Replace the body of `SourceManifest::parse_and_validate` between the schema check and the
overlay loop (lines 165-216) with:

```rust
        if doc.schema_version != 2
            || doc.target != *entry.target()
            || doc.mesa_policy != *entry.mesa_policy()
        {
            return Err(PayloadError::InvalidManifest(
                "sources.json does not exactly match catalog provenance".into(),
            ));
        }

        let rows = catalog_rows(&doc.sources);
        if rows.len() != entry.sources().len()
            || rows.iter().zip(entry.sources()).any(|(row, expected)| {
                row.0 != expected.url || row.1 != expected.commit || row.2 != expected.version
            })
        {
            return Err(PayloadError::InvalidManifest(
                "sources.json does not exactly match catalog provenance".into(),
            ));
        }

        for source in &doc.sources {
            match source {
                SourceRecord::Checkout(checkout) => validate_checkout(checkout, entry)?,
                SourceRecord::Built(built) => validate_built(built, entry)?,
            }
        }
```

and add these three functions beside `license_expression_is_declared`:

```rust
/// The `url`/`commit`/`version` rows one source contributes to a catalog entry.
///
/// A built record contributes itself and every input, in that order: what is inside the
/// binaries is part of what the payload carries, and the catalog is where a person looks
/// for that.
fn catalog_rows(sources: &[SourceRecord]) -> Vec<(&str, &str, &str)> {
    let mut rows = Vec::new();
    for source in sources {
        match source {
            SourceRecord::Checkout(checkout) => {
                rows.push((
                    checkout.url.as_str(),
                    checkout.commit.as_str(),
                    checkout.version.as_str(),
                ));
            }
            SourceRecord::Built(built) => {
                rows.push((
                    built.url.as_str(),
                    built.commit.as_str(),
                    built.version.as_str(),
                ));
                for input in &built.inputs {
                    rows.push((
                        input.url.as_str(),
                        input.commit.as_str(),
                        input.version.as_str(),
                    ));
                }
            }
        }
    }
    rows
}

fn validate_checkout(
    checkout: &CheckoutRecord,
    entry: &CatalogEntry,
) -> Result<(), PayloadError> {
    if checkout.paths.is_empty() || checkout.licenses.len() != checkout.paths.len() {
        return Err(PayloadError::InvalidManifest(
            "sources.json does not exactly match catalog provenance".into(),
        ));
    }
    let mut previous = "";
    for path in &checkout.paths {
        validate_path(path)?;
        if !previous.is_empty() && previous >= path.as_str() {
            return Err(PayloadError::InvalidManifest(
                "selected source paths must be unique and sorted".into(),
            ));
        }
        previous = path;
    }
    for (path, license) in checkout.paths.iter().zip(&checkout.licenses) {
        validate_path(&license.path)?;
        if license.path != *path
            || !license_expression_is_declared(&license.spdx, entry)
            || (license.path == D3DKMTHK_PATH && license.spdx != D3DKMTHK_LICENSE)
        {
            return Err(PayloadError::InvalidManifest(
                "selected source paths must carry their declared licenses".into(),
            ));
        }
    }
    Ok(())
}

fn validate_built(built: &BuiltRecord, entry: &CatalogEntry) -> Result<(), PayloadError> {
    validate_path(&built.output)?;
    if built.licenses.is_empty()
        || !built
            .licenses
            .iter()
            .all(|spdx| license_expression_is_declared(spdx, entry))
    {
        return Err(PayloadError::InvalidManifest(
            "a built source must declare licences the catalog knows".into(),
        ));
    }
    Ok(())
}
```

`validate_path` already rejects an empty string, `..`, a leading `/` and a backslash, so
`output` needs nothing further.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p vmlord-gpu-payload --lib manifest`
Expected: PASS, including the pre-existing tests
`every_source_requires_selected_paths_licenses_and_a_digest` and
`source_manifest_schema_rejects_unknown_fields`.

- [ ] **Step 6: Commit**

```bash
git add crates/gpu-payload/src/manifest.rs
git commit -m "$(cat <<'EOF'
TASK-108: Let a source record say it was compiled

Every source record claimed its upstream files travelled verbatim, which
describes dxgkrnl and describes a Mesa build not at all. A built record
names what shipped, the licences its material stands under, and the
upstreams that ended up inside its binaries.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: The builder checks the digest it can check

**Files:**
- Modify: `crates/gpu-payload/src/builder.rs:54-118` (recipe types),
  `builder.rs:135-155` (schema version), `builder.rs:346-380` (`catalog_entry`),
  `builder.rs:385-438` (`validate_prepared_provenance`)
- Modify: `crates/gpu-payload/tests/fixtures/recipe.json`,
  `crates/gpu-payload/tests/fixtures/prepared/sources.json`
- Create: `crates/gpu-payload/tests/fixtures/prepared/content/mesa/lib/x86_64-linux-gnu/marker.so`
- Test: `crates/gpu-payload/src/builder.rs` (the `mod tests` at line 625)

**Interfaces:**
- Consumes: the record shape from Task 1 — `kind`, `output`, `licenses`, `inputs`,
  `sha256`.
- Produces: `pack` accepting `schema_version: 2` recipes, emitting a catalog entry whose
  `sources` list is the flattened rows, and refusing a `built` record whose `sha256`
  disagrees with the staged tree.

- [ ] **Step 1: Give the fixture a built tree**

```bash
mkdir -p crates/gpu-payload/tests/fixtures/prepared/content/mesa/lib/x86_64-linux-gnu
printf 'not a real shared object, but a real file with real bytes\n' \
    > crates/gpu-payload/tests/fixtures/prepared/content/mesa/lib/x86_64-linux-gnu/marker.so
```

Compute the digest the record must carry — the same rule the upstream digest uses, over
the files under `content/mesa`, each contributed as path, NUL, contents:

```bash
python3 - <<'EOF'
import hashlib, pathlib
root = pathlib.Path("crates/gpu-payload/tests/fixtures/prepared")
digest = hashlib.sha256()
for path in sorted((root / "content/mesa").rglob("*")):
    if path.is_file():
        digest.update(path.relative_to(root).as_posix().encode())
        digest.update(b"\0")
        digest.update(path.read_bytes())
print(digest.hexdigest())
EOF
```

Note the value it prints; the next step uses it as `<MESA_DIGEST>`.

- [ ] **Step 2: Move both fixtures to version 2 and add the built record**

In `crates/gpu-payload/tests/fixtures/recipe.json` and
`crates/gpu-payload/tests/fixtures/prepared/sources.json`, set
`"schema_version": 2`, add `"kind": "checkout",` as the first field of the existing
source, and append this second source to both `sources` arrays (identical in both files —
the builder refuses the pair unless they agree field for field):

```json
    {
      "kind": "built",
      "url": "https://gitlab.freedesktop.org/mesa/mesa",
      "commit": "14794180686c2fb6307fbe359c359bec765249f3",
      "version": "26.1.2",
      "output": "content/mesa",
      "licenses": ["GPL-2.0"],
      "inputs": [
        {
          "url": "https://github.com/microsoft/DirectX-Headers",
          "commit": "14794180686c2fb6307fbe359c359bec765249f3",
          "version": "v1.615.0"
        }
      ],
      "sha256": "<MESA_DIGEST>"
    }
```

- [ ] **Step 3: Write the failing tests**

Add to `mod tests` in `crates/gpu-payload/src/builder.rs`:

```rust
    #[test]
    fn a_built_tree_that_does_not_match_its_recorded_digest_is_refused() {
        let fixture = PreparedFixture::new("built-digest");
        fs::write(
            fixture
                .prepared
                .join("content/mesa/lib/x86_64-linux-gnu/marker.so"),
            b"different bytes entirely\n",
        )
        .unwrap();

        let error = pack(fixture.request(
            &fixture.root.join("payload.zip"),
            &fixture.root.join("entry.json"),
        ))
        .unwrap_err();

        assert!(matches!(error, PayloadError::InvalidManifest(_)));
    }

    #[test]
    fn a_built_record_whose_output_holds_nothing_is_refused() {
        let fixture = PreparedFixture::new("built-empty");
        fixture.rewrite_recipe(|recipe| {
            recipe["sources"][1]["output"] = serde_json::json!("content/absent");
        });
        rewrite_json(&fixture.prepared.join("sources.json"), |sources| {
            sources["sources"][1]["output"] = serde_json::json!("content/absent");
        });

        let error = pack(fixture.request(
            &fixture.root.join("payload.zip"),
            &fixture.root.join("entry.json"),
        ))
        .unwrap_err();

        assert!(matches!(error, PayloadError::InvalidManifest(_)));
    }

    #[test]
    fn a_built_records_inputs_reach_the_catalog_beside_it() {
        let fixture = PreparedFixture::new("built-inputs");
        let entry_path = fixture.root.join("entry.json");
        pack(fixture.request(&fixture.root.join("payload.zip"), &entry_path)).unwrap();

        let document: serde_json::Value =
            serde_json::from_slice(&fs::read(&entry_path).unwrap()).unwrap();
        let urls: Vec<&str> = document["sources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|source| source["url"].as_str().unwrap())
            .collect();

        assert_eq!(
            urls,
            [
                "https://github.com/microsoft/WSL2-Linux-Kernel",
                "https://gitlab.freedesktop.org/mesa/mesa",
                "https://github.com/microsoft/DirectX-Headers",
            ]
        );
    }

    #[test]
    fn a_recipe_at_version_one_is_no_longer_understood() {
        let fixture = PreparedFixture::new("old-recipe");
        fixture.rewrite_recipe(|recipe| {
            recipe["schema_version"] = serde_json::json!(1);
        });

        let error = pack(fixture.request(
            &fixture.root.join("payload.zip"),
            &fixture.root.join("entry.json"),
        ))
        .unwrap_err();

        assert!(matches!(error, PayloadError::InvalidCatalog(_)));
    }
```

- [ ] **Step 4: Run the tests to verify they fail**

Run: `cargo test -p vmlord-gpu-payload --lib builder`
Expected: FAIL — every test in the module fails, because the version-2 fixture is refused
by the version check at builder.rs:141 and the `kind` field by `deny_unknown_fields`.

- [ ] **Step 5: Mirror the union into the builder**

In `crates/gpu-payload/src/builder.rs`, replace `RecipeSource` (line 67) with the same
untagged union — the builder keeps its own copy of these types because it compares recipe
against prepared for exact equality, and a shared type would tie the two documents'
schemas together:

```rust
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
enum RecipeSource {
    Checkout(RecipeCheckout),
    Built(RecipeBuilt),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum CheckoutKind {
    Checkout,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum BuiltKind {
    Built,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RecipeCheckout {
    kind: CheckoutKind,
    url: String,
    commit: String,
    version: String,
    paths: Vec<String>,
    licenses: Vec<RecipeSourceLicense>,
    sha256: Sha256Digest,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RecipeBuilt {
    kind: BuiltKind,
    url: String,
    commit: String,
    version: String,
    output: String,
    licenses: Vec<String>,
    inputs: Vec<RecipeSourceInput>,
    sha256: Sha256Digest,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RecipeSourceInput {
    url: String,
    commit: String,
    version: String,
}
```

The `paths` and `sha256` fields of `RecipeCheckout` lose their `#[serde(default)]` and
`Option`: a checkout without selected paths or without a digest was never valid, and the
union is the moment to stop accepting one.

At line 141, change `if recipe.schema_version != 1` to `!= 2`, and at line 394 change
`if prepared.schema_version != 1` to `!= 2`.

- [ ] **Step 6: Flatten inputs into the catalog entry**

In `catalog_entry` (line 346), replace the `sources` construction with:

```rust
    let mut sources = Vec::new();
    for source in &recipe.sources {
        let (url, commit, version) = match source {
            RecipeSource::Checkout(checkout) => {
                (&checkout.url, &checkout.commit, &checkout.version)
            }
            RecipeSource::Built(built) => (&built.url, &built.commit, &built.version),
        };
        sources.push(serde_json::json!({
            "url": url, "commit": commit, "version": version,
        }));
        if let RecipeSource::Built(built) = source {
            for input in &built.inputs {
                sources.push(serde_json::json!({
                    "url": input.url,
                    "commit": input.commit,
                    "version": input.version,
                }));
            }
        }
    }
```

- [ ] **Step 7: Verify the built digest against the staged files**

In `validate_prepared_provenance` (line 385), after the recipe/prepared comparison and
before the overlay loop, add:

```rust
    for source in &recipe.sources {
        let RecipeSource::Built(built) = source else {
            continue;
        };
        let digest = built_output_digest(files, &built.output)?;
        if digest != built.sha256 {
            return Err(PayloadError::InvalidManifest(format!(
                "the built tree at {} is not what the recipe recorded",
                built.output
            )));
        }
    }
```

and add beside it:

```rust
/// Digests the tree a built source produced, by the rule its record claims.
///
/// The same shape as the upstream digest -- each file, sorted by path, contributed as
/// path, NUL, contents -- but over files that are in this builder's hands, which makes
/// this the one digest in the document it can verify rather than record.
fn built_output_digest(
    files: &[PreparedInput],
    output: &str,
) -> Result<Sha256Digest, PayloadError> {
    let prefix = format!("{output}/");
    let mut members = files
        .iter()
        .filter(|file| file.archive_path.starts_with(&prefix))
        .peekable();
    if members.peek().is_none() {
        return Err(PayloadError::InvalidManifest(format!(
            "the built tree at {output} holds nothing"
        )));
    }

    let mut hasher = Sha256Digest::hasher();
    for file in members {
        hasher.update(file.archive_path.as_bytes());
        hasher.update(b"\0");
        let mut input = File::open(&file.host_path).map_err(|error| {
            PayloadError::io("read prepared file", file.host_path.clone(), error)
        })?;
        io::copy(&mut input, &mut hasher)
            .map_err(|error| PayloadError::io("read prepared file", file.host_path.clone(), error))?;
    }
    Ok(hasher.finish())
}
```

`files` is already sorted by `archive_path` (`collect_files` sorts before returning), so
the filter preserves the order the digest rule requires.

- [ ] **Step 8: Give `Sha256Digest` an incremental form if it has none**

Run: `grep -n "pub fn" crates/gpu-payload/src/digest.rs`

If `Sha256Digest` exposes only `hash_reader`, add the three methods
`built_output_digest` uses, following the file's existing style:

```rust
    /// An incremental digest, for content that is not one reader.
    pub fn hasher() -> Sha256Hasher {
        Sha256Hasher(sha2::Sha256::new())
    }
```

with `Sha256Hasher` a newtype over the crate's hasher implementing `update`,
`std::io::Write` (so `io::copy` accepts it) and `finish() -> Sha256Digest`. If an
equivalent already exists, use it and skip this step.

- [ ] **Step 9: Run the tests to verify they pass**

Run: `cargo test -p vmlord-gpu-payload`
Expected: PASS — all four new tests and every pre-existing builder, manifest, archive and
staging test.

- [ ] **Step 10: Run the whole suite and the linters**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check`
Expected: all pass.

- [ ] **Step 11: Commit**

```bash
git add crates/gpu-payload/src/builder.rs crates/gpu-payload/src/digest.rs crates/gpu-payload/tests/fixtures
git commit -m "$(cat <<'EOF'
TASK-108: Check the one digest the builder can check

A built tree is in the builder's hands, unlike the upstream it came
from, so its recorded digest is verifiable rather than believed. Drift
between what was compiled and what was recorded is now impossible
instead of unlikely.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: `prepare.py` writes the built record

**Files:**
- Modify: `payloads/ubuntu-26.04-amd64/prepare.py`
- Modify: `payloads/ubuntu-26.04-amd64/payload.spec.json`

**Interfaces:**
- Consumes: the record shape from Task 1, and the version check from Task 2.
- Produces: `prepare.py --spec … --overlays … --licenses … --checkout … --mesa <tree>
  --output <dir>`, writing `prepared/`, `prepared/sources.json` and `recipe.json` at
  `schema_version: 2`. `--mesa` is required when the spec's `mesa_policy` is `bundled`
  and refused otherwise.

- [ ] **Step 1: Add the Mesa source to the spec**

In `payloads/ubuntu-26.04-amd64/payload.spec.json`:

- `payload_id` becomes `"ubuntu-26.04-amd64-7.0.0-28-v2"`;
- `required_renderers` becomes `["d3d12-gallium", "dzn-vulkan"]`;
- `mesa_policy` becomes `"bundled"`;
- rename the existing `"source"` key to `"sources"` and make it a list whose single
  member is the current object with `"kind": "checkout"` added;
- append the Mesa member. `commit` and `version` are filled in Task 4, once the container
  build has reported which upstreams it actually consumed; leave them at the placeholder
  below and do **not** run `prepare.sh` before Task 4 completes them:

```json
    {
      "kind": "built",
      "url": "https://gitlab.freedesktop.org/mesa/mesa",
      "commit": "PINNED-IN-TASK-4",
      "version": "PINNED-IN-TASK-4",
      "output": "content/mesa",
      "licenses": ["MIT"],
      "inputs": []
    }
```

- add to `licenses`:

```json
    { "spdx": "MIT", "path": "licenses/MIT-Mesa.txt", "upstream": "docs/license.rst" }
```

- [ ] **Step 2: Teach `prepare.py` the two kinds**

Replace `source_record` and `copy_upstream`'s use of `spec["source"]` with a dispatch over
`spec["sources"]`. The new and changed functions in full:

```python
SCHEMA_VERSION = 2


def source_records(spec: dict, checkout: Path, prepared: Path, mesa: Path | None) -> list[dict]:
    """The provenance record for every source, in the order the spec lists them."""
    records = []
    for source in spec["sources"]:
        if source["kind"] == "checkout":
            records.append(checkout_record(source, checkout))
        else:
            records.append(built_record(source, prepared, mesa))
    return records


def checkout_record(source: dict, checkout: Path) -> dict:
    """The upstream record, with its paths sorted as the manifest requires."""
    selections = sorted(source["paths"], key=lambda selection: selection["path"])
    return {
        "kind": "checkout",
        "url": source["url"],
        "commit": source["commit"],
        "version": source["version"],
        "paths": [selection["path"] for selection in selections],
        "licenses": [
            {"path": selection["path"], "spdx": selection["spdx"]} for selection in selections
        ],
        "sha256": upstream_digest(selections, checkout),
    }


def built_record(source: dict, prepared: Path, mesa: Path | None) -> dict:
    """The record for a tree that was compiled rather than copied.

    The digest covers what shipped, by the same rule the upstream digest uses, so the
    builder can check it against the files it is about to pack instead of taking it on
    trust.
    """
    if mesa is None:
        raise SystemExit("this payload's policy is bundled: --mesa <tree> is required")
    output = prepared / source["output"]
    if output.exists():
        shutil.rmtree(output)
    shutil.copytree(mesa, output, symlinks=False)
    return {
        "kind": "built",
        "url": source["url"],
        "commit": source["commit"],
        "version": source["version"],
        "output": source["output"],
        "licenses": source["licenses"],
        "inputs": source["inputs"],
        "sha256": tree_digest(prepared, output),
    }


def tree_digest(root: Path, tree: Path) -> str:
    """Digests a shipped subtree: each file, sorted by payload path, path, NUL, bytes."""
    digest = hashlib.sha256()
    files = sorted(path for path in tree.rglob("*") if path.is_file())
    if not files:
        raise SystemExit(f"the built tree at {tree} holds nothing")
    for path in files:
        digest.update(path.relative_to(root).as_posix().encode())
        digest.update(b"\0")
        digest.update(path.read_bytes())
    return digest.hexdigest()
```

`copytree(..., symlinks=False)` dereferences: `collect_files` in the builder rejects a
symlink outright, so a link must arrive as the file it points at.

In `copy_upstream` and `selected_files`, read the checkout selections from
`[s for s in spec["sources"] if s["kind"] == "checkout"]` rather than `spec["source"]`.

In `main`, add the argument and thread it through:

```python
    parser.add_argument("--mesa", type=Path, default=None)
```

```python
    provenance = {
        "target": spec["target"],
        "mesa_policy": spec["mesa_policy"],
        "sources": source_records(spec, arguments.checkout, prepared, arguments.mesa),
        "overlays": [overlay_record(overlay, prepared) for overlay in spec["overlays"]],
    }
```

Order matters: `source_records` copies the built tree into `prepared`, so it must run
before the file count is reported and before `overlay_record` measures anything.

- [ ] **Step 3: Check the script parses and refuses a bundled spec without `--mesa`**

Run:
```bash
python3 -c "import ast,pathlib; ast.parse(pathlib.Path('payloads/ubuntu-26.04-amd64/prepare.py').read_text())"
python3 payloads/ubuntu-26.04-amd64/prepare.py --help
```
Expected: no syntax error, and `--mesa` appears in the usage.

- [ ] **Step 4: Commit**

```bash
git add payloads/ubuntu-26.04-amd64/prepare.py payloads/ubuntu-26.04-amd64/payload.spec.json
git commit -m "$(cat <<'EOF'
TASK-108: Lay out a built tree and record what it is

The spec grows a second source and a policy that means it. The tree
arrives dereferenced, because a payload whose members are links is a
payload the builder refuses.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: Mesa builds in a container

**Files:**
- Create: `payloads/ubuntu-26.04-amd64/mesa/build.sh`
- Create: `payloads/ubuntu-26.04-amd64/mesa/closure.sh`
- Create: `payloads/ubuntu-26.04-amd64/Dockerfile`
- Modify: `payloads/ubuntu-26.04-amd64/payload.spec.json` (fill the pins)
- Create: `payloads/ubuntu-26.04-amd64/licenses/MIT-Mesa.txt` and one text per input

**Interfaces:**
- Consumes: `prepare.py --mesa` from Task 3.
- Produces: `docker build --build-arg …  --output type=local,dest=<dir>` writing
  `<dir>/prepared/` and `<dir>/recipe.json`. Build arguments: `MESA_COMMIT`,
  `DIRECTX_HEADERS_COMMIT`, `KERNEL_COMMIT`, plus one `<NAME>_COMMIT` per further input
  the build turns out to need.

- [ ] **Step 1: Find the base image digest**

Run:
```bash
docker pull ubuntu:26.04
docker inspect --format='{{index .RepoDigests 0}}' ubuntu:26.04
```
Note the `ubuntu@sha256:…` value; every `FROM` below uses it.

- [ ] **Step 2: Discover what the build actually demands**

This step answers a question the design deliberately left open: which upstreams
`-Dvulkan-drivers=microsoft-experimental` pulls in. Run a throwaway container and read
meson's own answer:

```bash
docker run --rm -it ubuntu@sha256:<digest> bash -c '
  apt-get update &&
  apt-get install -y --no-install-recommends \
    build-essential meson ninja-build pkg-config git ca-certificates \
    python3-mako python3-ply python3-yaml bison flex cmake \
    libdrm-dev libx11-dev libxext-dev libxfixes-dev libxdamage-dev \
    libxshmfence-dev libxxf86vm-dev libxrandr-dev libx11-xcb-dev \
    libxcb-dri2-0-dev libxcb-dri3-dev libxcb-glx0-dev libxcb-present-dev \
    libxcb-randr0-dev libxcb-shm0-dev libxcb-sync-dev libxcb-xfixes0-dev \
    libwayland-dev libwayland-egl-backend-dev wayland-protocols \
    libexpat1-dev zlib1g-dev libzstd-dev libglvnd-dev &&
  git clone --depth 1 https://gitlab.freedesktop.org/mesa/mesa /src &&
  cd /src && git log -1 --format=%H && git describe --tags &&
  meson setup build --wrap-mode=nodownload \
    -Dprefix=/opt/vmlord/wsl-mesa -Dlibdir=lib/x86_64-linux-gnu \
    -Dgallium-drivers=d3d12,softpipe -Dvulkan-drivers=microsoft-experimental \
    -Dllvm=disabled -Dglvnd=enabled -Dglvnd-vendor-name=mesa \
    -Dplatforms=x11,wayland -Dbuildtype=release -Db_ndebug=true
'
```

`--wrap-mode=nodownload` is what makes meson name every subproject it wanted instead of
fetching it silently. Record from the output:

1. the Mesa commit and the tag it is on — these fill `commit` and `version` in the spec;
2. every subproject or `dependency()` meson reported missing — each becomes an `inputs`
   entry, a pinned checkout in the `sources` stage, and a license text under
   `licenses/`;
3. any apt package the configure step demanded beyond the list above.

If meson refuses the pin because the tag is older than the option names above, pin Mesa
to the newest release tag that configures, and record that tag.

- [ ] **Step 3: Write the build script**

Create `payloads/ubuntu-26.04-amd64/mesa/build.sh`:

```sh
#!/usr/bin/env bash
# Configures, compiles and trims the Mesa a bundled payload carries.
#
# The prefix is not a parameter: Mesa's loader finds its own dri/ directory by the
# prefix compiled into it, and the guest stages this tree at exactly /opt/vmlord/wsl-mesa.
#
# Run with no network. Every source is already in place; a wrap that reaches out is a
# source nobody recorded.

set -euo pipefail

source="${1:?usage: build.sh <source> <destination>}"
destination="${2:?usage: build.sh <source> <destination>}"

meson setup "$source/build" "$source" \
	--wrap-mode=nodownload \
	-Dprefix=/opt/vmlord/wsl-mesa \
	-Dlibdir=lib/x86_64-linux-gnu \
	-Dgallium-drivers=d3d12,softpipe \
	-Dvulkan-drivers=microsoft-experimental \
	-Dllvm=disabled \
	-Dglvnd=enabled \
	-Dglvnd-vendor-name=mesa \
	-Dplatforms=x11,wayland \
	-Dbuildtype=release \
	-Db_ndebug=true

meson compile -C "$source/build"
DESTDIR="$source/install" meson install -C "$source/build" --strip

staged="$source/install/opt/vmlord/wsl-mesa"
[ -d "$staged" ] || {
	echo "meson installed nothing at $staged" >&2
	exit 1
}

# Headers, pkg-config files and static archives are for building against this Mesa,
# which nothing in a guest ever does.
rm -rf "$staged/include" "$staged/lib/x86_64-linux-gnu/pkgconfig"
find "$staged" -name '*.a' -delete
find "$staged" -name '*.la' -delete

# Every member arrives as a plain file: the payload builder rejects a symlink outright,
# and Mesa installs its DRI modules as names pointing at one gallium module. The cost is
# a second copy of that module per driver, and it is the price of a payload with no
# links in it.
mkdir -p "$destination"
cp -rL "$staged/." "$destination/"

icd="$destination/share/vulkan/icd.d/dzn_icd.x86_64.json"
[ -f "$icd" ] || {
	echo "the dozen ICD is not at $icd, which is the only name the guest registers" >&2
	exit 1
}
[ -f "$destination/lib/x86_64-linux-gnu/dri/d3d12_dri.so" ] || {
	echo "the d3d12 gallium driver is missing, and the probe looks for it by that path" >&2
	exit 1
}
```

- [ ] **Step 4: Write the closure gate**

Create `payloads/ubuntu-26.04-amd64/mesa/closure.sh`:

```sh
#!/usr/bin/env bash
# Proves the staged tree loads in a guest, not only in the image that built it.
#
# Run in a clean Ubuntu with no -dev package installed: what resolves here resolves in a
# guest under the bundled policy, where no apt step installs Mesa's build dependencies.

set -euo pipefail

tree="${1:?usage: closure.sh <tree>}"

echo "$tree/lib/x86_64-linux-gnu" > /etc/ld.so.conf.d/vmlord-closure.conf
ldconfig

unresolved=0
while IFS= read -r library; do
	missing="$(ldd "$library" 2>/dev/null | awk '/not found/ { print $1 }')"
	if [ -n "$missing" ]; then
		echo "$library needs $(echo "$missing" | tr '\n' ' ')" >&2
		unresolved=1
	fi
done < <(find "$tree" -name '*.so' -o -name '*.so.*' | sort)

[ "$unresolved" -eq 0 ] || {
	echo "the payload would ship libraries a guest cannot load" >&2
	exit 1
}
echo "every shared object in $tree resolves against a clean Ubuntu"
```

- [ ] **Step 5: Write the Dockerfile**

Create `payloads/ubuntu-26.04-amd64/Dockerfile`. Substitute the digest from Step 1 and
add a checkout per input discovered in Step 2:

```dockerfile
# syntax=docker/dockerfile:1.7
#
# The whole prepare step for this payload. `prepare.sh` runs this and nothing else, so
# the host needs docker and no toolchain of its own -- and so the toolchain that built a
# payload is a pinned image rather than whatever the machine happened to have.

ARG BASE=ubuntu@sha256:<digest from step 1>

FROM ${BASE} AS toolchain
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential meson ninja-build pkg-config git ca-certificates jq python3 \
        python3-mako python3-ply python3-yaml bison flex cmake \
        libdrm-dev libx11-dev libxext-dev libxfixes-dev libxdamage-dev \
        libxshmfence-dev libxxf86vm-dev libxrandr-dev libx11-xcb-dev \
        libxcb-dri2-0-dev libxcb-dri3-dev libxcb-glx0-dev libxcb-present-dev \
        libxcb-randr0-dev libxcb-shm0-dev libxcb-sync-dev libxcb-xfixes0-dev \
        libwayland-dev libwayland-egl-backend-dev wayland-protocols \
        libexpat1-dev zlib1g-dev libzstd-dev libglvnd-dev \
    && rm -rf /var/lib/apt/lists/*

# Every checkout is a layer keyed by its pin: an unchanged pin is an unchanged layer, and
# a changed one invalidates exactly what it should.
FROM toolchain AS sources
ARG KERNEL_URL=https://github.com/microsoft/WSL2-Linux-Kernel
ARG KERNEL_COMMIT
RUN git init /src/kernel \
    && git -C /src/kernel remote add origin ${KERNEL_URL} \
    && git -C /src/kernel sparse-checkout init --cone \
    && git -C /src/kernel sparse-checkout set drivers/hv/dxgkrnl include/uapi/misc \
    && git -C /src/kernel fetch --depth 1 --filter=blob:none origin ${KERNEL_COMMIT} \
    && git -C /src/kernel checkout -q --detach ${KERNEL_COMMIT}

ARG MESA_URL=https://gitlab.freedesktop.org/mesa/mesa
ARG MESA_COMMIT
RUN git init /src/mesa \
    && git -C /src/mesa remote add origin ${MESA_URL} \
    && git -C /src/mesa fetch --depth 1 origin ${MESA_COMMIT} \
    && git -C /src/mesa checkout -q --detach ${MESA_COMMIT}

ARG DIRECTX_HEADERS_URL=https://github.com/microsoft/DirectX-Headers
ARG DIRECTX_HEADERS_COMMIT
RUN git init /src/directx-headers \
    && git -C /src/directx-headers remote add origin ${DIRECTX_HEADERS_URL} \
    && git -C /src/directx-headers fetch --depth 1 origin ${DIRECTX_HEADERS_COMMIT} \
    && git -C /src/directx-headers checkout -q --detach ${DIRECTX_HEADERS_COMMIT} \
    && mv /src/directx-headers /src/mesa/subprojects/DirectX-Headers

FROM sources AS mesa
COPY mesa/build.sh /usr/local/bin/build-mesa
# No network: everything that reaches the binaries came from a pin above, and a wrap
# cannot quietly add to it.
RUN --network=none chmod +x /usr/local/bin/build-mesa \
    && build-mesa /src/mesa /out/mesa

FROM ${BASE} AS closure
COPY --from=mesa /out/mesa /check/mesa
COPY mesa/closure.sh /usr/local/bin/closure
RUN chmod +x /usr/local/bin/closure && closure /check/mesa

FROM toolchain AS prepared
COPY --from=mesa /out/mesa /mesa
# The closure stage produces nothing this stage needs, but copying one file out of it is
# what makes BuildKit run it: a gate nobody depends on is a gate that gets skipped.
COPY --from=closure /check/mesa/share/vulkan/icd.d/dzn_icd.x86_64.json /closure-passed.json
COPY payload.spec.json prepare.py ./
COPY overlays ./overlays
COPY licenses ./licenses
RUN --network=none python3 prepare.py \
        --spec payload.spec.json \
        --overlays overlays \
        --licenses . \
        --checkout /src/kernel \
        --mesa /mesa \
        --output /output

FROM scratch AS output
COPY --from=prepared /output /
```

- [ ] **Step 6: Fill the pins and the license texts**

Write the commit and version from Step 2 into `payload.spec.json`'s Mesa record, replacing
both `PINNED-IN-TASK-4` placeholders, and add one `inputs` entry per upstream Step 2
reported. Fetch each license text into `payloads/ubuntu-26.04-amd64/licenses/` — Mesa's
from `docs/license.rst` of the pinned commit, DirectX-Headers' from its `LICENSE` — and
declare each in the spec's `licenses` list with its `upstream` path.

- [ ] **Step 7: Build it**

Run:
```bash
DOCKER_BUILDKIT=1 docker build \
    --build-arg KERNEL_COMMIT=$(jq -r '.sources[] | select(.kind=="checkout") | .commit' payloads/ubuntu-26.04-amd64/payload.spec.json) \
    --build-arg MESA_COMMIT=<mesa commit> \
    --build-arg DIRECTX_HEADERS_COMMIT=<directx commit> \
    --output type=local,dest=target/gpu-payload \
    payloads/ubuntu-26.04-amd64
```
Expected: the closure stage prints `every shared object in /check/mesa resolves against a
clean Ubuntu`, and `target/gpu-payload/` holds `recipe.json` and `prepared/`.

If the closure gate fails on a library the guest will not have, fix it at the source: link
that dependency statically, or add it as a pinned input the payload ships. Do not add a
package to the guest's apt steps — the bundled policy exists so that the guest installs
nothing.

- [ ] **Step 8: Pack it and see the numbers**

Run:
```bash
cargo run -p xtask -- gpu-payload pack \
    --recipe target/gpu-payload/recipe.json \
    --input target/gpu-payload/prepared \
    --archive target/gpu-payload/payload.zip \
    --catalog-entry target/gpu-payload/catalog-entry.json
jq '{expanded_size_limit, file_count_limit, required_renderers, mesa_policy, sources}' \
    target/gpu-payload/catalog-entry.json
```
Expected: `pack` succeeds, `mesa_policy` is `bundled`, `required_renderers` holds both
renderers, `sources` lists Mesa and its inputs beside the kernel, and the two limits are
the real measurements rather than the old 481306 and 20.

- [ ] **Step 9: Commit**

```bash
git add payloads/ubuntu-26.04-amd64/Dockerfile payloads/ubuntu-26.04-amd64/mesa \
        payloads/ubuntu-26.04-amd64/payload.spec.json payloads/ubuntu-26.04-amd64/licenses
git commit -m "$(cat <<'EOF'
TASK-108: Build Mesa with dozen, offline, in a pinned image

Compilation runs with the network switched off, so everything in the
binaries came from a commit the spec records. A clean Ubuntu then loads
every shared object, which is the guest's question asked before the
guest is there to ask it.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: `prepare.sh` becomes a wrapper

**Files:**
- Modify: `payloads/ubuntu-26.04-amd64/prepare.sh`

**Interfaces:**
- Consumes: the Dockerfile and its build arguments from Task 4.
- Produces: `prepare.sh --output <directory>`, unchanged in name and in what it leaves
  behind, needing only docker on the host. `--work` is gone.

- [ ] **Step 1: Rewrite the script**

Replace `payloads/ubuntu-26.04-amd64/prepare.sh` entirely:

```sh
#!/usr/bin/env bash
# Prepares the input `cargo xtask gpu-payload pack` needs for this target.
#
# Everything happens in the image beside this file: the pinned checkouts, the Mesa
# build, the closure check, and the layout. The host needs docker and nothing else --
# not jq, not python3, not a git new enough for partial clones -- and the toolchain that
# produced a payload is a pinned image rather than whatever the machine happened to have.
#
# Commits come from payload.spec.json, read inside the image and passed back in as build
# arguments so that each checkout is a layer keyed by its own pin.

set -euo pipefail

SPEC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

usage() {
	cat <<'USAGE'
usage: prepare.sh --output <directory>

  --output  where the prepared tree and recipe.json are written
USAGE
}

output=""
while [[ $# -gt 0 ]]; do
	case "$1" in
	--output)
		output="${2-}"
		[[ -n "$output" ]] || {
			echo "--output needs a directory" >&2
			exit 2
		}
		shift 2
		;;
	-h | --help)
		usage
		exit 0
		;;
	*)
		echo "unknown argument: $1" >&2
		usage >&2
		exit 2
		;;
	esac
done

[[ -n "$output" ]] || {
	echo "missing --output <directory>" >&2
	usage >&2
	exit 2
}

mkdir -p "$output"
output="$(cd "$output" && pwd)"

# The spec is read by the image's own jq, so that a host without jq can still tell the
# build which commits to fetch. The toolchain stage is built once and reused: it is the
# same layer the full build will hit, so this costs a cache lookup and not a build.
toolchain="$(DOCKER_BUILDKIT=1 docker build --quiet --target toolchain "$SPEC_DIR")"

pins="$(
	docker run --rm \
		-v "$SPEC_DIR/payload.spec.json:/spec.json:ro" \
		--entrypoint jq "$toolchain" -r '
			(.sources[] | select(.kind == "checkout") | "KERNEL\t" + .commit),
			(.sources[] | select(.kind == "built")
				| ("MESA\t" + .commit),
				  (.inputs[]
					| (.url | split("/") | last | ascii_upcase | gsub("-"; "_"))
					  + "\t" + .commit))
		' /spec.json
)"

arguments=()
while IFS=$'\t' read -r name commit; do
	[[ -n "$name" ]] || continue
	arguments+=(--build-arg "${name}_COMMIT=${commit}")
done <<<"$pins"

DOCKER_BUILDKIT=1 docker build \
	"${arguments[@]}" \
	--output "type=local,dest=$output" \
	"$SPEC_DIR"

echo "prepared tree and recipe.json written to $output"
```

The `gsub` turns a repository URL into the build argument name the Dockerfile declares:
`https://github.com/microsoft/DirectX-Headers` becomes `DIRECTX_HEADERS_COMMIT`. Adding an
input to the spec and an `ARG` to the Dockerfile is therefore all it takes; this script
does not learn about it.

- [ ] **Step 2: Run it end to end from a clean output directory**

Run:
```bash
rm -rf target/gpu-payload
payloads/ubuntu-26.04-amd64/prepare.sh --output target/gpu-payload
ls target/gpu-payload
```
Expected: `prepared/` and `recipe.json`, and the build arguments derived from the spec
match the commits used by hand in Task 4.

- [ ] **Step 3: Run it a second time and confirm the cache**

Run: `payloads/ubuntu-26.04-amd64/prepare.sh --output target/gpu-payload-again`
Expected: every stage reports `CACHED` except the export; the Mesa compile does not run
again.

- [ ] **Step 4: Commit**

```bash
git add payloads/ubuntu-26.04-amd64/prepare.sh
git commit -m "$(cat <<'EOF'
TASK-108: Ask the host for docker and nothing else

prepare.sh needed bash, git, jq and python3 and therefore ran only under
WSL. All of it is in the image now, so what produced a payload is a
pinned base rather than whatever the machine happened to have.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 6: The README, and the only proof that counts

**Files:**
- Modify: `payloads/ubuntu-26.04-amd64/README.md`
- Modify: `ARCHITECTURE.md` (the GPU payload section, if it describes `mesa_policy`)

**Interfaces:**
- Consumes: everything above.
- Produces: documentation matching what the code now does, and a verified real-host run.

- [ ] **Step 1: Rewrite the README's build section**

Replace the "Building" section's two-command recipe with the single `prepare.sh` call and
say what it needs (docker) and what it no longer needs (jq, python3, git). Keep the `pack`
and `cargo dist --gpu-payload` invocations exactly as they are — they did not change.

- [ ] **Step 2: Replace the `distro` section**

The section titled "The `distro` Mesa policy, and what it costs" becomes a `bundled`
section saying: what travels (the d3d12 and softpipe gallium drivers, the dozen ICD, the
glvnd vendor libraries), why the prefix is not negotiable, why glvnd matters — we replace
the vendor implementation behind libglvnd's dispatch and never the dispatch itself,
because `bundled_mesa` writes its `ld.so.conf.d` entry unguarded — and why softpipe is
there: a guest whose adapter goes away on a later start still draws.

- [ ] **Step 3: Document the built record**

Add a paragraph to "What the spec holds" giving the `built` record the same treatment the
upstream digest already has: `output` is a payload path and not an upstream one,
`licenses` are bare SPDX identifiers because a shared object attributes to no single
file, `inputs` are the upstreams inside the binaries, and the digest is the one in the
document the builder verifies rather than records.

- [ ] **Step 4: Note the symlink cost**

One paragraph: the payload holds no links because `collect_files` refuses them, so the
gallium module travels once per gallium driver. Give the measured size from Task 4 Step 8
rather than an estimate.

- [ ] **Step 5: Build a release and start a VM on the real host**

Run:
```bash
cargo dist --gpu-payload target/gpu-payload
```
Then, on the Windows host with the GPU-PV adapter, create and start a VM from the release
and read the agent's GPU report.

- [ ] **Step 6: Confirm the verdict**

Expected, and nothing less: the verdict is `RENDERS`; **both** the Opengl and the Vulkan
checks are `ok`; and the Vulkan device is named `Microsoft Direct3D12 (…)` with
`DRIVER_ID_MESA_DOZEN` and `PHYSICAL_DEVICE_TYPE_DISCRETE_GPU` — not llvmpipe, and not a
`WARN`.

If the Vulkan check reports llvmpipe, the ICD was not registered: read the agent's
`VulkanIcd` step, check `/etc/vulkan/icd.d` in the guest and the `library_path` inside
`dzn_icd.x86_64.json`, and fix the build rather than the guest.

- [ ] **Step 7: Commit and record the result**

```bash
git add payloads/ubuntu-26.04-amd64/README.md ARCHITECTURE.md
git commit -m "$(cat <<'EOF'
TASK-108: Say what the bundled policy carries and what it costs

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

Then comment on Vikunja task 108 with the verdict, the Vulkan device string as the guest
reported it, the payload's measured size and file count, and the branch name.

---

## Self-Review

**Spec coverage.** Provenance union — Task 1. Builder-side verification and catalog
flattening — Task 2. `prepare.py` — Task 3. The five container stages, the meson
configuration, the trimming and the closure gate — Task 4. `prepare.sh` as a wrapper —
Task 5. Payload identity, licenses and limits — Tasks 3 and 4 (the limits need no code:
Task 4 Step 8 reads them back rather than setting them). README — Task 6. The real-host
criterion — Task 6.

**Placeholders.** Two deliberate ones, both with a rule that resolves them:
`PINNED-IN-TASK-4` in the spec, resolved by Task 4 Step 2's discovery run, and the base
image digest, resolved by Task 4 Step 1. Task 3 says explicitly not to run `prepare.sh`
before those are filled. Task 4 Step 2 exists because which upstreams
`microsoft-experimental` demands is a fact to be read from meson, not asserted here.

**Type consistency.** `SourceRecord`/`CheckoutRecord`/`BuiltRecord`/`SourceInputRecord` in
manifest.rs; `RecipeSource`/`RecipeCheckout`/`RecipeBuilt`/`RecipeSourceInput` in
builder.rs — deliberately separate types for two documents the builder compares for
equality. `built_output_digest` (Rust) and `tree_digest` (Python) implement one rule and
must agree; Task 2 Step 1 computes the fixture's digest with the Python rule and Task 2
Step 9 proves the Rust side accepts it, which is the check that they do.
