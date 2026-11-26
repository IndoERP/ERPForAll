#!/usr/bin/env bash
# Local checks for contributors: formatting, layer check, lints; --tests adds tests; --affected narrows to changed packages.
#
#   scripts/check.sh                         cargo fmt --check, layer check, clippy (--workspace)
#   scripts/check.sh --tests                 also cargo nextest and doctests (--workspace; needs DATABASE_URL)
#   scripts/check.sh --affected [BASE]       narrow clippy/tests to packages affected vs BASE (default: origin/main)
#   scripts/check.sh -p <pkg> [--tests]      narrow clippy/tests to explicit Cargo package(s) (repeatable)
#   scripts/check.sh --committed-only        with --affected: diff BASE...HEAD only (CI mode)
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

run_tests=0
affected=0
affected_base="origin/main"
committed_only=0
explicit_pkgs=()

while [ "$#" -gt 0 ]; do
  case "$1" in
    --tests) run_tests=1 ;;
    --committed-only) committed_only=1 ;;
    --affected)
      affected=1
      if [ "$#" -ge 2 ] && [[ "$2" != -* ]]; then
        affected_base="$2"
        shift
      fi
      ;;
    -p|--package)
      [ "$#" -ge 2 ] || { echo "check.sh: $1 requires a package name" >&2; exit 2; }
      explicit_pkgs+=("$2")
      shift
      ;;
    -h|--help) sed -n '2,8p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "check.sh: unknown option $1" >&2; exit 2 ;;
  esac
  shift
done

step() { echo "== $*"; "$@"; }

if [ "${#explicit_pkgs[@]}" -gt 0 ] && [ "$affected" = 1 ]; then
  echo "check.sh: combine either --affected or -p, not both" >&2
  exit 2
fi

cargo_scope=(--workspace)
run_rust=1

if [ "${#explicit_pkgs[@]}" -gt 0 ]; then
  cargo_scope=()
  for p in "${explicit_pkgs[@]}"; do
    cargo_scope+=(-p "$p")
  done
elif [ "$affected" = 1 ]; then
  plan_args=(--base "$affected_base" --format shell)
  [ "$committed_only" = 0 ] || plan_args+=(--committed-only)
  plan_out="$(python3 -I scripts/affected-packages.py "${plan_args[@]}")"
  eval "$plan_out"
  echo "== affected plan: mode=$MODE run_rust=$RUN_RUST run_web=$RUN_WEB ($REASON)"
  if [ "$RUN_RUST" = 0 ]; then
    run_rust=0
  elif [ "$MODE" = "affected" ] && [ "${#RUST_PKGS[@]}" -gt 0 ]; then
    cargo_scope=()
    for p in "${RUST_PKGS[@]}"; do
      cargo_scope+=(-p "$p")
    done
  fi
fi

if [ "$run_rust" = 1 ]; then
  if [ -f .cargo/debug-cache-limit-gib ]; then
    # This checkout opted in. Also retire completed harnesses after a failing verification.
    step python3 -I scripts/trim-debug-tests.py
    trap 'python3 -I scripts/trim-debug-tests.py' EXIT
  fi
  step python3 -I scripts/check-layers.py
  step cargo fmt --all --check
  step cargo clippy --locked "${cargo_scope[@]}" --all-targets -- -D warnings
  if [ "$run_tests" = 1 ]; then
    # unoptimized test builds put the module install chain on deep frames: the default 2 MiB test thread overflows
    export RUST_MIN_STACK="${RUST_MIN_STACK:-16777216}"
    step cargo nextest run --locked "${cargo_scope[@]}" --no-fail-fast
    step cargo test --locked "${cargo_scope[@]}" --doc
  fi
else
  echo "== no Rust packages affected; skipping Rust checks"
fi
echo "check: ok"
