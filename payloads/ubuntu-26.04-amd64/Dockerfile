# syntax=docker/dockerfile:1.7
#
# The whole prepare step for this payload. `prepare.sh` runs this and nothing else, so
# the host needs docker and no toolchain of its own -- and so the toolchain that built a
# payload is a pinned image rather than whatever the machine happened to have.

ARG BASE=ubuntu@sha256:6df9e8dd1eac389ebfef692c9648449adeb815d01e16e29cd6f3e50fe64ba9a6

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
    && mv /src/directx-headers /src/mesa/subprojects/DirectX-Headers-1.0

FROM sources AS mesa
COPY mesa/build.sh /usr/local/bin/build-mesa
# No network: everything that reaches the binaries came from a pin above, and a wrap
# cannot quietly add to it.
RUN --network=none chmod +x /usr/local/bin/build-mesa \
    && build-mesa /src/mesa /out/mesa

FROM ${BASE} AS closure
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
        libwayland-client0 zlib1g libzstd1 \
        binutils \
    && rm -rf /var/lib/apt/lists/*
COPY --from=mesa /out/mesa /check/mesa
COPY mesa/closure.sh /usr/local/bin/closure
RUN --network=none chmod +x /usr/local/bin/closure && closure /check/mesa

FROM sources AS prepared
COPY --from=mesa /out/mesa /mesa
# The closure stage produces nothing this stage needs, but copying one file out of it is
# what makes BuildKit run it: a gate nobody depends on is a gate that gets skipped.
COPY --from=closure /check/mesa/share/vulkan/icd.d/dzn_icd.x86_64.json /closure-passed.json
COPY payload.spec.json prepare.py prepare_test.py ./
COPY overlays ./overlays
COPY licenses ./licenses
# The layout script's digest rule has a twin in crates/gpu-payload/src/builder.rs, and a
# disagreement between them surfaces only as `pack` refusing a tree it just built. The
# golden vector that ties them together is checked here, in the one place that is
# guaranteed to run on every payload build and needs nothing the image does not have.
RUN --network=none python3 prepare_test.py
RUN --network=none python3 prepare.py \
        --spec payload.spec.json \
        --overlays overlays \
        --licenses . \
        --checkout /src/kernel \
        --mesa /mesa \
        --output /output

FROM scratch AS output
COPY --from=prepared /output /
