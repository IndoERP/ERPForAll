<h1 align="center">ERPForAll</h1>

<p align="center">
  <b>An open-source ERP framework rebuilt in Rust, compatible with the way Odoo works.</b><br>
  Models, views, menus, access rules and addons that extend each other, the way Odoo developers expect,<br>
  on a typed Rust ORM over PostgreSQL and a native web client. Open source, self-hosted, and yours.
</p>

<p align="center">
  <a href="#why-erpforall">Why</a> ·
  <a href="#what-works-today">What works today</a> ·
  <a href="#addons-all-the-way-down">Addons</a> ·
  <a href="#built-for-agents">Agents</a> ·
  <a href="#under-the-hood">Under the hood</a> ·
  <a href="#get-started">Get started</a> ·
  <a href="#contributing">Contributing</a> ·
  <a href="#license">License</a>
</p>

> [!IMPORTANT]
> **Status: early development.** The framework core (ORM, addon platform, security, web client) works and is tested,
> and the first business addons are being built on it. It is **not ready to run a business yet**: accounting,
> inventory and sales are still ahead. Issues and pull requests are welcome.

<br>

<table>
  <tr>
    <td width="25%" valign="top">
      <h3>Familiar by design</h3>
      Models with <code>_inherit</code>, XML views with XPath extension, menus, actions, access rules, record rules,
      data files with xml ids, demo data. If you have written an Odoo addon, you already know the shape of an
      ERPForAll addon.
    </td>
    <td width="25%" valign="top">
      <h3>Rust and PostgreSQL</h3>
      A typed ORM with a columnar cache, domains compiled to SQL and access rules compiled into the query. Errors
      fail loudly instead of being swallowed.
    </td>
    <td width="25%" valign="top">
      <h3>Addons all the way down</h3>
      The core stays small. Contacts, products, mail and every business app are addons: native Rust crates,
      data-only addons, or sandboxed WebAssembly packages.
    </td>
    <td width="25%" valign="top">
      <h3>Agent-ready</h3>
      Every exposed model method is a JSON-2 RPC call under the same access rules as the web client, with API keys
      for scripts and agents.
    </td>
  </tr>
</table>

<br>

## Why ERPForAll

An ERP that companies shape to themselves through addons, without forking the core, is the right model.
ERPForAll keeps that model and rebuilds what is underneath: a typed ORM, a compiled query layer, a small
native web client and a sandboxed extension path. We look at how the best systems behave, adopt what is proven,
and change what can be better. Behaviour is pinned down by tests.

- **Compatible concepts, not copied code.** ERPForAll is a clean reimplementation: no Odoo source code is copied.
- **Fail loudly.** Unsupported features return a clear error naming what is missing; no silent defaults.
- **Measure, don't claim.** No performance claim without a published, reproducible measurement.

## What works today

| Area | State |
|---|---|
| **ORM** | Models and inheritance, all field types, recordsets, CRUD, domains to SQL, prefetch and cache, computed and related fields, constraints, onchange, `read_group`, sequences, translations, multi-company |
| **Addon platform** | Manifests and dependency graph; install, upgrade and uninstall per database in one transaction; model and view extension across addons; data files and demo data; data-only addons; WebAssembly addon packages |
| **Security** | Access rules and record rules compiled to SQL, field-level groups, users and companies, login, sessions, API keys, password change |
| **Web client (Elang)** | Shell, menus, actions, form and list views, search panel, field widgets and editors, dialogs and wizards, settings, chatter (in progress) |
| **Addons** | `base`, `web`, `uom`, `contacts`, `product`, `mail` (in progress); the inventory addons are being specified |
| **Operations** | One `erp` command line for databases, modules, servers and node rollouts |

## Addons all the way down

An addon is a folder with a `manifest.toml`. It can add models, extend models from other addons, add or patch views,
menus, actions, access rules and data, and extend the web client through registries. The `addons/` directory holds
the ones in this repository; `examples/` holds sample WebAssembly addons.

```sh
erp scaffold my_addon --summary "My custom sales module" --author "Your Name"   # a native Rust addon
erp scaffold custom_tags --data-only --summary "Declarative tags"                # data and views only
erp module install -d my_db my_addon --addons-path ./addons
```

## Built for agents

The same typed methods serve the web client, scripts and AI agents. A model method marked `#[rpc]` is reachable at
`/json/2/<model>/<method>`, checked against the caller's access rules like any other request, and API keys let a
script or an agent act as a scoped user. Nothing an agent does bypasses the rules a person is held to.

The repository itself is set up for coding agents: [`AGENTS.md`](AGENTS.md) holds the working rules and a map of the
code (Claude Code reads it through `CLAUDE.md`). Tools and skills for agents will follow.

## Under the hood

- **Rust crates under `crates/`**, layered so lower layers never depend on higher ones (`scripts/check-layers.py` checks it).
- **`erp-orm`** on PostgreSQL: typed recordsets, a columnar cache and SQL generated from domains.
- **`erp-modules`**: the addon graph, install, upgrade and uninstall, with a migration gate.
- **Elang**, the web client under `web/` and `addons/web/`: TypeScript on a small runtime of its own, with views
  served as typed descriptions.
- **WebAssembly addons** on wasmtime, so third-party code runs sandboxed.

## Get started

You need Rust (the version is pinned in `rust-toolchain.toml`; rustup installs it), PostgreSQL (we develop and test on
version 18), and for the web client Node and pnpm at the versions pinned in `scripts/web-toolchain.sh`.

```sh
git clone https://github.com/IndoERP/ERPForAll.git
cd ERPForAll
cargo build -p erp-cli --bin erp      # the erp command line
scripts/check.sh                      # format, lints, build; add --tests to run the test suites
```

Tests that need a database create and drop their own databases on the PostgreSQL server named by `DATABASE_URL`.
The step-by-step guide to a running server with the web client is [docs/install.md](docs/install.md). A
one-command local setup (database, demo data and the web client in a browser) is not available yet; it is one of the
first things we want help with.

## Repository layout

| Path | What it holds |
|---|---|
| `crates/` | The Rust framework: ORM, registry, module system, data files, security, calls and HTTP server, addon API, the `erp` command line, test support. |
| `addons/` | Addons that ship here: `base`, `web`, `uom`, `product`, `contacts`, `mail`, the stock family and a sample. |
| `web/` | The web client runtime (TypeScript, pnpm workspace). The client side of each addon is in `addons/<name>/static/`. |
| `examples/` | Sample addons built as WebAssembly components. |
| `wit/` | The WebAssembly interface those components implement. |
| `fuzz/` | Fuzz targets. |
| `vendor/` | Patched third-party crates, with their own licences. |
| `testdata/` | Shared test fixtures. |
| `scripts/` | Helper scripts: checks, generators, the pinned web toolchain. |
| `docs/` | The guides below. |

## Guides

- [Install](docs/install.md): from a checkout to a running server.
- [Develop](docs/development.md): layout, writing an addon, the web client, how a change flows.
- [Deploy](docs/deployment.md): release build, service, HTTPS, upgrades, backups.

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md): licence of contributions, sign-off, and how to build and test, and
[docs/development.md](docs/development.md) for how the code is organised. Coding agents start with
[`AGENTS.md`](AGENTS.md). Security reports go through [`SECURITY.md`](SECURITY.md).

## License

ERPForAll is licensed under either of

- [Apache License, Version 2.0](LICENSE-APACHE)
- [MIT license](LICENSE-MIT)

at your option. See [`NOTICE`](NOTICE). Vendored third-party code keeps its own licence (see `vendor/`).
Unless you state otherwise, any contribution you submit is dual licensed as above, without additional terms.

<sub>Odoo is a trademark of Odoo S.A. ERPForAll is an independent open-source project, not affiliated with, sponsored by or endorsed by Odoo S.A.</sub>
