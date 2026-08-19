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

# The unversioned `libfoo.so` names are the same thing the headers and the pkg-config
# files are: they exist so something can be linked against this Mesa. Nothing in a guest
# loads a library by one -- a program loads `libEGL_mesa.so.0` by its soname, or a driver
# module by a path its loader computes -- and upstream ships them as symlinks, so `cp -rL`
# below would turn each into a full second copy of the library it points at.
#
# Dropped by rule and not by name: an unversioned `.so` with a versioned sibling is a
# linker name, because the versioned sibling is what anything real refers to. That is
# what leaves `libvulkan_dzn.so`, `dri/*.so` and `gbm/dri_gbm.so` alone. They are runtime
# modules that carry no version suffix at all, so they have no sibling to be the
# development alias of.
while IFS= read -r -d '' library; do
	versions=("$library".*)
	if [ -e "${versions[0]}" ]; then
		rm -f "$library"
	fi
done < <(find "$staged" -name '*.so' -print0)

# The shared form of the SPIR-V to DXIL translator, whose 12 MB is a fifth of what is
# left. It goes for a reason particular to it rather than as the bin/ rule spreading: it
# is a correctly built library that this payload's drivers never load. dozen has the
# translator linked into libvulkan_dzn.so, no shipped object lists this soname as NEEDED,
# and no shipped file names it for a dlopen -- the only occurrence of the string in the
# tree is the file's own SONAME.
rm -f "$staged/lib/x86_64-linux-gnu/libspirv_to_dxil.so"

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
