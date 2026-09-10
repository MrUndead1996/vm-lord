# GPU payload for Arch Linux

A second GPU payload target, `arch-rolling-amd64`, built the way the Ubuntu one
is and shipped beside it. Task #197.

## Why a second payload rather than a wider first one

The display payload serves every guest on one architecture: what it carries --
DKMS sources, static musl binaries, units -- knows nothing about a package
manager, so its `target` is provenance rather than a condition. The GPU payload
cannot do that, and ARCHITECTURE.md ("Display: the guest payload") already says
why: a `mesa_policy: bundled` payload carries Mesa compiled against its base
image's glibc, so its distribution and release stay hard conditions.
`PayloadCatalog::select_for_guest` matches distribution, release and
architecture exactly, and an Arch guest today gets `NoPayloadForGuest` and a VM
that starts without GPU support.

The guest half needs nothing: `guest_platform` reads `ID=arch` with
`BUILD_ID=rolling`, `guest_packages` names `dkms`, `base-devel` and
`linux-headers` for pacman, and no GPU recipe writes `apt` anywhere. What is
missing is a payload built for Arch.

## What moves: `payloads/gpu/`

`payloads/ubuntu-26.04-amd64/` becomes `payloads/gpu/`, laid out as the display
payload is: one `Dockerfile`, `prepare.sh`, `prepare.py`, `prepare_test.py`,
`mesa/` (`build.sh`, `closure.sh`, `patches/`), `overlays/` and `licenses/`
shared, and one directory per target holding nothing but its spec:

```
payloads/gpu/ubuntu-26.04-amd64/payload.spec.json
payloads/gpu/arch-rolling-amd64/payload.spec.json
```

Which target is built is the `--spec` that is passed, exactly as for display.
Two copies of a Dockerfile would drift, and the two targets differ in their base
image, their package manager and their library directory -- not in what a
payload is.

The move is its own commit, content unchanged, so that the Arch work reads as a
diff rather than as a rewrite.

`prepare.py` does not change: it already knows nothing about a distribution.

## What becomes a parameter

`prepare.sh` already reads the pinned commits out of the spec and passes them in
as build arguments, checking its table against the `ARG` names in the Dockerfile
in both directions. Two more values join them, read from a new `build` object in
the spec:

* `base_image` -- the base image, pinned by digest.
* `libdir` -- the library directory inside the payload, relative to its prefix.

The same bidirectional check covers them, so a spec that names neither, or a
Dockerfile that declares neither, fails before the build starts.

## The Arch spec

```
payload_id  arch-rolling-amd64-<kernel>-v1
target      distribution "arch", release "rolling", architecture "amd64",
            kernel_release <the build image's kernel>, payload_abi 1
```

Sources, patches, licences and overlays are the Ubuntu target's: the same pinned
WSL2 kernel commit for `dxgkrnl`, the same Mesa commit with the same two
patches. `mesa_policy` is `bundled`, `required_renderers` is unchanged, and
`guest_capabilities` declares `compositor-scanout` -- the patch is the same, so
the promise is the same.

`kernel_release` stays what it is for Ubuntu: a record of what a build was proven
on, not a condition. `select_for_guest` ignores the kernel and takes the entry
with the highest one, so repacking Arch against a newer kernel adds an entry that
wins over the old one rather than replacing it by hand.

### Pinning a rolling distribution

`archlinux@sha256:...` pins a rootfs and nothing else: `pacman -Sy` inside it
fetches whatever the mirrors hold today, so the toolchain that compiled a payload
would not be recoverable from this repository. The Arch build therefore points
`pacman.conf` at `https://archive.archlinux.org/repos/YYYY/MM/DD/`, and that date
lives in the spec beside the image digest. It is a pin like any other pin here,
and "the same command produces the same tree" stays true of the Arch target as
the GPU payload README claims it of the Ubuntu one.

## `library_layout`: what the payload says about itself

`bundled_mesa` (`crates/agent/src/gpu_kernel.rs:495`) writes
`/etc/ld.so.conf.d/vmlord-wsl-mesa.conf` from a `LibraryLayout` derived from the
*guest* -- `Multiarch("x86_64-linux-gnu")` where `/usr/lib/x86_64-linux-gnu`
exists, `Flat` otherwise -- while the *build* decides the tree's real shape with
meson's `-Dlibdir`, hard-coded today as `lib/x86_64-linux-gnu`. Two sides answer
one question independently. On Arch they answer it differently: no multiarch
directory exists, so the guest would point the linker at
`/opt/vmlord/wsl-mesa/lib` while an Ubuntu-shaped tree put its libraries in
`lib/x86_64-linux-gnu`. The failure is silent -- Mesa is staged, the linker never
finds it, and the guest falls back to software rendering.

So the payload states its own layout, by the route `guest_capabilities` already
takes:

* a `library_layout` field in `payload.spec.json` -- `"multiarch:<triplet>"` or
  `"flat"`;
* `prepare.py` writes it into both `recipe.json` and `prepared/sources.json`;
* `builder.rs` compares the two field for field, where it already compares
  `guest_capabilities`, and refuses a pair that disagrees;
* `gpu_kernel.rs` reads it from `sources.json` where it already reads
  `mesa_policy` (`gpu_kernel.rs:423`) and hands it to `bundled_mesa` in place of
  the guest-derived layout;
* the same field is what `prepare.sh` passes as the build's `libdir`, so one
  statement decides both the tree and the linker.

The field is optional, for the reason `guest_capabilities` is optional: a payload
that omits it promises nothing and gets today's behaviour, the guest-derived
layout. `SPEC_SCHEMA_VERSION`, `DOCUMENT_SCHEMA_VERSION` and
`ENTRY_SCHEMA_VERSION` all stay at `2`; an optional field is not a wire break.

The Ubuntu spec declares `multiarch:x86_64-linux-gnu`, the Arch spec declares
`flat`.

The guest's own `LibraryLayout` keeps its job: it describes the guest's
directories, which is what `gpu_probe` and the environment generator ask about.
Only the staged payload's own tree now comes from the payload.

## A compile gate for `dxgkrnl`

The display payload's container build *is* the proof its module compiles for a
release. The GPU payload has no such gate: it ships sources and DKMS builds them
in the guest, and the Ubuntu target's proof is a line in its README recording one
manual run on `7.0.0-28-generic`. Arch runs a mainline kernel while the sources
come from `linux-msft-wsl-6.18.y`, and `dxgkrnl_compat.h` was written against
Ubuntu's `<linux/hyperv.h>`. Without a gate, "does not build on Arch" is
discovered inside a guest, as a black screen.

A `module` stage therefore installs `linux-headers` from pacman, builds
`content/dxgkrnl` against them with our `Kbuild` and `dxgkrnl_compat.h`, and
throws the result away -- the payload still ships sources. Like the `closure`
stage it is pulled into the build by copying one file out of it, because a gate
nothing depends on is a gate BuildKit skips.

If that stage shows the compat header is not enough for a mainline kernel, the
fix is an addition to the overlay, shared by both targets, and it is part of this
work rather than a surprise inside it.

## Closure, per distribution

`closure.sh` stays one file: its allow-list of external sonames is the display
stack's, and those names are the same on both distributions. What differs is the
`ld.so.conf.d` line it writes, which follows `libdir`, and the packages the
closure *stage* installs. The Dockerfile therefore carries two closure stages
selected by an `ARG` -- apt names for Ubuntu, pacman names for Arch -- each
installing that distribution's runtime halves of the display stack and its
`readelf`, and no `-dev` package in either.

## Release

`cargo dist` already accepts `--gpu-payload` more than once
(`crates/xtask/src/dist_arguments.rs`), and `PayloadCatalog::from_release_directory`
assembles a catalog from every `*.json` in the directory. Two payloads beside
each other are an ordinary release, and nothing in the release tooling changes.

`rebuild_gpu_payload.sh`, modelled on `rebuild_payload.sh`, builds both targets
and packs **both**. That is where it differs from the display script, which packs
one of three: the display trees differ in `recipe.json` alone, while these two
are different binaries built against different C libraries.

## Documentation

* ARCHITECTURE.md, "GPU: guest payload": that there are two targets and what
  makes them two -- glibc and the library directory -- and `library_layout` as a
  payload's statement about itself.
* `payloads/gpu/README.md`: the existing README, with what is per-target said per
  target.

## Testing

Automatic, in the container, on every build of either target: the Mesa closure
gate, the new `dxgkrnl` compile gate, and `prepare_test.py`'s golden digest
vector.

Rust tests:

* `builder.rs` -- a `library_layout` that differs between `recipe.json` and
  `sources.json` is refused, as a differing `guest_capabilities` already is.
* `catalog.rs` -- an Arch guest selects the Arch entry and an Ubuntu guest the
  Ubuntu one; two GPU entries in one release directory are a valid catalog.
* `gpu_kernel.rs` -- a declared layout decides where the linker is pointed; an
  absent one leaves today's guest-derived behaviour exactly as it is.

Not covered by anything in this repository, and recorded as such: a release on a
Windows host with a GPU-PV adapter, an Arch guest started from it, and the
agent's probe reporting `RENDERS` with a Vulkan device named
`Microsoft Direct3D12 (...)` rather than llvmpipe. The Ubuntu target has no such
line for its bundled Mesa either; the Arch target must not claim one.
