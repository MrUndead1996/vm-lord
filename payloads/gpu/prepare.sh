#!/usr/bin/env bash
# Prepares the input `cargo xtask gpu-payload pack` needs for this target.
#
# Everything happens in the image beside this file: the pinned checkouts, the Mesa
# build, the closure check, and the layout. What the host does not need is any part of
# the payload's own toolchain -- not jq, not python3, not a git new enough for partial
# clones -- and the toolchain that produced a payload is therefore a pinned image rather
# than whatever the machine happened to have.
#
# What the host does need, beyond docker: a bash 4 or newer, for the associative arrays
# below; sed, which reads the ARG names out of the Dockerfile; and a docker daemon that
# shares this filesystem, because the spec is read through a -v bind mount. All three
# fail loudly and immediately, so this paragraph is a courtesy and not a contract.
#
# Commits come from payload.spec.json, read inside the image and passed back in as build
# arguments so that each checkout is a layer keyed by its own pin. Nothing here is
# committed: the output is a build artifact, and the spec plus the overlays are what the
# repository keeps.

set -euo pipefail

# The build context is this directory: one Dockerfile, one prepare.py and one Mesa
# recipe serve every target, and what differs between two targets is the spec.
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DOCKERFILE="$HERE/Dockerfile"
SPEC=""

# Which pair of build arguments carries which upstream. The mapping is written out here
# instead of being computed from the repository URL because a computed name has no way to
# be wrong loudly: a URL that ends in `.git` or a slash, or whose last segment simply is
# not what the Dockerfile chose to call it, would yield a name no `ARG` declares, and
# docker would then build with that `ARG` unset -- a checkout of nothing, discovered much
# later if at all. Written out, the pairing is one line per upstream and the checks below
# can hold it to the Dockerfile.
#
# Both halves of the pair are passed, the URL as well as the commit, because the URL the
# Dockerfile would otherwise default to is not the URL the payload's provenance claims:
# `recipe.json` and `sources.json` record what the spec says. A default edited to name a
# fork would build from the fork while the provenance still named the original, and
# nothing would notice. Sending the spec's own URL leaves the default with nothing to
# decide.
declare -A ARGUMENT_FOR=(
	["https://github.com/microsoft/WSL2-Linux-Kernel"]="KERNEL"
	["https://gitlab.freedesktop.org/mesa/mesa"]="MESA"
	["https://github.com/microsoft/DirectX-Headers"]="DIRECTX_HEADERS"
)

usage() {
	cat <<'USAGE'
usage: prepare.sh --spec <payload.spec.json> --output <directory>

  --spec    the target to build, e.g. payloads/gpu/arch-rolling-amd64/payload.spec.json
  --output  where the prepared tree and recipe.json are written
USAGE
}

output=""
while [[ $# -gt 0 ]]; do
	case "$1" in
	--spec)
		SPEC="${2-}"
		[[ -n "$SPEC" ]] || {
			echo "--spec needs a payload.spec.json" >&2
			exit 2
		}
		shift 2
		;;
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

[[ -n "$SPEC" ]] || {
	echo "missing --spec <payload.spec.json>" >&2
	usage >&2
	exit 2
}
[[ -f "$SPEC" ]] || {
	echo "no such spec: $SPEC" >&2
	exit 2
}
SPEC="$(cd "$(dirname "$SPEC")" && pwd)/$(basename "$SPEC")"
# Which target directory the Dockerfile copies the spec out of. The context is shared, so
# the spec's own directory name is what tells the build which of them to read.
TARGET="$(basename "$(dirname "$SPEC")")"

# meson's libdir, from the one place that states where this payload's libraries go. Read
# with sed rather than jq because this runs before the toolchain image is built, and a
# host with no jq is exactly what this script exists to allow. One statement decides both
# the tree meson installs and the directory the guest points its linker at: two fields
# would be two answers to one question, and the wrong one is silent -- Mesa staged, and
# nothing ever loading it.
LAYOUT="$(sed -nE 's/.*"library_layout"[[:space:]]*:[[:space:]]*"([^"]*)".*/\1/p' "$SPEC" | head -n1)"
case "$LAYOUT" in
flat) LIBDIR="lib" ;;
multiarch:?*) LIBDIR="lib/${LAYOUT#multiarch:}" ;;
*)
	echo "$SPEC must state a library_layout of 'flat' or 'multiarch:<triplet>'," >&2
	echo "because it is what decides both where meson installs and where the guest" >&2
	echo "points its linker. Found: ${LAYOUT:-nothing}" >&2
	exit 1
	;;
esac

# The build's own pins: which image, which stages, and -- where the distribution has no
# release archive of its own -- which day's packages. Read the same line-oriented way, and
# for the same reason.
field() {
	sed -nE "s/.*\"$1\"[[:space:]]*:[[:space:]]*\"([^\"]*)\".*/\1/p" "$SPEC" | head -n1
}

BASE="$(field base_image)"
TOOLCHAIN="$(field toolchain)"
CLOSURE="$(field closure)"
MODULE="$(field module_gate)"
# Deliberately outside the loop below: a distribution whose release is already an archive
# needs no day, and an empty value is the right answer for it rather than a missing one.
PACKAGE_SNAPSHOT="$(field package_snapshot)"

for name in BASE TOOLCHAIN CLOSURE MODULE; do
	[[ -n "${!name}" ]] || {
		echo "$SPEC does not say what $name is; every target names its base image and" >&2
		echo "the stages its package manager needs, in the spec's \"build\" object." >&2
		exit 1
	}
done

mkdir -p "$output"
output="$(cd "$output" && pwd)"

# The upstream arguments the Dockerfile actually declares. Reading them out of the file
# itself is what lets the checks below be checks and not comments: the table above is a
# claim about the Dockerfile, and this is the Dockerfile.
declare -A DECLARED=()
while read -r name; do
	[[ -n "$name" ]] || continue
	DECLARED["$name"]=1
done < <(sed -nE 's/^ARG[[:space:]]+([A-Z0-9_]+_(URL|COMMIT))([[:space:]]*|=.*)$/\1/p' "$DOCKERFILE")

[[ ${#DECLARED[@]} -gt 0 ]] || {
	echo "no ARG <NAME>_URL or <NAME>_COMMIT found in $DOCKERFILE" >&2
	exit 1
}

# The spec is read by the image's own jq, so that a host without jq can still tell the
# build which commits to fetch. The toolchain stage is built once and reused: it is the
# same layer the full build will hit, so this costs a cache lookup and not a build.
toolchain="$(DOCKER_BUILDKIT=1 docker build --quiet \
	--build-arg "BASE=$BASE" \
	--build-arg "TOOLCHAIN=$TOOLCHAIN" \
	--build-arg "CLOSURE=$CLOSURE" \
	--build-arg "MODULE=$MODULE" \
	--build-arg "PACKAGE_SNAPSHOT=$PACKAGE_SNAPSHOT" \
	--target toolchain "$HERE")"

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
declare -A COMMIT_FOR=()
while IFS=$'\t' read -r url commit; do
	[[ -n "$url" ]] || continue
	# An empty commit reaches docker as `--build-arg NAME=`, which is not a missing
	# argument the bidirectional check above would catch: the name is declared and it is
	# supplied. `git fetch origin ""` is what happens next, and the failure surfaces
	# inside the image, far from the spec row that caused it.
	[[ -n "$commit" ]] || {
		echo "payload.spec.json pins $url with an empty commit." >&2
		echo "Every source and every input needs a commit to check out." >&2
		exit 1
	}
	# ARGUMENT_FOR is keyed by URL, so one URL gets one pair of build arguments. Two rows
	# naming it would both resolve to that pair and the second --build-arg would win
	# silently -- and if their commits differ, the build then checks out one of them while
	# payload.spec.json, sources.json and the catalog entry all attest to two. Nothing
	# downstream can notice: the provenance documents are generated from the spec, and the
	# spec is the half that is wrong.
	[[ -z "${COMMIT_FOR["$url"]-}" ]] || {
		echo "payload.spec.json pins $url twice, at ${COMMIT_FOR["$url"]} and at $commit." >&2
		echo "One URL carries one pair of build arguments, so only one of the two could" >&2
		echo "be built while the recorded provenance would claim both." >&2
		exit 1
	}
	COMMIT_FOR["$url"]="$commit"
	prefix="${ARGUMENT_FOR["$url"]-}"
	[[ -n "$prefix" ]] || {
		echo "payload.spec.json pins $url, which prepare.sh has no build arguments for." >&2
		echo "Add it to ARGUMENT_FOR in this script, and ARGs to $DOCKERFILE." >&2
		exit 1
	}
	for pair in "URL=$url" "COMMIT=$commit"; do
		name="${prefix}_${pair%%=*}"
		[[ -n "${DECLARED["$name"]-}" ]] || {
			echo "prepare.sh passes $name for $url, which $DOCKERFILE does not declare." >&2
			exit 1
		}
		arguments+=(--build-arg "${name}=${pair#*=}")
		SUPPLIED["$name"]=1
	done
done <<<"$pins"

# The other direction, and the one that would otherwise fail silently: an ARG the spec
# says nothing about is an ARG the Dockerfile answers out of its own default, or with the
# empty string when it has none.
for name in "${!DECLARED[@]}"; do
	[[ -n "${SUPPLIED["$name"]-}" ]] || {
		echo "$DOCKERFILE declares $name, which payload.spec.json pins nothing for." >&2
		exit 1
	}
done

# BuildKit's local exporter merges into the destination rather than replacing it, so
# anything already sitting in these two paths survives a run and is packed as if the
# build had produced it -- a leftover from an older pin, or half a tree from an export
# that was interrupted. Inside the image /output is always fresh; on the host it has to
# be made so. Only the two paths this build writes are cleared, and not the directory
# around them, because the documented workflow packs `payload.zip` and
# `catalog-entry.json` into that same directory: refusing a non-empty --output, or
# emptying it, would turn the README's own second run into a failure or a surprise.
# This happens last, after the arguments are settled, so a rejected invocation leaves the
# previous run's output intact.
rm -rf "$output/prepared" "$output/recipe.json"

DOCKER_BUILDKIT=1 docker build \
	--build-arg "TARGET=$TARGET" \
	--build-arg "LIBDIR=$LIBDIR" \
	--build-arg "BASE=$BASE" \
	--build-arg "TOOLCHAIN=$TOOLCHAIN" \
	--build-arg "CLOSURE=$CLOSURE" \
	--build-arg "MODULE=$MODULE" \
	--build-arg "PACKAGE_SNAPSHOT=$PACKAGE_SNAPSHOT" \
	"${arguments[@]}" \
	--output "type=local,dest=$output" \
	"$HERE"

echo "prepared tree and recipe.json written to $output"
