#!/usr/bin/env bash
# Configures, compiles and trims the Mesa a bundled payload carries.
#
# The prefix is not a parameter: Mesa's loader finds its own dri/ directory by the
# prefix compiled into it, and the guest stages this tree at exactly /opt/vmlord/wsl-mesa.
#
# Run with no network. Every source is already in place; a wrap that reaches out is a
# source nobody recorded.

set -euo pipefail

source="${1:?usage: build.sh <source> <destination>}"
destination="${2:?usage: build.sh <source> <destination>}"

meson setup "$source/build" "$source" \
	--wrap-mode=nodownload \
	-Dprefix=/opt/vmlord/wsl-mesa \
	-Dlibdir=lib/x86_64-linux-gnu \
	-Dgallium-drivers=d3d12,softpipe \
	-Dvulkan-drivers=microsoft-experimental \
	-Dllvm=disabled \
	-Dglvnd=enabled \
	-Dglvnd-vendor-name=mesa \
	-Dplatforms=x11,wayland \
	-Dbuildtype=release \
	-Db_ndebug=true

meson compile -C "$source/build"
DESTDIR="$source/install" meson install -C "$source/build" --strip

staged="$source/install/opt/vmlord/wsl-mesa"
[ -d "$staged" ] || {
	echo "meson installed nothing at $staged" >&2
	exit 1
}

# Headers, pkg-config files and static archives are for building against this Mesa,
# which nothing in a guest ever does, and bin/ holds spirv2dxil -- twelve megabytes of
# developer tool no guest runs.
rm -rf "$staged/bin" "$staged/include" "$staged/lib/x86_64-linux-gnu/pkgconfig"
find "$staged" -name '*.a' -delete
find "$staged" -name '*.la' -delete

# Every member arrives as a plain file: the payload builder rejects a symlink outright.
# Measured on mesa-26.2.0 that costs about 2 MB across nine links -- the DRI names point
# at a 121 KB loader shim, and the 22 MB gallium library is a real file nothing links to.
mkdir -p "$destination"
cp -rL "$staged/." "$destination/"

icd="$destination/share/vulkan/icd.d/dzn_icd.x86_64.json"
[ -f "$icd" ] || {
	echo "the dozen ICD is not at $icd, which is the only name the guest registers" >&2
	exit 1
}
[ -f "$destination/lib/x86_64-linux-gnu/dri/d3d12_dri.so" ] || {
	echo "the d3d12 gallium driver is missing, and the probe looks for it by that path" >&2
	exit 1
}
