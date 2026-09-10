# Arch GPU Payload Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship a second GPU payload, `arch-rolling-amd64`, built beside the Ubuntu one, so that an Arch guest gets GPU-PV rendering instead of `NoPayloadForGuest`.

**Architecture:** The one GPU payload target directory becomes a shared `payloads/gpu/` with one spec directory per target, as `payloads/display/` already is. The base image, the package snapshot and the library directory stop being constants of the Dockerfile and become values the spec states. A new optional `library_layout` field is how a payload tells the guest where it put its libraries, replacing a guess the agent makes today from the guest's own directories.

**Tech Stack:** Docker/BuildKit (pinned frontend and base images), Python 3 (`prepare.py`), bash (`prepare.sh`, `build.sh`, `closure.sh`), Rust (`crates/gpu-payload`, `crates/agent`), meson (Mesa).

**Spec:** `docs/superpowers/specs/2026-09-10-arch-gpu-payload-design.md`

## Global Constraints

- Rust only; no C code, no FFI. Log through `tracing`, never `log`.
- Commit subjects are `TASK-197: comment`, and every commit body carries `Refs: #197`.
- Agent tests: `cargo test -p vmlord-agent --target x86_64-unknown-linux-musl`.
- Host-side crate tests: `cargo test -p vmlord-gpu-payload`.
- The payload prefix is not a parameter and never becomes one: `/opt/vmlord/wsl-mesa`, compiled into Mesa's loader and staged by `bundled_mesa`.
- `SPEC_SCHEMA_VERSION`, `DOCUMENT_SCHEMA_VERSION` and `ENTRY_SCHEMA_VERSION` all stay `2`. `library_layout` is optional, exactly as `guest_capabilities` is.
- The payload holds no symbolic links: `collect_files` rejects one rather than resolving it.
- Provenance is written once, in `prepare.py`, into both `recipe.json` and `prepared/sources.json`; `builder.rs` refuses the pair unless they agree field for field.
- No new dependency that makes the agent link against a system C library.

---

### Task 1: Move the GPU payload to `payloads/gpu/`

**Files:**
- Move: `payloads/ubuntu-26.04-amd64/*` → `payloads/gpu/`, except `payload.spec.json`
- Move: `payloads/ubuntu-26.04-amd64/payload.spec.json` → `payloads/gpu/ubuntu-26.04-amd64/payload.spec.json`
- Modify: `payloads/gpu/prepare.sh` (gains `--spec`, loses its own spec path)
- Modify: `payloads/gpu/Dockerfile` (the spec is copied from a target subdirectory)
- Modify: `payloads/gpu/README.md`, `ARCHITECTURE.md` (paths only)

**Interfaces:**
- Consumes: nothing.
- Produces: `payloads/gpu/prepare.sh --spec <path> --output <directory>`, the invocation every later task uses.

- [ ] **Step 1: Move the files with git so history follows**

```bash
mkdir -p payloads/gpu/ubuntu-26.04-amd64
git mv payloads/ubuntu-26.04-amd64/payload.spec.json payloads/gpu/ubuntu-26.04-amd64/payload.spec.json
for item in Dockerfile README.md prepare.py prepare.sh prepare_test.py licenses mesa overlays; do
    git mv "payloads/ubuntu-26.04-amd64/$item" "payloads/gpu/$item"
done
rm -rf payloads/ubuntu-26.04-amd64
```

- [ ] **Step 2: Give `prepare.sh` a `--spec` argument**

In `payloads/gpu/prepare.sh`, replace the three lines that resolve the script's own directory as the spec directory:

```bash
SPEC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SPEC="$SPEC_DIR/payload.spec.json"
DOCKERFILE="$SPEC_DIR/Dockerfile"
```

with a context directory that is the script's own, and a spec that is passed:

```bash
# The build context is this directory: one Dockerfile, one prepare.py and one Mesa
# recipe serve every target, and what differs between two targets is the spec.
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DOCKERFILE="$HERE/Dockerfile"
SPEC=""
```

Extend the argument loop (beside the existing `--output` case) with:

```bash
	--spec)
		SPEC="${2-}"
		[[ -n "$SPEC" ]] || {
			echo "--spec needs a payload.spec.json" >&2
			exit 2
		}
		shift 2
		;;
```

and the usage block with `--spec  the target to build, e.g. payloads/gpu/arch-rolling-amd64/payload.spec.json`.

After the loop, beside the existing `--output` check:

```bash
[[ -n "$SPEC" ]] || {
	echo "missing --spec <payload.spec.json>" >&2
	usage >&2
	exit 2
}
[[ -f "$SPEC" ]] || {
	echo "no such spec: $SPEC" >&2
	exit 2
}
SPEC="$(cd "$(dirname "$SPEC")" && pwd)/$(basename "$SPEC")"
# Which target directory the Dockerfile copies the spec out of. The context is shared, so
# the spec's own directory name is what tells the build which of them to read.
TARGET="$(basename "$(dirname "$SPEC")")"
```

Then replace every remaining `"$SPEC_DIR"` with `"$HERE"`, and add the target to the final build:

```bash
DOCKER_BUILDKIT=1 docker build \
	--build-arg "TARGET=$TARGET" \
	"${arguments[@]}" \
	--output "type=local,dest=$output" \
	"$HERE"
```

- [ ] **Step 3: Teach the Dockerfile where the spec now lives**

In `payloads/gpu/Dockerfile`, in the `prepared` stage, replace:

```dockerfile
COPY payload.spec.json prepare.py prepare_test.py ./
```

with:

```dockerfile
# Which target is being built. The context holds every target's spec, and this is the one
# line that picks one: a build argument rather than a copy of the whole directory, so that
# two targets cannot end up reading each other's provenance.
ARG TARGET
COPY ${TARGET}/payload.spec.json ./payload.spec.json
COPY prepare.py prepare_test.py ./
```

- [ ] **Step 4: Build the Ubuntu target from the new layout**

Run:

```bash
payloads/gpu/prepare.sh \
    --spec payloads/gpu/ubuntu-26.04-amd64/payload.spec.json \
    --output target/gpu-payload/ubuntu
```

Expected: the build runs to completion and prints `prepared tree and recipe.json written to …/target/gpu-payload/ubuntu`.

- [ ] **Step 5: Pack it, to prove the move changed nothing**

Run:

```bash
cargo run -p xtask -- gpu-payload pack \
    --recipe        target/gpu-payload/ubuntu/recipe.json \
    --input         target/gpu-payload/ubuntu/prepared \
    --archive       target/gpu-payload/ubuntu/payload.zip \
    --catalog-entry target/gpu-payload/ubuntu/catalog-entry.json
```

Expected: PASS. `catalog-entry.json` names `payload_id` `ubuntu-26.04-amd64-7.0.0-28-v2`.

- [ ] **Step 6: Update the paths in the prose**

In `payloads/gpu/README.md` retitle the first line to `# GPU payload` and replace every `payloads/ubuntu-26.04-amd64/` with `payloads/gpu/`; the build commands gain `--spec payloads/gpu/ubuntu-26.04-amd64/payload.spec.json`. In `ARCHITECTURE.md`, replace every occurrence of `payloads/ubuntu-26.04-amd64/` the same way. Find them with:

```bash
grep -rn "payloads/ubuntu-26.04-amd64" --include='*.md' --include='*.rs' --include='*.sh' .
```

Every hit must be gone when this step ends.

- [ ] **Step 7: Commit**

```bash
git add -A payloads ARCHITECTURE.md
git commit -m "TASK-197: Give the GPU payload one build and a directory per target

Refs: #197

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 2: `library_layout` through the spec and the packer

**Files:**
- Modify: `payloads/gpu/prepare.py:60-73` (the `provenance` dict), plus a validator
- Modify: `payloads/gpu/ubuntu-26.04-amd64/payload.spec.json`
- Modify: `crates/gpu-payload/src/builder.rs:28-44` (`PackRecipe`), `:147-157` (`PreparedSources`), `:332-350` (the cross-check)
- Test: `crates/gpu-payload/src/builder.rs` (its existing `mod tests`)

**Interfaces:**
- Consumes: nothing.
- Produces: the string form `"flat"` or `"multiarch:<triplet>"`, written by `prepare.py` into `recipe.json` and `prepared/sources.json` under the key `library_layout`; absent where the spec omits it. Task 3 parses it in the agent, Task 4 turns it into meson's `libdir`.

- [ ] **Step 1: Write the failing test**

In `crates/gpu-payload/src/builder.rs`, beside the existing test that a differing `guest_capabilities` is refused, add:

```rust
    #[test]
    fn a_library_layout_that_differs_between_the_two_documents_is_refused() {
        let declared = serde_json::json!("multiarch:x86_64-linux-gnu");
        let mut fixture = Fixture::new();
        fixture.rewrite_sources(|sources| {
            sources["library_layout"] = declared.clone();
        });
        fixture.rewrite_recipe(|recipe| recipe["library_layout"] = serde_json::json!("flat"));

        let error = fixture.pack().expect_err(
            "a payload whose two provenance documents disagree about where its libraries \
             are is a payload the guest would stage and then not find",
        );

        assert!(
            format!("{error}").contains("does not exactly match recipe provenance"),
            "{error}"
        );
    }

    #[test]
    fn a_payload_that_declares_no_library_layout_still_packs() {
        let fixture = Fixture::new();

        fixture
            .pack()
            .expect("an older payload states no layout, and that is a payload, not an error");
    }
```

Read the existing `guest_capabilities` tests (`builder.rs:730-760`) first and match the fixture helpers they use exactly — `rewrite_sources`/`rewrite_recipe` names come from there.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p vmlord-gpu-payload a_library_layout_that_differs`
Expected: FAIL — the field is unknown, so `deny_unknown_fields` rejects the document with a parse error rather than the provenance message.

- [ ] **Step 3: Add the field on the Rust side**

In `PackRecipe`, after `guest_capabilities`:

```rust
    /// Where in the payload this build put its libraries, as the build itself
    /// states it: `flat`, or `multiarch:<triplet>`.
    ///
    /// Absent in a payload prepared before the field existed, and absent is a
    /// promise of nothing: the guest then falls back to the layout it derives
    /// from its own directories, which is what every payload before this one
    /// relied on.
    #[serde(default)]
    library_layout: Option<String>,
```

The same field, with the same `#[serde(default)]`, in `PreparedSources`. In `validate_prepared_provenance`, add one clause to the existing chain:

```rust
        || prepared.library_layout != recipe.library_layout
```

The catalog entry does not carry it: like `guest_capabilities`, it is read by the guest out of `sources.json`, and the entry is what the host selects on.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-gpu-payload`
Expected: PASS, both new tests included.

- [ ] **Step 5: Write it in `prepare.py`**

In `payloads/gpu/prepare.py`, add above `main`:

```python
def library_layout(spec: dict) -> str | None:
    """Where this build puts the payload's libraries, as the spec states it.

    Two forms and nothing else: `flat`, and `multiarch:<triplet>`. Checked here
    because this is the one place the value is written, and a typo that reached a
    guest would look exactly like a payload that had staged correctly and then not
    been found by the linker.
    """
    value = spec.get("library_layout")
    if value is None or value == "flat":
        return value
    triplet = value.removeprefix("multiarch:")
    if triplet == value or not triplet:
        raise SystemExit(
            f"library_layout must be 'flat' or 'multiarch:<triplet>', not {value!r}"
        )
    return value
```

and in the `provenance` dict, after `guest_capabilities`:

```python
        # Where this build put the libraries, carried into both documents for the same
        # reason capabilities are: the guest reads sources.json, the packer reads the
        # recipe, and the two are refused unless they agree.
        "library_layout": library_layout(spec),
```

- [ ] **Step 6: Declare it in the Ubuntu spec**

In `payloads/gpu/ubuntu-26.04-amd64/payload.spec.json`, after `"mesa_policy": "bundled",`:

```json
  "library_layout": "multiarch:x86_64-linux-gnu",
```

- [ ] **Step 7: Rebuild the Ubuntu target and pack it**

Run:

```bash
payloads/gpu/prepare.sh --spec payloads/gpu/ubuntu-26.04-amd64/payload.spec.json --output target/gpu-payload/ubuntu
grep -o '"library_layout":"[^"]*"' target/gpu-payload/ubuntu/prepared/sources.json
cargo run -p xtask -- gpu-payload pack \
    --recipe        target/gpu-payload/ubuntu/recipe.json \
    --input         target/gpu-payload/ubuntu/prepared \
    --archive       target/gpu-payload/ubuntu/payload.zip \
    --catalog-entry target/gpu-payload/ubuntu/catalog-entry.json
```

Expected: the grep prints `"library_layout":"multiarch:x86_64-linux-gnu"`, and `pack` succeeds.

- [ ] **Step 8: Commit**

```bash
git add payloads/gpu crates/gpu-payload/src/builder.rs
git commit -m "TASK-197: Let a payload say where it put its libraries

Refs: #197

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 3: The agent stages by what the payload declares

**Files:**
- Modify: `crates/agent/src/gpu_recipe.rs` (a parser beside `parse_mesa_policy`, `gpu_recipe.rs:209-229`)
- Modify: `crates/agent/src/gpu_kernel.rs:412-447` (`Userspace`, `userspace_stage`), `:495` (`bundled_mesa`)
- Modify: `crates/agent/src/gpu_probe.rs:231-247` (`required_libraries`)
- Modify: `crates/agent/src/gpu_render.rs:191-201` (its one caller)
- Test: the `mod tests` already in each of those files

**Interfaces:**
- Consumes: the `library_layout` string Task 2 writes into `sources.json`.
- Produces:
  - `pub fn parse_library_layout(json: &str) -> Result<Option<LibraryLayout>, String>` in `gpu_recipe`;
  - `pub fn required_libraries(guest: &LibraryLayout, staged: Option<(&str, &LibraryLayout)>) -> Vec<String>` in `gpu_probe`, where the pair is the payload's prefix and the payload's own layout.

- [ ] **Step 1: Write the failing tests**

In `crates/agent/src/gpu_recipe.rs`, inside `mod tests`:

```rust
    #[test]
    fn a_payload_states_its_own_library_layout() {
        assert_eq!(
            parse_library_layout(r#"{"library_layout":"flat","mesa_policy":"bundled"}"#).unwrap(),
            Some(LibraryLayout::Flat)
        );
        assert_eq!(
            parse_library_layout(r#"{"library_layout":"multiarch:x86_64-linux-gnu"}"#).unwrap(),
            Some(LibraryLayout::Multiarch("x86_64-linux-gnu".to_owned()))
        );
    }

    #[test]
    fn a_payload_that_states_nothing_leaves_the_guest_to_decide() {
        assert_eq!(parse_library_layout(r#"{"mesa_policy":"bundled"}"#).unwrap(), None);
    }

    #[test]
    fn a_layout_this_build_cannot_read_fails_rather_than_being_guessed_at() {
        for document in [
            r#"{"library_layout":"multiarch:"}"#,
            r#"{"library_layout":"lib64"}"#,
            r#"{"library_layout":[]}"#,
        ] {
            assert!(parse_library_layout(document).is_err(), "{document}");
        }
        assert!(parse_library_layout("not json").is_err());
    }
```

In `crates/agent/src/gpu_probe.rs`, inside `mod tests`:

```rust
    #[test]
    fn a_staged_tree_is_looked_for_where_the_payload_put_it_and_not_where_the_guest_would() {
        // An Arch guest: no multiarch directory of its own, and a payload that
        // says its libraries are flat. Both halves have to be read from their
        // own side, or the probe reports a driver missing on a guest that draws.
        let required = required_libraries(
            &LibraryLayout::Flat,
            Some(("/opt/vmlord/wsl-mesa", &LibraryLayout::Flat)),
        );

        assert!(required.contains(&"/opt/vmlord/wsl-mesa/lib/dri/d3d12_dri.so".to_owned()));
        assert!(required.contains(&"/usr/lib/libvulkan.so.1".to_owned()));
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p vmlord-agent --target x86_64-unknown-linux-musl gpu_recipe::tests::a_payload_states`
Expected: FAIL with `cannot find function parse_library_layout in this scope`.

- [ ] **Step 3: Write the parser**

In `crates/agent/src/gpu_recipe.rs`, beside `parse_mesa_policy`:

```rust
/// Reads `library_layout` out of a payload's `sources.json`.
///
/// `None` is a payload that says nothing, which is every payload built before
/// the field existed: the guest then uses the layout it derived from its own
/// directories, which is what those payloads were built against.
///
/// A form this build cannot read is an error rather than a fallback. The two
/// answers differ by exactly one directory level, and the wrong one is not a
/// failure anybody sees: Mesa is staged, the linker is pointed somewhere empty,
/// and the guest quietly draws in software.
pub fn parse_library_layout(json: &str) -> Result<Option<LibraryLayout>, String> {
    let document: serde_json::Value = serde_json::from_str(json)
        .map_err(|error| format!("sources.json is unreadable: {error}"))?;
    let Some(value) = document.get("library_layout") else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let text = value
        .as_str()
        .ok_or_else(|| "sources.json states a library layout that is not a string".to_owned())?;
    if text == "flat" {
        return Ok(Some(LibraryLayout::Flat));
    }
    match text.strip_prefix("multiarch:") {
        Some(triplet) if !triplet.is_empty() => {
            Ok(Some(LibraryLayout::Multiarch(triplet.to_owned())))
        }
        _ => Err(format!(
            "vmlord-agent has no recipe for the library layout {text}"
        )),
    }
}
```

`LibraryLayout` is already imported in this file's `use crate::guest_platform::{...}` list for the tests; add it to the non-test import at the top if it is not there.

- [ ] **Step 4: Run the parser tests**

Run: `cargo test -p vmlord-agent --target x86_64-unknown-linux-musl gpu_recipe`
Expected: PASS.

- [ ] **Step 5: Use it where Mesa is staged**

In `crates/agent/src/gpu_kernel.rs`, give `Userspace` the layout it settled on:

```rust
/// The userspace this guest ended up with, and where it lives.
struct Userspace {
    policy: MesaPolicy,
    /// Where a bundled Mesa was staged; nothing under `distro`.
    prefix: Option<PathBuf>,
    /// How the staged tree lays its libraries out -- the payload's answer where
    /// it gives one, and the guest's where it does not.
    layout: LibraryLayout,
    /// The directories a process has to load libraries from, in order.
    library_paths: Vec<String>,
}
```

and in `userspace_stage`, read both documents once and let the payload's answer win:

```rust
fn userspace_stage(report: &mut Report, guest: &GuestFacts) -> Result<Userspace, String> {
    let sources = read(&Path::new(PAYLOAD).join("sources.json"));
    let policy = parse_mesa_policy(&sources)
        .inspect_err(|error| report.failed(GpuRecipeStep::Userspace, error.clone()))?;
    let declared = parse_library_layout(&sources)
        .inspect_err(|error| report.failed(GpuRecipeStep::Userspace, error.clone()))?;

    match policy {
        MesaPolicy::Distro => Ok(Userspace {
            policy,
            prefix: None,
            layout: guest.library_layout.clone(),
            library_paths: vec![WSL_LIB.to_owned()],
        }),
        MesaPolicy::Bundled => {
            let layout = declared.unwrap_or_else(|| guest.library_layout.clone());
            let prefix = bundled_mesa(report, &layout)?;
            Ok(Userspace {
                policy,
                library_paths: vec![layout.directory_under(MESA_PREFIX), WSL_LIB.to_owned()],
                layout,
                prefix: Some(prefix),
            })
        }
    }
}
```

The `MesaPolicy::Distro` arm keeps calling `distribution_mesa(report, guest)?` before it returns — the snippet above shows the shape, not a licence to drop that call. `bundled_mesa` itself needs no change: it already takes a `&LibraryLayout`.

- [ ] **Step 6: Let the probe look in the right two places**

In `crates/agent/src/gpu_probe.rs`:

```rust
/// The libraries a renderer opens, and where each of them lives.
///
/// Two layouts, because two trees answer to different rules: the guest's own
/// libraries are laid out the way the guest lays libraries out, and the staged
/// payload's are laid out the way the build that made them chose. On Ubuntu the
/// two agree; on Arch, whose guest has no multiarch directory, they do not.
pub fn required_libraries(
    guest: &LibraryLayout,
    staged: Option<(&str, &LibraryLayout)>,
) -> Vec<String> {
    let distribution = guest.directory();
    let mesa = match staged {
        Some((prefix, layout)) => layout.directory_under(prefix),
        None => distribution.clone(),
    };

    vec![
        format!("{mesa}/dri/d3d12_dri.so"),
        format!("{distribution}/libvulkan.so.1"),
        // `d3d12_dri.so` opens these itself, out of the host's mounted WSL
        // userspace: without them the GL path loads and then falls back.
        format!("{WSL_LIB}/libd3d12.so"),
        format!("{WSL_LIB}/libdxcore.so"),
    ]
}
```

Update the three existing call sites in that file's tests to pass `None` or `Some(("/opt/vmlord/wsl-mesa", &multiarch()))`.

In `crates/agent/src/gpu_render.rs`, `libraries_check`:

```rust
    let sources = read(&Path::new(PAYLOAD).join("sources.json"));
    let layout = parse_library_layout(&sources)
        .ok()
        .flatten()
        .unwrap_or_else(|| guest.library_layout.clone());
    let staged = match parse_mesa_policy(&sources) {
        Ok(MesaPolicy::Bundled) => Some((MESA_PREFIX, &layout)),
        // A payload that is not mounted, or one whose policy this build cannot
        // read, is not a reason to look nowhere: the distribution's own path is
        // where a guest without a staged Mesa has its driver.
        Ok(MesaPolicy::Distro) | Err(_) => None,
    };

    let required = required_libraries(&guest.library_layout, staged);
```

An unreadable layout is `None` here and not a failure, for the reason the doc comment on `libraries_check` already gives: this check never ends a probe. The staging step in Task 3's `userspace_stage` is where an unreadable layout does fail, and that is the step that would have acted on it.

- [ ] **Step 7: Check whether the staging decision itself can be tested**

`userspace_stage` reads `PAYLOAD`, a module constant, and `bundled_mesa` writes to
`/etc/ld.so.conf.d`, so neither takes a path a test could point elsewhere. Look for an
existing test in `gpu_kernel.rs` that works around this (its `payload_stage` tests write
into a temporary directory — read them and see what seam they use). If one exists, add a
test that a declared `flat` layout puts `/opt/vmlord/wsl-mesa/lib` in `library_paths`
while the guest is `Multiarch`. If no seam exists, do not invent one for this task: the
decision is covered by `parse_library_layout`'s tests and by the probe's, and say so in
the commit body rather than leaving the reader to wonder.

- [ ] **Step 8: Run the whole agent suite**

Run: `cargo test -p vmlord-agent --target x86_64-unknown-linux-musl`
Expected: PASS, 206 plus the new tests, 0 failed.

- [ ] **Step 9: Commit**

```bash
git add crates/agent/src
git commit -m "TASK-197: Stage the payload's Mesa where the payload says it is

Refs: #197

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 4: `libdir` follows `library_layout`

**Files:**
- Modify: `payloads/gpu/mesa/build.sh` (third argument)
- Modify: `payloads/gpu/mesa/closure.sh` (second argument)
- Modify: `payloads/gpu/Dockerfile` (`ARG LIBDIR`, passed to both scripts)
- Modify: `payloads/gpu/prepare.sh` (derives `LIBDIR` from the spec's `library_layout`)

**Interfaces:**
- Consumes: `library_layout` from Task 2.
- Produces: `--build-arg LIBDIR=<lib|lib/<triplet>>`, and a Mesa tree whose libraries are under `<prefix>/<LIBDIR>`.

- [ ] **Step 1: Take the directory as an argument in `build.sh`**

In `payloads/gpu/mesa/build.sh`, after the two existing positional arguments:

```bash
libdir="${3:?usage: build.sh <source> <destination> <libdir>}"
```

Replace `-Dlibdir=lib/x86_64-linux-gnu` with `-Dlibdir="$libdir"`, and every later `lib/x86_64-linux-gnu` in the file with `$libdir`:

```bash
rm -rf "$staged/bin" "$staged/include" "$staged/$libdir/pkgconfig"
...
rm -f "$staged/$libdir/libspirv_to_dxil.so"
...
[ -f "$destination/$libdir/dri/d3d12_dri.so" ] || {
```

Check with `grep -n 'x86_64-linux-gnu' payloads/gpu/mesa/build.sh` — it must print nothing when this step ends.

- [ ] **Step 2: Do the same in `closure.sh`**

```bash
tree="${1:?usage: closure.sh <tree> <libdir>}"
libdir="${2:?usage: closure.sh <tree> <libdir>}"
```

and the one line that uses it:

```bash
echo "$tree/$libdir" > /etc/ld.so.conf.d/vmlord-closure.conf
```

The allow-list of sonames does not change: those names are the display stack's, and they are the same on both distributions.

- [ ] **Step 3: Pass it through the Dockerfile**

Declare `ARG LIBDIR` in the `mesa` and `closure` stages and hand it to each script:

```dockerfile
RUN --network=none chmod +x /usr/local/bin/build-mesa \
    && git -C /src/mesa apply --verbose /src/patches/*.patch \
    && build-mesa /src/mesa /out/mesa ${LIBDIR}
```

```dockerfile
RUN --network=none chmod +x /usr/local/bin/closure && closure /check/mesa ${LIBDIR}
```

- [ ] **Step 4: Derive it in `prepare.sh`**

Beside the `TARGET` line added in Task 1:

```bash
# meson's libdir, from the one place that states where this payload's libraries go. Read
# with sed rather than jq because this runs before the toolchain image is built, and a
# host with no jq is exactly what this script exists to allow.
LAYOUT="$(sed -nE 's/.*"library_layout"[[:space:]]*:[[:space:]]*"([^"]*)".*/\1/p' "$SPEC" | head -n1)"
case "$LAYOUT" in
flat) LIBDIR="lib" ;;
multiarch:?*) LIBDIR="lib/${LAYOUT#multiarch:}" ;;
*)
	echo "$SPEC must state a library_layout of 'flat' or 'multiarch:<triplet>'," >&2
	echo "because it is what decides both where meson installs and where the guest" >&2
	echo "points its linker. Found: ${LAYOUT:-nothing}" >&2
	exit 1
	;;
esac
```

and add `--build-arg "LIBDIR=$LIBDIR"` to the final `docker build`.

Note that this makes `library_layout` required in practice for a `bundled` payload while staying optional in the schema: the schema's optionality is about payloads already built, and this script only builds new ones.

- [ ] **Step 5: Rebuild the Ubuntu target and check the tree is unchanged**

Run:

```bash
payloads/gpu/prepare.sh --spec payloads/gpu/ubuntu-26.04-amd64/payload.spec.json --output target/gpu-payload/ubuntu
find target/gpu-payload/ubuntu/prepared/content/mesa -name 'd3d12_dri.so'
```

Expected: the path printed is `…/content/mesa/lib/x86_64-linux-gnu/dri/d3d12_dri.so` — the same place it was before this task, reached now through the spec.

- [ ] **Step 6: Commit**

```bash
git add payloads/gpu
git commit -m "TASK-197: Build Mesa into the directory the spec names

Refs: #197

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 5: An Arch toolchain, an Arch closure, and a module gate

**Files:**
- Modify: `payloads/gpu/Dockerfile` (per-manager stages selected by build arguments)
- Modify: `payloads/gpu/prepare.sh` (reads `build` out of the spec, passes the stage names)
- Modify: `payloads/gpu/ubuntu-26.04-amd64/payload.spec.json` (gains a `build` object)

**Interfaces:**
- Consumes: `LIBDIR` from Task 4.
- Produces: build arguments `BASE`, `TOOLCHAIN`, `CLOSURE`, `MODULE`, `PACKAGE_SNAPSHOT`, each named by the spec's `build` object; and stages `toolchain-apt`/`toolchain-pacman`, `closure-apt`/`closure-pacman`, `module-none`/`module-pacman`.

- [ ] **Step 1: Split the base and the toolchain**

At the top of `payloads/gpu/Dockerfile`, replace the single `ARG BASE=ubuntu@sha256:…` and `FROM ${BASE} AS toolchain` with:

```dockerfile
# Which base image, and which of the per-manager stages below serve it. Named by the
# spec's `build` object rather than defaulted here: a target that forgot to say would
# otherwise be built against another target's distribution and would look fine.
ARG BASE
ARG TOOLCHAIN
ARG CLOSURE
ARG MODULE

FROM ${BASE} AS base

FROM base AS toolchain-apt
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

# Arch pins its packages by date, because `archlinux@sha256:...` pins a rootfs and
# nothing else: `pacman -Sy` against a live mirror fetches whatever exists today, and the
# toolchain that compiled a payload would not be recoverable from this repository. The
# date must not precede the base image's own, or the update below is a downgrade the
# keyring may refuse.
FROM base AS toolchain-pacman
ARG PACKAGE_SNAPSHOT
RUN printf 'Server = https://archive.archlinux.org/repos/%s/$repo/os/$arch\n' "${PACKAGE_SNAPSHOT}" \
        > /etc/pacman.d/mirrorlist \
    && pacman -Syu --noconfirm --needed \
        base-devel meson ninja pkgconf git jq python python-mako python-ply python-yaml \
        bison flex cmake libdrm libx11 libxext libxfixes libxdamage libxshmfence \
        libxxf86vm libxrandr libxcb wayland wayland-protocols expat zlib zstd libglvnd \
    && rm -rf /var/cache/pacman/pkg/*

FROM ${TOOLCHAIN} AS toolchain
```

The `sources`, `mesa` and `prepared` stages keep their `FROM toolchain`/`FROM sources` lines exactly as they are.

- [ ] **Step 2: Split the closure stage the same way**

Replace `FROM ${BASE} AS closure` and its `RUN apt-get …` with two stages and a selector, keeping the whole existing comment block above `closure-apt`:

```dockerfile
FROM base AS closure-apt
RUN apt-get update && apt-get install -y --no-install-recommends \
        libdrm2 libexpat1 libx11-6 libx11-xcb1 libxext6 libxxf86vm1 \
        libxcb1 libxcb-dri3-0 libxcb-glx0 libxcb-present0 libxcb-randr0 \
        libxcb-shm0 libxcb-sync1 libxcb-xfixes0 libxshmfence1 \
        libwayland-client0 zlib1g libzstd1 \
        binutils \
    && rm -rf /var/lib/apt/lists/*

# The same guest, modelled with the same rule on the other distribution: the runtime
# halves of the display stack and binutils for readelf. Arch has no split -dev packages,
# so these names are the whole library and there is nothing to leave out; what matters is
# that no Mesa of the distribution's own is installed, because that would hide a
# dependency of ours behind one of its.
FROM base AS closure-pacman
ARG PACKAGE_SNAPSHOT
RUN printf 'Server = https://archive.archlinux.org/repos/%s/$repo/os/$arch\n' "${PACKAGE_SNAPSHOT}" \
        > /etc/pacman.d/mirrorlist \
    && pacman -Syu --noconfirm --needed \
        libdrm expat libx11 libxext libxxf86vm libxcb libxshmfence wayland zlib zstd \
        binutils \
    && rm -rf /var/cache/pacman/pkg/*

FROM ${CLOSURE} AS closure
ARG LIBDIR
COPY --from=mesa /out/mesa /check/mesa
COPY mesa/closure.sh /usr/local/bin/closure
RUN --network=none chmod +x /usr/local/bin/closure && closure /check/mesa ${LIBDIR}
```

- [ ] **Step 3: Add the module gate**

After the closure stages:

```dockerfile
# Does the driver this payload ships actually build against this distribution's kernel?
# The display payload's container build answers that for its module by building it; the
# GPU payload ships sources and lets DKMS build them in the guest, so nothing answered it
# here until now. It matters most where the payload's sources and the guest's kernel come
# from different lineages: dxgkrnl is taken from a WSL kernel branch, and Arch runs
# mainline. The module built here is thrown away -- the payload still carries sources.
FROM toolchain AS module-pacman
ARG PACKAGE_SNAPSHOT
ARG TARGET
RUN pacman -Syu --noconfirm --needed linux-headers \
    && rm -rf /var/cache/pacman/pkg/*
COPY --from=prepared-sources /output/prepared/content/dxgkrnl /src/dxgkrnl
RUN --network=none set -eu; \
    headers="$(find /usr/lib/modules -maxdepth 2 -name build -type d | head -n1)"; \
    [ -n "$headers" ] || { echo "no kernel headers in the image" >&2; exit 1; }; \
    make -C "$headers" M=/src/dxgkrnl modules; \
    echo "dxgkrnl builds against $(basename "$(dirname "$headers")")" > /gate.txt

# A target whose distribution has no gate stage yet. The file is what the prepared stage
# copies, so the two shapes have to agree on producing one.
FROM base AS module-none
RUN echo "no module gate for this target" > /gate.txt

FROM ${MODULE} AS module
```

The gate reads the driver out of the tree `prepare.py` laid out, so the `prepared` stage is split in two: rename the existing one to `prepared-sources` (everything up to and including the `python3 prepare.py …` run), and add after `module`:

```dockerfile
FROM prepared-sources AS prepared
# A gate nothing depends on is a gate BuildKit skips, which is why one file is copied out
# of each of them. Neither file is part of the payload: `output` below takes /output alone.
COPY --from=module /gate.txt /module-passed.txt
```

The existing `COPY --from=closure … /closure-passed.json` line stays where it is, in `prepared-sources`.

- [ ] **Step 4: Give the Ubuntu spec its `build` object**

In `payloads/gpu/ubuntu-26.04-amd64/payload.spec.json`, after `"library_layout"`:

```json
  "build": {
    "base_image": "ubuntu@sha256:6df9e8dd1eac389ebfef692c9648449adeb815d01e16e29cd6f3e50fe64ba9a6",
    "toolchain": "toolchain-apt",
    "closure": "closure-apt",
    "module_gate": "module-none",
    "package_snapshot": ""
  },
```

`module_gate` is `module-none` here: this target's proof that `dxgkrnl` builds is the manual run its README records against `7.0.0-28-generic`, and turning that into a container gate is not this task.

- [ ] **Step 5: Pass the object through `prepare.sh`**

Beside the `LIBDIR` derivation:

```bash
# The build's own pins: which image, which stages, and -- where the distribution has no
# release archive of its own -- which day's packages.
field() {
	sed -nE "s/.*\"$1\"[[:space:]]*:[[:space:]]*\"([^\"]*)\".*/\1/p" "$SPEC" | head -n1
}
BASE="$(field base_image)"
TOOLCHAIN="$(field toolchain)"
CLOSURE="$(field closure)"
MODULE="$(field module_gate)"
PACKAGE_SNAPSHOT="$(field package_snapshot)"

for name in BASE TOOLCHAIN CLOSURE MODULE; do
	[[ -n "${!name}" ]] || {
		echo "$SPEC does not say what $name is" >&2
		exit 1
	}
done
```

and to the final `docker build`:

```bash
	--build-arg "BASE=$BASE" \
	--build-arg "TOOLCHAIN=$TOOLCHAIN" \
	--build-arg "CLOSURE=$CLOSURE" \
	--build-arg "MODULE=$MODULE" \
	--build-arg "PACKAGE_SNAPSHOT=$PACKAGE_SNAPSHOT" \
	--build-arg "LIBDIR=$LIBDIR" \
```

`PACKAGE_SNAPSHOT` is deliberately outside the loop that refuses an empty value: a distribution whose release is already an archive needs none.

- [ ] **Step 6: Rebuild the Ubuntu target**

Run:

```bash
payloads/gpu/prepare.sh --spec payloads/gpu/ubuntu-26.04-amd64/payload.spec.json --output target/gpu-payload/ubuntu
```

Expected: the build succeeds and the tree is what Task 4's step 5 produced. Nothing about the Ubuntu target's content has changed in this task; only who states the base image.

- [ ] **Step 7: Commit**

```bash
git add payloads/gpu
git commit -m "TASK-197: Let the spec name the image and the stages a target builds in

Refs: #197

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 6: The Arch target

**Files:**
- Create: `payloads/gpu/arch-rolling-amd64/payload.spec.json`

**Interfaces:**
- Consumes: everything Tasks 1, 2, 4 and 5 built.
- Produces: `target/gpu-payload/arch/{prepared,recipe.json}`, and after `pack`, a catalog entry whose `payload_id` starts `arch-rolling-amd64-`.

- [ ] **Step 1: Find the two values only the image can tell you**

The base image digest, and the kernel the `linux-headers` of the pinned snapshot resolve to:

```bash
docker pull archlinux:base
docker image inspect --format '{{index .RepoDigests 0}}' archlinux:base
```

Pick a `package_snapshot` (`YYYY/MM/DD`) no earlier than that image's build date; `2026/09/01` is the working default. The kernel string is read after the first build, in step 3.

- [ ] **Step 2: Write the spec**

`payloads/gpu/arch-rolling-amd64/payload.spec.json`, with `<digest>` from step 1 and `<kernel>` filled in by step 3:

```json
{
  "schema_version": 2,
  "payload_id": "arch-rolling-amd64-<kernel>-v1",
  "target": {
    "distribution": "arch",
    "release": "rolling",
    "architecture": "amd64",
    "kernel_release": "<kernel>",
    "payload_abi": 1
  },
  "required_renderers": ["d3d12-gallium", "dzn-vulkan"],
  "guest_capabilities": ["compositor-scanout"],
  "mesa_policy": "bundled",
  "library_layout": "flat",
  "build": {
    "base_image": "archlinux@sha256:<digest>",
    "toolchain": "toolchain-pacman",
    "closure": "closure-pacman",
    "module_gate": "module-pacman",
    "package_snapshot": "2026/09/01"
  },
  "sources": [],
  "licenses": [],
  "overlays": []
}
```

`sources`, `licenses` and `overlays` are copied verbatim from `payloads/gpu/ubuntu-26.04-amd64/payload.spec.json`: the same pinned WSL2 kernel commit and paths, the same Mesa commit with the same two patches and the same DirectX-Headers input, the same four licence rows, the same three overlays. Copy them rather than retyping them:

```bash
python3 - <<'PY'
import json, pathlib
ubuntu = json.loads(pathlib.Path("payloads/gpu/ubuntu-26.04-amd64/payload.spec.json").read_text())
arch = json.loads(pathlib.Path("payloads/gpu/arch-rolling-amd64/payload.spec.json").read_text())
for key in ("sources", "licenses", "overlays"):
    arch[key] = ubuntu[key]
pathlib.Path("payloads/gpu/arch-rolling-amd64/payload.spec.json").write_text(
    json.dumps(arch, indent=2) + "\n"
)
PY
```

The `built` record's `sha256` inside `sources` is the one digest the packer verifies rather than records, and it is computed over the files under `content/mesa`. A tree built on Arch is not byte-identical to one built on Ubuntu, so this value will be wrong on the first run and `pack` will say so, naming the tree. Step 4 is where it is corrected.

- [ ] **Step 3: Build it, and read the kernel back out**

Run:

```bash
payloads/gpu/prepare.sh --spec payloads/gpu/arch-rolling-amd64/payload.spec.json --output target/gpu-payload/arch
```

Expected on a first run: the toolchain, sources, mesa, closure and module stages all run. The module stage prints `dxgkrnl builds against <kernel>` — that string is the `kernel_release` and the `payload_id` suffix. Put it in the spec and rebuild.

If the module stage fails to compile, stop and read the error: a missing declaration from `<linux/hyperv.h>` is the expected shape, and the fix is an addition to `payloads/gpu/overlays/dxgkrnl_compat.h`, guarded by a kernel-version check so that the Ubuntu target's build is unaffected. Both targets must build after such a change.

- [ ] **Step 4: Pack it, and record the digest the build produced**

Run:

```bash
cargo run -p xtask -- gpu-payload pack \
    --recipe        target/gpu-payload/arch/recipe.json \
    --input         target/gpu-payload/arch/prepared \
    --archive       target/gpu-payload/arch/payload.zip \
    --catalog-entry target/gpu-payload/arch/catalog-entry.json
```

Expected on the first attempt: `the built tree at content/mesa is not what the recipe recorded`. Read the digest the build measured out of the recipe and write it into the spec's `built` record:

```bash
python3 -c "import json;print(json.load(open('target/gpu-payload/arch/recipe.json'))['sources'][1]['sha256'])"
```

Then rebuild and repack. Expected: PASS, with `payload_id` `arch-rolling-amd64-<kernel>-v1`.

- [ ] **Step 5: Check the tree is flat**

Run:

```bash
find target/gpu-payload/arch/prepared/content/mesa -name 'd3d12_dri.so'
find target/gpu-payload/arch/prepared/content/mesa -type l
```

Expected: `…/content/mesa/lib/dri/d3d12_dri.so` — `lib`, with no triplet — and no symbolic link at all.

- [ ] **Step 6: Commit**

```bash
git add payloads/gpu/arch-rolling-amd64 payloads/gpu/overlays
git commit -m "TASK-197: Add the Arch target

Refs: #197

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 7: Two payloads in one release

**Files:**
- Create: `rebuild_gpu_payload.sh`
- Test: `crates/gpu-payload/src/catalog.rs` (its existing `mod tests`)

**Interfaces:**
- Consumes: both specs.
- Produces: `target/gpu-payload/{ubuntu,arch}/` each holding `payload.zip` and `catalog-entry.json`, ready for `cargo dist --gpu-payload … --gpu-payload …`.

- [ ] **Step 1: Write the failing test**

In `crates/gpu-payload/src/catalog.rs`, inside `mod tests`, using the `entry_json` helper already there (`catalog.rs:420`):

```rust
    #[test]
    fn a_release_carrying_two_distributions_serves_each_its_own() {
        let catalog = PayloadCatalog::from_entries(vec![
            CatalogEntry::from_json(
                entry_json("ubuntu", "26.04", "amd64", "7.0.0-28-generic").as_bytes(),
            )
            .unwrap(),
            CatalogEntry::from_json(
                entry_json("arch", "rolling", "amd64", "6.17.4-arch1-1").as_bytes(),
            )
            .unwrap(),
        ])
        .expect("two targets are two entries, not a duplicate");

        assert_eq!(
            catalog
                .select_for_guest(&GuestSelector {
                    distribution: "arch",
                    release: "rolling",
                    architecture: "amd64",
                })
                .unwrap()
                .target
                .distribution,
            "arch"
        );
        assert_eq!(
            catalog
                .select_for_guest(&GuestSelector {
                    distribution: "ubuntu",
                    release: "26.04",
                    architecture: "amd64",
                })
                .unwrap()
                .target
                .distribution,
            "ubuntu"
        );
    }
```

Match the surrounding tests' exact way of building a `GuestSelector` and reaching the entry's target — read `catalog.rs:420-520` before writing this, and adjust field access to what is actually public there.

- [ ] **Step 2: Run it**

Run: `cargo test -p vmlord-gpu-payload a_release_carrying_two_distributions`
Expected: PASS with no production change — this is a characterisation test, and it must pass on the first run. If it fails, the failure is the finding: stop and report it rather than changing `catalog.rs` to suit the test.

- [ ] **Step 3: Write the rebuild script**

`rebuild_gpu_payload.sh`, modelled on `rebuild_payload.sh`:

```bash
#!/usr/bin/env bash
# Builds every GPU payload target and packs each of them.
#
# Both, unlike the display payload's script, which builds three and packs one: those three
# prepared trees differ in recipe.json alone, while these two are different binaries built
# against different C libraries, and a guest gets the one built for its distribution.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

for spec in "$ROOT"/payloads/gpu/*/payload.spec.json; do
	target="$(basename "$(dirname "$spec")")"
	output="$ROOT/target/gpu-payload/$target"

	echo "== $target"
	"$ROOT/payloads/gpu/prepare.sh" --spec "$spec" --output "$output"
	cargo run -p xtask -- gpu-payload pack \
		--recipe "$output/recipe.json" \
		--input "$output/prepared" \
		--archive "$output/payload.zip" \
		--catalog-entry "$output/catalog-entry.json"
done

echo
echo "Release with:"
for spec in "$ROOT"/payloads/gpu/*/payload.spec.json; do
	printf '    --gpu-payload target/gpu-payload/%s \\\n' "$(basename "$(dirname "$spec")")"
done
```

```bash
chmod +x rebuild_gpu_payload.sh
```

- [ ] **Step 4: Run it**

Run: `./rebuild_gpu_payload.sh`
Expected: both targets build and pack; the two `catalog-entry.json` files name two different `payload_id`s.

- [ ] **Step 5: Commit**

```bash
git add rebuild_gpu_payload.sh crates/gpu-payload/src/catalog.rs
git commit -m "TASK-197: Build and pack every GPU payload target

Refs: #197

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 8: Write down what is now true

**Files:**
- Modify: `ARCHITECTURE.md` ("GPU: guest payload", from line 1271)
- Modify: `payloads/gpu/README.md`
- Modify: `README.md` (the command table, if it lists payload builds)

**Interfaces:**
- Consumes: everything above.
- Produces: nothing code depends on.

- [ ] **Step 1: ARCHITECTURE.md**

In "GPU: guest payload", after the paragraph listing the file pair a release carries, add:

> A release carries one GPU payload per target and today that is two, `ubuntu-26.04-amd64` and `arch-rolling-amd64`. Two rather than one because a `bundled` payload's binaries are compiled against its base image's glibc and laid out the way that distribution lays libraries out, which is the asymmetry with the display payload recorded under "Display: the guest payload". `payloads/gpu/` holds one Dockerfile, one `prepare.py` and one Mesa recipe; a target is a directory holding a spec, and what that spec adds beyond provenance is its base image, the stages its package manager needs, and — for a distribution with no release archive of its own — the day its packages are pinned to.
>
> Where a payload's libraries are is the payload's statement rather than the guest's guess. `library_layout` is `flat` or `multiarch:<triplet>`; `prepare.sh` turns it into meson's `libdir`, `prepare.py` carries it into both provenance documents, and `userspace_stage` points `/etc/ld.so.conf.d/vmlord-wsl-mesa.conf` at what it names. The field is optional and an absent one means the guest decides, which is what every payload built before it relied on — but the two answers differ by one directory level on a guest with no multiarch directory, and the wrong one is silent: Mesa is staged, the linker finds nothing, and the desktop draws in software.

Then, in the same section, note the Arch gate:

> The Arch target builds `dxgkrnl` against the `linux-headers` of its pinned snapshot inside the image and throws the module away. The payload ships sources and DKMS builds them in the guest, as before; the stage exists because the sources come from a WSL kernel branch and Arch runs mainline, and without it "does not build here" is discovered inside a guest.

- [ ] **Step 2: `payloads/gpu/README.md`**

Retitle to `# GPU payload`, and add a "Targets" section naming both, with the values each spec decides (base image, snapshot, layout, kernel proven on) and the `rebuild_gpu_payload.sh` invocation. Keep the existing prose about the spec, the `bundled` policy and the closure gate: it describes both targets. Update the measured sizes paragraph to say which target its numbers came from, and add the Arch tree's measured size and file count beside it, read from that build's `catalog-entry.json`:

```bash
python3 -c "import json;e=json.load(open('target/gpu-payload/arch/catalog-entry.json'));print(e['expanded_size_limit'], e['file_count_limit'])"
```

- [ ] **Step 3: Say what is still unproven**

In the README's "Proven on" section, add for the Arch target: the kernel the module gate compiled against, that the closure gate passed, and — in the same words the Ubuntu target uses — that no run on a Windows host with a GPU-PV adapter has reported `RENDERS` with a Vulkan device named `Microsoft Direct3D12 (…)`. Neither target may claim a live-host proof it does not have.

- [ ] **Step 4: Commit**

```bash
git add ARCHITECTURE.md README.md payloads/gpu/README.md
git commit -m "TASK-197: Record the second GPU payload target

Refs: #197

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

- [ ] **Step 5: Run everything once more**

```bash
cargo test -p vmlord-gpu-payload
cargo test -p vmlord-agent --target x86_64-unknown-linux-musl
cargo check-windows
./rebuild_gpu_payload.sh
```

Expected: all four succeed. `cargo check-windows` is here because `crates/gpu-payload` is host code and the application links it.
