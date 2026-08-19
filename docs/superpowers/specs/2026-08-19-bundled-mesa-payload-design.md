# Bundled Mesa payload design

## Goal

The guest half of `mesa_policy: bundled` is finished and unused. `bundled_mesa`
(gpu_kernel.rs:564) stages `<payload>/content/mesa` into `/opt/vmlord/wsl-mesa`, writes
`/etc/ld.so.conf.d/vmlord-wsl-mesa.conf` and runs `ldconfig`; `vulkan_stage`
(gpu_kernel.rs:612) registers whatever ICD the prefix carries; the probe reads dozen's
output correctly, fixed by a test on the real thing. What does not exist is the build
side: `builder.rs` carries `mesa_policy` through to `sources.json` and no step ever
produces `content/mesa`, so a payload declaring `bundled` is honestly rejected by the
guest.

Under `distro` the guest gets GL through the d3d12 gallium driver and lavapipe for
Vulkan, because Ubuntu builds Mesa without `microsoft-experimental`. TASK-107 proved on
the real host that a Mesa built with it gives a hardware device —
`PHYSICAL_DEVICE_TYPE_DISCRETE_GPU`, `Microsoft Direct3D12 (NVIDIA GeForce RTX 5070
Ti)`, `DRIVER_ID_MESA_DOZEN` — over `/dev/dxg` and the merged `/usr/lib/wsl/lib`. The
payoff is measured, not assumed.

The end state: one `docker build` produces the prepared tree, the payload carries a Mesa
with both d3d12 and dozen, and its provenance says what was compiled rather than
pretending binaries are source files.

## What the guest already fixes

Three constraints come from code that is not changing, and the build has to satisfy them
rather than negotiate with them.

`MESA_PREFIX` is `/opt/vmlord/wsl-mesa` (gpu_kernel.rs:52, gpu_render.rs:40). Mesa's
loader finds its own `dri/` directory by the prefix compiled into it, so the build
configures `-Dprefix=/opt/vmlord/wsl-mesa -Dlibdir=lib/x86_64-linux-gnu` and nothing else
will do.

`required_libraries` (gpu_probe.rs:226) looks for `d3d12_dri.so` **inside** the prefix
when the policy is bundled. A payload carrying only the Vulkan driver would fail the
library check, so the bundled tree carries the GL stack too.

`libvulkan.so.1` stays the distribution's, out of `/usr/lib/<triplet>`: the payload ships
an ICD, never a loader. `vulkan-tools`, which the probe installs for `vulkaninfo`, brings
it.

## Provenance for something that was built

`SourceManifest` (manifest.rs:180) requires of every source a non-empty `paths`, one
license per path, a mandatory `sha256`, and a 40-hex `commit`. That model — *these
upstream files travelled verbatim, and this digest is recomputable from a fresh
checkout* — describes `dxgkrnl` exactly and describes Mesa not at all. Binaries
correspond to no upstream file.

So the source record becomes a union tagged by `kind`, and both `recipe.json` and
`sources.json` go to `schema_version: 2`. Payloads are rebuilt, not migrated, so version
1 is refused rather than accepted.

`kind: "checkout"` is today's record unchanged, and dxgkrnl keeps it.

`kind: "built"` is Mesa's:

```json
{
  "kind": "built",
  "url": "https://gitlab.freedesktop.org/mesa/mesa",
  "commit": "<40 hex>",
  "version": "26.1.2",
  "output": "content/mesa",
  "licenses": ["MIT"],
  "inputs": [
    {
      "url": "https://github.com/microsoft/DirectX-Headers",
      "commit": "<40 hex>",
      "version": "v1.615.0"
    },
    {
      "url": "https://github.com/KhronosGroup/SPIRV-Tools",
      "commit": "<40 hex>",
      "version": "v2025.3"
    }
  ],
  "sha256": "<digest of the staged tree>"
}
```

`output` is a path **in the payload**, not upstream. Naming an upstream path here would
be the lie that made the alternative — recording binaries as if they were the source
files — not worth its convenience.

`licenses` is a list of SPDX identifiers without paths. Attributing `libgallium.so` to
one upstream file is not possible and not meaningful; the list says under which terms the
material it was compiled from stands. Each identifier must be declared in the recipe's
`licenses`, exactly as overlay licenses already must.

The commits are chosen when the recipe is written, not here: Mesa is pinned to a release
tag at or above the 26.x the guest's own Ubuntu ships, and each `inputs` entry to whatever
that tag actually builds against. The list above is illustrative — which upstreams
`microsoft-experimental` pulls in is a fact the first container build reports, and the
recipe records what was consumed rather than what we expected.

`inputs` are the other upstreams that ended up inside those binaries. They carry no
digest of their own — their bytes are not separable from the output's — but their commits
are pinned, and they reach the catalog entry as ordinary `sources` rows, where the
`url`/`commit`/`version` shape fits them without strain.

`sha256` follows the same rule as the upstream digest — for each file, sorted by its
path, the path, a NUL, its contents — computed over the files under `output`. This is the
one digest in the document the builder can **check** rather than record: the files are in
its hands. `validate_prepared_provenance` (builder.rs:385) gains that check, and drift
between what was compiled and what was recorded becomes impossible instead of unlikely.

The catalog entry format does not change. `catalog_entry` (builder.rs:346) already
projects each source down to `url`/`commit`/`version`, and `inputs` are flattened into
that list beside their parent. `schema_version: 2` of the entry, settled in TASK-109,
stands.

## The build moves into a container

`prepare.sh` today needs `bash`, `git`, `jq` and `python3` from the host and therefore
runs only under WSL. All of it moves into `payloads/ubuntu-26.04-amd64/Dockerfile`, a
multi-stage BuildKit build:

1. **`toolchain`** — `ubuntu:26.04@sha256:…` plus apt: Mesa's build dependencies, `git`,
   `jq`, `python3`. The base is pinned by digest so that glibc and libstdc++ are the
   guest's. This is the one stage where network access is legitimate.
2. **`sources`** — the pinned checkouts: Mesa, DirectX-Headers, SPIRV-Tools, and
   WSL2-Linux-Kernel for `dxgkrnl`. Commits arrive as `ARG`s, so the layer is cached by
   the pin: an unchanged pin is an unchanged layer. DirectX-Headers is placed into Mesa's
   `subprojects/` here rather than left to meson's wrap.
3. **`mesa`** — configure and compile under `RUN --network=none`. Everything that ends up
   in the binaries came from a commit recorded in the spec, and no wrap can quietly fetch
   anything else. Isolation is a property of the instruction that needs it, not of the
   whole run.
4. **`closure`** — a clean `ubuntu:26.04` with no `-dev` package in it, where the trimmed
   tree is copied and `ldd` runs over every `.so`. Any `not found` fails the build.
5. **`prepared`** — `prepare.py` lays out `content/dxgkrnl` from the checkout,
   `content/mesa` from stage 3, the overlays and license texts from the build context,
   and writes `sources.json` and `recipe.json`.

The result leaves through `docker build --output type=local,dest=<output>` as `prepared/`
and `recipe.json` in today's layout, so the `pack` invocation in the README does not
change by a character. `pack` stays outside: it is the project's Rust half, and a Rust
toolchain in the payload image would be there for no one.

`prepare.sh` becomes a wrapper of a dozen lines — parse `--output`, run `docker build`.
`--work` loses its meaning and goes: caching is the layer cache now, not our `upstream`
directory. The host needs docker and nothing else.

`prepare.py` keeps its shape. It already takes `--checkout`, `--overlays`, `--licenses`
and `--output`; it gains `--mesa <tree>` and the branch that writes the `built` record and
digests that tree.

## What the bundled Mesa is

```
-Dprefix=/opt/vmlord/wsl-mesa -Dlibdir=lib/x86_64-linux-gnu
-Dgallium-drivers=d3d12,softpipe -Dvulkan-drivers=microsoft-experimental
-Dllvm=disabled -Dglvnd=enabled -Dglvnd-vendor-name=mesa
-Dplatforms=x11,wayland -Dbuildtype=release -Db_ndebug=true
```

`glvnd` decides what we are allowed to replace. With it Mesa installs
`libGLX_mesa.so.0` and `libEGL_mesa.so.0`, and `libGL.so.1` and `libEGL.so.1` stay
libglvnd's. That matters because `bundled_mesa` writes its `ld.so.conf.d` entry
unconditionally — unlike `vmlord-gpu.sh`, which is guarded by `/dev/dxg` — so after one
run of the recipe our directory is on every process's search path for good. Replacing the
vendor implementation behind glvnd's dispatch is a change the guest can survive; replacing
the dispatch itself is not. `__GLX_VENDOR_LIBRARY_NAME=mesa`, which `environment_stage`
already exports, is written for exactly this arrangement.

`softpipe` is the same argument for the same reason. A guest whose adapter goes away on a
later start falls back to a software rasteriser instead of losing GL entirely, and
softpipe — unlike llvmpipe — needs no LLVM. `-Dllvm=disabled` then removes the question of
where a guest under `bundled` would get `libLLVM`, since no apt step under this policy
installs one. Vulkan is dozen alone: lavapipe *is* LLVM.

`meson install --strip` into a DESTDIR, then `bin/`, `include/`, `lib/*/pkgconfig`, `*.a`
and `*.la` are dropped, and every symlink is replaced by the file it points at. That last
one is not tidiness: `collect_files` (builder.rs:462) rejects a symlink in the prepared
tree outright, and Mesa installs its DRI modules as names pointing at one shared module
and its libraries as soname chains. Measured on `mesa-26.2.0`, that costs about 2 MB
across nine links — the DRI names point at a 121 KB loader shim, not at the 22 MB gallium
library, which is a real file nothing links to. `bin/` goes because `spirv2dxil` is a 12 MB
developer tool no guest runs. An installed tree of 88 MB becomes roughly 50 MB. What travels is `lib/x86_64-linux-gnu/` — `libEGL_mesa.so.0*`,
`libGLX_mesa.so.0*`, `libgbm.so.1*`, `libgallium*.so`, `dri/d3d12_dri.so`,
`dri/swrast_dri.so`, `libvulkan_dzn.so` — and `share/vulkan/icd.d/dzn_icd.x86_64.json`,
whose name `vulkan_stage` already expects.

Nothing external is needed beyond DirectX-Headers: a configure run under
`--wrap-mode=nodownload` accepts this option set with that subproject in place and asks
for nothing else, so `inputs` holds one entry and the licence list gains MIT alone. The
closure gate is what keeps that a fact rather than an intention.

## Identity, limits and licenses

The payload is replaced, not doubled. `select_for_guest` (catalog.rs:285) filters by
distribution, release and architecture and picks the highest kernel among the survivors —
two payloads with one `kernel_release` would resolve arbitrarily. So `payload_id` goes
`…-7.0.0-28-v1` → `…-7.0.0-28-v2`, `mesa_policy` becomes `bundled`,
`required_renderers` becomes `["d3d12-gallium", "dzn-vulkan"]`, and the `distro` variant
ceases to exist. Existing VMs are recreated, which this project's rules already require.

The limits need no hand. `pack` derives `expanded_size_limit` from the content it just
measured and `file_count_limit` from the files it just counted (builder.rs:206), and
`CatalogEntry::validate` (catalog.rs:121) imposes no ceiling above zero. They will grow
from 481306 and 20 to tens of megabytes and thousands of files, and they will grow
correctly. Staging hard-links from a shared content-addressed cache, so the growth costs
disk once for all VMs rather than once per VM.

Licenses arrive with the material: MIT for Mesa and MIT for DirectX-Headers, different
texts, both travelling. Both must be declared, or `license_expression_is_declared`
(manifest.rs:263) rejects the `built` record, which is the correct outcome.

## Tests

Crate tests:

* a `built` record parses and validates, and its `inputs` reach the catalog entry as
  sources;
* `pack` refuses a `built` record whose `sha256` disagrees with the tree it just staged;
* `sources.json` and `recipe.json` at `schema_version: 1` are refused as unknown;
* an undeclared SPDX identifier in a `built` record's `licenses` is refused;
* fixtures under `crates/gpu-payload/tests/fixtures/` move to schema version 2.

The closure gate inside the container is the second level: no shipped `.so` may need
anything a clean `ubuntu:26.04` does not have.

The third is the only one that decides the task. A VM on the real host with this payload
must report `RENDERS` with **both** checks — Opengl and Vulkan — `ok`, and the Vulkan
device must be named `Microsoft Direct3D12 (…)` with `DRIVER_ID_MESA_DOZEN`, not
llvmpipe. Until that run exists the task is not done, however green the tests are.

## Documentation

`payloads/ubuntu-26.04-amd64/README.md` is rewritten: the section arguing why
`mesa_policy` is `distro` is now the section describing what `bundled` carries and what it
costs, the build instructions become the single `prepare.sh` call over docker, and the
`built` record gets the same treatment the upstream digest already has — what it covers,
and what recomputing it proves.

## Out of scope

Publishing payloads anywhere but beside the executable. TASK-109 removed `archive_url`
and the network from this crate, and this design does not bring either back: the pair
`dist` writes into the release directory is still the whole distribution story. Nothing
here adds a second target either: `ubuntu-26.04-amd64` is the only payload, and
a second distribution or architecture is its own task.
