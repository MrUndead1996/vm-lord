#!/usr/bin/env bash
# Prepares the input `cargo xtask gpu-payload pack` needs for this target.
#
# Fetches the pinned upstream sources, lays them out beside this payload's
# overlays and license texts, and writes the two provenance documents the
# builder cross-checks against each other. Nothing here is committed: the
# output is a build artifact, and the spec plus the overlays are what the
# repository keeps.

set -euo pipefail

SPEC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SPEC="$SPEC_DIR/payload.spec.json"

usage() {
	cat <<'USAGE'
usage: prepare.sh --output <directory> [--work <directory>]

  --output  where the prepared tree and recipe.json are written
  --work    where the upstream checkout is cached (default: <output>/upstream)
USAGE
}

output=""
work=""
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
	--work)
		work="${2-}"
		[[ -n "$work" ]] || {
			echo "--work needs a directory" >&2
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
work="${work:-$output/upstream}"

repository="$(git -C "$SPEC_DIR" rev-parse --show-toplevel)"
revision="$(git -C "$repository" rev-parse HEAD)"
if [[ -n "$(git -C "$repository" status --porcelain)" ]]; then
	echo "warning: working tree is dirty, so $revision does not describe what is being packed" >&2
fi

url="$(jq -r '.source.url' "$SPEC")"
branch="$(jq -r '.source.branch' "$SPEC")"
commit="$(jq -r '.source.commit' "$SPEC")"
mapfile -t sparse_paths < <(
	jq -r '.source.paths[]
		| if .kind == "directory" then .path
		  else (.path | split("/")[:-1] | join("/")) end' "$SPEC" | sort -u
)

checkout="$work/$(basename "$url")"
if [[ ! -d "$checkout/.git" ]]; then
	mkdir -p "$checkout"
	git -C "$checkout" init -q
	git -C "$checkout" remote add origin "$url"
fi
git -C "$checkout" sparse-checkout init --cone >/dev/null
git -C "$checkout" sparse-checkout set "${sparse_paths[@]}"
if ! git -C "$checkout" cat-file -e "${commit}^{commit}" 2>/dev/null; then
	echo "fetching $commit from $url"
	# GitHub serves a commit by its own name; the branch is the fallback for a
	# remote that refuses, and it only helps while the branch still reaches the
	# pin. A branch that moved past it fails loudly at the checkout below,
	# which is the point: the payload is built from the pin or not at all.
	git -C "$checkout" fetch --depth 1 --filter=blob:none origin "$commit" ||
		git -C "$checkout" fetch --depth 1 --filter=blob:none origin "$branch"
fi
git -C "$checkout" checkout -q --detach "$commit"

python3 "$SPEC_DIR/prepare.py" \
	--spec "$SPEC" \
	--overlays "$SPEC_DIR/overlays" \
	--licenses "$SPEC_DIR" \
	--checkout "$checkout" \
	--revision "$revision" \
	--output "$output"
