# Deploying ERPForAll

This guide covers a single-node deployment on Linux: a release build, the database, a service, HTTPS behind a
reverse proxy, upgrades, backups and rollback. Read the [installation guide](install.md) first; this one assumes
its steps and only adds what a long-running server needs.

ERPForAll is in early development and has no stable release yet. Treat every deployment as a test installation,
keep backups, and rehearse each upgrade on a copy of the database. Each command below was checked against the `erp`
source and `erp --help`; the places where a step was **run** on a test machine or **not run** are marked.

## Build a release

```sh
cargo build --release -p erp-cli --bin erp -p erp-ui-compiler-cli --bin erp-ui-compile
eval "$(scripts/web-toolchain.sh env)"
pnpm --dir web install --frozen-lockfile
ERP_UI_COMPILER="$PWD/target/release/erp-ui-compile" node web/scripts/build-human-ui.mjs
rsync -a --exclude node_modules --exclude target addons/ /srv/erp/addons/
install -m 0755 target/release/erp /usr/local/bin/erp
mkdir -p /srv/erp/assets && cp -r web/dist/human-ui/. /srv/erp/assets/
```

Keep the build machine apart from the machine that serves. The server needs three things: the `erp` binary, a
clean addons directory (no symbolic links, so no `node_modules`), and the client assets directory with
`manifest.json`. Set `assets_manifest = '/srv/erp/assets/manifest.json'`. (Run with a debug build for this guide;
the release build uses the same package and binary names. The copy to `/srv/erp/assets` was not run.)

## Database and roles

Create the database and the roles exactly as in the installation guide: an owner, a request role, a separate node
role, and a `NOLOGIN` maintenance role. Notes for a real deployment:

- Use long random passwords and keep each URL in its own file (mode 600, outside any repository).
- The three login roles must connect to the same database at the same host and port.
- PostgreSQL 18, session pooling only. Set `max_connections` high enough for the pools of every server process.
- Ordinary module operations and `erp db ...` commands use the owner role through `DATABASE_URL`; the running server
  uses the request and node roles and, for lifecycle work, the owner connection named in `server.toml`.

## Configuration reference

`server.toml` is strict: an unknown key is an error. Relative paths are resolved from the file's directory. Keys:

| Key | Required | Meaning |
|---|---|---|
| `database` | yes | Database name (letters, digits, underscore; at most 63). |
| `database_url_file` | yes | File with the request role's `postgres://` URL. |
| `owner_database_url_file` | yes | File with the owner role's URL. |
| `node_database_url_file` | for serving | File with the node role's URL. |
| `node_name` | no | Label of this process in the node table (default `erp-server`; letters, digits, `.`, `_`, `-`). |
| `bind` | yes | Listen address, for example `127.0.0.1:8069`. |
| `public_origin` | yes | The external origin users type, for example `https://erp.example.com`. All replicas of one service use the same value. |
| `proxy_mode` | yes | `direct` (loopback listener, no proxy) or `authenticated` (behind a reverse proxy; see below). |
| `trusted_proxies` | with `authenticated` | List of proxy IP addresses the server accepts requests from. |
| `proxy_token_file` | with `authenticated` | File with a 64-character lowercase hexadecimal token the proxy must send. |
| `addons_path` | yes | List of addon directories. |
| `assets_manifest` | yes | Path of the client `manifest.json`. |
| `shutdown_deadline_secs` | no | Time running requests get to finish on shutdown; 5 to 3600, default 45. |
| `http_header_deadline_secs`, `http_keep_alive_idle_secs`, `http_max_connection_age_secs` | no | HTTP/1 connection limits; defaults 5, 30 and 120. |
| `[startup_limits]` | no | Optional tables of memory and size limits; invalid or inconsistent values stop the start with an error. Defaults apply when absent. |

Secret files must be regular files with mode 600, not symbolic links. In `direct` mode `bind` must be a loopback
address and the proxy keys must be absent.

## Run it as a service

Run the server under a supervisor that restarts it: a database outage of more than about a minute makes it exit by
design. Raise the open-file limit above the connection limit (the default `max_connections` of the HTTP layer is
8192) plus the database pools.

### systemd

An example unit (an example written for this guide; it has been checked for syntax with `systemd-analyze verify`
but the service was not started by systemd):

```ini
[Unit]
Description=ERPForAll server
After=network-online.target postgresql.service
Wants=network-online.target

[Service]
User=erp
Group=erp
ExecStart=/usr/local/bin/erp server --config /srv/erp/private/server.toml
Restart=on-failure
RestartSec=5
LimitNOFILE=65536
# systemd stops the service with SIGTERM; the server drains requests within shutdown_deadline_secs.
TimeoutStopSec=60
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true

[Install]
WantedBy=multi-user.target
```

Save it as `/etc/systemd/system/erp.service`, then `systemctl daemon-reload` and `systemctl enable --now erp`.
The `erp` user needs read access to `/srv/erp` and must be the owner of the mode-600 secret files.

### Docker

An example Dockerfile and command (written for this guide; **not built or run**). The image only needs the binary,
the addons and the assets; PostgreSQL runs outside:

```dockerfile
FROM debian:stable-slim
RUN useradd --system --home /srv/erp erp
COPY erp /usr/local/bin/erp
COPY addons /srv/erp/addons
COPY assets /srv/erp/assets
USER erp
ENTRYPOINT ["erp", "server", "--config", "/run/erp/server.toml"]
```

```sh
docker run -d --name erp --restart on-failure \
  -v /srv/erp/private:/run/erp:ro -p 127.0.0.1:8069:8069 erp:local
```

The `erp` binary must be built for the image's libc (for example in a matching builder image). Inside a container
`bind` has to be a non-loopback address, which requires `proxy_mode = 'authenticated'` and `trusted_proxies` naming
the address the container sees the proxy as; the container's memory limit is not read by the server, whose startup
memory check looks at the host's RAM, so set the limits in `[startup_limits]` explicitly if the container is small.

## HTTPS behind a reverse proxy

Never expose the `direct` listener to a network. Put a reverse proxy in front that terminates TLS, and configure:

```toml
bind = '127.0.0.1:8069'
public_origin = 'https://erp.example.com'
proxy_mode = 'authenticated'
trusted_proxies = ['127.0.0.1']
proxy_token_file = 'proxy-token'
```

Create the token with `openssl rand -hex 32 > proxy-token` (mode 600). `public_origin` must use `https`. The server
accepts a request only from a listed proxy address that sends the token in exactly one `X-Erp-Proxy-Token` header, and
it rejects requests that carry the standardised `Forwarded` header (the example clears it). An nginx example (not run; adapt names and certificates):

```nginx
server {
    listen 443 ssl;
    server_name erp.example.com;
    ssl_certificate     /etc/ssl/erp.example.com.pem;
    ssl_certificate_key /etc/ssl/erp.example.com.key;

    location / {
        proxy_pass http://127.0.0.1:8069;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $remote_addr;
        proxy_set_header X-Forwarded-Proto https;
        proxy_set_header Forwarded "";
        proxy_set_header X-Erp-Proxy-Token "the-64-hex-characters-from-proxy-token";
    }
}
```

The proxy checks themselves were run without nginx, with `curl` from the listed address: with the token the sign-in
page is served (HTTP 200); without the token or with a wrong one, HTTP 403; with the token and a `Forwarded` header,
HTTP 400. The health route answers without the token, so a supervisor can probe it directly.

## Health

`GET /erp/health/ready` answers HTTP 200 when the node is ready and 503 while it settles, cannot renew its lease, or
cannot publish its registry. Use it for the proxy's and the supervisor's health checks, with bounded retries, and keep
a node out of traffic until it answers 200. `erp cluster nodes -d erp` (with `DATABASE_URL` set to the owner URL) lists
the registered nodes and their state.

## Upgrades

An upgrade replaces the binary, the addons directory and the client assets, then upgrades the modules in the
database. Rehearse it on a copy first:

1. **Back up** (next section) and note the version you run.
2. **Rehearse on a copy.** Restore the backup into a scratch database (`createdb` and `pg_restore`), point a copy of
   `server.toml` at it, and run the steps below against the copy. Only continue if they finish with exit code 0.
3. Stop the server.
4. Install the new binary, addons directory and assets.
5. Upgrade the modules, with the owner URL in `DATABASE_URL`:

   ```sh
   erp module upgrade -d erp base --addons-path /srv/erp/addons
   ```

   Name the modules to upgrade (installed modules that depend on them are upgraded too); `--dry-run` shows the plan
   without changing anything. A non-zero exit code means the operation was refused or failed; the message names the
   module and step. This command, for `base`, was run against a test database.
6. `erp db publish-grants --config /srv/erp/private/server.toml`, then start the server and wait for the health route.

Upgrades and uninstalls compare each addon's files against a content receipt stored in the database. When the files
of an installed addon changed, the command refuses with `identity unknown, run identity-enroll first` or asks for the
previous release: `erp module identity-enroll` and `erp module upgrade --identity-previous <release>` exist for this,
but their procedure is not described in this guide yet. Rehearse them on a copy before using them.

If the schema of an existing deployment needs the login helper step after an upgrade, repeat
`erp cluster login-selection publish` as in the installation guide.

## Backups and rollback

Back up the database with a `pg_dump` of **version 18** (a client from an older PostgreSQL refuses to dump a 18
server):

```sh
pg_dump -h db.example.com -U erp_owner -Fc erp -f erp-$(date +%F).dump
pg_restore --list erp-$(date +%F).dump | head      # check that the archive is readable
```

Both commands were run (with the `pg_dump` shipped in the PostgreSQL 18 container). The dump holds users, settings and
the stored client assets. Also keep copies of `server.toml` and the secret files, stored separately and protected.

To roll back an upgrade: stop the server, restore the pre-upgrade dump into a new database (`createdb`, then
`pg_restore -d <new database> <file>`; this restore was **not run** for this guide), put the previous binary, addons
and assets back, point `server.toml` at the restored database, and start. Rolling a database back loses everything
written after the backup.

## Hardening checklist

- PostgreSQL listens only where it must, and only the three login roles can reach this database.
- Secret files are mode 600 and owned by the service user.
- The server listens on loopback behind a TLS proxy; the proxy token is long and random.
- The service runs as an unprivileged user.
- Backups are tested by restoring them.
