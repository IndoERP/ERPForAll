#!/usr/bin/env bash
# Run through scripts/build-gate.sh on an authorized host. Never run on the laptop.
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"
printf 'host=%s\ntime=%s\n' "$(hostname)" "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
if [ -s REVISION ]; then
  printf 'revision=%s\n' "$(cat REVISION)"
else
  printf 'revision=%s\n' "$(git rev-parse HEAD)"
fi
rustc --version
cargo --version
printf '%s\n' 'command=cargo test --locked -p erp-expr --test alloc_wire -- --nocapture'
cargo test --locked -p erp-expr --test alloc_wire -- --nocapture
