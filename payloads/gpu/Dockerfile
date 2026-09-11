# syntax=docker/dockerfile:1.7@sha256:a57df69d0ea827fb7266491f2813635de6f17269be881f696fbfdf2d83dda33e
#
# The frontend is pinned by digest for the same reason the base image below is: it is
# part of the toolchain that produces a payload, and `1.7` is a tag that upstream may
# move. The tag is kept beside the digest so a reader can see which release this is.
#
# The whole prepare step for this payload. `prepare.sh` runs this and nothing else, so
# the host needs docker and no toolchain of its own -- and so the toolchain that built a
# payload is a pinned image rather than whatever the machine happened to have.

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
        python-packaging \
        bison flex cmake libdrm libx11 libxext libxfixes libxdamage libxshmfence \
        libxxf86vm libxrandr libxcb wayland wayland-protocols expat zlib zstd libglvnd \
    && rm -rf /var/cache/pacman/pkg/*

FROM ${TOOLCHAIN} AS toolchain

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
    && mv /src/directx-headers /src/mesa/subprojects/DirectX-Headers-1.0

FROM sources AS mesa
COPY mesa/patches /src/patches
COPY mesa/build.sh /usr/local/bin/build-mesa
# The patches are applied here rather than inside build.sh so that the script stays about
# building: what the tree is, is decided before it is compiled. `git apply` refuses a
# patch that does not apply exactly, which is what makes a pin bump that upstream has
# moved under fail loudly here instead of silently shipping an unpatched Mesa.
#
# Every patch is recorded in payload.spec.json, and prepare.py digests it into the
# manifest: the binaries no longer correspond to the pinned commit alone, and a payload
# that did not say so would be claiming a provenance it does not have.
#
# No network: everything that reaches the binaries came from a pin above or from
# /src/patches, and a wrap cannot quietly add to it.
ARG LIBDIR
RUN --network=none chmod +x /usr/local/bin/build-mesa \
    && git -C /src/mesa apply --verbose /src/patches/*.patch \
    && build-mesa /src/mesa /out/mesa ${LIBDIR}

FROM base AS closure-apt
# A guest, modelled: the base image plus the display stack and nothing else.
#
# These are the libraries that talk to the X server, the Wayland compositor and the DRM
# device, so they must be the guest's own -- a payload that carried its own libX11 would
# be shipping the client half of a protocol whose server half it does not control. Mesa
# does not offer to link them statically and it would be wrong if it did.
#
# The guest installs none of this on the payload's account. Every package here is already
# a dependency of `mesa-utils` and `vulkan-tools`, the two the probe installs in
# gpu_render.rs, so anything that can ask Mesa to draw has them before it asks. They are
# named one by one rather than taken via those two packages because `mesa-utils` pulls in
# the distribution's Mesa and with it libLLVM, which would hide an LLVM dependency of
# ours instead of exposing it. Runtime packages only: a -dev package here would defeat
# the whole stage.
#
# binutils is here for `readelf`, which reads the sonames out of the tree. It is a tool
# and not a library: it cannot satisfy anything the payload links against, so it cannot
# make a dependency of ours resolve that would not have resolved without it.
RUN apt-get update && apt-get install -y --no-install-recommends \
        libdrm2 libexpat1 libx11-6 libx11-xcb1 libxext6 libxxf86vm1 \
        libxcb1 libxcb-dri3-0 libxcb-glx0 libxcb-present0 libxcb-randr0 \
        libxcb-shm0 libxcb-sync1 libxcb-xfixes0 libxshmfence1 \
        libwayland-client0 zlib1g libzstd1 libudev1 \
        binutils \
    && rm -rf /var/lib/apt/lists/*

# The same guest, modelled with the same rule on the other distribution: the runtime
# halves of the display stack, and binutils for readelf. Arch does not split -dev packages
# out, so these names are the whole library and there is nothing to leave out; what
# matters is the part that is the same on both, that no Mesa of the distribution's own is
# installed, because that would hide a dependency of ours behind one of its.
FROM base AS closure-pacman
ARG PACKAGE_SNAPSHOT
RUN printf 'Server = https://archive.archlinux.org/repos/%s/$repo/os/$arch\n' "${PACKAGE_SNAPSHOT}" \
        > /etc/pacman.d/mirrorlist \
    && pacman -Syu --noconfirm --needed \
        libdrm expat libx11 libxext libxxf86vm libxcb libxshmfence wayland zlib zstd \
        systemd-libs \
        binutils \
    && rm -rf /var/cache/pacman/pkg/*

FROM ${CLOSURE} AS closure
COPY --from=mesa /out/mesa /check/mesa
COPY mesa/closure.sh /usr/local/bin/closure
ARG LIBDIR
RUN --network=none chmod +x /usr/local/bin/closure && closure /check/mesa ${LIBDIR}

FROM sources AS prepared-sources
COPY --from=mesa /out/mesa /mesa
# The closure stage produces nothing this stage needs, but copying one file out of it is
# what makes BuildKit run it: a gate nobody depends on is a gate that gets skipped.
COPY --from=closure /check/mesa/share/vulkan/icd.d/dzn_icd.x86_64.json /closure-passed.json
# Which target is being built. The context holds every target's spec, and this is the one
# line that picks one: a build argument rather than a copy of the whole directory, so that
# two targets cannot end up reading each other's provenance.
ARG TARGET
COPY ${TARGET}/payload.spec.json ./payload.spec.json
COPY prepare.py prepare_test.py ./
COPY overlays ./overlays
COPY mesa/patches ./patches
COPY licenses ./licenses
# The layout script's digest rule has a twin in crates/gpu-payload/src/builder.rs, and a
# disagreement between them surfaces only as `pack` refusing a tree it just built. The
# golden vector that ties them together is checked here, in the one place that is
# guaranteed to run on every payload build and needs nothing the image does not have.
RUN --network=none python3 prepare_test.py
RUN --network=none python3 prepare.py \
        --spec payload.spec.json \
        --overlays overlays \
        --patches patches \
        --licenses . \
        --checkout /src/kernel \
        --mesa /mesa \
        --output /output

# Does the driver this payload ships actually build against this distribution's kernel?
# The display payload's container build answers that for its module by building it; the
# GPU payload ships sources and lets DKMS build them in the guest, so nothing answered it
# here until now. It matters most where the payload's sources and the guest's kernel come
# from different lineages: dxgkrnl is taken from a WSL kernel branch, and Arch runs
# mainline. The module built here is thrown away -- the payload still carries the sources.
FROM toolchain-pacman AS module-pacman
ARG PACKAGE_SNAPSHOT
RUN pacman -Syu --noconfirm --needed linux-headers \
    && rm -rf /var/cache/pacman/pkg/*
COPY --from=prepared-sources /output/prepared/content/dxgkrnl /src/dxgkrnl
RUN --network=none set -eu; \
    headers="$(find /usr/lib/modules -maxdepth 2 -name build -type d | head -n1)"; \
    [ -n "$headers" ] || { echo "no kernel headers in the image" >&2; exit 1; }; \
    make -C "$headers" M=/src/dxgkrnl modules; \
    echo "dxgkrnl builds against $(basename "$(dirname "$headers")")" > /gate.txt

# A target whose distribution has no gate stage yet -- today that is Ubuntu, whose proof
# that this module builds is the manual run its README records. The file is what the
# stage below copies, so the two shapes have to agree on producing one.
FROM base AS module-none
RUN echo "no module gate for this target" > /gate.txt

FROM ${MODULE} AS module

FROM prepared-sources AS prepared
# A gate nothing depends on is a gate BuildKit skips, which is why one file is copied out
# of it. The file is not part of the payload: `output` below takes /output alone.
COPY --from=module /gate.txt /module-passed.txt

FROM scratch AS output
COPY --from=prepared /output /
