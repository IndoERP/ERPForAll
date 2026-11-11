#!/usr/bin/env bash
# Tests and clippy for the bounded html5ever fork with and without the `budget` feature
# (ENGINE MUST 1). The fork is not a workspace member, so it is copied
# to a scratch directory and built there. ci.sh runs this on every full run; the release build and
# the html5lib conformance suite stay in scripts/html-budget-receipt.sh.
#
#   scripts/html-budget-fork-tests.sh
set -euo pipefail
export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:$PATH"
cd "$(dirname "${BASH_SOURCE[0]}")/.."
run() { echo "+ $*"; "$@"; }
work="$(mktemp -d)"; trap 'rm -rf "$work"' EXIT
cp -R vendor/html5ever-bounded "$work/h5e"
cd "$work/h5e"
run cargo test --locked
run cargo test --locked --features budget
run cargo clippy --locked --all-targets --features budget -- -D warnings -A clippy::unnecessary_fold -A clippy::unnecessary_sort_by
echo "html-budget-fork-tests: OK"
