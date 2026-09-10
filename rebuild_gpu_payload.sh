#!/usr/bin/env bash
# Builds every GPU payload target and packs each of them.
#
# Both, unlike the display payload's script, which builds three and packs one: those
# three prepared trees differ in recipe.json alone, while these two are different
# binaries built against different C libraries, and a guest gets the one built for its
# distribution.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

for spec in "$ROOT"/payloads/gpu/*/payload.spec.json; do
	target="$(basename "$(dirname "$spec")")"
	output="$ROOT/target/gpu-payload/$target"

	echo "== $target"
	"$ROOT/payloads/gpu/prepare.sh" --spec "$spec" --output "$output"
	# `pack` refuses to overwrite, which is what makes a stale archive impossible to
	# ship by accident. This script is the rebuild, so it clears the two files it is
	# about to write and nothing else in that directory.
	rm -f "$output/payload.zip" "$output/catalog-entry.json"
	cargo run -p xtask -- gpu-payload pack \
		--recipe "$output/recipe.json" \
		--input "$output/prepared" \
		--archive "$output/payload.zip" \
		--catalog-entry "$output/catalog-entry.json"
done

echo
echo "Release with:"
for spec in "$ROOT"/payloads/gpu/*/payload.spec.json; do
	printf '    --gpu-payload target/gpu-payload/%s \\\n' "$(basename "$(dirname "$spec")")"
done
