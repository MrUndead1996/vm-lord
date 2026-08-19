#!/usr/bin/env bash
# Proves the staged tree loads in a guest, not only in the image that built it.
#
# Run in a clean Ubuntu with no -dev package installed: what resolves here resolves in a
# guest under the bundled policy, where no apt step installs Mesa's build dependencies.
#
# The baseline is the display stack and nothing else -- see the closure stage in the
# Dockerfile for why those packages are the guest's and not ours. What that baseline
# deliberately withholds is every library Mesa links only when it is built to: LLVM,
# SPIRV-Tools, clang, libelf. `-Dllvm=disabled` is supposed to keep all four out of the
# payload, and a gate that cannot tell is a gate worth nothing, so the second loop below
# fails on them by name even if some future baseline happens to supply them.

set -euo pipefail

tree="${1:?usage: closure.sh <tree>}"

# Linked against by a build, never by a guest: if one of these reaches a shipped object,
# the payload stopped being self-contained and the option set that kept it so has drifted.
build_only='libLLVM|libSPIRV|libspirv|libclang|libelf'

echo "$tree/lib/x86_64-linux-gnu" > /etc/ld.so.conf.d/vmlord-closure.conf
# The tree holds no symlink by design, so ldconfig says so about every soname it finds.
# That is the payload builder's rule being obeyed, not a fault: drop the noise.
ldconfig 2>/dev/null

unresolved=0
while IFS= read -r library; do
	resolution="$(ldd "$library" 2>/dev/null || true)"

	missing="$(echo "$resolution" | awk '/not found/ { print $1 }')"
	if [ -n "$missing" ]; then
		echo "$library needs $(echo "$missing" | tr '\n' ' ')" >&2
		unresolved=1
	fi

	toolchain="$(echo "$resolution" | awk '{ print $1 }' | grep -E "$build_only" || true)"
	if [ -n "$toolchain" ]; then
		echo "$library needs $(echo "$toolchain" | tr '\n' ' '), which only a build has" >&2
		unresolved=1
	fi
done < <(find "$tree" -name '*.so' -o -name '*.so.*' | sort)

[ "$unresolved" -eq 0 ] || {
	echo "the payload would ship libraries a guest cannot load" >&2
	exit 1
}
echo "every shared object in $tree resolves against a clean Ubuntu"
