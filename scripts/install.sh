#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
# One-command local install of ERPForAll with demo data: build, PostgreSQL 18, database, web client, server.
# It automates the manual steps and changes nothing outside the state directory, the
# checkout's build outputs, and the PostgreSQL objects it creates (and records, so --reset can remove them).
# Run `scripts/install.sh --help` for the flags.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Business modules installed with demo data after the base. Chosen from the applications the README lists as
# working: accounting (account, account_reporting), inventory (stock), sales (sale, sale_management),
# purchase (purchase), the two bridges to inventory (sale_stock, purchase_stock), contacts, and the chatter
# addons of those applications. Dependencies (uom, product, mail, ...) install with them.
DEFAULT_MODULES="contacts approvals hr account account_reporting stock sale sale_management purchase sale_stock purchase_stock account_mail purchase_mail stock_mail product_mail"

STATE="${ERPFORALL_LOCAL_STATE:-$HOME/.erpforall/local}"
PORT=8069
DB_NAME=erpforall_local
CONTAINER=erpforall-local-postgres
MODULES="$DEFAULT_MODULES"
PROFILE="release"
REBUILD=0
CLEAN_CACHE=0
RESET=0
YES=0
DRY=0
ARG_DB=""
ARG_PORT=""
ARG_PG_USER=""
ARG_PG_HOST=""
ARG_PG_PORT=""
ARG_CONTAINER=""
HEALTH_WAIT_SECS=180
CURRENT_STEP="start"

usage() {
  cat <<'EOF'
Usage: scripts/install.sh [flags]

Builds ERPForAll, creates a PostgreSQL 18 database with demo data, builds the web client and starts the
server on http://127.0.0.1:8069 . Run it from a checkout. Supported: Linux x86_64 and macOS arm64.
A second run reuses the state and only starts what is not running.

PostgreSQL 18 is taken from, in this order:
  1. a server you name with --pg-user (or PGUSER): a superuser of an existing local PostgreSQL 18, reached by
     psql (PGHOST default 127.0.0.1, PGPORT default 5432, PGPASSWORD from the environment);
  2. otherwise a container of postgres:18 that this script starts on a free loopback port, with a volume of
     its own (needs Docker);
  3. otherwise it stops and says what to install.
It never touches a database, role or container it did not create.

Flags:
  --state-dir DIR     where everything it owns lives (default ~/.erpforall/local, or ERPFORALL_LOCAL_STATE)
  --port N            server port on 127.0.0.1 (default 8069)
  --db-name NAME      database name; the roles are NAME_owner, NAME_web, NAME_node, NAME_maint
                      (default erpforall_local)
  --pg-user USER      superuser of an existing PostgreSQL 18 (selects option 1; also PGUSER)
  --pg-host HOST      its TCP host (also PGHOST); a socket directory is not accepted
  --pg-port PORT      its port (also PGPORT)
  --pg-container NAME name of the container of option 2 (default erpforall-local-postgres)
  --modules LIST      comma- or space-separated modules to install with demo data
                      (default: contacts hr account account_reporting stock sale sale_management purchase
                      sale_stock purchase_stock account_mail purchase_mail stock_mail product_mail)
  --fast              build with Cargo's incremental `local` profile (faster rebuilds, smaller cache)
  --rebuild           incrementally rebuild changed Rust/web code, sync addons, and restart without resetting DB
                      (it upgrades no module: the install guide says how to apply changed views and data)
  --clean-cache       remove stale debug/tmp caches and old hashed web bundles while keeping the active binary cache
  --reset             stop the server and remove only what this script created (listed first); needs --yes
  --yes               confirm --reset
  --dry-run           print the steps and the checks that would fail, change nothing
  -h, --help          this text

Files (mode 600, in DIR/private): owner-url, request-url, node-url, admin-password, server.toml.
The administrator login is "admin"; its password is in DIR/private/admin-password and is never printed.
Stop the server with: kill "$(cat DIR/server.pid)"
EOF
}

say() { printf '%s\n' "$*"; }
step() { CURRENT_STEP="$1"; printf '\n==> %s\n' "$1"; }
die() { printf 'install: %s\n' "$*" >&2; exit 1; }
# In a dry run a failed check is reported and the run goes on; otherwise it stops.
refuse() { if [ "$DRY" = 1 ]; then printf '  WOULD REFUSE: %s\n' "$*"; return 0; fi; die "$*"; }
dry() { [ "$DRY" = 1 ] || return 1; printf '  [dry-run] %s\n' "$*"; }
on_exit() { local rc=$?; [ "$rc" = 0 ] || printf '\ninstall: stopped during "%s" (exit %s). Fix the cause and run again, or run with --reset.\n' "$CURRENT_STEP" "$rc" >&2; }
trap on_exit EXIT

# ---- arguments ----------------------------------------------------------------------------------
need_value() { [ "$#" -ge 2 ] && [ -n "$2" ] || { printf 'install: %s needs a value (try --help)\n' "$1" >&2; exit 2; }; }
while [ "$#" -gt 0 ]; do
  case "$1" in
    --state-dir) need_value "$@"; STATE="$2"; shift 2 ;;
    --port) need_value "$@"; ARG_PORT="$2"; shift 2 ;;
    --db-name) need_value "$@"; ARG_DB="$2"; shift 2 ;;
    --pg-user) need_value "$@"; ARG_PG_USER="$2"; shift 2 ;;
    --pg-host) need_value "$@"; ARG_PG_HOST="$2"; shift 2 ;;
    --pg-port) need_value "$@"; ARG_PG_PORT="$2"; shift 2 ;;
    --pg-container) need_value "$@"; ARG_CONTAINER="$2"; shift 2 ;;
    --modules) need_value "$@"; MODULES="$(printf '%s' "$2" | tr ',' ' ')"; shift 2 ;;
    --fast) PROFILE="local"; shift ;;
    --rebuild) REBUILD=1; shift ;;
    --clean-cache) CLEAN_CACHE=1; shift ;;
    --reset) RESET=1; shift ;;
    --yes) YES=1; shift ;;
    --dry-run) DRY=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) printf 'install: unknown flag %s (try --help)\n' "$1" >&2; exit 2 ;;
  esac
done
[ -z "$ARG_PORT" ] || PORT="$ARG_PORT"
[ -z "$ARG_DB" ] || DB_NAME="$ARG_DB"
[ -z "$ARG_CONTAINER" ] || CONTAINER="$ARG_CONTAINER"
case "$PORT" in ''|*[!0-9]*) die "--port must be a number, got '$PORT'" ;; esac
{ [ "$PORT" -ge 1024 ] && [ "$PORT" -le 65535 ]; } || die "--port must be between 1024 and 65535, got $PORT"
printf '%s' "$DB_NAME" | grep -Eq '^[a-z][a-z0-9_]{0,40}$' \
  || die "--db-name must be lower-case letters, digits and underscores, starting with a letter, at most 41 characters; got '$DB_NAME'"
printf '%s' "$CONTAINER" | grep -Eq '^[A-Za-z0-9][A-Za-z0-9_.-]{0,60}$' || die "--pg-container '$CONTAINER' is not a valid container name"
for m in $MODULES; do printf '%s' "$m" | grep -Eq '^[a-z][a-z0-9_]*$' || die "--modules: '$m' is not a module name"; done
case "$STATE$ROOT" in *"'"*|*[[:space:]]*) die "the state directory and the checkout path must not contain spaces or quotes (server.toml quotes paths); got state '$STATE', checkout '$ROOT'" ;; esac
case "$STATE" in /*) ;; *) die "--state-dir must be an absolute path, got '$STATE'" ;; esac

# ---- state --------------------------------------------------------------------------------------
PRIV="$STATE/private"
STATE_FILE="$STATE/state.env"
set_state() { # key value: record a fact the script owns, so a second run and --reset can rely on it
  local t="$STATE_FILE.tmp"
  { if [ -f "$STATE_FILE" ]; then grep -v "^$1=" "$STATE_FILE" || true; fi; printf '%s=%q\n' "$1" "$2"; } > "$t"
  mv "$t" "$STATE_FILE"
}
# The copy of addons/ is replaced with --delete, so the target must be this script's own state directory: one that
# holds the state file it writes (PG_MODE is recorded before the first copy) and is not the checkout itself.
sync_addons() {
  { [ -f "$STATE_FILE" ] && grep -q '^PG_MODE=' "$STATE_FILE"; } \
    || die "$STATE holds no state file of this script ($STATE_FILE); refusing to replace $STATE/addons"
  [ "$(cd "$STATE" && pwd -P)" != "$(cd "$ROOT" && pwd -P)" ] || die "the state directory must not be the checkout ($ROOT)"
  rsync -a --delete --exclude node_modules --exclude target "$ROOT/addons/" "$STATE/addons/"
}
secret_write() { ( umask 077; cat > "$1" ); chmod 600 "$1"; }
rand_hex() { head -c "$1" /dev/urandom | od -An -tx1 | tr -d ' \n'; }

PG_MODE=""; PG_HOST=""; PG_PORT=""; PG_VOLUME=""; PG_CREATED=""; DATABASE_PHASE=""; BUILD_SECONDS=""
if [ -f "$STATE_FILE" ]; then
  prev_db="$DB_NAME"; prev_port="$PORT"
  # shellcheck disable=SC1090
  . "$STATE_FILE"
  [ -z "$ARG_DB" ] || [ "$ARG_DB" = "$DB_NAME" ] || die "$STATE was installed with database $DB_NAME; use --reset first to change it (asked for $ARG_DB)"
  [ -z "$ARG_PORT" ] || [ "$ARG_PORT" = "$PORT" ] || die "$STATE was installed with port $PORT; use --reset first to change it (asked for $ARG_PORT)"
  : "$prev_db$prev_port"
fi
ROLE_OWNER="${DB_NAME}_owner"; ROLE_WEB="${DB_NAME}_web"; ROLE_NODE="${DB_NAME}_node"; ROLE_MAINT="${DB_NAME}_maint"
CFG="$PRIV/server.toml"
CARGO_PROFILE_DIR="$PROFILE"
ERP="$ROOT/target/$CARGO_PROFILE_DIR/erp"
ERP_UI_COMPILER_BIN="$ROOT/target/$CARGO_PROFILE_DIR/erp-ui-compile"

# ---- platform and helpers -----------------------------------------------------------------------
OS="${ERPFORALL_INSTALL_UNAME_S:-$(uname -s)}"   # the two overrides exist for tests/scripts/test_install.py
ARCH="${ERPFORALL_INSTALL_UNAME_M:-$(uname -m)}"
case "$OS/$ARCH" in
  Linux/x86_64|Darwin/arm64) ;;
  *) die "unsupported platform $OS/$ARCH. Supported: Linux x86_64 and macOS arm64 (Windows is not supported)." ;;
esac

host_ram_bytes() {
  if [ "$OS" = Darwin ]; then sysctl -n hw.memsize
  else awk '/^MemTotal:/ { print $2 * 1024 }' /proc/meminfo; fi
}
port_in_use() { ( exec 3<>"/dev/tcp/127.0.0.1/$1" ) 2>/dev/null; }
free_port() { local p="$1"; while port_in_use "$p"; do p=$((p + 1)); done; printf '%s' "$p"; }
server_pid() { # the pid of our server when it is running, else nothing
  local pid
  [ -f "$STATE/server.pid" ] || return 0
  pid="$(cat "$STATE/server.pid")"
  # A zombie (exited, not yet reaped by its parent) still answers kill -0, so the state column decides.
  case "$(ps -p "$pid" -o stat= 2>/dev/null)" in ''|Z*) return 0 ;; esac
  case "$(ps -p "$pid" -o comm= 2>/dev/null)" in *erp*) printf '%s' "$pid" ;; esac
}
has_line() { printf '%s\n' "$1" | grep -qx -- "$2"; }
http_code() { curl -s -o /dev/null -w '%{http_code}' --max-time 5 "$@" 2>/dev/null || true; }

pg_sql() { # SQL on stdin, as the superuser of the chosen server; extra args go to psql
  if [ "$PG_MODE" = docker ]; then
    docker exec -i "$CONTAINER" psql -X -q -v ON_ERROR_STOP=1 -U postgres -d postgres "$@"
  else
    psql -X -q -v ON_ERROR_STOP=1 -d postgres "$@"
  fi
}

# ---- 1. prerequisites ---------------------------------------------------------------------------
need() { # command, how to get it
  command -v "$1" >/dev/null 2>&1 || refuse "$1 is missing. $2"
}
check_prereqs() {
  step "1/8 Checking the platform ($OS $ARCH) and the tools"
  if [ -d "$HOME/.cargo/bin" ]; then PATH="$HOME/.cargo/bin:$PATH"; fi
  need git "Install git (for example: apt-get install git, or xcode-select --install)."
  need rsync "Install rsync (for example: apt-get install rsync)."
  need curl "Install curl (for example: apt-get install curl ca-certificates)."
  need tar "Install tar."
  need cc "Install a C toolchain (for example: apt-get install build-essential, or xcode-select --install)."
  need rustup "Install Rust with rustup: https://rustup.rs (the pinned version in rust-toolchain.toml is installed on the first build)."
  need cargo "Install Rust with rustup: https://rustup.rs"
  [ -x "$ROOT/scripts/web-toolchain.sh" ] || refuse "scripts/web-toolchain.sh is missing in $ROOT; run this script from a full checkout."
  say "  checked: git rsync curl tar cc rustup cargo"
}

# ---- 2. PostgreSQL choice -----------------------------------------------------------------------
choose_postgres() {
  step "2/8 Choosing PostgreSQL 18"
  if [ -n "$PG_MODE" ]; then
    say "  state says: $PG_MODE (reused)"
    if [ "$PG_MODE" = local ]; then export PGHOST="$PG_HOST" PGPORT="$PG_PORT"; [ -z "${ARG_PG_USER:-${PGUSER:-}}" ] || export PGUSER="${ARG_PG_USER:-$PGUSER}"; fi
    return 0
  fi
  local named="${ARG_PG_USER:-${PGUSER:-}}" probe
  if [ -n "$named" ]; then
    export PGUSER="$named"
    [ -z "$ARG_PG_HOST" ] || export PGHOST="$ARG_PG_HOST"
    [ -z "$ARG_PG_PORT" ] || export PGPORT="$ARG_PG_PORT"
    export PGHOST="${PGHOST:-127.0.0.1}" PGPORT="${PGPORT:-5432}"
    case "$PGHOST" in /*) refuse "PGHOST is a socket directory ($PGHOST); name a TCP host, because the server connects by URL." ;; esac
    command -v psql >/dev/null 2>&1 || { refuse "you named the PostgreSQL superuser $named but psql is missing. Install the PostgreSQL 18 client tools."; return 0; }
    probe="$(psql -X -At -d postgres -c "select current_setting('server_version_num')::int/10000 || '|' || (select rolsuper::text from pg_roles where rolname = current_user)" 2>&1)" \
      || { refuse "psql could not reach PostgreSQL at $PGHOST:$PGPORT as $named: $probe. Fix PGHOST/PGPORT/PGUSER/PGPASSWORD or drop --pg-user to use Docker."; return 0; }
    [ "$probe" = "18|true" ] || { refuse "the server at $PGHOST:$PGPORT answered '$probe' (expected version 18 and a superuser $named). ERPForAll needs PostgreSQL 18; use another server or drop --pg-user to use Docker."; return 0; }
    PG_MODE=local; PG_HOST="$PGHOST"; PG_PORT="$PGPORT"
    say "  using the local PostgreSQL 18 at $PG_HOST:$PG_PORT as superuser $named"
  elif command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
    PG_MODE=docker; PG_HOST=127.0.0.1; PG_VOLUME="${CONTAINER}-data"
    say "  no superuser named: a postgres:18 container '$CONTAINER' (volume '$PG_VOLUME') will be started on a free loopback port"
    if has_line "$(docker ps -a --format '{{.Names}}')" "$CONTAINER"; then
      refuse "a container named $CONTAINER already exists and was not created by this script. Pick another name with --pg-container."; return 0
    fi
    if has_line "$(docker volume ls -q)" "$PG_VOLUME"; then
      refuse "a Docker volume named $PG_VOLUME already exists and was not created by this script. Pick another name with --pg-container."; return 0
    fi
  else
    refuse "no PostgreSQL 18 available. Either install PostgreSQL 18 and name a superuser (--pg-user USER, PGHOST/PGPORT/PGPASSWORD), or install Docker and start it, then run again."
    PG_MODE=none
  fi
  return 0
}

# ---- 3. build -----------------------------------------------------------------------------------
build_binaries() {
  step "3/8 Building erp and erp-ui-compile ($PROFILE)"
  local cargo_flag=(--release)
  [ "$PROFILE" = release ] || cargo_flag=(--profile "$PROFILE")
  dry "cd $ROOT && cargo build ${cargo_flag[*]} -p erp-cli --bin erp -p erp-ui-compiler-cli --bin erp-ui-compile" && return 0
  local first=rebuild t0
  [ -x "$ERP" ] || first="first build"
  t0="$(date +%s)"
  ( cd "$ROOT" && cargo build "${cargo_flag[@]}" -p erp-cli --bin erp -p erp-ui-compiler-cli --bin erp-ui-compile )
  BUILD_SECONDS=$(( $(date +%s) - t0 ))
  [ -x "$ERP" ] && [ -x "$ERP_UI_COMPILER_BIN" ] || die "the build finished but $ERP or $ERP_UI_COMPILER_BIN is missing"
  say "  build time: ${BUILD_SECONDS}s ($first)"
}

# ---- 4. web client ------------------------------------------------------------------------------
build_web() {
  step "4/8 Building the web client"
  dry "scripts/web-toolchain.sh install; pnpm --dir web install --frozen-lockfile; node web/scripts/build-human-ui.mjs --addons web,approvals,reporting,stock_account,hr" && return 0
  ( cd "$ROOT"
    scripts/web-toolchain.sh install
    eval "$(scripts/web-toolchain.sh env)"
    pnpm --dir web install --frozen-lockfile
    ERP_UI_COMPILER="$ERP_UI_COMPILER_BIN" node web/scripts/build-human-ui.mjs --addons web,approvals,reporting,stock_account,hr )
  [ -f "$ROOT/web/dist/human-ui/manifest.json" ] || die "the web client build left no web/dist/human-ui/manifest.json"
}

# ---- 5. state directory, secrets, addons, configuration -----------------------------------------
prepare_state() {
  step "5/8 Writing the state directory $STATE"
  dry "mkdir $PRIV (700); generate role passwords and the admin password (mode 600); copy addons/ without node_modules and target; write server.toml" && return 0
  mkdir -p "$STATE"; chmod 700 "$STATE"
  mkdir -p "$PRIV"; chmod 700 "$PRIV"
  set_state DB_NAME "$DB_NAME"; set_state PORT "$PORT"; set_state PG_MODE "$PG_MODE"
  set_state PG_HOST "$PG_HOST"; set_state CONTAINER "$CONTAINER"; set_state PG_VOLUME "$PG_VOLUME"
  chmod 600 "$STATE_FILE"
  if [ "$PG_MODE" = docker ] && [ -z "$PG_PORT" ]; then PG_PORT="$(free_port 55432)"; fi
  set_state PG_PORT "$PG_PORT"
  if [ ! -f "$PRIV/owner-url" ]; then
    for pair in "$ROLE_OWNER:owner" "$ROLE_WEB:request" "$ROLE_NODE:node"; do
      printf 'postgres://%s:%s@%s:%s/%s\n' "${pair%%:*}" "$(rand_hex 24)" "$PG_HOST" "$PG_PORT" "$DB_NAME" | secret_write "$PRIV/${pair##*:}-url"
    done
  fi
  [ -f "$PRIV/admin-password" ] || rand_hex 24 | secret_write "$PRIV/admin-password"
  [ "$PG_MODE" != docker ] || [ -f "$PRIV/pg.env" ] || printf 'POSTGRES_PASSWORD=%s\n' "$(rand_hex 24)" | secret_write "$PRIV/pg.env"
  # The server refuses a tree with symbolic links; pnpm links under addons/web/static/node_modules.
  sync_addons
  if [ -n "$(find "$STATE/addons" -type l | head -1)" ]; then
    die "the copy of addons/ in $STATE/addons still holds symbolic links ($(find "$STATE/addons" -type l | head -3 | tr '\n' ' ')); the server refuses them. Remove the links from addons/ in the checkout and run again."
  fi
  local ram soft=""
  ram="$(host_ram_bytes)"
  # The server's default process memory cap is 4 GiB and must not exceed the host's RAM.
  if [ "$ram" -lt 4294967296 ]; then
    soft=$(( ram / 10 * 9 ))
    [ "$soft" -ge 268435456 ] || die "host RAM is $ram bytes; the server's memory cap needs at least 256 MiB and this host leaves $soft. Use a larger host."
    say "  host RAM $ram bytes is below the server's default cap of 4 GiB: writing [startup_limits] \"process.memory_soft_bytes\" = $soft into server.toml"
  fi
  { printf "database = '%s'\ndatabase_url_file = 'request-url'\nowner_database_url_file = 'owner-url'\nnode_database_url_file = 'node-url'\n" "$DB_NAME"
    printf "bind = '127.0.0.1:%s'\npublic_origin = 'http://127.0.0.1:%s'\nproxy_mode = 'direct'\n" "$PORT" "$PORT"
    printf "addons_path = ['%s']\nassets_manifest = '%s'\n" "$STATE/addons" "$ROOT/web/dist/human-ui/manifest.json"
    [ -z "$soft" ] || printf '\n[startup_limits]\n"process.memory_soft_bytes" = %s\n' "$soft"; } | secret_write "$CFG"
  say "  wrote $PRIV (mode 600 files), $STATE/addons, $CFG"
}

# ---- 6. PostgreSQL: start it, create the roles and the database ---------------------------------
start_postgres() {
  if [ "$PG_MODE" = docker ]; then
    if has_line "$(docker ps --format '{{.Names}}')" "$CONTAINER"; then return 0; fi
    if has_line "$(docker ps -a --format '{{.Names}}')" "$CONTAINER"; then docker start "$CONTAINER" >/dev/null
    else
      docker volume create "$PG_VOLUME" >/dev/null
      docker run -d --name "$CONTAINER" --env-file "$PRIV/pg.env" -v "$PG_VOLUME:/var/lib/postgresql" \
        -p "127.0.0.1:$PG_PORT:5432" postgres:18 >/dev/null
    fi
    local i=0
    until docker exec "$CONTAINER" pg_isready -q -h 127.0.0.1 -p 5432 >/dev/null 2>&1; do
      i=$((i + 1)); [ "$i" -le 90 ] || die "the container $CONTAINER did not become ready in 90 s; see: docker logs $CONTAINER"
      sleep 1
    done
  else
    ( exec 3<>"/dev/tcp/$PG_HOST/$PG_PORT" ) 2>/dev/null || die "cannot reach the PostgreSQL server at $PG_HOST:$PG_PORT; start it and run again"
  fi
}

create_roles_and_database() {
  [ "${PG_CREATED:-}" != 1 ] || return 0
  local clash pw_owner pw_web pw_node
  clash="$(pg_sql -At <<SQL
select 'role ' || rolname from pg_roles where rolname in ('$ROLE_OWNER','$ROLE_WEB','$ROLE_NODE','$ROLE_MAINT')
union all select 'database ' || datname from pg_database where datname = '$DB_NAME';
SQL
)"
  [ -z "$clash" ] || die "PostgreSQL already has: $(printf '%s' "$clash" | tr '\n' ',') . Those were not created by this script, so it leaves them alone. Pick another name with --db-name."
  pw_owner="$(sed -E 's#^postgres://[^:]*:([^@]*)@.*#\1#' "$PRIV/owner-url")"
  pw_web="$(sed -E 's#^postgres://[^:]*:([^@]*)@.*#\1#' "$PRIV/request-url")"
  pw_node="$(sed -E 's#^postgres://[^:]*:([^@]*)@.*#\1#' "$PRIV/node-url")"
  set_state PG_CREATED 1   # recorded first: from here on --reset drops these names with IF EXISTS
  pg_sql <<SQL
CREATE ROLE "$ROLE_OWNER" LOGIN PASSWORD '$pw_owner';
CREATE ROLE "$ROLE_WEB"   LOGIN PASSWORD '$pw_web'  NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION;
CREATE ROLE "$ROLE_NODE"  LOGIN PASSWORD '$pw_node' NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOINHERIT;
CREATE ROLE "$ROLE_MAINT" NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS;
GRANT "$ROLE_MAINT" TO "$ROLE_OWNER";
CREATE DATABASE "$DB_NAME" OWNER "$ROLE_OWNER";
SQL
  say "  created the roles $ROLE_OWNER, $ROLE_WEB, $ROLE_NODE, $ROLE_MAINT and the database $DB_NAME"
}

setup_postgres() {
  step "6/8 Starting PostgreSQL and creating the roles and the database"
  dry "mode $PG_MODE: $([ "$PG_MODE" = docker ] && echo "docker run postgres:18 as $CONTAINER; ")create roles $ROLE_OWNER $ROLE_WEB $ROLE_NODE $ROLE_MAINT and database $DB_NAME (fails if any exists)" && return 0
  start_postgres
  create_roles_and_database
}

# ---- 7. database: init with demo data, modules, grants ------------------------------------------
init_database() {
  step "7/8 Initialising the database with demo data and installing modules"
  dry "erp db init --with-demo; erp module install $MODULES; erp db publish-grants; erp cluster login-selection publish; erp db publish-grants" && return 0
  [ "$DATABASE_PHASE" != done ] || { say "  already done"; return 0; }
  [ "$DATABASE_PHASE" != started ] || die "a previous run stopped while setting up the database, which cannot be resumed safely. Run scripts/install.sh --reset --yes and start again."
  set_state DATABASE_PHASE started
  "$ERP" db init --config "$CFG" --admin-login admin --admin-password-file "$PRIV/admin-password" --with-demo
  ( export DATABASE_URL; DATABASE_URL="$(cat "$PRIV/owner-url")"
    # shellcheck disable=SC2086
    "$ERP" module install -d "$DB_NAME" $MODULES --addons-path "$STATE/addons" --dry-run
    # shellcheck disable=SC2086
    "$ERP" module install -d "$DB_NAME" $MODULES --addons-path "$STATE/addons"
    "$ERP" db publish-grants --config "$CFG"
    "$ERP" cluster login-selection publish -d "$DB_NAME" --maintenance-role "$ROLE_MAINT" --request-role "$ROLE_WEB"
    "$ERP" db publish-grants --config "$CFG" )
  set_state DATABASE_PHASE done
}

# ---- 8. server ----------------------------------------------------------------------------------
sign_in_check() { # an HTTP sign-in as the administrator: login page, CSRF token, POST, then the signed-in page
  local base="http://127.0.0.1:$PORT" jar form page csrf code loc
  jar="$(mktemp)"; form="$(mktemp)"; chmod 600 "$jar" "$form"
  page="$(curl -s --max-time 20 -c "$jar" "$base/erp/login")"
  csrf="$(printf '%s' "$page" | sed -n 's/.*"csrf":"\([^"]*\)".*/\1/p' | head -1)"
  [ -n "$csrf" ] || { rm -f "$jar" "$form"; die "sign-in check: the login page at $base/erp/login carries no CSRF token"; }
  printf 'db=%s&login=admin&password=%s&csrf_token=%s' "$DB_NAME" "$(cat "$PRIV/admin-password")" "$csrf" > "$form"
  code="$(curl -s -o /dev/null -w '%{http_code} %{redirect_url}' --max-time 20 -b "$jar" -c "$jar" -H "Origin: $base" --data "@$form" "$base/erp/login")"
  loc="${code#* }"; code="${code%% *}"
  grep -q 'session_id' "$jar" || { rm -f "$jar" "$form"; die "sign-in check failed: POST /erp/login answered HTTP $code and set no session cookie (see $STATE/server.log)"; }
  local after; after="$(http_code -b "$jar" "$base/erp")"
  rm -f "$jar" "$form"
  [ "$after" = 200 ] || die "sign-in check failed: the session cookie was set but GET /erp answered HTTP $after"
  say "  sign-in check: POST /erp/login as admin answered HTTP $code (redirect to ${loc#"$base"}), session cookie set, GET /erp with it answered HTTP $after"
}

start_server() {
  step "8/8 Starting the server"
  dry "erp server --config $CFG; wait for HTTP 200 on http://127.0.0.1:$PORT/erp/health/ready; HTTP sign-in as admin" && return 0
  local pid started=0
  pid="$(server_pid)"
  if [ -n "$pid" ]; then say "  server already running (pid $pid)"
  else
    port_in_use "$PORT" && die "port $PORT on 127.0.0.1 is in use by another program. Free it or run with --port N (after --reset when installed already)."
    nohup "$ERP" server --config "$CFG" >> "$STATE/server.log" 2>&1 < /dev/null &
    echo "$!" > "$STATE/server.pid"; started=1
    say "  started pid $! (log: $STATE/server.log)"
  fi
  local i=0
  until [ "$(http_code "http://127.0.0.1:$PORT/erp/health/ready")" = 200 ]; do
    i=$((i + 1))
    [ -n "$(server_pid)" ] || { tail -n 20 "$STATE/server.log" >&2; die "the server exited before it was ready. The last log lines are above; if the message names a startup limit, see startup_limits in the configuration reference of the deployment guide."; }
    [ "$i" -le "$HEALTH_WAIT_SECS" ] || { tail -n 20 "$STATE/server.log" >&2; die "no HTTP 200 on /erp/health/ready after ${HEALTH_WAIT_SECS}s. If you stopped the server a moment ago, it may still be shutting down (up to 45 s): wait for it to exit and run again."; }
    sleep 1
  done
  say "  /erp/health/ready answered HTTP 200 after about ${i}s"
  [ "$started" = 1 ] && [ "$DATABASE_FRESH" = 1 ] && sign_in_check
  return 0
}

finish() {
  printf '\nERPForAll is running.\n  URL:       http://127.0.0.1:%s/erp\n  Login:     admin\n  Password:  in %s (mode 600, not printed)\n  Log:       %s\n  Stop:      kill "$(cat %s)"\n  Start:     scripts/install.sh   (again, from this checkout)\n  Remove:    scripts/install.sh --reset --yes\n' \
    "$PORT" "$PRIV/admin-password" "$STATE/server.log" "$STATE/server.pid"
  [ -z "$BUILD_SECONDS" ] || printf '  Build:     %ss\n' "$BUILD_SECONDS"
}

# ---- reset --------------------------------------------------------------------------------------
do_reset() {
  [ -f "$STATE_FILE" ] || die "nothing to reset: $STATE_FILE does not exist (this script created nothing there)"
  say "--reset would remove only what this script created:"
  local pid; pid="$(server_pid)"
  [ -z "$pid" ] || say "  the running server (pid $pid)"
  if [ "${PG_CREATED:-}" = 1 ] || [ "$PG_MODE" = docker ]; then
    if [ "$PG_MODE" = docker ]; then say "  the container $CONTAINER and the volume $PG_VOLUME (all its data)"
    else say "  from the PostgreSQL server at $PG_HOST:$PG_PORT: the database $DB_NAME and the roles $ROLE_OWNER $ROLE_WEB $ROLE_NODE $ROLE_MAINT"; fi
  fi
  say "  in $STATE: private/ addons/ server.log server.pid state.env"
  say "  left alone: the build outputs in the checkout (target/, web/node_modules, web/dist) and the pinned Node and pnpm under ~/.erpforall/toolchain"
  [ "$DRY" = 0 ] || { say "(dry run: nothing removed)"; return 0; }
  [ "$YES" = 1 ] || die "add --yes to remove the items above"
  if [ -n "$pid" ]; then
    kill "$pid"; local i=0
    while [ -n "$(server_pid)" ] && [ "$i" -lt 60 ]; do sleep 1; i=$((i + 1)); done
    [ -z "$(server_pid)" ] || die "the server (pid $pid) did not stop within 60 s; stop it and run --reset again"
  fi
  if [ "$PG_MODE" = docker ]; then
    docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
    docker volume rm "$PG_VOLUME" >/dev/null 2>&1 || true
  elif [ "${PG_CREATED:-}" = 1 ]; then
    [ -n "${ARG_PG_USER:-${PGUSER:-}}" ] || die "name the PostgreSQL superuser (--pg-user USER, PGPASSWORD in the environment) so the roles and the database can be dropped"
    export PGUSER="${ARG_PG_USER:-$PGUSER}" PGHOST="$PG_HOST" PGPORT="$PG_PORT"
    pg_sql <<SQL
SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '$DB_NAME';
DROP DATABASE IF EXISTS "$DB_NAME";
DROP ROLE IF EXISTS "$ROLE_NODE";
DROP ROLE IF EXISTS "$ROLE_WEB";
DROP ROLE IF EXISTS "$ROLE_OWNER";
DROP ROLE IF EXISTS "$ROLE_MAINT";
SQL
  fi
  rm -rf "$PRIV" "$STATE/addons" "$STATE/server.log" "$STATE/server.pid" "$STATE_FILE"
  rmdir "$STATE" 2>/dev/null || true
  say "removed."
}

# ---- clean-cache --------------------------------------------------------------------------------
do_clean_cache() {
  step "Cleaning stale build caches"
  if [ -f "$ROOT/.cargo/debug-cache-limit-gib" ]; then
    dry "trim workspace test executables to the checkout's debug cache budget" || python3 -I "$ROOT/scripts/trim-debug-tests.py"
  else
    dry "remove $ROOT/target/debug" || rm -rf "$ROOT/target/debug"
  fi
  if [ "$DRY" = 1 ]; then
    say "  [dry-run] remove $ROOT/target/tmp"
    say "  [dry-run] prune unreferenced client.*.js|css in $ROOT/web/dist/human-ui"
    return 0
  fi
  rm -rf "$ROOT/target/tmp"
  if [ -f "$ROOT/web/dist/human-ui/manifest.json" ] && command -v python3 >/dev/null 2>&1; then
    python3 -c '
import json, pathlib, sys
dist = pathlib.Path(sys.argv[1])
manifest = json.loads((dist / "manifest.json").read_text())
keep = {"manifest.json"} | {a["path"] for a in manifest.get("assets", [])}
for f in dist.iterdir():
    if f.is_file() and f.name not in keep:
        f.unlink()
' "$ROOT/web/dist/human-ui"
  fi
  say "  cleaned stale debug/tmp caches and old web bundles (kept target/$CARGO_PROFILE_DIR)."
}

# ---- main ---------------------------------------------------------------------------------------
if [ "$CLEAN_CACHE" = 1 ]; then
  do_clean_cache
  [ "$REBUILD" = 1 ] || [ "$RESET" = 1 ] || exit 0
fi
if [ "$RESET" = 1 ]; then
  do_reset
  exit 0
fi

[ "$DRY" = 0 ] || say "Dry run: nothing is created or started."
DATABASE_FRESH=0
if [ "$DATABASE_PHASE" = done ]; then
  if [ "$REBUILD" = 1 ]; then
    step "Rebuilding incrementally for $STATE ($PROFILE)"
    check_prereqs
    build_binaries
    build_web
    if [ "$DRY" = 0 ]; then
      sync_addons
      local_pid="$(server_pid)"
      if [ -n "$local_pid" ]; then
        kill "$local_pid"; i=0
        while [ -n "$(server_pid)" ] && [ "$i" -lt 60 ]; do sleep 1; i=$((i + 1)); done
      fi
    fi
    start_postgres
    start_server
    [ "$DRY" = 1 ] || finish
    exit 0
  fi
  # Installed already: reuse everything, only start what is not running.
  step "Reusing the installation in $STATE"
  need curl "Install curl (for example: apt-get install curl ca-certificates)."
  if [ "$DRY" = 1 ]; then say "  [dry-run] start PostgreSQL ($PG_MODE) if stopped; start the server if stopped"; exit 0; fi
  [ -x "$ERP" ] || die "$ERP is missing (the checkout was cleaned?). Run scripts/install.sh --reset --yes and install again."
  start_postgres
  start_server
  finish
  exit 0
fi
DATABASE_FRESH=1
check_prereqs
choose_postgres
build_binaries
build_web
prepare_state
setup_postgres
init_database
start_server
[ "$DRY" = 1 ] || finish
