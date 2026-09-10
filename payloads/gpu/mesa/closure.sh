#!/usr/bin/env bash
# Proves the staged tree loads in a guest, not only in the image that built it.
#
# Run in a clean Ubuntu with no -dev package installed: what resolves here resolves in a
# guest under the bundled policy, where no apt step installs Mesa's build dependencies.

set -euo pipefail

tree="${1:?usage: closure.sh <tree>}"

# Every external soname a shipped object may name.
#
# libc, libm, libgcc_s and libstdc++ are the C and C++ runtimes; libz and libzstd are the
# compression libraries Mesa's shader cache uses, and both are already in the base
# rootfs. Everything after them is the client half of the display stack -- X11, XCB,
# Wayland and DRM -- which the guest gets from mesa-utils and vulkan-tools, the two
# packages gpu_render.rs installs before anything asks Mesa to draw. The closure stage in
# the Dockerfile installs this same set and nothing else.
#
# An allow-list and not a list of libraries to reject, because the failure worth catching
# is a dependency nobody looked at, and only the permitted set can be stated in advance.
# A name printed below is not necessarily wrong -- it is unreviewed. The fix is to decide
# whether a guest really has it, then either add it here and to the Dockerfile's closure
# stage, or build without it.
allowed_external=(
	libc.so.6
	libm.so.6
	libgcc_s.so.1
	libstdc++.so.6
	libz.so.1
	libzstd.so.1
	libdrm.so.2
	libexpat.so.1
	libX11.so.6
	libX11-xcb.so.1
	libXext.so.6
	libXxf86vm.so.1
	libxcb.so.1
	libxcb-dri3.so.0
	libxcb-glx.so.0
	libxcb-present.so.0
	libxcb-randr.so.0
	libxcb-shm.so.0
	libxcb-sync.so.1
	libxcb-xfixes.so.0
	libxshmfence.so.1
	libwayland-client.so.0
)

objects=()
while IFS= read -r object; do
	objects+=("$object")
done < <(find "$tree" \( -name '*.so' -o -name '*.so.*' \) | sort)

# A gate pointed at the wrong directory would otherwise pass by finding nothing.
[ "${#objects[@]}" -gt 0 ] || {
	echo "no shared object under $tree: that is not the tree this gate was meant to check" >&2
	exit 1
}

echo "$tree/lib/x86_64-linux-gnu" > /etc/ld.so.conf.d/vmlord-closure.conf
# The tree holds no symlink by design, so ldconfig says so about every soname it finds.
# That is the payload builder's rule being obeyed, not a fault: drop the noise.
ldconfig 2>/dev/null

unresolved=0
for object in "${objects[@]}"; do
	missing="$(ldd "$object" 2>/dev/null | awk '/not found/ { print $1 }')"
	if [ -n "$missing" ]; then
		echo "$object needs $(echo "$missing" | tr '\n' ' ')" >&2
		unresolved=1
	fi
done

# What the tree supplies to itself, measured rather than written down: libgallium carries
# Mesa's version in its soname, so a version bump would read as a new dependency against
# any list kept by hand.
provided="$(
	for object in "${objects[@]}"; do
		readelf -d "$object" | awk '/SONAME/ { gsub(/[][]/, "", $5); print $5 }'
	done | sort -u
)"
needed="$(
	for object in "${objects[@]}"; do
		readelf -d "$object" | awk '/NEEDED/ { gsub(/[][]/, "", $5); print $5 }'
	done | sort -u
)"
permitted="$(printf '%s\n' "${allowed_external[@]}" "$provided" | sort -u)"

newcomers="$(comm -23 <(echo "$needed") <(echo "$permitted"))"
if [ -n "$newcomers" ]; then
	for newcomer in $newcomers; do
		echo "the payload has grown a dependency on $newcomer, which nobody has reviewed" >&2
	done
	unresolved=1
fi

[ "$unresolved" -eq 0 ] || {
	echo "the payload would ship libraries a guest cannot load" >&2
	exit 1
}
echo "every shared object in $tree resolves against a clean Ubuntu"
echo "and needs nothing beyond $(echo "$needed" | wc -l) reviewed sonames"
