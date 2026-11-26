# Developing ERPForAll

This guide is a map for people changing ERPForAll or writing addons for it. Installing and running a server is in
the [installation guide](install.md); running one for real use is in the [deployment guide](deployment.md). The
rules for contributions (licence, sign-off, pull requests) and the build and test commands are in
[CONTRIBUTING.md](../CONTRIBUTING.md); they are not repeated here.

## Repository layout

| Path | What is in it |
|---|---|
| `crates/` | The Rust framework, one crate per concern (below). |
| `addons/` | Addons that ship in this repository: `base`, `web`, `uom`, `product`, `contacts`, `mail`, the stock family, a theme, and `sample_library` (a data-only example). |
| `web/` | The web client runtime ("Elang") as a pnpm workspace: `src/` (runtime), `packages/`, `tests/`, `scripts/`, `vendor/` (vendored web libraries). The client side of each addon lives next to the addon, in `addons/<name>/static/`. |
| `examples/` | Sample addons built as WebAssembly components (`examples/wasm`). |
| `wit/` | The WebAssembly interface (WIT) that addon components implement, with its golden files. |
| `fuzz/` | Fuzz targets. |
| `vendor/` | Patched copies of third-party crates, wired in through `[patch.crates-io]` in the root `Cargo.toml`. They are not workspace members and keep their own licences. |
| `testdata/` | Shared fixtures used by tests. |
| `scripts/` | Helper scripts: `check.sh` (format, lints, build, tests), `web-toolchain.sh` (pinned Node and pnpm), generators and checks. |
| `docs/` | These guides. |

### Crates

The framework crates are layered: a crate depends only on crates below it. `scripts/check-layers.py` enforces the
rule, and addon crates may depend only on `erp-sdk`.

| Crate | Role |
|---|---|
| `erp-fields`, `erp-domain`, `erp-expr`, `erp-html`, `erp-sql`, `erp-store-wire` | Pure building blocks: field types and values, the search-domain language, the expression evaluator, the HTML sanitiser, SQL text builders, the messaging wire format. |
| `erp-registry`, `erp-views` | Model metadata and inheritance; view architecture inheritance. |
| `erp-orm` | The ORM: environments, transactions, recordsets, schema sync. |
| `erp-security` | Access rules, groups, authentication and sessions. |
| `erp-data`, `erp-modules` | Data files (XML, CSV, external ids) and the module system: manifests, dependency graph, install, upgrade, uninstall. |
| `erp-rpc`, `erp-server` | The JSON-2 call layer and the HTTP server. |
| `erp-sdk`, `erp-macros`, `erp-guest`, `erp-wasm-host` | The addon API and its macros; the WebAssembly guest bindings and host. |
| `erp-ui-compiler`, `erp-ui-compiler-cli` | The template compiler for the web client and its `erp-ui-compile` program. |
| `erp-cli` | The `erp` program. |
| `erp-suite-*` | Isolated L13 domain integration test suites (`crates/erp-suite-<domain>`): depend only on core crates and their domain addon closure, never on `erp-cli`. |
| `erp-testkit`, `erp-test-models`, `erp-alloc-budget` | Test support: throwaway databases, core fixture addons, allocation budgets. |

## Building and testing

Use the commands in [CONTRIBUTING.md](../CONTRIBUTING.md#build-and-test). `scripts/check.sh --affected --tests` narrows
clippy and tests to the Cargo packages affected by your diff against `origin/main` (using `scripts/affected-packages.py`
and `scripts/check-layers.py`); `-p <pkg>` targets explicit packages, and `scripts/check.sh --tests` runs the full
workspace. Tests that need a database create and drop their own databases on the PostgreSQL server named by the
`DATABASE_URL` environment variable (or, for several servers, a space-separated `ERP_TEST_DATABASE_URLS`); the role must
be allowed to create databases.

## How a change flows

1. Write the behaviour down as a test first, next to the code it covers (or in a domain suite crate `crates/erp-suite-<domain>` for multi-addon integration tests). One test per behaviour; name it after
   what it proves.
2. Make it pass. **Fail loudly:** return a clear error for anything unsupported; never substitute a default or
   swallow an error to look finished.
3. Keep the core small. Business domains are addons, not core.
4. Run `scripts/check.sh --affected --tests`, and for web changes the web checks below.
5. Open a pull request: one topic, tests included, commits signed off.

## Writing an addon

An addon is a directory with a `manifest.toml`. Scaffold one:

```sh
erp scaffold my_addon --summary "My custom module" --author "Your Name"      # native Rust addon, in ./addons/my_addon
erp scaffold my_tags --data-only --summary "Declarative tags"               # data and views only
```

A native addon is a Rust crate with these parts (this is what `erp scaffold` writes):

```text
my_addon/
├── Cargo.toml          # depends on erp-sdk
├── manifest.toml       # name, summary, version, licence, author, depends, data files
├── src/lib.rs          # erp_sdk::addon!(name = "my_addon", depends = ["base"]);
├── src/models.rs       # models declared with #[model]
├── views/              # form and list views in XML
├── data/               # records and seed data in XML or CSV
└── tests/              # integration tests
```

Models are Rust structs:

```rust
use erp_sdk::prelude::*;

#[model(name = "my_addon.record", description = "My custom module")]
pub struct Record {
    #[field(char, default = "")]
    pub name: Option<Arc<str>>,
}
```

A data-only addon has no Rust code: models go in `models/*.toml`, and views, menus, access rows and demo data in data
files. `addons/sample_library` is a worked example, with `manifest.toml` keys such as `depends`, `data` (views,
`security/ir.access.csv`) and `demo`. The pieces an addon can contribute are models, extensions of models from other
addons, views and view patches (XPath), menus and actions, access rules and record rules, data and demo data, and
client-side code for the web client.

To add a native addon to this repository, put the crate under `addons/`, list it in the root `Cargo.toml`
(`[workspace] members` and `[workspace.dependencies]`, like the existing `addons/*` entries) and rebuild `erp`.
`scripts/check-layers.py` checks that the addon only depends on allowed crates.

To try an addon: install it into a database as shown in the [installation guide](install.md#install-an-addon-from-a-folder).

An addon can rebrand the server by shipping `static/brand/icon.svg`, `logo.svg` and `logo-dark.svg` and listing
them under `static_files` in its asset manifest (`static/dist/manifest.json`). Once it is installed, `/favicon.ico`
and `/erp/brand/icon.svg`, `logo.svg` and `logo-dark.svg` (used by the login page, the error page and the
application) answer with its images; an image it does not ship keeps the built-in one. The responses carry an
`ETag` and `Cache-Control: no-cache`, and SVG is sent with a sandboxing `Content-Security-Policy`. Only one
installed addon may ship brand images: installing a second one fails.

A field's `groups` (`#[field(..., groups = "base.group_system")]`) say who may read and write it. An administrator
can change that without changing the addon, in Settings › Technical › Security › Field Access (`ir.field.access`,
one override per field): **Also require these groups** (`extend`) requires one of its groups as well as one of the
declared ones, so it only narrows; **Replace the declared groups** (`replace`) puts its groups in their place (no
group: everyone who may access the model). The groups of a custom `x_` field stay in both cases. An override applies
wherever declared groups do: reads, writes, searches, `fields_get`, and the fields a view description shows (a
replaced field's view node takes no guard from the declaration; the description leaves the field out for a caller
who may not read it). A field declared for nobody (`groups = "."`, such as key digests) cannot be overridden, and
the policy ignores and reports a stored `replace` row for one. An override names one field: a related or delegated
(`_inherits`) copy keeps its own groups and needs its own override. Overrides are policy rows: a write rebuilds the
policy (the `policy` generation) and is audited like every write.

A form view opens a saved record editable. `<form edit_mode="read">` opens it read-first instead: the fields show as
read values with an Edit button (or the E key outside a field); a successful Save or a Discard return to read mode,
a refused save stays in edit mode with its errors, and a new record opens editable. A form with `edit="0"` is always
read-only and cannot declare read mode. A field widget of the record form receives `mode` (`read` or `edit`) and
`onRecordEvent(listener)`, which returns an unsubscribe function and reports `saved` (with the record id),
`saveFailed`, `discarded` and `modeChanged` after the form has shown the outcome; subscriptions end with the widget.

### Protecting addon records

Use `protect="1"` on an XML `<record>` with an external ID owned by your addon. For example,
`<record id="master" model="my_addon.record" protect="1">` protects the record behind
`my_addon.master`. Protection applies to regular PostgreSQL tables with a unique record ID.

Deleting the record, including through a foreign-key cascade, or deleting, renaming or
reassigning its protected external ID raises a validation error. Protection decisions apply
independently to each external ID, so another addon's alias can keep the same record protected.
Only the addon that owns an ID can change its declaration. The record's ordinary fields remain
subject to the model's own write rules.

An upgrade applies explicit protection changes even when it skips the record values under
`noupdate`. Use `protect="0"` to release protection while keeping the external ID. An upgrade
also releases protection for obsolete external IDs it removes. An omitted `noupdate` record
keeps its external ID and its protection until its owner explicitly releases it or uninstalls.
The addon lifecycle loader releases the departing addons' protection before removing their
data. A staying addon's protection still blocks a deletion, including one reached through a
cascade. Reviewed schema-only uninstall refuses plans that require releasing protection.

### Required migrations

List essential upgrade steps in the top level of the addon's `manifest.toml`.

```toml
required_migrations = ["migrations/1.1/post-backfill.sql"]
```

This release requires one implementation of `backfill` at stage `post` when upgrading from a
version below `1.1` to `1.1` or later. Keep the declaration in later releases that support
upgrades from before `1.1`. The addon can supply the SQL file or link a native migration named
`backfill` for version `1.1` and stage `post`. A package can supply SQL files when its
administrator approves the `migrations-sql` capability. Those scripts run as trusted SQL inside
the upgrade transaction. The upgrade refuses a missing or duplicate provider before applying any
migration. Fresh installation uses the target schema and data straight away.

### Approving RPC calls

Approval policies apply to ordinary RPC method dispatch, including browser requests and tools
called by a scoped agent. An explicit Deny refuses the call. A user call with no applicable
revision follows the usual authorization rules. An agent's unmanaged reads also follow those
rules, and its unmanaged mutations require an explicit policy. An Allow policy keeps the
caller's ordinary model, field, record and key-scope checks in force.

To request approval for a write or destructive method, send an `Idempotency-Key`. An unkeyed
call needing approval returns 422 with `context.approval_key_required`. The keyed 409 response
carries `context.approval_request_id`. The requester and eligible reviewers can call
`ir.approval.request.erp_status` with that ID. The receipt includes the requester's ID,
companies, payload digest and `can_decide`. An eligible signed-in reviewer decides by calling
`ir.approval.request.erp_decide` with `request_id`, `approve` and the optional displayed
`payload_sha256`.
The requester and anyone impersonating them are excluded from the decision.
An MCP tool call returns the same ID in `structuredContent.approval_request_id`.

In the web client, the requester gives the displayed request number to their reviewer.
The reviewer opens **Review approval** in the user menu, checks the operation, companies,
requester and argument digest, then chooses **Approve** or **Deny**. The requester selects
**Check approval** in the original window to finish. That window holds the original request
in memory and sends the same bytes and idempotency key after approval.

After approval, the requester repeats the original request with the same key. The server
checks the bound arguments, authenticated identity, policy revision, and the approver's current
eligibility. The call runs with the requester's authority. The request stores a digest of
the arguments and records the execution operation ID when the call commits. Pending and
approved requests expire after 24 hours according to the database clock. A fresh request needs
its own key. A policy revision change keeps earlier approval keys reserved.

Managed password operations use the checked session's approval policy in the Auth
transaction. An applicable Deny or Approve policy refuses the operation. Credentials
stay within password handling, with no approval request, argument digest or idempotency
record generated by that operation.

### Settings entries of an addon

Company settings use the exact selected company, including when that company is a branch. A
branch keeps its own saved values and audit revision. A corrupt stored value can be replaced with
a valid value through the normal settings write. Read the scope revision with an empty key list
when the corrupt value prevents a typed read. The audit records the invalid old value as a marker
and the accepted replacement as the new value.

Besides its settings (`erp_sdk::setting_declarations!`), an addon puts rows on its Settings page with
`erp_sdk::settings_panel_items!(items, resolve)`; each `PanelItemDecl` needs `.access(&[...])` (who sees it). An
information row (`PanelItemDecl::info`) shows a value its resolver computes; a link row (`action_link`, `menu_link`)
opens an action or a menu; a module toggle (`module_toggle(key, label, addon)`) installs or uninstalls an addon that
the install route offers (not the declaring addon or one it needs; a name the addon directory does not know is
refused when the page is published, on every load path of the `erp` command). The row shows the addon's state as the
database records it; while an install, upgrade or removal is under way the page answers with the same retry error as
the Addons page. A group toggle (`group_toggle(key, label, "my_addon.group_feature")`) turns a feature group on or
off for every internal user: on, `base.group_user` implies the group; off, that implication is removed, so internal
users lose the group unless they were given it directly or hold another group that implies it. The page calls
`ir.config.setting` `erp_toggle_group(key, enabled)`, which only Settings administrators (`base.group_system`) may
call; it writes a Settings audit row, and the application reloads afterwards so menus, views and fields follow. Off
is refused, naming the group, while `base.group_user` still implies the feature group through another group. Both
toggle kinds are readable only by `base.group_system`. The declared group must belong to an addon the declaring addon
depends on, not be `base.group_user`, `base.group_portal` or `base.group_public`, and be a light group (the
`res.groups` `_get_light_group_xmlids` list), so it can never hand out administrative rights; any other group is
refused by key and group when the page is published. When an administrator later deletes the group, or makes it
imply a group that is not light, in the Groups form, the page keeps working: only that row shows as not available,
with the reason, and its switch refuses (a row that is on for every internal user says so and can still be switched
off). The server logs a warning with the key and the reason for each such row when the registry is built, so a
mistyped group id or a group your addon ships with an implication that is not light would otherwise only show on
the page. Give your addon an install test that describes the Settings page and asserts that no group row you ship
has a `reason`. Use a group toggle only for a group that already shows or hides something (a menu, a view element
or a field).

### Company-dependent fields

Search ordering, grouped queries and keyset pagination use the current company's effective value. Use `create` or
`write` for company-dependent relations. In-memory records can link to another draft. Save the target or use a draft
with a saved origin before serializing the link for persistence.

Upsert can replace a scalar company value while preserving the other companies' values. Accumulation is unavailable
for company-dependent and monetary fields. Upserting a monetary amount requires a direct, stored currency field.
Put the currency of a company-dependent amount in the business key when updating the amount on conflict.

A stored company-independent output cannot depend on a company-varying input. Selection catalogs support direct
company-dependent fields and refuse defaults that need a named precision lookup. Big integer export uses decimal
text and exports nulls as empty cells. Big integer value aggregation is unavailable.

On local PostgreSQL 18 on Apple Silicon, creating and flushing 50,000 company-dependent bigint
records took 881 ms. Importing and flushing another 50,000 took 1.012 s in the same transaction.
Seven reads of the 100-record page at offset 40,000 produced median times of 18.51 ms with offset
and 18.80 ms with keyset paging. Each pair returned the same IDs. These timings cover bulk work
inside one transaction. An isolated indexed 100,000-row integer-to-bigint migration took 40.31 ms.
Reproduce the ORM measurement with a test cluster configured through `DATABASE_URL`.

```sh
cargo test --locked -p erp-test-models --test company_dependent_spec_cases profile_company_bigint_bulk_and_paging -- --ignored --exact --nocapture
```

### The request's client address

A method called over JSON-2 can read where the request came from: `env.request()` is the client address the server
resolved, the peer or, behind a [trusted proxy](deployment.md#https-behind-a-reverse-proxy), the address the proxy
forwarded. It is `None` for jobs, the command line, tests and internal calls, so a rule must decide what "unknown"
means. A client cannot set it: it is not part of the context, and a forwarded address from a peer that is not a
trusted proxy is refused before any method runs. `erp_sdk::net::IpNetwork` parses CIDR ranges and matches IPv4
clients seen through an IPv6 socket (`::ffff:a.b.c.d`).

```rust
use erp_sdk::net::IpNetwork;

let office: Vec<IpNetwork> = ["10.20.0.0/16", "2001:db8:20::/48"]
    .iter()
    .map(|text| text.parse())
    .collect::<Result<_>>()?;
match tx.env().request().map(|request| request.client_address()) {
    Some(address) if office.iter().any(|network| network.contains(address)) => {}
    Some(_) => return Err(Error::user("Clock-in is allowed only from the office network.")),
    None => return Err(Error::user("The network of this request is unknown.")),
}
```

### Recurring jobs of an addon

A native addon runs work on a schedule by giving one of its step job kinds a `Schedule`:

```rust
use erp_sdk::jobs::{HandlerRef, JobKindDecl, Schedule};

erp_sdk::jobs::register_job_kind!(
    DAILY_REFRESH,
    JobKindDecl::new(
        "my_addon",
        "my_addon.daily_refresh",
        HandlerRef::Step { model: "my_addon.record", method: "refresh_step" },
    )
    .schedule(Schedule::daily_at(1, 0, "Asia/Jakarta", "my_addon.user_scheduler"))
);
```

- The recurrence is `Schedule::daily_at(hour, minute, time_zone, run_as)` (wall-clock time in an IANA zone) or
  `Schedule::every(interval, run_as)` / `Schedule::every_from(interval, offset, run_as)` (every 60 s to 31 days,
  aligned to the Unix epoch plus the offset).
- `run_as` is the external id of an active internal `res.users` record the addon ships in its data, a technical
  user whose access rights the job runs with. A scheduled kind never runs as the system.
- Installing or upgrading the addon creates the kind's one job row, due at the next instant; uninstalling it removes
  the row (refused while it runs). A row that already exists keeps its due time, and a row an operator held stays held.
  The row cannot be enqueued by code.
- The row runs on whichever node runs jobs (a server with `jobs_run_in_app = true`); with several nodes it still runs once per
  due time. The step receives the payload `{"scheduled_for": "<RFC 3339>", "previous": "<RFC 3339>" | null}`:
  the instant this run stands for and the one the last completed run stood for. Runs missed while no node ran jobs
  collapse into one run for the latest missed instant, so a step that processes "the period since `previous`" stays
  correct and idempotent.
- When the step reports `finished`, the row is rescheduled to the next instant instead of being removed, and a kind
  that keeps history writes one `done` row per run. A failing run is retried with the kind's backoff and, when its
  attempts run out, dead-lettered like any job; the schedule then waits for an operator to requeue it, and resumes the
  same run.
The same addon can name the product and theme the sign-in pages:

- `static/brand/brand.json` is exactly `{"name": "<product name>"}`: plain text, 1 to 64 characters, no control
  or invisible formatting characters, no leading or trailing whitespace. The name replaces "ERPForAll" in the
  `<title>` of the login, login-success, error and application pages and in the logo text of the login page,
  the error page and the application header (the server sends it as `<meta name="application-name">`, always
  HTML-escaped). A manifest that is anything else fails the install, and the server refuses it again when it
  publishes the assets.
- `static/brand/login.css` is linked after the client stylesheet on the login and login-success pages, as
  `/erp/brand/login.css` (same headers as the brand images, served from the published assets; not found when no
  addon ships one). It is a theme, not new markup: set the `--login-*` custom properties
  (`--login-bg`, `--login-card`, `--login-fg`, `--login-line`, `--login-input-*`, `--login-link`, ...) and the
  colour tokens (`--accent`, `--canvas`, `--surface`, ...) on `:root` or `.login-screen`, and style the
  `.login-screen`, `.login-brand`, `.login-logo`, `.login-card`, `.login-field`, `.login-submit`,
  `.login-error`, `.login-hint` and `.login-footer` classes. Other selectors are not a supported surface and may
  change. The page policy is unchanged: the stylesheet is same-origin (`style-src-elem 'self'`), and the images
  and fonts it uses must be same-origin assets of the addon. The error page keeps its own built-in style.

Both files count as brand files: they must come from the addon that ships the brand images, if any.

### Outbox delivery

The owner connection can replay a named outbox consumer in bounded batches with
`Orm::replay_outbox`. Put its database effects in the supplied transaction so delivery and
acknowledgement commit together. A still-running transaction can hold back later events until the
database has resolved that part of the transaction-ID range. Use a stable consumer name across
nodes and restarts.

The server also polls outbox triggers on trusted System job kinds. `PerKind` groups events by
model and event kind within a batch. `PerRecord` also groups by record ID. A later batch can
schedule follow-up work while a prior job is running. A trigger is a signal to recompute from
current state. Its job payload contains `model`, `kind`, and a nullable `record_id`.

Owner-side retention advances only through old events acknowledged by every consumer. A
retention hold pauses deletion while delivery continues. During a hold, an operator can restore
a cursor to a retained position for deliberate redelivery. Positions older than the retention
floor are unavailable for replay.

Set `outbox_retention_days` in the protected server configuration to enable automatic retirement.
Each node processes at most 128 old acknowledged events per minute, with a shared database lock
coordinating retirement and replay. Give every node the same policy. A durable hold survives a
server restart.

Run these operations with the owner connection in the protected deployment configuration.

```sh
erp outbox status --config server.toml --consumer core.jobs
erp outbox retain --config server.toml --days 30 --limit 128
erp outbox hold --config server.toml
erp outbox restore --config server.toml --consumer core.jobs --xid 0 --id 0
erp outbox release-hold --config server.toml
```

Status reports the age of the consumer's cursor and the age of the next committed event in cursor order.
For the saved-thread WebSocket, the `thread socket delivery totals` debug record counts wakeups, heartbeats,
and resync frames after subscription admission without including session or record identifiers. A local two-node
PostgreSQL 18 run delivered seven resync frames for 16 committed posts. The fixture also checks
a hidden thread and closes the socket after the subscriber loses access. Re-run it to see how
the observed frame count changes with scheduling.

```sh
cargo test --locked -p erp-cli --test multiuser_mail_access saved_socket_filters_hidden_wakes_and_closes_after_role_change -- --exact --nocapture
```

### Retried writes: `Idempotency-Key`

A client that may send the same change twice (a phone with an offline outbox, a script that retries on a timeout)
sends an `Idempotency-Key` header with a JSON-2 call that changes data:

```text
POST /json/2/my_addon.record/submit
Authorization: Bearer <api key>
Idempotency-Key: 6f1c0a7e-1b8e-4a33-9a1f-2a6c4d1e9b10
```

- The key is a UUID (any version; v7 is recommended), one per user action, reused unchanged on every retry of
  that action. It is claimed in the same transaction as the change, so the change and its key commit or roll back
  together; a failed call stores nothing and may be sent again.
- Keys are private to the caller: the user, and for a Bearer request also the API key. The same key from another
  user or another API key is a different key.
- A retry with the same body within one hour gets the first answer again, byte for byte, with the response header
  `Idempotent-Replayed: true`; the method does not run again. Key order and whitespace of the JSON body do not
  matter.
- A key stays claimed for 24 hours. A retry after the first hour, or of an answer larger than 1 MiB, gets 409
  `erp.idempotency_result_expired` ("the original call committed; its result is no longer available"): the
  change happened, read it back instead. After 24 hours the key is free again and the request executes as new.
- The same key with a different body is refused with 422 `erp.idempotency_mismatch`; a retry while the first
  request still runs gets 409 `erp.idempotency_in_progress` with `Retry-After`; a key on a call whose effect is
  `read` is refused with 422 `erp.idempotency_not_allowed`. A malformed key, or the header sent twice, is 400.

A method reads the key it runs under with `tx.idempotency_key()` (`None` without the header, and in every
transaction the RPC layer did not open: jobs, cron runs and scripts never inherit it). Every retried attempt of
the same call sees the same key. Storing it on the record the method creates lets the client show which
submission a record came from, and lets the method answer the existing record when a retry arrives after the
24 hours instead of creating a second one:

```rust
#[method]
#[rpc(effect = write, model)]
async fn submit(
    this: &RecordSet<Request>,
    tx: &mut Transaction,
    note: String,
) -> Result<RecordSet<Request>> {
    let key = tx.idempotency_key().map(str::to_owned);
    if let Some(key) = &key {
        let existing = this
            .search(tx, &Domain::field("submission_key")?.eq(key.as_str()))
            .await?;
        if !existing.as_records().is_empty() {
            return Ok(existing);
        }
    }
    let mut vals = this.vals().set(Request::NOTE, note.as_str());
    if let Some(key) = &key {
        vals = vals.set(Request::SUBMISSION_KEY, key.as_str());
    }
    this.create(tx, vec![vals]).await
}
```

The `create`, `edit` and `delete` a list, form, kanban or calendar view describes are narrowed to the caller's
model rights by `get_views`: without the create right a view answers `create: false` (no New, no Duplicate, no
quick create), without the write right `edit: false` (a read-only form without read mode, no Archive, no dragging
cards), without the unlink right `delete: false` (no Delete). A view's own `create="0"` stays off for every caller.
Only the model-level rights are used; a record rule can still refuse one record when the command runs.

A refusal can name the fields of the record it is about: `Error::validation("…").with_fields(["date_end"])` puts
`fields: ["date_end"]` in the JSON-2 error body's `context` (other context keys are kept). The ORM names them itself for
a missing required value, an inverted period of a typed exclusion and a `#[constrains(...)]` method that names none
(the constrained fields this write changed). The web client also accepts `context.field` with one name. A form shows such
a refusal on those fields, with the server's message, instead of an error dialog; a validation or user error that names
no shown field gets a dialog with its message only, without the Error ID and technical details (kept when the server
granted debug details).

A list's control row has an Import / Export menu beside New. When first opened it asks the model's
`erp_impex_rights` (`{export, import}`): export needs the read right and the export right `export_data` checks (a member
of `base.group_allow_export` (Export Records, defined by base) or `base.group_erp_manager`), import needs the create and write rights. Import is also
offered only when the list allows creating and editing. Export writes the chosen columns of the selected records, or of
every record of the current search (at most 10,000), as CSV with a byte order mark; text that a spreadsheet could read as
a formula gets a leading apostrophe. Import reads a CSV file (at most 10 MB and 10,000 rows) whose first line names the
columns by field label or field name (`partner_id/id` paths included) and sends it to `load`; an unknown or repeated
column is refused before the call, and the messages `load` returns are shown with their file lines.

An import screen reads the user's file with `read_import_file` (on any model, like `load`): `file` (base64),
`file_type` (`csv` or `xlsx`), and optionally `sheet` (the worksheet; required when a workbook has more than one) or
`separator` (`,`, `;` or a tab, CSV only). It answers `{header, rows, sheet}`: the first row as column titles and the
data rows as strings, squared to the titles, without entirely empty rows. The screen previews them and passes the
same matrix to `load`, so a workbook is checked exactly like a CSV file. Workbook cells are read as text `load`
understands: numbers as stored, booleans as `1`/`0`, date-formatted cells as `YYYY-MM-DD` (with ` HH:MM:SS` when
they have a time), formulas as their saved value. An error value, a formula saved without its value, a date before
1 March 1900 or a value beyond the titled columns is refused with the cell's reference. A file is at most 16 MiB,
100 000 data rows and 1 000 columns, and each workbook part is bounded when it is unpacked; the request itself is
also bounded by `limits.rpc_body_bytes` (2 MiB by default).

A notebook `<page>` may hide per record: `<page name="history" string="History" invisible="state == 'draft'">`. The
expression reads fields this view shows (names, literals, comparisons, `not`/`and`/`or`, arithmetic and conditionals,
as for button `invisible`), and the client evaluates it on the record's current values: while it is true the page has
no tab and its content is not rendered, and a selected page that hides gives way to the nearest shown page before it.
`invisible="1"` still removes the page from the description. A page is not a security boundary: use `groups` to keep
its fields from users who may not read them.

### Batch transfers (the `stock` addon)

A batch transfer groups transfers of one company and one operation type so that a person confirms, reserves and
validates them together. Switch the feature on under Settings, Inventory, Batch transfers; it adds the Batch Transfers
menu under Operations and an Add to Batch button on the transfer form. Every batch action checks the feature group
`stock.group_stock_picking_batch`.

- A batch is Draft, In progress, Done or Cancelled. Transfers join through the Add to Batch dialog (a new batch, or an
  existing draft or in-progress one): a transfer that does not fit (another company, another operation type, already in a
  batch, wrong state) is named in the refusal and nothing is added. Confirm confirms the draft transfers; Check
  Availability reserves them; Cancel lets the open transfers go and keeps the done ones.
- Validate does not run in one transaction. It queues the job `stock.batch_validate`; each chunk validates up to
  `stock.batch_validate_chunk` transfers (system parameter, default 20, from 1 to 500), every transfer in its own
  savepoint, and writes one result row per transfer and run (`stock.picking.batch.result`: done, failed with the reason,
  or removed from the batch when nothing is reserved). A failed transfer stays in the batch; Validate again retries it
  as a new run. While a live job holds the batch, nothing joins, cancels or validates it again; if the job is gone, the
  batch is free again.
- `batch_id` of a transfer, and the status and transfer list of a batch, are written by the batch actions only. The type
  and company of a batch, and of a transfer in a batch, cannot change while the batch holds transfers. Create, edit,
  delete and every action of a batch need the feature group.
- Not yet: waves, automatic batching, merging batches, the batch report, scheduled date and responsible propagation.

## Session login hooks

An addon can override the private `res.users._after_session_login` method. Accept an
`erp_sdk::auth::SessionLogin`, pass it to `super`, and return the updated value. The hook
runs as the authenticated user inside the transaction that issues the active session.
It receives the canonical login, user ID, authentication method and device trust flag.
Use `set`, `get` and `remove` for addon-owned session data. Keys use lowercase ASCII
letters, digits, dots and underscores, starting with a letter or underscore. A session
admits 64 such keys, up to 63 bytes each and 16 KiB of JSON by default. Configure the
serialized size with `"session.data_bytes"` in the server's `[auth]` section. The hook
chain has a five-second deadline, configurable with `"limits.session_login_hook_deadline_ms"`. Hook errors
roll back both the session and transactional side effects. Identity confirmation leaves
session creation hooks alone.

Install and upgrade validate the composed login selection and session-token field
declarations before registry publication. A relational session-token field contributes its
ordered membership IDs, with a limit of 1,024. Changing membership invalidates existing
sessions bound to that field.

## The web client

The client is TypeScript, compiled ahead of time:

- `web/src` is the runtime (components and reactivity); `web/packages` holds shared packages such as the
  expression evaluator.
- The views, widgets and shell of the application are in the `web` addon: `addons/web/static/`.
- XML templates are compiled by `erp-ui-compile` and bundled by `web/scripts/build-human-ui.mjs`, as shown in the
  installation guide.
- A binary field shows as a file (the `binary` widget, also used when the view names no widget): its name, its size
  and Download, plus Upload or Replace and Clear when editable. `filename="document_name"` (or the `filename` option)
  names the char field of the same record that holds the file's name; Upload writes both. The
  `accepted_file_extensions` option (`'.pdf,image/*'`) is a hint for the file chooser; the server stays the
  authority. Use `widget="image"` for pictures.

A menu's icon (`web_icon` on `<menuitem>` or `ir.ui.menu`) is one of:

- `lucide:<name>`, a glyph the client ships (the menu icon picker lists them);
- `<addon>,static/<path>.svg` or `.png`, an image of the addon: list `static/<path>` under `static_files` in the
  addon's asset manifest (`static/dist/manifest.json`) so the server publishes it at `/<addon>/static/<path>`.
  The client draws it as `<img>` (an SVG there runs no script, and the server sends SVG files with a sandboxing
  `Content-Security-Policy`); a file that is not published shows the default glyph;
- a `data:image/svg+xml` or `data:image/png` URL of at most 2048 bytes.

The same icon is drawn on the home screen, in the navigation and in Settings. Any other value fails the write that
stores it, so an addon with a mistyped icon fails to install.

When the server refuses the window action an address names (403, for example a bookmark to an app the user may no
longer open), the shell keeps its header and menus and shows "This screen is not open to your account." with a "Go to
home" button; the address becomes the home address with the same companies, so Back does not return to the refusal.
Nothing the server said about the action is shown. Other failures still open the error dialog. An addon may replace
the body of that page (at most one page is registered) with `registerRefusedActionPage(key, { create(env) })` from
`public.ts`: `env` has `rpc` and `navigate(route)`, never the refused action; `create` returns
`{ element, dispose }`, and a `create` that throws is reported and the fixed page is shown instead.

### Active companies

The companies a page works in are the `allowed_company_ids` of every call, current company first. The shell
bootstrap (`GET /erp`) decides them once per load:

1. `cids` in the address (`/erp/action-42?cids=3,1`) wins for that load. A link carries its companies, and opening
   it does not change anything stored.
2. Otherwise, the companies the user last confirmed in the company switcher. The switcher stores them with
   `res.users.settings` / `set_res_users_settings({"active_company_ids": [3, 1]})` before it reloads; `False`
   clears the choice. The list must be non-empty, distinct, at most 64 ids, and only of the user's allowed companies:
   anything else is refused, on the generic `write` path too.
3. Otherwise, the user's default companies (main company first).

A stored choice never widens access. `get_res_users_settings` and the bootstrap keep only the companies the user is
still allowed, in the stored order. When none is left, the default companies apply again.

### Navigation from addon code

Systray items, service controllers, client actions and field widgets (in the main form, list cells and kanban cards;
not in dialogs or one2many lines) receive
two functions from the shell (`ShellNavigation` in `addons/web/static/src/client/registry.ts`, exported from
`public.ts` with the `Route` and `ActionRequest` types):

- `navigate(route)` opens a route through the shell's guards (an unsaved record is saved or the user is asked). The
  page's companies (`cids`) stay unless the route names its own; other companies load the page again with them, as
  the company switcher does. A malformed route or one that names a `method_action` is refused.
- `openAction({xmlId, viewMode, domain, context})` opens a window action by XML id as its list (or a new record in
  its form) with the given domain and `default_<field>` context keys, with a breadcrumb back to the page it was
  called from. Other context keys are refused.

Both do nothing once the page that handed them out was left. The address of every page keeps the active companies,
so a reload or a copied link opens the same companies.

A link to another record on a form (a many2one, a tag, a one2many cell, a wizard's link) or on a kanban card opens
that record with one call, `erp_resolve_record_action` on `ir.actions.act_window` (`model`, `record_id`): the server
picks the first window action of the user's visible menus, in menu order, that opens the current page, has a form view
of that model and whose domain holds the record, and answers it with its views, or `null` when the user may open none.
A domain of literal criteria and `uid` is checked against the record; an action whose domain reads anything else (the
context, for one) cannot be checked against one record and is taken only when no checkable action holds the record,
for a record the user may read. The form loads the record under the user's own access either way. Opened from a form, the new form goes on top of
it: the breadcrumb shows the list, the record and the opened record, and leads back to each. The same holds for a list
a form's button opens (an object button whose method returns a current-window action, or a declared action button), for
a form opened from that list, and for a link in a line dialog or a relation dialog, which closes first: every step stays
in the trail (up to 32) and each crumb returns to its own step. Dialogs and line views read their view descriptions
through the page's metadata cache, so a dialog opened again asks the server only for its record.

The trail survives a reload and follows the browser's Back and Forward. The client keeps, per history entry of the
tab, the trail as it was drawn (each entry's title and address, nothing else) in one session-storage key,
`erp.session.navigation` (`addons/web/static/src/client/navigation-store.ts`). Session storage belongs to the tab; the
key is erased on sign-out, when the session expires and on the sign-in page (nothing is written back after that, not
even by a request that fails later), and a store written for another user is dropped whole. A kept trail is shown only
with the page it was drawn for (same action, related list and record). What is read back is untrusted: each entry is checked on its own (a closed shape, an id of plain
characters, a title of at most 512 characters, an address the route parser admits) and an entry that fails is left
out, as is any entry past the cap of 32. Nothing in it is evaluated: an entry is only an address, which opens through
the normal navigation, so the server checks it again when the user goes to it. An entry that no longer opens (the
record is gone, the read finds no row for it, the user may no longer read it) drops out of the trail with the failure shown, and the other entries
stay usable; an entry that fails for another reason (a lost connection, a server failure) stays to be tried again. A
navigation that fails leaves the trail as it was for the page that stays. An address opened afresh (a pasted link, a new
tab, a full page load such as the company switcher's) has no history entry and starts its own trail; its entries are
numbered past the ones the tab keeps, so Back to an older entry still finds that entry's trail. A record saved on the way
out (the leave prompt, a crumb's autosave) is in the trail as the saved record, and a new record discarded there leaves
the trail. When that save happens on a Back or Forward step whose page then fails to open, the browser returns to the
record's entry and the entry takes the saved record's address.

A list a button opened (a current-window action result) has no address of its own: its address names it by an id of
the page (`method_action`). The same key keeps, for each such list, what the server answered for it (title, model,
views, literal domain and defaults) with the address and title of the page it was opened from, at most 64 lists of at
most 32 KiB each. Its context is read back only when every value is null, a boolean, a text, a number or a flat list of
numbers or texts. After a reload the list is decoded like a server answer and sent back to
`erp_normalize_action_result`, the check it passed when its button opened it, under the user's rights and the page's
companies; it opens as the server answers now. A list the server refuses, or one the client cannot read back, opens
nothing: the page offers the way back to the action it was opened under.

An object or stat button may return one record to open in its form: a current-window action with `res_id` and
exactly one form view (`[[view_id|false, "form"]]`), and no domain. `erp_normalize_action_result` admits it under the
same checks as a list (the model, the view, the defaults) and then checks that the record exists and that the user
may read it; a record the user may not read and one that does not exist are refused alike (`AccessError`), and a
malformed result (a domain next to `res_id`, a list view, an id that is not a positive integer) is refused by name
(`ValidationError`). The answer carries `res_id` and the domain `[["id", "=", res_id]]`. The client opens the form on
top of the page the button is on, so the breadcrumb leads back to it (opened from a page with no trail of its own, such
as a client screen, the form starts the trail; a result with no list view has no crumb for the action, which would open
a blank form), and it survives a reload like a list a button
opened. The count buttons of sales, purchase, invoicing and stock (an order's invoices, vendor bills, deliveries and
receipts, and the way back from an invoice, a bill or a transfer to its order) open the form directly when they count
exactly one document, and the list otherwise.

A current-window result from a button inside a dialog closes the dialog first, then opens on the main page, on top of
the page under the dialog.

A user's home action (`res.users.action_id`, a window action the user may set on their own record) opens in place of
the app grid when the address names no action. It loads through the usual access checks: an action the user may not
open leaves the grid with a notice. The logo still opens the grid.
The client's own texts (empty states, the New, Save, Discard and Edit buttons, the built-in commands, one2many
lines) are written in English and passed through `t()` (`addons/web/static/src/client/translate.ts`, exported by
`@erpforall/client`): `t("No records yet")`, or `t("No {name} records yet", { name })` with named placeholders. They
follow the user's language, the `lang` of the bootstrap locale: a text is shown from the catalog of that language,
else of its base language (`id` for `id_ID`), else in English. An addon ships a catalog, an object from English text
to translation, and registers it from its client entry:

```ts
import { registerTranslations } from "@erpforall/client";
registerTranslations("id_ID", { "No records yet": "…", "No {name} records yet": "… {name} …" });
```

A catalog that is not such an object, an empty translation, a translation that drops or adds a placeholder, and a
text two catalogs translate differently are refused with the text named, and nothing of the refused catalog is
kept. `node scripts/extract-client-strings.mjs [addon]` (in `web/`, default `web`) prints every `t()` text of an
addon's client sources as a catalog skeleton; a `t()` call whose text is not one plain string literal is refused,
so every text stays extractable. This repository ships the English sources only.

Checks (after `eval "$(scripts/web-toolchain.sh env)"` and `pnpm --dir web install --frozen-lockfile`):

```sh
cd web
pnpm run typecheck
pnpm run test
```

An object button in a form can ask for one short answer before its method runs, instead of a one-field wizard:

```xml
<button name="action_refuse" type="object" string="Refuse"
        prompt="text" prompt-label="Reason" prompt-required="1" prompt-max="500"/>
<button name="action_run" type="object" string="Run" prompt="date" prompt-label="Run for date"/>
<button name="action_close" type="object" string="Close"
        prompt="selection" prompt-label="Outcome" prompt-options="won:Won,lost:Lost"/>
```

The answer arrives as the `prompt_value` keyword argument: text, an ISO date or the chosen value, and `null` for an
empty optional answer. The method must take `prompt_value: String` (or `Option<String>` when the prompt is not
required); the view is refused otherwise. The client checks required, length, date and choice before calling, but the
method can be called directly over RPC, so it validates its argument like any other. A prompt cannot combine with
`confirm`, and a wizard stays the way for several fields.

### Runtime bundle budget

From `web/`, run `pnpm run size` to measure the complete Elang public API and compiled-template
ABI in one ES module. Run `node scripts/size.mjs --inputs` to see each source module's minified
byte contribution. The milestone suite pins the exact minified size and allows a 32-byte
difference in either direction for gzip level 9. Run all three milestone suites with
`pnpm run test:milestone`.

The M4 review rebuilt these sources with Node 24.22.0 and esbuild 0.28.2. The original test
declared a budget of 37,203 minified bytes and 13,379 compressed bytes.

| Source | Minified bytes | Gzip bytes |
| --- | ---: | ---: |
| Initial commit `4f1b9736` | 95,287 | 30,832 |
| Main `3215c5f8` | 95,543 | 30,926 |
| M2/M3 tree `0e4a9557` | 95,543 | 30,926 |
| M4 with restricted cards | 98,556 | 32,025 |

The optional card renderer accounts for 2,985 of the 3,013 additional minified bytes. It checks
the authorized card identity, field projections, HTML allowlists and output bounds. The remaining
28 bytes export the renderer. M4 adds 1,099 bytes to the compressed full API. For a consumer
importing only `signal`, esbuild retains just the card module's 49-byte license comment. An
ordinary CI test checks that the allowlists and renderer are removed from that consumer.

### Installed shell browser acceptance

Build `erp-ui-compile` in the same Cargo profile as the server. Set `SHELL_ACCEPTANCE_OUTPUT`
to the evidence directory and supply the approved WASI guest and its pinned digest in
`ERP_UI_CARD_GUEST` and `ERP_UI_CARD_GUEST_SHA256`. With PostgreSQL and Chrome available,
run the fixture through the normal installed server.

```sh
cargo test --locked -p erp-cli --test installed_shell_browser -- --ignored --nocapture
```

The shell fixture records request paths, status codes, time to ready and scoped accessibility
results in `installed-shell.json`. It opens a 500-line sales order, pages through 600 contacts,
and checks keyboard editing, locale direction, permitted records, preferences and logout.
A separate base-and-web fixture records its served Kanban card checks in
`installed-restricted-cards.json`. It renders a visible contact and checks that an administrator
and an internal reader receive different results for a group-protected card projection.

### Form open time

`crates/erp-cli/tests/form_open_browser.rs` measures how long a form takes to open in a real browser: it installs
sales, purchase, stock and accounting on a native server, creates a customer, products and orders with 0, 50 and 500
lines, and `web/scripts/browser-form-open.mjs` times click to paint, with every request the page makes (time to first
byte, size), for a list row to its form, a many2one link to its form, and an order's count button to its list and
on to a record. It is a measurement, not a gate: it is ignored in every ordinary run, asserts only that each step
reached its page, and writes `result.json` to the directory you name. Run it on a build machine with an optimized
build:

```sh
cargo build --locked --profile release-tests -p erp-ui-compiler-cli --bin erp-ui-compile
FORM_OPEN_OUTPUT=<dir> cargo test --locked --profile release-tests -p erp-cli --test form_open_browser -- --ignored --nocapture
```

One run on a 12-core Linux build machine (7 samples each, load average 18 at the start and 11 at the end, revision
74941cc), click to paint:

| Scenario | Median ms | Min to max ms | Requests before paint | Largest call before paint |
|---|---|---|---|---|
| list row to form, 0 lines, first | 47.9 | 41 to 66 | 5 | line views, 13 KB |
| list row to form, 0 lines, again | 33.4 | 31.8 to 60 | 4 | record read, 1 KB |
| list row to form, 50 lines, first | 72.9 | 69 to 77.9 | 6 | record read, 26 KB |
| list row to form, 50 lines, again | 59.4 | 52.8 to 75.2 | 5 | record read, 26 KB |
| list row to form, 500 lines, first | 310.5 | 297.6 to 340.1 | 6 | record read, 258 KB |
| list row to form, 500 lines, again | 331.6 | 300.4 to 435.1 | 5 | record read, 258 KB |
| many2one link to form, first | 66.4 | 59.5 to 82.4 | 5 | action resolve, 10 KB |
| many2one link to form, again | 50.1 | 46 to 80.5 | 2 | messages, 1 KB |
| count button to list | 48.3 | 47.4 to 54.2 | 6 | list views, 19 KB |
| that list's row to form | 62.0 | 46.9 to 80.1 | 8 | line views, 10 KB |

Every call's median time to first byte was under 15 ms. On the 500-line form the record read had finished about 35 ms
after the click and the last request about 115 ms after it; the rest of the 300 ms until paint is the client's own
work.

## Printed reports for native clients

`GET /erp/report/pdf/<report>/<ids>` (`?download=1` for an attachment) answers the PDF of a report. A browser sends
its session cookie; a native client without one (a mobile app) sends the API key it signed in with instead:

```http
GET /erp/report/pdf/my_addon.report_document/42?download=1
Authorization: Bearer <key>
```

The key is checked as on JSON-2 (an `rpc` user key, the address throttle, a foreign `Origin` refused, a cookie sent
alongside must belong to the key's user) and its user is the one the report prints for: the report's groups, read
access on every id and the id limits apply as for that user's browser session. The answer is
`application/pdf` with `Cache-Control: private, no-store` and no cookie. A refusal is the JSON error of JSON-2 (or a short text), not the HTML error page a browser
navigation gets. The HTML preview
(`/erp/report/html/...`) stays with the browser session; a key is refused there.

## Pull request rules

- One topic per pull request, with tests for the behaviour you change.
- Errors fail loudly; no silent defaults.
- No code copied from other ERP projects or from any source whose licence does not allow it.
- Every commit carries a sign-off (`git commit -s`).
- This project is built with AI agents alongside people; agents follow the same rules, and the person who submits
  the change signs it off.

### Grouped and analytical views

Kanban groups by the first active search grouping or its declared default. Each column loads cards
up to the configured limit. Folded columns keep their position in the page address. A card's
move selector and drag gesture use the same record write. A refused move leaves the card in its
original column. Quick creation passes the column value through the model's usual defaults.

Calendar date inputs use the session timezone. Moving the start also moves the end.
Changing only the end resizes an event. Open an event to see the actions its record rules allow.
Each multi-day event occupies one bar per displayed week. A pivot can declare a named
`variant` registered by its client bundle with `registerPivotVariant`. Date groupings default
to month. The pivot export worker bound is `web.pivot_export_max_concurrent` per server process.
Graph SVG export uses the chart and applied filters already on screen.

Install `web_map` to add Contact Map. Mark a contact as having coordinates and enter its stored
latitude and longitude on the Details tab. The optional addon owns the bare `map` type. Its
`<map latitude="..." longitude="..."/>` declaration takes two distinct stored float fields.
The action domain and the caller's usual record rules constrain the single bounded map read.
Zero is a valid coordinate. The map's land outlines come from Natural Earth's public-domain
1:110m land dataset, version 5.1.2. All map rendering happens locally.

Native view descriptions resolve types against the installed registry generation. A missing
declaration reports the unregistered type by name. A published custom type without a human UI
wire adapter reports that the type is unsupported. A client without the requested renderer or
pivot variant reports its name. A WebAssembly package can use installed view types in XML data.
The package ABI has no callback for declaring a new view type. Module composition returns
`GuestViewTypesUnsupported` with the package name if a package claims a native view registration.

### Localized template bundles and restricted cards

Extract source terms from trusted client templates with
`node web/scripts/build-human-ui.mjs --addons web --extract-terms /tmp/web-terms.json`.
Compile translations with `--catalogue /path/to/catalogue.json`.
The catalogue has a `language` and a `terms` array. Each term specifies `addon`, `source`
and `translation`. An empty addon selects a global term. Publishing through
`AssemblyOptions::with_catalogue` includes the language and canonical entries in the immutable
asset identity. A template call inherits the caller's translation context. Extensions retain
the original writer of each independently replaced literal.

Database card templates use the WASI build of the compiler. Build it with
`cargo build --release --target wasm32-wasip1 -p erp-ui-compiler-cli --bin erp-ui-compile`.
Set `ERP_UI_CARD_GUEST` to that file and `ERP_UI_CARD_GUEST_SHA256` to its approved digest on the server.
Distribute the digest through the same trusted channel as the server configuration.
`erp-ui-compiler-host GUEST.wasm PINNED_SHA256` accepts one framed `ArchCompileRequest` on stdin
and returns a JSON card plan. The request binds the source digest, arch identity, language,
field schema and compiler policy. Policy 1 accepts static same-arch template calls, ordinary
card elements and attributes, `t-if`, `t-out`, `t-esc` and field projections.
The browser API `prepareRestrictedCard(plan, identity)` accepts the matching authorized arch
identity and renders individual records into detached DOM fragments. The compiler reports
the original XML line and column for an unsupported element or field.

To verify the deployed guest against the native compiler, set `ERP_UI_COMPILE_WASM` to the
built WASI binary and run
`cargo test -p erp-ui-compiler-host --features test-raw-input --test wasm_artifact -- --ignored`.

### Report rendering and foreign PDF documents

Report dates and durations use the bundled CLDR tables for English, French and Indonesian.
Contact widgets can request address, telephone, email and tax fields through the report
data plan. Group conditions are resolved for the printing user. Marked-up report terms in
`model_terms:ir.ui.view,arch_db` use numbered placeholders such as `Partner {0} ready` to
reorder inline children while retaining their original markup and field permissions.
The HTML preview embeds each authorized raster once and reuses it at each placement. Uploaded
SVGs remain alt-text boxes. Barcodes use vector paths generated by the report engine.

The default print pool runs one worker, one document at a time, with a 30-second render
deadline and a 1 GiB resident-memory cap. On Linux the address-space limit includes a further
256 MiB of headroom. Operators can configure the pool separately from the ORM connection
pool. To measure cold and warm latency on the local host, run
`cargo test -p erp-cli --test report_print_locale -- --nocapture` with the test database
available. It renders the same two-page localized partner report four times through the
real print worker and reads the resulting PDF pages. On an Apple M3 Pro running macOS arm64,
a debug build took 109 ms for the first render in a newly created pool and 3, 1 and 2 ms for
the following renders. During concurrent workspace validation on the same host, a freshly
linked executable took 143,369 ms on its first render and 10, 3 and 4 ms on subsequent calls.
Both runs used an already prepared document. Each interval covers the worker call, including
process startup on the first call, and ends before the test decodes the PDF. Measure launch
latency separately when sizing a pool for a deployment.

`erp-pdf` accepts unencrypted input. A reading copy can retain self-contained annotations,
remap named destinations, and preserve bookmarks with `OutlinePolicy::Keep`. The default
omits bookmarks. Supported stream-decoding stages include Flate, LZW, ASCII85, ASCIIHex and
RunLength, with bounded buffers and supported TIFF/PNG predictors. Unsupported image codecs
and Crypt filters return a named error when decoding is required. Signed input defaults to
refusal. `SignedCopyPolicy::UnsignedCopy` creates a disclosed unsigned reading copy.
`SignedEditPolicy::AppendOnly` admits a bounded subset of incremental edits and preserves
the original file as the exact prefix. It checks declared modification restrictions and
provides no cryptographic verification of a signature. Run foreign-document processing in
an isolated worker and publish the result only after the full operation succeeds.

A stored Kanban arch can add a `<restricted_templates>` element beside its ordinary
`<templates>` declaration. Declare the record fields in the ordinary card and name the
restricted entry `card`. The server passes the visible template subtree and permitted scalar
card fields to the pinned WASI compiler. A compiler rejection identifies the source view and
the original start tag's line and column. A missing compiler produces an explicit error.

```xml
<kanban>
  <templates><t t-name="card"><field name="name"/></t></templates>
  <restricted_templates>
    <t t-name="card"><strong><t t-esc="record.name.value"/></strong></t>
  </restricted_templates>
</kanban>
```
