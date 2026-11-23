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
| `erp-testkit`, `erp-test-models`, `erp-alloc-budget` | Test support: throwaway databases, fixture addons, allocation budgets. |

## Building and testing

Use the commands in [CONTRIBUTING.md](../CONTRIBUTING.md#build-and-test). `scripts/check.sh` runs format, lints and
a build; `scripts/check.sh --tests` adds the Rust tests. Tests that need a database create and drop their own
databases on the PostgreSQL server named by the `DATABASE_URL` environment variable (or, for several servers, a
space-separated `ERP_TEST_DATABASE_URLS`); the role must be allowed to create databases.

## How a change flows

1. Write the behaviour down as a test first, next to the code it covers. One test per behaviour; name it after
   what it proves.
2. Make it pass. **Fail loudly:** return a clear error for anything unsupported; never substitute a default or
   swallow an error to look finished.
3. Keep the core small. Business domains are addons, not core.
4. Run `scripts/check.sh --tests`, and for web changes the web checks below.
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

A form view opens a saved record editable. `<form edit_mode="read">` opens it read-first instead: the fields show as
read values with an Edit button (or the E key outside a field); a successful Save or a Discard return to read mode,
a refused save stays in edit mode with its errors, and a new record opens editable. A form with `edit="0"` is always
read-only and cannot declare read mode. A field widget of the record form receives `mode` (`read` or `edit`) and
`onRecordEvent(listener)`, which returns an unsubscribe function and reports `saved` (with the record id),
`saveFailed`, `discarded` and `modeChanged` after the form has shown the outcome; subscriptions end with the widget.

A list's control row has an Import / Export menu beside New. When first opened it asks the model's
`erp_impex_rights` (`{export, import}`): export needs the read right and the export right `export_data` checks (a member
of `base.group_allow_export` (Export Records, defined by base) or `base.group_erp_manager`), import needs the create and write rights. Import is also
offered only when the list allows creating and editing. Export writes the chosen columns of the selected records, or of
every record of the current search (at most 10,000), as CSV with a byte order mark; text that a spreadsheet could read as
a formula gets a leading apostrophe. Import reads a CSV file (at most 10 MB and 10,000 rows) whose first line names the
columns by field label or field name (`partner_id/id` paths included) and sends it to `load`; an unknown or repeated
column is refused before the call, and the messages `load` returns are shown with their file lines.

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

## Pull request rules

- One topic per pull request, with tests for the behaviour you change.
- Errors fail loudly; no silent defaults.
- No code copied from other ERP projects or from any source whose licence does not allow it.
- Every commit carries a sign-off (`git commit -s`).
- This project is built with AI agents alongside people; agents follow the same rules, and the person who submits
  the change signs it off.
