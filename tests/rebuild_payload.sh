#!/usr/bin/env bash

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TEMP="$(mktemp -d)"
trap 'rm -rf "$TEMP"' EXIT

mkdir -p "$TEMP/repo/payloads/display" "$TEMP/bin"
cp "$ROOT/rebuild_payload.sh" "$TEMP/repo/rebuild_payload.sh"

cat >"$TEMP/bin/cargo" <<'EOF'
#!/usr/bin/env bash
printf 'cargo %s\n' "$*" >>"$CALLS"
EOF
chmod +x "$TEMP/bin/cargo"

cat >"$TEMP/bin/docker" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
chmod +x "$TEMP/bin/docker"

cat >"$TEMP/repo/payloads/display/prepare.sh" <<'EOF'
#!/usr/bin/env bash
printf 'prepare %s\n' "$*" >>"$CALLS"
EOF
chmod +x "$TEMP/repo/payloads/display/prepare.sh"

for release in 22.04 24.04 26.04; do
	mkdir -p "$TEMP/repo/payloads/display/ubuntu-$release-amd64"
	printf '{}\n' >"$TEMP/repo/payloads/display/ubuntu-$release-amd64/payload.spec.json"
done

CALLS="$TEMP/calls" PATH="$TEMP/bin:$PATH" "$TEMP/repo/rebuild_payload.sh"

cat >"$TEMP/expected" <<EOF
cargo display-services
prepare --spec $TEMP/repo/payloads/display/ubuntu-22.04-amd64/payload.spec.json --output $TEMP/repo/target/display-payload/ubuntu-22.04 --services $TEMP/repo/target/x86_64-unknown-linux-musl/release
cargo display-payload --recipe $TEMP/repo/target/display-payload/ubuntu-22.04/recipe.json --input $TEMP/repo/target/display-payload/ubuntu-22.04/prepared --archive $TEMP/repo/target/display-payload/ubuntu-22.04/payload.zip --catalog-entry $TEMP/repo/target/display-payload/ubuntu-22.04/catalog-entry.json
prepare --spec $TEMP/repo/payloads/display/ubuntu-24.04-amd64/payload.spec.json --output $TEMP/repo/target/display-payload/ubuntu-24.04 --services $TEMP/repo/target/x86_64-unknown-linux-musl/release
cargo display-payload --recipe $TEMP/repo/target/display-payload/ubuntu-24.04/recipe.json --input $TEMP/repo/target/display-payload/ubuntu-24.04/prepared --archive $TEMP/repo/target/display-payload/ubuntu-24.04/payload.zip --catalog-entry $TEMP/repo/target/display-payload/ubuntu-24.04/catalog-entry.json
prepare --spec $TEMP/repo/payloads/display/ubuntu-26.04-amd64/payload.spec.json --output $TEMP/repo/target/display-payload/ubuntu-26.04 --services $TEMP/repo/target/x86_64-unknown-linux-musl/release
cargo display-payload --recipe $TEMP/repo/target/display-payload/ubuntu-26.04/recipe.json --input $TEMP/repo/target/display-payload/ubuntu-26.04/prepared --archive $TEMP/repo/target/display-payload/ubuntu-26.04/payload.zip --catalog-entry $TEMP/repo/target/display-payload/ubuntu-26.04/catalog-entry.json
EOF

diff -u "$TEMP/expected" "$TEMP/calls"
