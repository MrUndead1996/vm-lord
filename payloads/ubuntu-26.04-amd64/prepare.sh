#!/usr/bin/env bash
# Prepares the input `cargo xtask gpu-payload pack` needs for this target.
#
# Everything happens in the image beside this file: the pinned checkouts, the Mesa
# build, the closure check, and the layout. The host needs docker and nothing else --
# not jq, not python3, not a git new enough for partial clones -- and the toolchain that
# produced a payload is a pinned image rather than whatever the machine happened to have.
#
# Commits come from payload.spec.json, read inside the image and passed back in as build
# arguments so that each checkout is a layer keyed by its own pin. Nothing here is
# committed: the output is a build artifact, and the spec plus the overlays are what the
# repository keeps.

set -euo pipefail

SPEC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SPEC="$SPEC_DIR/payload.spec.json"
DOCKERFILE="$SPEC_DIR/Dockerfile"

# Which build argument carries which upstream's commit. The mapping is written out here
# instead of being computed from the repository URL because a computed name has no way to
# be wrong loudly: a URL that ends in `.git` or a slash, or whose last segment simply is
# not what the Dockerfile chose to call it, would yield a name no `ARG` declares, and
# docker would then build with that `ARG` unset -- a checkout of nothing, discovered much
# later if at all. Written out, the pairing is one line per upstream and the two checks
# below can hold it to the Dockerfile.
declare -A ARGUMENT_FOR=(
	["https://github.com/microsoft/WSL2-Linux-Kernel"]="KERNEL_COMMIT"
	["https://gitlab.freedesktop.org/mesa/mesa"]="MESA_COMMIT"
	["https://github.com/microsoft/DirectX-Headers"]="DIRECTX_HEADERS_COMMIT"
)

usage() {
	cat <<'USAGE'
usage: prepare.sh --output <directory>

  --output  where the prepared tree and recipe.json are written
USAGE
}

output=""
while [[ $# -gt 0 ]]; do
	case "$1" in
	--output)
		output="${2-}"
		[[ -n "$output" ]] || {
			echo "--output needs a directory" >&2
			exit 2
		}
		shift 2
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

[[ -n "$output" ]] || {
	echo "missing --output <directory>" >&2
	usage >&2
	exit 2
}

mkdir -p "$output"
output="$(cd "$output" && pwd)"

# The commit arguments the Dockerfile actually declares. Reading them out of the file is
# what lets the two checks below be checks and not comments: the table above is a claim
# about the Dockerfile, and this is the Dockerfile itself.
declare -A DECLARED=()
while read -r name; do
	[[ -n "$name" ]] || continue
	DECLARED["$name"]=1
done < <(sed -n 's/^ARG[[:space:]]\{1,\}\([A-Z0-9_]*_COMMIT\)[[:space:]]*$/\1/p' "$DOCKERFILE")

[[ ${#DECLARED[@]} -gt 0 ]] || {
	echo "no ARG <NAME>_COMMIT found in $DOCKERFILE" >&2
	exit 1
}

# The spec is read by the image's own jq, so that a host without jq can still tell the
# build which commits to fetch. The toolchain stage is built once and reused: it is the
# same layer the full build will hit, so this costs a cache lookup and not a build.
toolchain="$(DOCKER_BUILDKIT=1 docker build --quiet --target toolchain "$SPEC_DIR")"

pins="$(
	docker run --rm \
		-v "$SPEC:/spec.json:ro" \
		--entrypoint jq "$toolchain" -r '
			(.sources[] | .url + "\t" + .commit),
			(.sources[] | select(.kind == "built") | .inputs[]? | .url + "\t" + .commit)
		' /spec.json
)"

arguments=()
declare -A SUPPLIED=()
while IFS=$'\t' read -r url commit; do
	[[ -n "$url" ]] || continue
	name="${ARGUMENT_FOR["$url"]-}"
	[[ -n "$name" ]] || {
		echo "payload.spec.json pins $url, which prepare.sh has no build argument for." >&2
		echo "Add it to ARGUMENT_FOR in this script, and an ARG to $DOCKERFILE." >&2
		exit 1
	}
	[[ -n "${DECLARED["$name"]-}" ]] || {
		echo "prepare.sh passes $name for $url, which $DOCKERFILE does not declare." >&2
		exit 1
	}
	arguments+=(--build-arg "${name}=${commit}")
	SUPPLIED["$name"]=1
done <<<"$pins"

# The other direction, and the one that would otherwise fail silently: an ARG the spec
# says nothing about is an ARG docker expands to the empty string.
for name in "${!DECLARED[@]}"; do
	[[ -n "${SUPPLIED["$name"]-}" ]] || {
		echo "$DOCKERFILE declares $name, which payload.spec.json pins nothing for." >&2
		exit 1
	}
done

DOCKER_BUILDKIT=1 docker build \
	"${arguments[@]}" \
	--output "type=local,dest=$output" \
	"$SPEC_DIR"

echo "prepared tree and recipe.json written to $output"
