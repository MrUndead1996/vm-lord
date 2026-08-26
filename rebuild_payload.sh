#!/usr/bin/env bash
# Rebuilds display payloads for every supported Ubuntu release.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SERVICES="$ROOT/target/x86_64-unknown-linux-musl/release"
RELEASES=(22.04 24.04 26.04)

usage() {
	cat <<'EOF'
usage: ./rebuild_payload.sh

Build the display services and recreate the Ubuntu 22.04, 24.04 and 26.04
payloads under target/display-payload/.

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

for release in "${RELEASES[@]}"; do
	spec="$ROOT/payloads/display/ubuntu-$release-amd64/payload.spec.json"
	output="$ROOT/target/display-payload/ubuntu-$release"

	[[ -f "$spec" ]] || {
		echo "no payload specification for Ubuntu $release: $spec" >&2
		exit 1
	}

	rm -rf "$output"
	"$ROOT/payloads/display/prepare.sh" \
		--spec "$spec" \
		--output "$output" \
		--services "$SERVICES"
	cargo display-payload \
		--recipe "$output/recipe.json" \
		--input "$output/prepared" \
		--archive "$output/payload.zip" \
		--catalog-entry "$output/catalog-entry.json"
done

echo "display payloads rebuilt under $ROOT/target/display-payload"
