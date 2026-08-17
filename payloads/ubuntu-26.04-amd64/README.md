# GPU payload: Ubuntu 26.04 amd64

What a guest needs to build `dxgkrnl` for itself, and the provenance that says
where every byte of it came from. The archive is not in this repository —
these files are what produces one.

## Building

```sh
payloads/ubuntu-26.04-amd64/prepare.sh --output target/gpu-payload
cargo run -p xtask -- gpu-payload pack \
    --recipe        target/gpu-payload/recipe.json \
    --input         target/gpu-payload/prepared \
    --archive       target/gpu-payload/payload.zip \
    --catalog-entry target/gpu-payload/catalog-entry.json
```

`prepare.sh` fetches the pinned upstream commit (cached under
`<output>/upstream`, so a second run costs nothing), lays the tree out, and
writes `recipe.json` and `prepared/sources.json`. `pack` writes the archive and
the catalog entry that describes it.

Commit before building. `vmlord_revision` is this repository's `HEAD`, and a
dirty tree means the revision in the payload describes something other than
what was packed; `prepare.sh` warns and continues rather than deciding for you.

## What the spec holds

`payload.spec.json` is the single source of truth. The builder reads provenance
twice — from the recipe, and from `sources.json` inside the tree — and refuses
the pair unless they agree field for field, so both are generated from the spec
rather than maintained side by side.

The upstream `sha256` covers exactly the files this payload takes, not the
whole tree the commit names: each selected file, sorted by its upstream path,
contributed as path, NUL, contents. Recomputable from a fresh checkout of the
same commit, which is what makes it worth recording next to the commit itself.

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

## The `distro` Mesa policy, and what it costs

`mesa_policy` is `distro`, so no Mesa is in the archive and the guest installs
Ubuntu's. Ubuntu's Mesa is not built with `microsoft-experimental`, so the
guest gets GL through the d3d12 gallium driver and lavapipe for Vulkan. That is
why `required_renderers` claims `d3d12-gallium` alone: claiming `dzn-vulkan`
under this policy would be a false statement in the provenance.

Switching to `bundled` means building Mesa for the guest, shipping it under
`content/mesa`, and adding `dzn-vulkan` back.

## Before this can be published

`archive_url` points at `payloads.vmlord.invalid`. The catalog requires an
immutable HTTPS URL, and there is nowhere to publish yet, so the placeholder is
deliberate and the embedded `catalog.json` stays empty. Publishing means
hosting the archive, putting its real URL in the spec, rebuilding, and pasting
`catalog-entry.json` into `crates/gpu-payload/catalog/catalog.json`.

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
