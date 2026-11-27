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
| `field_key_file` | when addons declare encrypted fields | File with the keys of encrypted fields (see [Encrypted fields](#encrypted-fields)). |
| `[startup_limits]` | no | Optional tables of memory and size limits; invalid or inconsistent values stop the start with an error. Defaults apply when absent. |

Secret files must be regular files with mode 600, not symbolic links. In `direct` mode `bind` must be a loopback
address and the proxy keys must be absent.

### Attachment downloads

A signed-in user or an unrestricted RPC API key can fetch an attachment at
`/erp/content/<attachment-id>`. A stored binary field is available at
`/erp/content/<model>/<id>/<field>`. For example, requesting `res.partner/<id>/image_128`
returns the stored thumbnail. Field downloads admit up to 8 MiB per value. Use an attachment
for larger files. Record rules, company scope and field permissions apply before conditional
or range headers are evaluated. GET and HEAD expose the same strong ETag for the same bytes.

A signed-in administrator can issue a read-only link with `ir.attachment.erp_share_content`.
Pass the attachment ID, `public_read`, and a `lifetime_seconds` from 1 to 604800. Each scoped
link replaces its predecessor. For a public URL, first set the attachment's `public` flag and
then issue a public grant. Changing the bytes or parent, expiry, loss of the issuer's access,
and `erp_revoke_content` end access. Keep query strings out of reverse-proxy access logs.

An attachment URL pointing to another binary on the configured public origin returns a
307 after both records pass the caller's access checks. The target enforces its own access
checks on every request. The same ETag, range and download headers apply to inline and
externally stored blobs.

The following optional settings belong in the server configuration. The values shown are
the defaults. The idle timer measures a stalled store read. Connection age and drain
limits also apply when a client stops reading.

```toml
[startup_limits]
"limits.stream_deadline_secs" = 600
"limits.stream_idle_secs" = 60
"limits.streams_per_user" = 4
```

The structured `erp::attachment` events publish process totals under
`attachment_missing_total` for missing objects, `attachment_read_failed_total` for lookup,
open and stream failures, and `attachment_limit_exhausted_total` for admission or time limits.
The `limit` field identifies the exhausted budget. Counters restart with the process.

Each active GET holds one user slot on the serving node. Exceeding the user limit returns
HTTP 429 and `Retry-After`. The node's attachment store retains its own stream limit.
HEAD and successful conditional requests complete without taking a user slot.

#### Optional storage handoff

For a local attachment store served by Nginx, set a distinct internal URI prefix per
store. Use the same folder for the store path and the proxy alias.

```toml
[attachments.stores.main]
kind = "local"
path = "/var/lib/erp/attachments"
x_accel_prefix = "/_erp_attachment/main/"
```

```nginx
location ^~ /_erp_attachment/main/ {
    internal;
    alias /var/lib/erp/attachments/;
    disable_symlinks on;
    etag off;
    if_modified_since off;
    add_header ETag $upstream_http_etag always;
    add_header X-Content-Type-Options nosniff always;
    add_header Content-Security-Policy "default-src 'none'; sandbox" always;
    add_header Cross-Origin-Resource-Policy same-origin always;
    add_header Referrer-Policy no-referrer always;
}
```

Allow the proxy process to traverse the store directories and read the content files. Keep
the ERP listener behind the authenticated reverse proxy and leave `X-Accel-Redirect`
processing enabled. Nginx forwards the application's selected content type and disposition
and derives the byte length from the file. Apply transfer concurrency and timeout limits at
the proxy for these transfers. [Nginx internal locations](https://nginx.org/en/docs/http/ngx_http_core_module.html#internal)
[Nginx upstream header processing](https://nginx.org/en/docs/http/ngx_http_proxy_module.html#proxy_ignore_headers)

For an HTTPS S3-compatible store, set `presign_downloads = true` in its store table to
allow a session or bearer read to receive a temporary 302 redirect for an eligible file.
`presign_min_bytes` defaults to 8388608. `presign_ttl_secs` defaults to 60 and accepts
1 through 300. Startup must successfully write and remove a probe object and verify
that the store enforces the signed media type, attachment disposition, and `no-store`
cache policy. Treat the signed URL as a credential until it expires and keep `Location`
and query strings out of proxy logs. After a redirect has been issued, its separate
expiry governs access at the object store.

Both handoffs are off by default. Eligibility requires a readable stored blob and an
unconditional full GET. The application continues to handle HEAD, ranges, validators,
inline content and binary fields. Scoped links and public reads stream through ERP
or use the internal proxy so their grant checks apply on every external request.
This release rejects encryption and non-off scanner modes at startup.

### Assistant connections

Open **My Preferences → Account Security → Manage MCP connections** to name a
connection and copy its bearer token once. The default is read only. A connection
may have exact model and method allowlists and an earlier expiry. An omitted expiry
uses the maximum lifetime configured for the user's groups. Issuance uses the password
exchange and honors its MFA, quota and issuance policies. A sign-in that requires an
additional factor cannot issue through this exchange. The owner can see expiry and
last use, and revoke a connection by name without presenting its token. Revocation
takes effect on the next authenticated request.

Connect a streamable HTTP client to `POST /erp/mcp` on the ERP origin with the
`Authorization: Bearer <MCP_TOKEN>` header. The UI shows a client configuration
template. Only registered assistant tools within the connection's scope and the user's
current access can appear. Eligible mutations also go through the tool's approval policy.

Native clients can request a connection at `POST /erp/session/api-key` with the usual
`db`, `login`, `password`, and `device_name` fields and `"audience":"mcp"`.
To narrow a connection, include an `mcp_access` object, for example
`{"read_only":true,"models":["sale.order"],"methods":["search_read"]}`.
An optional `expires_at` inside this object takes a UTC timestamp such as
`2027-01-15T08:00:00Z`. The server validates the date against issuance policy.
Omitting the access object retains the existing delegated tool and approval behavior.

### Signed provider callbacks

A server can accept provider deliveries at `POST /erp/callback/<binding>`. Configure each
binding on the nodes that serve it. Use a dedicated integration user with the ERP permissions
and company access the target needs. Issue a normal RPC API key for that user and keep it on
the server. Give the provider only the separate signing key and the signature contract.

```toml
[callbacks.delivery-status]
model = "my.integration"
method = "record_delivery"
signing_key_file = "credentials/delivery-signing.key"
api_key_file = "credentials/delivery-erp.key"
```

Both files are mode `0600` regular files, and relative paths are resolved alongside the server
configuration. The signing file holds 32 random bytes encoded as unpadded base64url. The ERP key
file holds the issued key as text. Each binding fixes one public RPC method with a `write` effect.
The credential, ORM access checks and approval policy are evaluated in the usual RPC transaction.
The request accepts JSON-2 arguments and record IDs, with the integration user's default company
context. Send `Content-Type: application/json` and at most 64 KiB of uncompressed body.

`Idempotency-Key` is a delivery UUID. `X-ERP-Callback-Timestamp` is Unix seconds within five minutes
of the server clock. `X-ERP-Callback-Signature` is `v1=` followed by an unpadded base64url HMAC-SHA256
tag. Compute it over the following UTF-8 lines, with one newline after every line, then append the
exact HTTP body bytes. Use the configured public origin with no trailing slash and the lower-case
UUID representation.

```text
erp-callback-v1
<public origin>
<database name>
<binding>
<model>
<method>
<timestamp>
<delivery UUID>
```

Successful delivery and replay return HTTP 204. Reuse the UUID when retrying a delivery and sign
with a fresh timestamp. The RPC idempotency windows govern replay of acknowledgements and
retention of delivery claims. Renaming a binding or replacing its ERP API key starts a new
idempotency scope, so coordinate those changes with the provider's retry window. The audit trail
uses the `inbound_callback` origin and records the acting integration user. HTTP refusals use
fixed messages, and the idempotency store holds only a fixed acknowledgement after success.

### Audit roles and upkeep

Configure `audit_reader_database_url_file` with a distinct PostgreSQL login when the deployment
uses the audit reader. `erp db publish-grants` grants this login read access through the audit
parent table. The runtime role appends evidence, and the owner role maintains the partition window
with `erp audit maintain --config <server.toml>`. Audit partitions remain attached by default.

Set the top-level `audit_retention_days` to an integer from 1 to 3650 to archive complete older
UTC days. The fixed owner task runs at 02:00 UTC and archives at most one eligible partition per
pass. `erp audit retain --config <server.toml>` runs one additional pass. Archival stores the
partition and its count and digest in the private `erp_audit_archive` schema. Chain verification
includes the archived rows. When the audit chain is initialized, provide the external anchor
inventory through `audit_witness_file` so the owner can verify chain coverage before archiving.

Use `erp audit hold --config <server.toml> --day YYYY-MM-DD` to keep a day attached. Add
`--release` to lift the hold. `erp audit restore --config <server.toml> --day YYYY-MM-DD`
checks the archived rows against their manifest, restores the partition for the audit reader,
and places it on hold.

Audit rows record the subject and the actor that authorized the call. Browser sessions act as
their authenticated user. API-key calls record the key identity. Delegated agent keys also record
the delegating user. A
reviewed approval still executes under the original requester's authority. The receipt links
to the execution operation, and the decision is a separate call by the eligible reviewer.

## Encrypted fields

A `char` or `text` field declared `encrypted = true` (`#[field(char, encrypted = true)]`) is stored encrypted: its
column is `bytea` and holds an envelope sealed with XChaCha20-Poly1305 (RustCrypto `chacha20poly1305`) with a new
random nonce on every write, bound to its model and field. The application reads and writes the clear value; the
database, its dumps and replicas, and any role that reads the table see only the envelope.

The keys come from `field_key_file`, never from the database. The file is mode 600, one key per line, the key that
encrypts new values first:

```sh
umask 077
printf 'k2026a %s\n' "$(openssl rand -base64 32)" > /srv/erp/private/field-keys
```

A key id is 1 to 32 letters, digits, `.`, `_` or `-`; at most 16 keys; lines starting with `#` are ignored. Every node
of a database needs the same file: the node settings check refuses a node whose key set differs.

- **Commands without `--config`:** `erp module install`/`upgrade`/`uninstall`, `erp db init` and the
  other commands that build the ORM from `DATABASE_URL` take the same file with the global option
  `--field-key-file <path>` (before or after the subcommand), under the same rules: a regular file of mode 600, not
  a symlink. A relative path is relative to the working directory, not to `server.toml`. Commands that read
  `--config` (the server among them) take the keys from its `field_key_file`: the node settings check compares that
  file's fingerprint, so keys given only with the option are refused there; giving both is accepted only when they
  hold the same keys.
- **Missing key:** when the linked addons declare an encrypted field and no key file is given, every command that
  builds the ORM (the server, `erp db init`, `erp module upgrade`, …) stops with an error naming the field.
- **Lost key:** values sealed with it cannot be read again. Back the key file up separately from the database dumps;
  a dump without its key file is unreadable for these fields, which is the point.
- **Rotation:** put a new key on the first line and keep the old ones below it. New and changed values are sealed
  with the new key; values sealed with an old key still open. Remove an old key only when no value uses it: a
  re-encryption job that rewrites old values is not built yet, so for now keep old keys. Blind indexes (below) are
  derived from the last key of the file, so adding a sealing key on top leaves them as they are.
- **Existing data:** when a field becomes encrypted, the schema synchronization of the module upgrade converts its
  column and encrypts every existing value in the same transaction (in batches of 1,000 rows; on a large table this
  lengthens the upgrade). Declaring an encrypted field plain again is refused: decrypting a column is not built yet.
- **Searching:** conditions, sorting, grouping and aggregates on an encrypted field are refused with an error naming
  the field (the database cannot compare encrypted values), except equality on a blind-indexed field (below).
  Keys, indexes and exclusion constraints on such a field are refused when the registry is built (a typed key on a
  blind-indexed field excepted); an addon's SQL must not read the column directly.
- **Audit:** the call digest of the audit trail replaces an encrypted field's value with `{"$redacted":"encrypted"}`;
  tracking an encrypted field is refused.

### Blind indexes

A field declared `blind_index = true` as well (`#[field(char, encrypted = true, blind_index = true)]`) can be
searched for equal values and made unique, while the database still holds no clear value:

```rust
#[field(char, encrypted = true, blind_index = true)]
pub national_id: Option<Arc<str>>,

static OBJECTS: &[TableObjectDecl] = &[TableObjectDecl {
    name: "_national_id_key",
    kind: TableObjectKind::Key(
        KeyDecl::new(&["company_id", "national_id"], "This national id is already used.").nulls_distinct(true),
    ),
}];
```

- **Storage:** a companion column `<field>__bidx` (`text`, not a field of the model) holds
  `<key id>:<HMAC-SHA256 of the value, in hex>`. The HMAC key is derived with HKDF-SHA256 from the last key of
  `field_key_file`, the model and the field: the same value in two fields has two unrelated hashes, and no key is
  ever in the database. The ORM indexes the column.
- **Searching:** `=`, `!=`, `in` and `not in` work (the ORM hashes the values it compares; `!=` keeps the records
  without a value, as on a clear column), and so does `= False`. `like`, `ilike`, comparisons, ordering and
  grouping stay refused. The value is hashed exactly as stored: normalise it (spaces, case) before writing it if
  two spellings must match.
- **Keys:** a typed key (`KeyDecl`) may name the field; it is built on the companion column. A key written as SQL
  text (`UNIQUE (national_id)`) is not parsed and must not name an encrypted or companion column. Typed indexes and
  exclusion constraints on the field stay refused. An upsert on a model with a blind-indexed field is refused for
  now (create and write maintain the hash; the upsert statement does not yet).
- **Existing data:** when a field becomes blind-indexed (or encrypted and blind-indexed in the same upgrade), the
  schema synchronization adds the companion column and fills it for every row with a value, in batches of 1,000,
  in the transaction of the module upgrade and before the table's keys are built; duplicate values stop the upgrade
  with the key's message. Declaring the field without `blind_index` again drops the companion column.
- **Changing the index key:** removing the last line of the key file changes the key the indexes derive from. Until
  the next `erp module upgrade` recomputes them (it rewrites every hash whose key id is not the current one),
  searches on the field find nothing and keys on it do not see the older rows: run the upgrade in the same
  maintenance window as the key change. Removing a key also requires that no envelope uses it (see Rotation).
- **What it reveals:** which rows hold equal values, which is what a unique key or an equality search needs. A
  short or guessable value (a small number range) can be found by whoever holds the key file and the database;
  without the key file the hashes give nothing back.
### Request throttling (`[auth]`)

Three per-process token buckets screen requests before the database is asked; a value outside its bounds stops
the start with an error naming the key. Each bucket table holds `throttle.table_entries` keys (default 4096).

| Key | Default | Bounds | What it limits |
|---|---|---|---|
| `throttle.login_burst`, `throttle.login_rate_per_sec` | 20, 2.0 | 5 – 1000, 0.1 – 1000 | Password logins per source address. A JSON-2 request whose API key is refused also spends a token here, and an address with an empty bucket is refused before its key is read, so guessing keys is screened like guessing passwords. |
| `throttle.bearer_burst`, `throttle.bearer_rate_per_sec` | 120, 30.0 | 5 – 10000, 0.1 – 10000 | JSON-2 requests with `Authorization: Bearer` per source address, one token per request. |
| `throttle.api_key_burst`, `throttle.api_key_rate_per_sec` | 60, 10.0 | 5 – 10000, 0.1 – 10000 | Requests per verified API key, from any address, one token per request. |

A JSON-2 request refused by a bucket gets 429 `erp.rate_limited` with `Retry-After` set to the seconds one token
takes to refill.
Many devices behind one address (an office NAT) share the Bearer bucket of that address, but each key has its own
budget, so one busy device does not slow its neighbours. Failed keys also count towards the address cooldown stored
in the database, as failed passwords do.

```toml
[auth]
"throttle.bearer_burst" = 240
"throttle.api_key_rate_per_sec" = 20.0
```

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

`X-Forwarded-For` must carry exactly one address, the client's (`$remote_addr` above, not
`$proxy_add_x_forwarded_for`, which appends to what the client sent). The server uses it for the sign-in and API key
throttles and hands it to model methods as the request's client address (`env.request()`), so a proxy that passes
on a client-supplied value would let clients choose the address business rules see.

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

   When the addons declare encrypted fields, add `--field-key-file /srv/erp/private/field-keys` (the file that
   `field_key_file` names), run as the user that owns that file.

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

### OpenID Connect sign-in

Register the ERPForAll callback `<public_origin>/erp/login/external/callback` at the identity provider as an exact redirect URI. Enable authorization codes with `S256` PKCE and `client_secret_basic`. Configure one provider per database. Pin the issuer, endpoints and signing keys on each serving node. The server accepts HTTPS endpoints and loopback HTTP for local testing. Keep the client secret in a private mode-600 file. The signing key file is a public JWKS containing RSA (`RS256`) or Ed25519 (`EdDSA`) signing keys with unique key IDs.

```toml
[external_identity]
client_secret_file = "oidc-client-secret"
signing_keys_file = "oidc-signing-keys.json"

[external_identity.provider]
issuer = "https://id.example.com"
authorization_endpoint = "https://id.example.com/authorize"
token_endpoint = "https://id.example.com/token"
client_id = "erpforall"
label = "Company identity"
required_acr = "urn:example:mfa"
```

Settings administrators can open **Settings → Technical → Security → Single Sign-On** to check
the issuer, callback URI, signing-key source and cache state of the serving node. **Refresh status**
reads the current cache. Disabling the provider ends its active ERP sessions. A neutralized
database displays its disabled state and keeps external sign-in unavailable.

Set `required_acr` to the identity provider's exact signed assurance claim for MFA. Omit it to use the provider's own sign-in policy. Apply compatible provider configuration to every node before serving traffic. For a local key file, stage the new public key alongside the current one on each node before the provider begins signing with the new key.

To follow the issuer's rotation automatically, replace `signing_keys_file` with these settings in the existing `[external_identity]` table.

```toml
signing_keys_url = "https://id.example.com/realms/company/protocol/openid-connect/certs"
signing_keys_ttl_seconds = 300
# Optional trust anchor for an issuer with a private certificate authority:
# ca_certificate_file = "company-root-ca.pem"
```

The key URL must use HTTPS and the issuer's exact origin. The server verifies TLS for both token and key requests and refuses redirects. Each node retrieves its first key set when it verifies a sign-in. An unfamiliar key ID can trigger one extra refresh every ten seconds. The cache lifetime defaults to five minutes and accepts values from 30 to 3600 seconds. During a provider outage, cached keys work until their configured expiry. A sign-in that needs a new or expired key requires a successful refresh. The provider may publish signing and encryption keys together. ERPForAll uses RS256 and Ed25519 signing keys.

Sign in to an existing ERP account and open `/erp/login` to connect the provider to that account. Connecting requires both a current ERP session and a fresh provider sign-in. Existing accounts are identified by the linked issuer and subject. Email is used for eligibility only when you opt into provisioning. The following configuration permits new portal accounts for verified addresses in one domain and grants access to one company.

```toml
[external_identity.provider.provisioning]
company_id = 1
email_domain = "example.com"
```

Use **Disconnect** on the sign-in page to revoke your linked identities and the sessions issued through that provider. A user with the Settings administrator group can post `enabled=false` with the current session and login-page `csrf_token` to `/erp/session/external/provider`. Disabling the provider removes pending sign-in requests and revokes provider-issued sessions. Post `enabled=true` to reopen sign-in. Disable the provider before removing its configuration from the serving nodes if you need to revoke existing provider sessions at the same time.

`erp db neutralize` clears external sign-in challenges and account links from the copy. A neutralized database keeps external sign-in disabled even if an issuer is configured at server startup.


### Local delivery of mail notifications

With the mail addon installed, each serving node replays committed message events through the
`mail.notifications` outbox consumer. Recipient receipts are unique per message, partner and
transport. Discuss refreshes its visible records after a payload-free wake. A private conversation
checks membership again at delivery and when the recipient opens it.

Set `mail_delivery_dir = "/var/lib/erpforall/outgoing-maildir"` in the server configuration to
deliver e-mail notices to a local Maildir. Grant the server account write access to that private
directory and configure the same shared directory on each node that performs handoff. The notices
contain a sign-in prompt. The database keeps addressed e-mail pending until a Maildir is
configured, and marks recipients without a usable address as failed. Relaying Maildir messages to
external mail systems is an operator transport choice.

### Company SMTP relays

On existing databases, follow the deployment upgrade procedure for `base`, `mail`, and the
installed `sale` and `account_mail` addons to load the associated models and views. Deploy the
matching web assets for Discuss, MCP connections and Single Sign-On. Existing Maildir
notifications keep their recorded transport.
Enable the job runner on at least one serving node using `jobs_run_in_app = true`. Declare the
same named profiles on every node for the database. A profile identifies the SMTP server and the
companies allowed to use its credentials. TLS modes are `starttls` (mandatory upgrade) and
`implicit` (TLS on connect). The `loopback` mode accepts a literal local IP for an
unauthenticated test relay. Omit both credential fields for an unauthenticated relay.

```toml
[smtp_profiles.sales]
host = "smtp.example.com"
port = 587
tls = "starttls"
companies = [1]
username = "erp-mail"
password_file = "credentials/sales-smtp"
```

The password file must be a regular mode-600 file, relative to the server configuration or
absolute. In **Technical → Messaging → Outgoing Mail Servers**, create a route for the
company, enter its profile name, sender address and optional Reply-To address. A shared
deployment profile such as `default` requires an explicit route in each authorized company.

Use **Test connection** to check the handshake and authentication. To verify delivery, enter a test
recipient, save the route, then use **Send test email**. Follow the job in **Job Runs**.
For document messages, SMTP acceptance records the relay's final successful response. When the
connection closes after submission and that response is lost, check the relay log for the
message ID before choosing to resend. Neutralized database copies disable outbound mail.

In **Settings → Technical → Messaging → Outgoing Mail Queue**, select a company to see its waiting
jobs, the age of the oldest due email, and its most recent delivery error. Worker status refers
to the server handling the request. The **Outgoing Email** form links a recipient's delivery
to the source document, job, and individual attempts. Before enabling a second company, send a
quotation and a posted invoice to a test mailbox and verify the sender, document, and PDF.
