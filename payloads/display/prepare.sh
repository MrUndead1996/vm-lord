#!/usr/bin/env bash
# Prepares the input `cargo xtask display-payload pack` needs for one release.
#
# Everything happens in the image beside this file: the module is built against that
# release's headers -- which is the proof it compiles for it -- the tree is laid out, and
# `recipe.json` is written with the kernel the build actually resolved. The host needs
# docker, a bash 4 or newer, and nothing else.
#
# What the recipe cannot be told from the outside is `proven_on`: it is whatever kernel
# `linux-headers-generic` resolved to inside the image, so it is read there and written
# there rather than guessed here.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

usage() {
	cat <<'USAGE'
usage: prepare.sh --spec <payload.spec.json> --output <directory>

  --spec    the release to build, e.g. payloads/display/ubuntu-24.04-amd64/payload.spec.json
  --output  where the prepared tree and recipe.json are written
USAGE
}

spec=""
output=""
while [[ $# -gt 0 ]]; do
	case "$1" in
	--spec)
		spec="${2-}"
		shift 2 || true
		;;
	--output)
		output="${2-}"
		shift 2 || true
		;;
	-h | --help)
		usage
		exit 0
		;;
	*)
		echo "unknown argument: $1" >&2
		usage >&2
		exit 2
		;;
	esac
done

[[ -n "$spec" && -n "$output" ]] || {
	echo "missing --spec or --output" >&2
	usage >&2
	exit 2
}
[[ -f "$spec" ]] || {
	echo "no such spec: $spec" >&2
	exit 2
}

mkdir -p "$output"
output="$(cd "$output" && pwd)"
spec="$(cd "$(dirname "$spec")" && pwd)/$(basename "$spec")"

# One field per line, read without jq so that a host with no jq can still build. The spec
# is ours and its shape is fixed, so a line-oriented read is enough -- and a field that is
# missing yields an empty value, which the check below refuses by name.
field() {
	sed -nE "s/.*\"$1\"[[:space:]]*:[[:space:]]*\"?([^\",}]*)\"?.*/\1/p" "$spec" | head -n1
}

BASE="$(field base)"
VERSION="$(field version)"
SOURCE_URL="$(field source_url)"
DISTRIBUTION="$(field distribution)"
RELEASE="$(field release)"
ARCHITECTURE="$(field architecture)"
PROTOCOL_MAJOR="$(field major)"
PROTOCOL_MIN_MINOR="$(field min_minor)"
PROTOCOL_MAX_MINOR="$(field max_minor)"

for name in BASE VERSION SOURCE_URL DISTRIBUTION RELEASE ARCHITECTURE \
	PROTOCOL_MAJOR PROTOCOL_MIN_MINOR PROTOCOL_MAX_MINOR; do
	[[ -n "${!name}" ]] || {
		echo "$spec does not say what $name is" >&2
		exit 1
	}
done

# The payload's provenance is this repository at this commit: a display payload has no
# upstream to pin, and what a person needs in order to rebuild one is the tree it was
# built from. A dirty tree is refused rather than recorded, because a commit that does not
# describe what was built is worse than no build.
COMMIT="$(git -C "$HERE" rev-parse HEAD)"
if ! git -C "$HERE" diff --quiet HEAD -- "$HERE"; then
	echo "payloads/display has uncommitted changes; commit them before packing a payload" >&2
	echo "-- the recipe records a commit, and it has to be one that describes the build." >&2
	exit 1
fi

# BuildKit's local exporter merges into the destination rather than replacing it, so a
# leftover tree from an earlier run would be packed as if this build had produced it.
rm -rf "$output/prepared" "$output/recipe.json"

DOCKER_BUILDKIT=1 docker build \
	--build-arg "BASE=$BASE" \
	--build-arg "VERSION=$VERSION" \
	--build-arg "COMMIT=$COMMIT" \
	--build-arg "SOURCE_URL=$SOURCE_URL" \
	--build-arg "DISTRIBUTION=$DISTRIBUTION" \
	--build-arg "RELEASE=$RELEASE" \
	--build-arg "ARCHITECTURE=$ARCHITECTURE" \
	--build-arg "PROTOCOL_MAJOR=$PROTOCOL_MAJOR" \
	--build-arg "PROTOCOL_MIN_MINOR=$PROTOCOL_MIN_MINOR" \
	--build-arg "PROTOCOL_MAX_MINOR=$PROTOCOL_MAX_MINOR" \
	--output "type=local,dest=$output" \
	"$HERE"

echo "prepared tree and recipe.json written to $output"
