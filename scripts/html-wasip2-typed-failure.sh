#!/usr/bin/env bash
# Build the erp-html typed-failure guest for wasm32-wasip2 and run it through wasmtime
# (CI step html-wasip2-typed-failure). The test is #[ignore]d in
# the workspace run and fails, never skips, without the artefact.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
target_dir="target/html-wasip2"
cargo build --locked --release -p erp-html --features wasip2-probe --bin wasip2_typed_failure --target wasm32-wasip2 --target-dir "$target_dir"
artefact="$ROOT/$target_dir/wasm32-wasip2/release/wasip2_typed_failure.wasm"
[ -s "$artefact" ] || { echo "html-wasip2-typed-failure: no component at $artefact" >&2; exit 1; }
out="$(mktemp)"
trap 'rm -f "$out"' EXIT
ERP_HTML_WASIP2_TYPED_FAILURE_WASM="$artefact" \
  cargo test --locked -p erp-wasm-host --test wasip2_typed_failure -- --ignored --exact \
  node_limit_refusal_is_a_typed_failure_in_a_wasip2_component 2>&1 | tee "$out"
# an --exact filter that matches nothing exits 0; the step must see the test actually run
grep -Eq '^test result: ok\. 1 passed;' "$out" || { echo "html-wasip2-typed-failure: the test did not run and pass (no '1 passed')" >&2; exit 1; }
