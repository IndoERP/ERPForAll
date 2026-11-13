#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
# Run on an approved build host only; the laptop never compiles or executes tests.
set -euo pipefail
cd "$(dirname "$0")/.."
command -v wasm-bindgen >/dev/null
eval "$(scripts/web-toolchain.sh env)"
scripts/web-toolchain.sh check
command -v node >/dev/null
out="${CARGO_TARGET_DIR:-target}/barcode-browser"
cargo rustc --locked --release -p erp-addon-barcodes --no-default-features --features browser --target wasm32-unknown-unknown --target-dir "$out" --crate-type cdylib
wasm-bindgen "$out/wasm32-unknown-unknown/release/erp_addon_barcodes.wasm" --target nodejs --out-dir "$out/binding"
wasm-bindgen "$out/wasm32-unknown-unknown/release/erp_addon_barcodes.wasm" --target web --out-dir "$out/web-binding"
node web/scripts/barcode-wasm-conformance.cjs "$out/binding/erp_addon_barcodes.js" "$out/web-binding"
