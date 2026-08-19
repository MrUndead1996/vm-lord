# GPU payload: Ubuntu 26.04 amd64

What a guest needs to build `dxgkrnl` for itself and to draw through it once it
has, and the provenance that says where every byte of it came from. The archive
is not in this repository — these files are what produces one.

## Building

```sh
payloads/ubuntu-26.04-amd64/prepare.sh --output target/gpu-payload
cargo run -p xtask -- gpu-payload pack \
    --recipe        target/gpu-payload/recipe.json \
    --input         target/gpu-payload/prepared \
    --archive       target/gpu-payload/payload.zip \
    --catalog-entry target/gpu-payload/catalog-entry.json
```

The host needs docker and nothing else. Not `jq`, not `python3`, not a `git`
new enough for partial clones: `prepare.sh` is a wrapper over one `docker build`
of the `Dockerfile` beside it, and everything that used to run on the host —
the pinned checkouts, the Mesa build, the closure check and the layout — now
runs in that image. The toolchain that produced a payload is therefore a base
image pinned by digest rather than whatever the machine happened to have, and
the same command produces the same tree on a machine that has never built one.

The commits still come from `payload.spec.json`. `prepare.sh` reads them with
the image's own `jq` and passes them back in as build arguments, so each
checkout is a layer keyed by its own pin: an unchanged pin is an unchanged
layer. Which pair of arguments carries which upstream is an explicit table in
the script rather than a name computed from the URL, because a computed name
has no way to be wrong loudly — an `ARG` no `ARG` declares is simply unset, and
a checkout of nothing is discovered much later if at all. The table is checked
against the `ARG` names in the Dockerfile in both directions before the build
starts: a pin the script has no arguments for, and an argument the spec pins
nothing for, each fail with the file and the name to fix.

Two more spec shapes are refused there, because neither is one the
bidirectional check can see — in both, every name is declared and every name is
supplied. A row with an empty `commit` would reach docker as `--build-arg
NAME=` and fetch nothing, failing much later and deep inside the image. And two
rows naming one URL would both resolve to the one pair of arguments that URL
has, so the second would silently win: the build would check out one commit
while `sources.json` and the catalog entry, generated from the spec, attested
to two.

`prepare.sh` clears `<output>/prepared` and `<output>/recipe.json` before it
builds, and exactly those two paths. BuildKit's `type=local` exporter merges
into the destination rather than replacing it, so a file left over from an
older pin, or half a tree from an export that was interrupted, would otherwise
survive the run and be packed as if this build had produced it. The directory
around them is left alone, because `pack` writes `payload.zip` and
`catalog-entry.json` into that same directory and emptying it would turn the
second command above into a surprise.

The two outputs travel together. `target/gpu-payload` is what `cargo dist`
wants:

```sh
cargo dist --gpu-payload target/gpu-payload
```

`dist` re-reads `catalog-entry.json` through the crate's own validation, hashes
`payload.zip` against the digest that entry claims, and copies **both** files
beside `vmlord.exe` — `gpu-payload/<payload_id>.json` and
`gpu-payload/<payload_id>.zip`. That pair is the catalog: the application has
none compiled into it and assembles one from that directory at startup. Without
the argument `dist` builds a release with no payload and says so, and such a
release simply starts VMs without GPU support.

## What the spec holds

`payload.spec.json` is the single source of truth. The builder reads provenance
twice — from the recipe, and from `sources.json` inside the tree — and refuses
the pair unless they agree field for field, so both are generated from the spec
rather than maintained side by side.

Two schema versions live in `prepare.py` and they are separate on purpose.
`SPEC_SCHEMA_VERSION` versions `payload.spec.json`, the file this repository
edits by hand; `DOCUMENT_SCHEMA_VERSION` versions the wire format the Rust
packer reads out of `recipe.json` and `prepared/sources.json`, which literals
in `builder.rs` and `manifest.rs` pin from the other side without ever reading
the spec. They are both `2` today by coincidence and not by rule.

The upstream `sha256` covers exactly the files this payload takes, not the
whole tree the commit names: each selected file, sorted by its upstream path,
contributed as path, NUL, contents. Recomputable from a fresh checkout of the
same commit, which is what makes it worth recording next to the commit itself.

The `built` record — the Mesa one — answers the same questions for a tree that
was compiled rather than copied, and every field means something slightly
different for it. `output` is a path **in the payload**, `content/mesa`, and
never an upstream one: naming an upstream path would be the lie that made the
alternative, recording binaries as though they were source files, not worth its
convenience. `licenses` is a list of bare SPDX identifiers with no paths,
because attributing a shared object to one upstream file is neither possible
nor meaningful; the identifiers say under which terms the material it was
compiled from stands, and each must be declared in the recipe's own `licenses`
exactly as an overlay's licence must. `inputs` are the other upstreams that
ended up inside those binaries — here DirectX-Headers alone, at `v1.619.1`,
which is the revision Mesa's own wrap names. They carry no digest, because
their bytes are not separable from the output's, but their commits are pinned
and they reach the catalog entry as ordinary source rows. And the `sha256`,
computed by the same rule as the upstream one but over the files under
`output`, is the one digest in the document the builder **verifies** rather
than records: those files are in its hands when it packs them, so drift between
what was compiled and what was written down is impossible rather than unlikely.

That last one is a rule written twice — `tree_digest` in `prepare.py` and
`built_output_digest` in `crates/gpu-payload/src/builder.rs` — and the two
disagreeing would show up only as `pack` refusing a tree it had just built,
naming neither the rule nor the file. A golden vector ties them: one small tree
whose members (`lib-extra`, `lib.conf`, `lib/dri.so`) sort one way as joined
strings and the other way as path components, and one expected digest asserted
from both sides. The Rust half is a unit test in `builder.rs`; the Python half
is `prepare_test.py`, which the Dockerfile runs in the same stage that runs
`prepare.py`, so every payload build executes it and no host needs a test
framework — or a `python3` — to get the check.

Overlay digests are measured from the files that were just copied. Editing an
overlay changes them on the next run; nothing needs transcribing.

## What travels, and what does not

`content/dxgkrnl/` carries the upstream driver sources and `d3dkmthk.h`
verbatim, plus three files of ours:

* `Kbuild` — the driver builds in tree upstream, where Kconfig picks the
  objects and the uapi header sits on the kernel's include path. Out of tree
  neither holds.
* `dxgkrnl_compat.h` — Ubuntu's `<linux/hyperv.h>` does not declare the two GPU
  paravirtualization channel GUIDs, because the driver was never merged
  upstream. They are copied from the same pinned commit and force-included, so
  the driver sources stay byte-for-byte upstream. As of Ubuntu 26.04's
  `7.0.0-28-generic` this is the *only* compatibility shim the module needs.
* `dkms.conf` — the registration the guest builds through.

`dkms` and `build-essential` do not travel: the guest installs them from its
own apt, as the kernel recipe design lays out.

## The `bundled` Mesa policy, and what it costs

`mesa_policy` is `bundled`, so `content/mesa` carries a Mesa built from the
`mesa-26.2.0` tag with `-Dgallium-drivers=d3d12,softpipe`,
`-Dvulkan-drivers=microsoft-experimental` and `-Dllvm=disabled`. What that
leaves in the tree is `dri/`, where the `libdril_dri.so` loader shim answers to
`d3d12_dri.so` for the hardware path and to `swrast_dri.so` and
`kms_swrast_dri.so` for softpipe; `libgallium-26.2.0.so` behind all of them;
`libvulkan_dzn.so` with its ICD document at
`share/vulkan/icd.d/dzn_icd.x86_64.json`; the glvnd vendor libraries
`libGLX_mesa.so.0` and `libEGL_mesa.so.0`; `libgbm.so.1` with its
`dri_gbm.so`; and the `drirc.d` defaults Mesa ships for those drivers. That is
why `required_renderers` names both `d3d12-gallium` and `dzn-vulkan`: under
this policy the guest has a hardware path for each API rather than the
distribution's lavapipe for Vulkan.

The prefix is not a parameter. Mesa's loader finds its own `dri/` directory by
the prefix compiled into it, so a tree built for one prefix and staged at
another would not find its own drivers, and `bundled_mesa` in
`crates/agent/src/gpu_kernel.rs` stages this tree at exactly
`/opt/vmlord/wsl-mesa`. `build.sh` passes that same
path to meson and refuses to continue if meson installed elsewhere. Changing
one of the two without the other is not a configuration choice; it is a break.

`-Dglvnd=enabled` with `-Dglvnd-vendor-name=mesa` decides what we are allowed
to replace, and the answer is the vendor implementation and never the dispatch.
Mesa installs `libGLX_mesa.so.0` and `libEGL_mesa.so.0`; `libGL.so.1` and
`libEGL.so.1` stay libglvnd's, and are not in the payload at all. That matters
because `bundled_mesa` writes its `/etc/ld.so.conf.d/vmlord-wsl-mesa.conf`
entry unguarded — unlike `vmlord-gpu.sh` and the environment generator, which
probe `/dev/dxg` on every start — so after one run of the recipe our directory
is on every process's library search path for good, including on a boot where
the adapter is gone. A guest can survive Mesa's vendor library being ours. A
guest whose `libGL.so.1` is ours is a guest where every GL program on the
system now depends on this payload having been built correctly.

`softpipe` is there for the same reason read from the other end: a guest whose
adapter goes away on a later start still draws. It falls back to a software
rasteriser instead of losing GL entirely, and softpipe — unlike llvmpipe —
needs no LLVM, which is what keeps `libLLVM` out of what the payload carries.
That does not keep LLVM out of the guest: `tools_check` installs `mesa-utils`
for `eglinfo`, and that pulls the distribution's Mesa and libLLVM in with it.
The payload simply no longer depends on which of those arrived. Vulkan is dozen
alone, because lavapipe *is* LLVM.

`closure.sh` runs in a stage that is a clean Ubuntu with the display-stack
runtime libraries and no `-dev` package, which is the guest under this policy,
and every shared object in the tree must resolve there. Beyond that it holds
the tree's `NEEDED` sonames against an allow-list — the C and C++ runtimes,
zlib and zstd, and the client halves of X11, XCB, Wayland and DRM that
`mesa-utils` and `vulkan-tools` already bring in. An allow-list and not a list
of names to reject, because the failure worth catching is a dependency nobody
looked at: a name it prints is not necessarily wrong, it is unreviewed, and the
fix is to decide whether a guest really has it and then either add it in both
places or build without it.

Three things are dropped after `meson install --strip`. `bin/`, `include/`,
`pkgconfig/`, `*.a` and `*.la` go because they are for building against this
Mesa, which nothing in a guest does. So do the unversioned `libEGL_mesa.so`,
`libGLX_mesa.so` and `libgbm.so`, for the same reason read one level down:
those are the linker names, upstream ships them as symlinks, and `cp -rL` would
otherwise turn each into a full second copy of the library it points at —
905,408 bytes of one. They go by rule rather than by name — an unversioned
`.so` that has a versioned sibling is a development alias — which is what
leaves `libvulkan_dzn.so`, `dri/*.so` and `gbm/dri_gbm.so` alone: those are
runtime modules that carry no version suffix and so have no sibling. And
`lib/x86_64-linux-gnu/libspirv_to_dxil.so` goes for a reason particular to it:
dozen links the SPIR-V to DXIL translator into `libvulkan_dzn.so`, no shipped
object lists the shared form as `NEEDED` and nothing names it for a `dlopen` —
the only occurrence of the string in the tree is the file's own `SONAME` — and
it was 12 MB of what would otherwise have travelled.

The payload holds no symbolic links, because `collect_files` in the builder
rejects one outright rather than resolving it, so `build.sh` copies the staged
tree with `cp -rL` and every link arrives as the file it pointed at. Measured
on `mesa-26.2.0` the whole payload is 38,289,686 bytes across 40 files, with
`expanded_size_limit` at 38,295,400 — the tree plus the 5,714-byte manifest the
builder generates — and `file_count_limit` at 40; the archive is 9,017,460
bytes. The previous, `distro` payload's limits were 481,306 and 20. The cost of
the rule is smaller than it looks: `d3d12_dri.so`, `swrast_dri.so` and
`kms_swrast_dri.so` are each a full copy of the 121,136-byte `libdril_dri.so`
loader shim, and the `.so.0`/`.so.0.0.0` pairs for the two glvnd vendor
libraries and for libgbm cost their sizes a second time. The 22 MB
`libgallium-26.2.0.so` is a real file that nothing links to, so it travels
once — an earlier draft of the design feared a copy of it per gallium driver,
and that was wrong.

One gap is known and is not a design. The payload ships
`share/glvnd/egl_vendor.d/50_mesa.json`, and nothing in the guest registers it
the way `vulkan_stage` symlinks the dozen ICD into `/etc/vulkan/icd.d`. The EGL
path therefore does not run on the payload's copy of that file at all: it works
because the distribution has its own `50_mesa.json`, which `tools_check` brings
in with `mesa-utils`, and because that file names `libEGL_mesa.so.0` by soname
rather than by path, so the linker resolves it to ours. And the linker resolves
it to ours only because `vmlord-wsl-mesa.conf` sorts before
`x86_64-linux-gnu.conf` in `/etc/ld.so.conf.d`. Three things have to hold that
nobody asserts anywhere, and whether they do is one of the questions the
real-host run answers.

## Proven on

`7.0.0-28-generic`, by hand: `dkms add`, `build` and `install`, `modprobe`, and
`/dev/dxg`. `kernel_release` records the kernel the payload was proven on and
does not gate the guest — DKMS builds against whatever kernel is running, and
`AUTOINSTALL` carries the module across Ubuntu's own upgrades.

A VM with no GPU-PV adapter assigned still gets `/dev/dxg` — `dxgkrnl`
registers its misc device whether or not a vGPU vmbus channel exists — but
opening it fails with `EBADF`, because there is no global channel to make a
`dxgprocess` on. Opening the device, which the recipe's `DEVICE` stage does, is
what separates the two.

The bundled Mesa half has no such line yet. The tree builds, the closure gate
passes in the image, and the payload packs; what has **not** been done is a
release on a Windows host with a GPU-PV adapter, a VM started from it, and the
agent's probe read back. Until that run reports `RENDERS` with both the OpenGL
and the Vulkan check `ok` and a Vulkan device named `Microsoft Direct3D12 (…)`
rather than llvmpipe, this section says nothing about whether the guest draws.
