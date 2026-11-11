#!/usr/bin/env bash
# Local checks for contributors: formatting, lints and a build; --tests adds the test suites.
#
#   scripts/check.sh            cargo fmt --check, clippy, cargo check
#   scripts/check.sh --tests    also cargo nextest (needs DATABASE_URL for database tests)
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

run_tests=0
for a in "$@"; do
  case "$a" in
    --tests) run_tests=1 ;;
    -h | --help) sed -n '2,6p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "check.sh: unknown option $a" >&2; exit 2 ;;
  esac
done

step() { echo "== $*"; "$@"; }

step cargo fmt --all --check
step cargo clippy --locked --workspace --all-targets -- -D warnings
step cargo check --locked --workspace --all-targets
if [ "$run_tests" = 1 ]; then
  step cargo nextest run --locked --workspace --no-fail-fast
  step cargo test --locked --workspace --doc
fi
echo "check: ok"
