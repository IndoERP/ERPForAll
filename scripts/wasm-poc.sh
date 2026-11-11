#!/usr/bin/env bash
# build and package the sample WebAssembly addons with the erp CLI, then run the
# runtime-proof tests of crates/erp-cli/tests/wasm_addon_poc.rs against them.
set -euo pipefail
case "${1:-}" in
  "" | --startup-catalogue) ;;
  *) echo "usage: scripts/wasm-poc.sh [--startup-catalogue]" >&2; exit 64 ;;
esac
if [ "$#" -gt 1 ]; then echo "wasm-poc: expected at most one mode" >&2; exit 64; fi
cd "$(dirname "$0")/.."
OUT="${ERP_POC_PACKAGES:-$(mktemp -d)}"
mkdir -p "$OUT"
cargo build --locked -p erp-cli
ERP="${CARGO_TARGET_DIR:-target}/debug/erp"
# the sample sources live in examples/wasm, never in addons/ (the release copies addons/ into the
# server's addon path); a name is looked up in both
PATHS=addons,examples/wasm
for addon in sample_library_wasm sample_library_wasm_policy; do
  "$ERP" addon build "$addon" --target wasm --addons-path "$PATHS"
  "$ERP" addon package "$addon" --addons-path "$PATHS" -o "$OUT/$addon.erpaddon"
done
# Required native admission rehearsal; ordinary workspace nextest cannot build its guest fixtures.
if [ "${1:-}" = --startup-catalogue ]; then
  export ERP_POC_PACKAGES="$OUT"
  cargo nextest run --locked -p erp-cli --test wasm_addon_poc --run-ignored only --no-tests=fail -E 'test(native_startup_catalogue_)'
  exit 0
fi
# The exact preceding XML/version, using unchanged compiled method code, for the coordinated header upgrade.
PREVIOUS_WORK="$(mktemp -d)"
cp -R examples/wasm/sample_library_wasm "$PREVIOUS_WORK/sample_library_wasm"
cp crates/erp-cli/tests/fixtures/office-blue-previous/wasm-manifest.toml "$PREVIOUS_WORK/sample_library_wasm/manifest.toml"
cp crates/erp-cli/tests/fixtures/office-blue-previous/wasm-views.xml "$PREVIOUS_WORK/sample_library_wasm/views/sample_library_wasm_views.xml"
"$ERP" addon package "$PREVIOUS_WORK/sample_library_wasm" --addons-path "$PATHS" --wasm examples/wasm/sample_library_wasm/wasm/sample_library_wasm.wasm -o "$OUT/sample_library_wasm-previous.erpaddon"
# a newer version of package 1 for the upgrade step: a private copy with the next minor version
# of its manifest (the tests open it as sample_library_wasm-next)
WORK="$(mktemp -d)"
cp -R examples/wasm/sample_library_wasm "$WORK/sample_library_wasm"
python3 -I - "$WORK/sample_library_wasm/manifest.toml" <<'PY'
import re, sys
p = sys.argv[1]
s = open(p).read()
m = re.search(r'^version = "(\d+)\.(\d+)\.(\d+)"$', s, re.M)
if m is None:
    sys.exit(f"{p}: no version = \"X.Y.Z\" line")
major, minor = int(m.group(1)), int(m.group(2))
open(p, "w").write(s[:m.start()] + f'version = "{major}.{minor + 1}.0"' + s[m.end():])
PY
"$ERP" addon package "$WORK/sample_library_wasm" --addons-path "$PATHS" --wasm examples/wasm/sample_library_wasm/wasm/sample_library_wasm.wasm -o "$OUT/sample_library_wasm-next.erpaddon"
export ERP_POC_PACKAGES="$OUT"
cargo nextest run --locked -p erp-cli --test wasm_addon_poc --run-ignored only --no-fail-fast
