#!/usr/bin/env bash
# Generate Rust-owned UI declarations. Works in a sync copy with no .git directory.
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
if [[ $# != 1 || ( "$1" != --write && "$1" != --check ) ]]; then
    echo 'usage: scripts/gen-ui-types.sh --write|--check' >&2
    exit 2
fi
MODE=$1
TEMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/erp-ui-types.XXXXXX")
trap 'rm -rf "$TEMP_DIR"' EXIT
cd "$ROOT"
# Explicit roots live in this tooling-only example. Production builds never enable schema-gen.
cargo run --quiet --locked -p erp-expr --features schema-gen --example ui_schema -- "$TEMP_DIR"
python3 -I "$ROOT/scripts/ui-types-files.py" "$MODE" "$TEMP_DIR" "$ROOT/web/src/generated"
