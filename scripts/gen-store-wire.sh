#!/usr/bin/env bash
# Generate the Rust-owned store wire declarations (web/packages/store-wire/src/generated).
# Works in a sync copy with no .git directory. Separate from gen-ui-types.sh so each generator
# manages only its own output directory.
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
if [[ $# != 1 || ( "$1" != --write && "$1" != --check ) ]]; then
    echo 'usage: scripts/gen-store-wire.sh --write|--check' >&2
    exit 2
fi
MODE=$1
TEMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/erp-store-wire.XXXXXX")
trap 'rm -rf "$TEMP_DIR"' EXIT
cd "$ROOT"
# The roots live in this tooling-only example. Production builds never enable schema-gen.
cargo run --quiet --locked -p erp-store-wire --features schema-gen --example store_wire_schema -- "$TEMP_DIR"
python3 -I "$ROOT/scripts/ui-types-files.py" "$MODE" "$TEMP_DIR" "$ROOT/web/packages/store-wire/src/generated"
