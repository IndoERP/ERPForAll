# Installing ERPForAll

This guide takes you from a fresh checkout to a running server with the web client, on one machine. ERPForAll is
in early development: there is **no one-command local setup yet** (not available yet), so the steps below are the
explicit ones. Every command in this guide was checked against the `erp` command-line source and `erp --help`; the
sequence from "Create the roles and the database" to "Start the server" was also run end to end on a Linux x86_64
machine with PostgreSQL 18. Where a step was not run, the text says so.

## Prerequisites

- A Unix-like system. The server checks Unix file permissions on its secret files, so Windows is not supported.
  The web toolchain script supports Linux x86_64 and macOS arm64.
- **Rust**, through [rustup](https://rustup.rs/). The exact version is pinned in `rust-toolchain.toml`; rustup
  installs it (with the WebAssembly targets listed there) the first time you run `cargo` in the checkout.
- **PostgreSQL 18**, with the matching `psql` and `pg_dump` clients. ERPForAll connects directly or through a
  pooler in *session* mode; a pooler in transaction mode is not supported.
- **Node and pnpm**, at the versions pinned by `scripts/web-toolchain.sh`. The script installs them for this project
  only (under `~/.erpforall/toolchain`, or the directory in `ERPFORALL_TOOLCHAIN`):

  ```sh
  scripts/web-toolchain.sh install      # download and verify the pinned Node and pnpm (not run for this guide)
  scripts/web-toolchain.sh check        # confirm both are present at the pinned versions
  eval "$(scripts/web-toolchain.sh env)"  # put them on PATH in this shell
  ```

## Get the source and build

```sh
git clone https://github.com/IndoERP/ERPForAll.git
cd ERPForAll
cargo build --release -p erp-cli --bin erp -p erp-ui-compiler-cli --bin erp-ui-compile
```

This builds two programs in `target/release/`:

- `erp`, the command line and the server. The framework and the native addons of this repository are compiled
  into it; data-only addons are read from an addons directory when the server starts.
- `erp-ui-compile`, the template compiler used to build the web client.

(The commands in this guide were run with a debug build, `cargo build` without `--release`; the package and binary
names are the same.)

## Create the roles and the database

The server uses three PostgreSQL roles for one database, and refuses to start if they are not kept apart:

| Role | Used for | Rules |
|---|---|---|
| owner | schema changes and module operations (`erp db init`, `erp module ...`) | owns the database |
| request | all ordinary application work | not the owner, not a superuser, may not create roles or databases |
| node | the server's registration and heartbeat | a third role; not the request role, not the owner, and not a member of either |

A fourth role, a `NOLOGIN` *maintenance* role, owns a few narrow helper functions. Create everything as a PostgreSQL
superuser (replace the passwords):

```sh
psql -U postgres <<'SQL'
CREATE ROLE erp_owner LOGIN PASSWORD 'change-me-owner';
CREATE ROLE erp_web   LOGIN PASSWORD 'change-me-web'  NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION;
CREATE ROLE erp_node  LOGIN PASSWORD 'change-me-node' NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOINHERIT;
CREATE ROLE erp_maint NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS;
GRANT erp_maint TO erp_owner;
CREATE DATABASE erp OWNER erp_owner;
SQL
```

## Build the web client

The client is compiled ahead of time into a manifest and two hashed files that the server serves:

```sh
eval "$(scripts/web-toolchain.sh env)"
pnpm --dir web install --frozen-lockfile
ERP_UI_COMPILER="$PWD/target/release/erp-ui-compile" node web/scripts/build-human-ui.mjs
```

The output is in `web/dist/human-ui/`; `manifest.json` there is what the server configuration points at. To include
the client side of other addons, name them: `node web/scripts/build-human-ui.mjs --addons web,my_addon` (the list
must start with `web`).

## Prepare the addons directory

The server reads addons from a directory you name, and refuses a tree that contains symbolic links. The `pnpm
install` above puts links under `addons/web/static/node_modules`, so serve a copy without them:

```sh
mkdir -p /srv/erp
rsync -a --exclude node_modules --exclude target addons/ /srv/erp/addons/
```

## Write the configuration

Keep the secrets in their own files, mode 600, outside the checkout. Each URL names the **same** database and host
and its own role:

```sh
mkdir -p /srv/erp/private && chmod 700 /srv/erp/private
cd /srv/erp/private
umask 077
echo 'postgres://erp_owner:change-me-owner@127.0.0.1:5432/erp' > owner-url
echo 'postgres://erp_web:change-me-web@127.0.0.1:5432/erp'     > request-url
echo 'postgres://erp_node:change-me-node@127.0.0.1:5432/erp'   > node-url
printf '%s\n' 'choose-a-password-of-12-or-more-characters'      > admin-password
```

Then `/srv/erp/private/server.toml` (relative paths are resolved from the directory of this file):

```toml
database = 'erp'
database_url_file = 'request-url'
owner_database_url_file = 'owner-url'
node_database_url_file = 'node-url'
bind = '127.0.0.1:8069'
public_origin = 'http://127.0.0.1:8069'
proxy_mode = 'direct'
addons_path = ['/srv/erp/addons']
assets_manifest = '/path/to/ERPForAll/web/dist/human-ui/manifest.json'
```

`proxy_mode = 'direct'` is for a local, loopback-only server. For anything reachable from other machines, see
[deployment](deployment.md). The full list of keys is in the deployment guide.

## Initialise the database

```sh
erp db init --config /srv/erp/private/server.toml \
    --admin-login admin --admin-password-file /srv/erp/private/admin-password
```

(Use `target/release/erp` if `erp` is not on your `PATH`.) This installs the base modules (`base`, and `web`, which
installs together with it) into the empty database and creates the first administrator. There are no default
credentials, the password must have at least 12 bytes, and the command refuses to run again once an active user
exists.

## Install more modules

Module commands read the owner connection from the `DATABASE_URL` environment variable (never from the command
line) and the database name from `-d`. Look at the plan first:

```sh
erp module install -d erp contacts product --addons-path /srv/erp/addons --dry-run
export DATABASE_URL="$(cat /srv/erp/private/owner-url)"
erp module install -d erp contacts product --addons-path /srv/erp/addons
```

`contacts` and `product` are examples; their dependencies (`uom`, for instance) are installed with them. Exit code 0
means done; a non-zero code means the request was refused or a step failed, and the message says which. Run module
operations while the server is stopped, as this guide did.

## Publish the runtime grants

After `db init` and after every module operation, publish the privileges the request and node roles need. Once, for
a new database, also publish the login helper functions under the maintenance role:

```sh
erp db publish-grants --config /srv/erp/private/server.toml
erp cluster login-selection publish -d erp --maintenance-role erp_maint --request-role erp_web
erp db publish-grants --config /srv/erp/private/server.toml
```

(`erp cluster login-selection publish` also reads `DATABASE_URL`.) Without the helper step the server refuses to
start with a message that a node feature helper "is not safely published".

## Start the server

```sh
erp server --config /srv/erp/private/server.toml
```

The server is ready when `GET /erp/health/ready` answers HTTP 200; a listening socket alone does not mean ready:

```sh
curl -i http://127.0.0.1:8069/erp/health/ready
```

Open <http://127.0.0.1:8069/erp>. It redirects to the sign-in page at `/erp/login`; sign in with the administrator
login and password you gave to `erp db init`. (The run for this guide confirmed the health route, the sign-in page and
the redirect; the browser sign-in itself was not scripted.)

Stop the server with `SIGTERM` (Ctrl-C in a terminal): it stops taking new requests, lets running ones finish within
`shutdown_deadline_secs` (default 45), and exits.

## Where configuration lives

- **Server configuration:** the `server.toml` you passed with `--config`, plus the secret files it names.
- **Everything else** (users, companies, settings, installed modules) is stored in the database.
- **Addons** are read from `addons_path`; the client assets from `assets_manifest`.

## Install an addon from a folder

A *data-only* addon is a folder with a `manifest.toml`, data and views, and no Rust code. Create one, copy it into
the addons directory, and install it:

```sh
erp scaffold my_tags --data-only --summary "Declarative tags" --dest /srv/erp/addons/my_tags
export DATABASE_URL="$(cat /srv/erp/private/owner-url)"
erp module install -d erp my_tags --addons-path /srv/erp/addons
erp db publish-grants --config /srv/erp/private/server.toml
```

(Installing this scaffolded folder was not run for this guide. `erp scaffold --data-only` was run on its own, and
`erp module install` and `erp db publish-grants` were run with `contacts` and `product`.) Start the server afterwards. A *native* addon (with Rust code) is compiled into the `erp` binary, so adding one
means adding the crate to the workspace and rebuilding; see [development](development.md). Addons can also be packed
as WebAssembly packages (`erp addon --help` lists `build`, `package`, `install`, `upgrade` and `uninstall`); that
route is not covered here.

## Troubleshooting

- `Database <name> not initialized`: run `erp db init` first.
- The server exits at start with a message about a symbolic link in an addon folder: serve the copy made in
  "Prepare the addons directory", not the checkout.
- The server exits with a message that a node feature helper "is not safely published": run the
  `erp cluster login-selection publish` step.
- A refused secret file: it must be a regular file with mode 600, not a symbolic link, and not empty.
