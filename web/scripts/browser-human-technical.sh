#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
# Real-server browser evidence for Settings > Technical (review T3).
#
# Build host only (never the laptop). It:
#   1. builds the `erp` server binary, the template compiler and the compiled client (web addon);
#   2. creates a fresh database and a runtime role on the cluster named by DATABASE_URL;
#   3. initializes base with `erp db init` and a generated administrator (a base.group_system user,
#      not the superuser);
#   4. serves it with `erp server` on loopback;
#   5. drives headless Chrome through every Technical entry with browser-human-technical.mjs;
#   6. stops the server and drops the database and the role, also on failure.
# No fixture server, request interception or fabricated RPC. Generated secrets stay in a mode-700
# temporary directory and are never printed.
#
# Usage: DATABASE_URL=postgres://<owner>:<password>@<host>:<port>/<db> \
#          web/scripts/browser-human-technical.sh <output-dir> [--allow-dirty]
# The DATABASE_URL role needs CREATEDB and CREATEROLE (the dev cluster's `erpforall` role).
# Without --allow-dirty the run refuses a source tree whose REVISION is dirty.
set -euo pipefail
umask 077

die() { echo "browser-human-technical: $*" >&2; exit 1; }

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
OUT="${1:-}"
[ -n "$OUT" ] || die "usage: $0 <output-dir> [--allow-dirty]"
ALLOW_DIRTY=0
if [ "$#" -ge 2 ]; then
  [ "$2" = "--allow-dirty" ] && [ "$#" -eq 2 ] || die "unknown arguments after the output directory"
  ALLOW_DIRTY=1
fi
[ -n "${DATABASE_URL:-}" ] || die "DATABASE_URL (owner connection of the test cluster) is required"

if [ -s REVISION ]; then
  REVISION_FULL="$(head -n 1 REVISION)"
else
  REVISION_FULL="$(git rev-parse HEAD)$( [ -z "$(git status --porcelain --untracked-files=no)" ] || echo -dirty)"
fi
case "$REVISION_FULL" in
  *-dirty*) [ "$ALLOW_DIRTY" = 1 ] || die "source revision $REVISION_FULL is dirty; evidence needs a clean committed tree" ;;
esac
echo "== browser-human-technical revision=$REVISION_FULL host=$(hostname) started=$(date -u +%Y-%m-%dT%H:%M:%SZ)"

eval "$(scripts/web-toolchain.sh env)"
echo "== node $(node --version)"
CHROME_BIN="${CHROME_BIN:-google-chrome}"
export CHROME_BIN
echo "== chrome $("$CHROME_BIN" --version)"
command -v psql >/dev/null || die "psql is required"

echo "== build: cargo build --locked -p erp-cli --bin erp -p erp-ui-compiler-cli --bin erp-ui-compile"
cargo build --locked -p erp-cli --bin erp -p erp-ui-compiler-cli --bin erp-ui-compile
TARGET_DIR="$(cargo metadata --locked --format-version 1 --no-deps | python3 -I -c 'import json,sys;print(json.load(sys.stdin)["target_directory"])')"
ERP_BIN="$TARGET_DIR/debug/erp"
[ -x "$ERP_BIN" ] || die "server binary missing at $ERP_BIN"
echo "== build: pnpm --dir web install --frozen-lockfile"
pnpm --dir web install --frozen-lockfile
echo "== build: node web/scripts/build-human-ui.mjs --addons web"
ERP_UI_COMPILER="$TARGET_DIR/debug/erp-ui-compile" node web/scripts/build-human-ui.mjs --addons web
MANIFEST="$ROOT/web/dist/human-ui/manifest.json"
[ -s "$MANIFEST" ] || die "compiled client manifest missing"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/erp-m4tech-browser.XXXXXX")"
DB="erp_m4tech_browser_$(date -u +%Y%m%d%H%M%S)_$$"
ROLE="${DB}_web"
SERVER_PID=""
DB_CREATED=0
ROLE_CREATED=0
# The owner URL with the maintenance database replaced; values are percent-encoded by Python.
owner_url() {
  python3 -I - "$1" <<'PY'
import sys, urllib.parse
url = urllib.parse.urlsplit(__import__("os").environ["DATABASE_URL"])
if url.scheme not in ("postgres", "postgresql") or not url.hostname or not url.username:
    sys.exit("DATABASE_URL must be postgres://user:password@host:port/db")
print(urllib.parse.urlunsplit((url.scheme, url.netloc, "/" + sys.argv[1], "", "")))
PY
}
# The password comes from the environment (RUNTIME_PASSWORD), never from argv.
runtime_url() {
  python3 -I - "$1" "$2" <<'PY'
import os, sys, urllib.parse
url = urllib.parse.urlsplit(os.environ["DATABASE_URL"])
role, database = sys.argv[1:3]
password = os.environ["RUNTIME_PASSWORD"]
host = url.hostname if ":" not in url.hostname else f"[{url.hostname}]"
netloc = f"{urllib.parse.quote(role, safe='')}:{urllib.parse.quote(password, safe='')}@{host}:{url.port or 5432}"
print(urllib.parse.urlunsplit((url.scheme, netloc, "/" + database, "", "")))
PY
}
# psql connects through PG* variables derived from DATABASE_URL, so no password reaches argv.
eval "$(python3 -I - <<'PY'
import os, shlex, urllib.parse
url = urllib.parse.urlsplit(os.environ["DATABASE_URL"])
if url.scheme not in ("postgres", "postgresql") or not url.hostname or not url.username or not url.path.strip("/"):
    raise SystemExit("DATABASE_URL must be postgres://user:password@host:port/db")
values = {"PGHOST": url.hostname, "PGPORT": str(url.port or 5432), "PGUSER": urllib.parse.unquote(url.username),
          "PGPASSWORD": urllib.parse.unquote(url.password or ""), "PGDATABASE": url.path.strip("/")}
print(" ".join(f"export {k}={shlex.quote(v)};" for k, v in values.items()))
PY
)"
admin_sql() { psql -X -v ON_ERROR_STOP=1 -q -c "$1"; }
cleanup() {
  local status=$?
  if [ -n "$SERVER_PID" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
    kill "$SERVER_PID"
    wait "$SERVER_PID" 2>/dev/null || true # its exit status after our TERM is expected nonzero
  fi
  if [ "$DB_CREATED" = 1 ]; then
    admin_sql "DROP DATABASE IF EXISTS \"$DB\" WITH (FORCE)" || { echo "cleanup: could not drop database $DB" >&2; status=1; }
  fi
  if [ "$ROLE_CREATED" = 1 ]; then
    admin_sql "DROP ROLE IF EXISTS \"$ROLE\"" || { echo "cleanup: could not drop role $ROLE" >&2; status=1; }
  fi
  rm -rf -- "$WORK"
  echo "== browser-human-technical finished status=$status"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 143' TERM
trap 'exit 130' INT

echo "== database $DB (fresh, template0, C collation, pg_trgm) and runtime role $ROLE"
admin_sql "CREATE DATABASE \"$DB\" TEMPLATE template0 ENCODING 'UTF8' LC_COLLATE 'C'"
DB_CREATED=1
PGDATABASE="$DB" psql -X -v ON_ERROR_STOP=1 -q -c "CREATE EXTENSION IF NOT EXISTS pg_trgm"
RUNTIME_PASSWORD="$(openssl rand -hex 24)"
# The generated password is passed on stdin so it never appears in a process listing.
printf "CREATE ROLE \"%s\" LOGIN PASSWORD '%s'\n" "$ROLE" "$RUNTIME_PASSWORD" | psql -X -v ON_ERROR_STOP=1 -q
ROLE_CREATED=1

PORT="$(python3 -I -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')"
owner_url "$DB" > "$WORK/owner-url"
RUNTIME_PASSWORD="$RUNTIME_PASSWORD" runtime_url "$ROLE" "$DB" > "$WORK/runtime-url"
unset RUNTIME_PASSWORD
openssl rand -hex 24 > "$WORK/admin-password"
chmod 600 "$WORK/owner-url" "$WORK/runtime-url" "$WORK/admin-password"
cat > "$WORK/server.toml" <<EOF
database = "$DB"
database_url_file = "runtime-url"
owner_database_url_file = "owner-url"
bind = "127.0.0.1:$PORT"
public_origin = "http://127.0.0.1:$PORT"
proxy_mode = "direct"
addons_path = ["$ROOT/addons"]
assets_manifest = "$MANIFEST"
EOF

ADMIN_LOGIN="technical-admin"
echo "== erp db init --config <work>/server.toml --admin-login $ADMIN_LOGIN"
"$ERP_BIN" db init --config "$WORK/server.toml" --admin-login "$ADMIN_LOGIN" --admin-password-file "$WORK/admin-password"
echo "== erp server --config <work>/server.toml (127.0.0.1:$PORT)"
"$ERP_BIN" server --config "$WORK/server.toml" > "$WORK/server.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 600); do
  if python3 -I -c 'import socket,sys;socket.create_connection(("127.0.0.1",int(sys.argv[1])),timeout=1).close()' "$PORT" 2>/dev/null; then
    break
  fi
  kill -0 "$SERVER_PID" 2>/dev/null || { cat "$WORK/server.log" >&2; die "server exited before listening"; }
  sleep 0.2
done
python3 -I -c 'import socket,sys;socket.create_connection(("127.0.0.1",int(sys.argv[1])),timeout=1).close()' "$PORT" \
  || { cat "$WORK/server.log" >&2; die "server did not listen within 120 s"; }

echo "== node web/scripts/browser-human-technical.mjs --origin http://127.0.0.1:$PORT --login $ADMIN_LOGIN --output $OUT"
node web/scripts/browser-human-technical.mjs \
  --origin "http://127.0.0.1:$PORT" \
  --login "$ADMIN_LOGIN" \
  --password-file "$WORK/admin-password" \
  --technical-xml "$ROOT/addons/base/data/technical_views.xml" \
  --output "$OUT"
echo "== server log (stderr/stdout of erp server)"
cat "$WORK/server.log"
