# Display payload

What a guest needs for VMLord's display that its own apt cannot provide, and
the provenance that says where it came from. The archive is not in this
repository -- these files are what produces one.

## Building

```sh
payloads/display/prepare.sh \
    --spec   payloads/display/ubuntu-24.04-amd64/payload.spec.json \
    --output target/display-payload
cargo run -p xtask -- display-payload pack \
    --recipe        target/display-payload/recipe.json \
    --input         target/display-payload/prepared \
    --archive       target/display-payload/payload.zip \
    --catalog-entry target/display-payload/catalog-entry.json
cargo dist --display-payload target/display-payload
```

One `Dockerfile` and one `prepare.sh` serve all three supported releases: they
differ by base image and by nothing else, and three copies would drift. Which
release is built is the `--spec` that is passed.

**The container build is the test.** It installs that release's
`linux-headers-generic` and builds `vmlord_drm` against them, so a module that
does not compile for 22.04, 24.04 or 26.04 is an artifact that never exists
rather than a failure discovered inside a guest. `proven_on` in the recipe is
whatever kernel those headers resolved to -- read inside the image, because
that is the only place it is known.

## What the tree holds

```
prepared/payload.json     written by `pack`, not by the build
prepared/sources.json     this repository at the commit that was built
prepared/licenses/        GPL-2.0, the module's licence
prepared/content/drm/     dkms.conf, Kbuild, the sources, modprobe.d, the unit
prepared/content/services/  empty until task #115
```

## Provenance

A display payload has no upstream to pin: it is built from this repository, so
what `sources.json` records is this repository and the commit it was built at.
`prepare.sh` refuses to build from a tree with uncommitted changes under
`payloads/display`, because a commit that does not describe what was built is
worse than no build at all.

## Open

The base images are pinned by tag rather than by digest, unlike the GPU
payload's. A release build should pin the digest -- resolve it once and put it
in the release's `payload.spec.json` as `ubuntu@sha256:...`, which `prepare.sh`
passes through unchanged.
