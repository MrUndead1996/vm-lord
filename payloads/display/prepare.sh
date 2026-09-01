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
usage: prepare.sh --spec <payload.spec.json> --output <directory> --services <directory>

  --spec      the release to build, e.g. payloads/display/ubuntu-24.04-amd64/payload.spec.json
  --output    where the prepared tree and recipe.json are written
  --services  where the guest services were built, e.g.
              target/x86_64-unknown-linux-musl/release -- run `cargo display-services` first
USAGE
}

spec=""
output=""
services=""
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
	--services)
		services="${2-}"
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

[[ -n "$spec" && -n "$output" && -n "$services" ]] || {
	echo "missing --spec, --output or --services" >&2
	usage >&2
	exit 2
}
[[ -f "$spec" ]] || {
	echo "no such spec: $spec" >&2
	exit 2
}
[[ -d "$services" ]] || {
	echo "no such services directory: $services" >&2
	exit 2
}

mkdir -p "$output"
output="$(cd "$output" && pwd)"
spec="$(cd "$(dirname "$spec")" && pwd)/$(basename "$spec")"
services="$(cd "$services" && pwd)"

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
# Both trees, because the recipe's commit now describes the services in the archive as
# well as the module: the binaries are built from crates/display-services, and a commit
# that predates what was copied in would be a provenance that lies.
for tree in "$HERE" "$HERE/../../crates/display-services"; do
	if ! git -C "$HERE" diff --quiet HEAD -- "$tree"; then
		echo "$tree has uncommitted changes; commit them before packing a payload" >&2
		echo "-- the recipe records a commit, and it has to be one that describes the build." >&2
		exit 1
	fi
done

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

# The services are built by the host toolchain, not in the image: a static musl binary is
# identical for 22.04, 24.04 and 26.04, and the container exists to prove the *module*
# compiles against a release's headers. A Rust toolchain in there would be a third
# toolchain for no gain.
for binary in vmlord-display-broker vmlord-display-session vmlord-display-clipboard vmlord-display-audio; do
	[[ -x "$services/$binary" ]] || {
		echo "$services does not hold $binary; run 'cargo display-services' first" >&2
		exit 1
	}
	install -m 0755 "$services/$binary" "$output/prepared/content/services/$binary"
done
install -m 0644 "$HERE/services/"*.service "$output/prepared/content/services/"

# The loopback's configuration: what loads the module, what gives it one cable,
# and the WirePlumber rule in both of the forms the supported releases read.
# Which of the two is installed is decided inside the guest, by the directory
# its own WirePlumber ships.
install -m 0644 "$HERE/audio/"* "$output/prepared/content/audio/"

echo "prepared tree and recipe.json written to $output"
