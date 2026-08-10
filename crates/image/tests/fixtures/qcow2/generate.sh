#!/bin/sh
# Regenerates the qcow2 fixtures next to this script.
#
# They are committed because they weigh kilobytes, and because a test that
# builds its own input with the code under test proves nothing: these files come
# out of qemu-img, the program that writes every real cloud image.
#
# The guest content is a pattern the tests recompute rather than store -- see
# `pattern` in tests/support/qcow2.rs, which must agree with the loop below:
# every 512-byte sector is filled with ((n * 13 + 7) % 256), except the sectors
# of the 4096-byte clusters 0, 4 and 5, which are left zero so that qemu-img
# leaves those clusters unallocated. Cluster 0 is a hole on purpose -- it is the
# case a reader is most likely to get wrong.
#
# The virtual size is one sector past 64 KiB so that the last cluster is only
# partly inside the disk, which is what catches a reader that rounds the end of
# the disk up to a cluster boundary.
#
# Cluster sizes differ between the two content fixtures on purpose: real cloud
# images use 64 KiB, and a reader that has the number wired in rather than read
# from the header passes one fixture and fails the other.
set -eu
cd "$(dirname "$0")"

python3 - <<'PY'
SECTOR = 512
CLUSTER = 4096
SECTORS = (64 * 1024 + SECTOR) // SECTOR
HOLES = {0, 4, 5}

with open("reference.raw", "wb") as raw:
    for n in range(SECTORS):
        if n * SECTOR // CLUSTER in HOLES:
            raw.write(bytes(SECTOR))
        else:
            raw.write(bytes([(n * 13 + 7) % 256]) * SECTOR)
PY

qemu-img convert -f raw -O qcow2 -o cluster_size=4096 reference.raw sparse.qcow2
qemu-img convert -f raw -O qcow2 -o cluster_size=8192 -c reference.raw compressed.qcow2

# zstd clusters are the one supported use of an incompatible feature bit, so the
# fixture is what proves the header check lets that bit through.
qemu-img convert -f raw -O qcow2 -o cluster_size=8192,compression_type=zstd -c \
    reference.raw compressed-zstd.qcow2

# An overlay is rejected before anything is read, so neither file needs content;
# the base exists only because qemu-img refuses to reference one that is missing.
qemu-img create -f qcow2 -o cluster_size=4096 backing-base.qcow2 66048 >/dev/null
qemu-img create -f qcow2 -o cluster_size=4096 -F qcow2 -b backing-base.qcow2 \
    backing-child.qcow2 >/dev/null

# The legacy format, which this reader refuses: no feature bits, no way to know
# what an image claiming to be v1 really contains.
qemu-img create -f qcow legacy-v1.qcow 66048 >/dev/null

rm reference.raw
ls -l *.qcow2 *.qcow
