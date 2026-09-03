#!/usr/bin/env bash
# Builds the display payload against every supported Ubuntu release, and packs one.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SERVICES="$ROOT/target/x86_64-unknown-linux-musl/release"
OUTPUT="$ROOT/target/display-payload"

# Every release the module must compile against. Each is a container build and
# each is a gate: a module that does not build for one of them is an artifact
# that never exists, rather than a failure discovered inside a guest.
RELEASES=(22.04 24.04 26.04)

# The one that becomes the artifact. Since task #169 the catalogue reads a
# payload's distribution and release as provenance rather than as a key, and a
# display payload carries DKMS sources and static musl binaries -- so the three
# prepared trees differ in `recipe.json` and in nothing else. Packing all three
# would ship one payload's content three times under three IDs.
#
# 24.04 because it is the release the payload is proven on in the entry every
# guest ends up reading; the other two stay gates.
PACKED=24.04

usage() {
	cat <<'EOF'
usage: ./rebuild_payload.sh

Build the display services, then build the display payload inside the Ubuntu
22.04, 24.04 and 26.04 containers -- all three, because each is the proof the
module compiles against that release's headers.

One of them is packed into the artifact a release ships:

    target/display-payload/release/{payload.zip,catalog-entry.json}

which is what `cargo dist --display-payload` takes. The distribution and
release in its entry are provenance, not a key, so that one payload serves
every guest on the same architecture.

Requires cargo and Docker. payloads/display/prepare.sh also refuses dirty
payload or display-services sources because their commit is recorded in the
payload recipe.
EOF
}

case "${1-}" in
"") ;;
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

for command in cargo docker; do
	command -v "$command" >/dev/null 2>&1 || {
		echo "$command is required to rebuild display payloads" >&2
		exit 1
	}
done

cd "$ROOT"
cargo display-services

# One sweep of the whole tree rather than one per release, so that a run which
# changes which release is packed cannot leave the previous artifact behind.
rm -rf "$OUTPUT"

for release in "${RELEASES[@]}"; do
	spec="$ROOT/payloads/display/ubuntu-$release-amd64/payload.spec.json"

	[[ -f "$spec" ]] || {
		echo "no payload specification for Ubuntu $release: $spec" >&2
		exit 1
	}

	"$ROOT/payloads/display/prepare.sh" \
		--spec "$spec" \
		--output "$OUTPUT/ubuntu-$release" \
		--services "$SERVICES"
done

prepared="$OUTPUT/ubuntu-$PACKED"
[[ -d "$prepared" ]] || {
	echo "Ubuntu $PACKED is packed but is not among the releases built" >&2
	exit 1
}

# `pack` writes both files with `File::create`, which does not make parents.
mkdir -p "$OUTPUT/release"
cargo display-payload \
	--recipe "$prepared/recipe.json" \
	--input "$prepared/prepared" \
	--archive "$OUTPUT/release/payload.zip" \
	--catalog-entry "$OUTPUT/release/catalog-entry.json"

echo "display payload built for ${RELEASES[*]}, packed from $PACKED"
echo "the artifact is $OUTPUT/release; pass it to cargo dist --display-payload"
