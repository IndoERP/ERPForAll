#!/usr/bin/env bash
# The pinned Node and pnpm of ERPForAll's web workspace, installed for
# this project only under ~/.erpforall/toolchain (override: ERPFORALL_TOOLCHAIN). Nothing system-wide.
#
#   scripts/web-toolchain.sh install   download, check the pinned SHA-256, unpack (idempotent)
#   scripts/web-toolchain.sh check     both binaries present at exactly the pinned versions, and the pins
#                                      agree with web/package.json; exit 1 with the fix otherwise
#   scripts/web-toolchain.sh env       print the PATH line: eval "$(scripts/web-toolchain.sh env)"
#   scripts/web-toolchain.sh pins      print the pins
#
# Any other platform is refused.
set -euo pipefail

NODE_VERSION="24.22.0"
PNPM_VERSION="10.34.7"
# SHA-256 of the official tarballs (https://nodejs.org/dist/v24.22.0/SHASUMS256.txt) and of the pnpm npm
# tarball (whose sha512 matches the registry's dist.integrity).
NODE_SHA256_LINUX_X64="28ea0a438de9a13bd7891fd758eee6c7819ee8af5442dfd8d45bd65e56d4228c"
NODE_SHA256_DARWIN_ARM64="6a9724d9b50a5f22b4816cded441b5a8d9ddae546191a5e5b7efa5a5e2ee26d5"
PNPM_SHA256="c729f6538e005d234889910466fd1c4c44affb0b28ea86d93fcf88aa1aefa694"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TOOLCHAIN="${ERPFORALL_TOOLCHAIN:-$HOME/.erpforall/toolchain}"
# Download sources; only tests point these elsewhere (a local directory served over file://).
NODE_DIST="${WEB_TOOLCHAIN_NODE_DIST:-https://nodejs.org/dist}"
PNPM_DIST="${WEB_TOOLCHAIN_PNPM_DIST:-https://registry.npmjs.org/pnpm/-}"

die() { echo "web-toolchain: $*" >&2; exit 1; }

platform() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os/$arch" in
    Linux/x86_64) echo "linux-x64" ;;
    Darwin/arm64) echo "darwin-arm64" ;;
    *) die "unsupported platform $os/$arch (pinned for Linux x86_64 and macOS arm64)" ;;
  esac
}

node_sha() {
  case "$1" in
    linux-x64) echo "$NODE_SHA256_LINUX_X64" ;;
    darwin-arm64) echo "$NODE_SHA256_DARWIN_ARM64" ;;
  esac
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

node_dir() { echo "$TOOLCHAIN/node-v${NODE_VERSION}-$(platform)"; }
pnpm_dir() { echo "$TOOLCHAIN/pnpm-${PNPM_VERSION}"; }
bin_dir() { echo "$TOOLCHAIN/bin-${NODE_VERSION}-${PNPM_VERSION}"; }

fetch() {
  local url="$1" out="$2" want="$3" got
  curl -fsSL --retry 2 --max-time 300 -o "$out" "$url" || die "download failed: $url"
  got="$(sha256_of "$out")"
  [ "$got" = "$want" ] || die "SHA-256 mismatch for $url: got $got, pinned $want"
}

INSTALL_TMP=""
cleanup_install() { [ -z "$INSTALL_TMP" ] || rm -rf "$INSTALL_TMP"; }

install() {
  local plat tmp
  plat="$(platform)"
  mkdir -p "$TOOLCHAIN"
  tmp="$(mktemp -d "$TOOLCHAIN/.install.XXXXXX")"
  INSTALL_TMP="$tmp"
  trap cleanup_install EXIT
  if [ ! -x "$(node_dir)/bin/node" ]; then
    local name="node-v${NODE_VERSION}-${plat}"
    fetch "$NODE_DIST/v${NODE_VERSION}/${name}.tar.gz" "$tmp/node.tar.gz" "$(node_sha "$plat")"
    tar -xzf "$tmp/node.tar.gz" -C "$tmp"
    [ -x "$tmp/$name/bin/node" ] || die "the Node tarball has no $name/bin/node"
    rm -rf "$(node_dir)"
    mv "$tmp/$name" "$(node_dir)"
    echo "installed node $NODE_VERSION in $(node_dir)"
  fi
  if [ ! -f "$(pnpm_dir)/bin/pnpm.cjs" ]; then
    fetch "$PNPM_DIST/pnpm-${PNPM_VERSION}.tgz" "$tmp/pnpm.tgz" "$PNPM_SHA256"
    mkdir "$tmp/pnpm"
    tar -xzf "$tmp/pnpm.tgz" -C "$tmp/pnpm"
    [ -f "$tmp/pnpm/package/bin/pnpm.cjs" ] || die "the pnpm tarball has no package/bin/pnpm.cjs"
    rm -rf "$(pnpm_dir)"
    mv "$tmp/pnpm/package" "$(pnpm_dir)"
    echo "installed pnpm $PNPM_VERSION in $(pnpm_dir)"
  fi
  # One bin directory per pin pair: node, npm (Node's own, unused by CI) and a pnpm wrapper bound to this
  # node, so PATH order cannot pick another pnpm or node.
  local bin
  bin="$(bin_dir)"
  mkdir -p "$tmp/bin"
  ln -s "$(node_dir)/bin/node" "$tmp/bin/node"
  printf '#!/bin/sh\nexec "%s" "%s" "$@"\n' "$(node_dir)/bin/node" "$(pnpm_dir)/bin/pnpm.cjs" > "$tmp/bin/pnpm"
  chmod 755 "$tmp/bin/pnpm"
  rm -rf "$bin"
  mv "$tmp/bin" "$bin"
  check
}

# web/package.json allows any Node of the pinned major from the pin on (engines, so a plain `pnpm install`
# works for addon authors on a later patch release, review M9); web/.node-version holds the exact pin
# CI and the build hosts run.
pins_match_package_json() {
  local workspace pkg nv
  for workspace in web; do
  pkg="$ROOT/$workspace/package.json"
  nv="$ROOT/$workspace/.node-version"
  [ -f "$pkg" ] || die "$pkg is missing"
  [ -f "$nv" ] || die "$nv is missing"
  local pm engines want_engines exact
  pm="$(sed -n 's/^ *"packageManager": *"\([^"]*\)".*/\1/p' "$pkg")"
  engines="$(sed -n '/"engines"/,/}/s/^ *"node": *"\([^"]*\)".*/\1/p' "$pkg")"
  want_engines=">=${NODE_VERSION} <$(( ${NODE_VERSION%%.*} + 1 ))"
  exact="$(cat "$nv")"
  [ "$pm" = "pnpm@${PNPM_VERSION}" ] || die "$workspace/package.json packageManager is '$pm', the script pins pnpm@${PNPM_VERSION}"
  [ "$engines" = "$want_engines" ] || die "$workspace/package.json engines.node is '$engines', the script pins '$want_engines'"
  [ "$exact" = "$NODE_VERSION" ] || die "$workspace/.node-version is '$exact', the script pins $NODE_VERSION"
  done
}

check() {
  local bin got
  bin="$(bin_dir)"
  pins_match_package_json
  [ -x "$bin/node" ] && [ -x "$bin/pnpm" ] \
    || die "toolchain missing in $bin (node $NODE_VERSION, pnpm $PNPM_VERSION): run scripts/web-toolchain.sh install"
  got="$("$bin/node" --version)" || die "$bin/node does not run"
  [ "$got" = "v${NODE_VERSION}" ] || die "$bin/node reports $got, pinned v${NODE_VERSION}: run scripts/web-toolchain.sh install"
  got="$("$bin/pnpm" --version)" || die "$bin/pnpm does not run"
  [ "$got" = "$PNPM_VERSION" ] || die "$bin/pnpm reports $got, pinned $PNPM_VERSION: run scripts/web-toolchain.sh install"
  echo "web toolchain ok: node v${NODE_VERSION}, pnpm ${PNPM_VERSION} ($bin)"
}

case "${1:-}" in
  install) install ;;
  check) check ;;
  env) printf 'export PATH=%q:"$PATH"\n' "$(bin_dir)" ;;
  pins) printf 'node %s\npnpm %s\n' "$NODE_VERSION" "$PNPM_VERSION" ;;
  *) sed -n '2,12p' "${BASH_SOURCE[0]}" >&2; exit 2 ;;
esac
