# syntax=docker/dockerfile:1.7@sha256:a57df69d0ea827fb7266491f2813635de6f17269be881f696fbfdf2d83dda33e
#
# The whole prepare step for a display payload. `prepare.sh` runs this and nothing else,
# so the host needs docker and no toolchain of its own -- and so the toolchain that built
# a payload is a pinned image rather than whatever the machine happened to have.
#
# One Dockerfile for all three supported releases: they differ by base image and by
# nothing else, and three copies of this file would drift.

ARG BASE
FROM ${BASE} AS toolchain
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential linux-headers-generic \
    && rm -rf /var/lib/apt/lists/*

# The build is the test. A module that does not compile against this release's headers is
# an artifact that never exists, rather than a surprise discovered inside a guest -- which
# is the whole reason this stage builds the module it is about to ship as source.
FROM toolchain AS build
COPY module /src/module
RUN set -eu; \
    kernel="$(ls /lib/modules | sort -V | tail -n1)"; \
    make -C "/lib/modules/$kernel/build" M=/src/module modules; \
    make -C "/lib/modules/$kernel/build" M=/src/module clean; \
    printf '%s' "$kernel" > /src/proven_on

FROM build AS layout
ARG VERSION
ARG COMMIT
ARG SOURCE_URL
ARG DISTRIBUTION
ARG RELEASE
ARG ARCHITECTURE
ARG PROTOCOL_MAJOR
ARG PROTOCOL_MIN_MINOR
ARG PROTOCOL_MAX_MINOR
COPY licenses /src/licenses
RUN set -eu; \
    proven_on="$(cat /src/proven_on)"; \
    mkdir -p /output/prepared/content/drm /output/prepared/content/services \
             /output/prepared/licenses; \
    cp /src/module/vmlord_drm.c /src/module/vmlord_compat.h \
       /src/module/vmlord-display.conf /src/module/vmlord-display-unbind-simpledrm.service \
       /output/prepared/content/drm/; \
    sed "s/^VMLORD_VERSION ?= .*/VMLORD_VERSION ?= ${VERSION}/" /src/module/Kbuild \
       > /output/prepared/content/drm/Kbuild; \
    sed "s/@VERSION@/${VERSION}/" /src/module/dkms.conf.in \
       > /output/prepared/content/drm/dkms.conf; \
    cp /src/licenses/GPL-2.0.txt /output/prepared/licenses/; \
    printf '{"schema_version":1,"sources":[{"url":"%s","commit":"%s","version":"%s"}]}\n' \
       "${SOURCE_URL}" "${COMMIT}" "${VERSION}" > /output/prepared/sources.json; \
    printf '{"schema_version":1,"version":"%s","target":{"distribution":"%s","release":"%s","architecture":"%s","payload_abi":1},"proven_on":"%s","protocol":{"major":%s,"min_minor":%s,"max_minor":%s},"sources":[{"url":"%s","commit":"%s","version":"%s"}],"licenses":[{"spdx":"GPL-2.0","path":"licenses/GPL-2.0.txt"}]}\n' \
       "${VERSION}" "${DISTRIBUTION}" "${RELEASE}" "${ARCHITECTURE}" "${proven_on}" \
       "${PROTOCOL_MAJOR}" "${PROTOCOL_MIN_MINOR}" "${PROTOCOL_MAX_MINOR}" \
       "${SOURCE_URL}" "${COMMIT}" "${VERSION}" > /output/recipe.json

# `content/services` leaves the build empty and stays empty until task #115 fills it.
# It is created from the first version so that adding it later is not a second manifest
# schema.
FROM scratch AS output
COPY --from=layout /output /
