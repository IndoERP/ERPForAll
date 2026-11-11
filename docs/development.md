# Developing ERPForAll

This guide is a map for people changing ERPForAll or writing addons for it. Installing and running a server is in
the [installation guide](install.md); running one in production is in the [deployment guide](deployment.md). The
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

## The web client

The client is TypeScript, compiled ahead of time:

- `web/src` is the runtime (components and reactivity); `web/packages` holds shared packages such as the
  expression evaluator.
- The views, widgets and shell of the application are in the `web` addon: `addons/web/static/`.
- XML templates are compiled by `erp-ui-compile` and bundled by `web/scripts/build-human-ui.mjs`, as shown in the
  installation guide.

Checks (after `eval "$(scripts/web-toolchain.sh env)"` and `pnpm --dir web install --frozen-lockfile`):

```sh
cd web
pnpm run typecheck
pnpm run test
```

## Pull request rules

- One topic per pull request, with tests for the behaviour you change.
- Errors fail loudly; no silent defaults.
- No code copied from other ERP projects or from any source whose licence does not allow it.
- Every commit carries a sign-off (`git commit -s`).
- This project is built with AI agents alongside people; agents follow the same rules, and the person who submits
  the change signs it off.
