# AGENTS.md

Instructions for coding agents and for people working in this repository. Read this first; it is short on purpose.

## What this project is

ERPForAll is an ERP framework written in Rust: a typed ORM over PostgreSQL, an addon system (models, views, menus,
access rules and data that extend each other), a JSON-2 call layer, an HTTP server and a web client. The core stays
small; business domains are addons. It is in early development and not ready to run a business yet.

## Repository map

- `crates/`: the framework, one crate per concern. Lower layers never depend on higher ones
  (`scripts/check-layers.py` checks it). `erp-orm` (ORM), `erp-registry` (model metadata), `erp-modules` (module
  system), `erp-data` (data files), `erp-security` (access and sessions), `erp-rpc` and `erp-server` (calls and HTTP),
  `erp-sdk` and `erp-macros` (the addon API), `erp-cli` (the `erp` program), `erp-testkit` (test databases).
- `addons/`: addons that ship here (`base`, `web`, `uom`, `product`, `contacts`, `mail`, the stock family,
  `sample_library`). Addon crates may depend only on `erp-sdk`.
- `web/`: the web client runtime (TypeScript, pnpm workspace). The client side of an addon is in
  `addons/<name>/static/`.
- `examples/`: sample WebAssembly addons. `wit/`: their interface. `fuzz/`: fuzz targets.
- `vendor/`: patched third-party crates; keep their licences, do not edit casually.
- `testdata/`: shared test fixtures. `scripts/`: helper scripts. `docs/`: the guides.

## Build and test

Commands are in [CONTRIBUTING.md](CONTRIBUTING.md#build-and-test) and [docs/development.md](docs/development.md).
In short: `scripts/check.sh` (format, lints, build) and `scripts/check.sh --tests` (adds the Rust tests; needs
PostgreSQL 18 reachable through `DATABASE_URL`). Web checks: `pnpm run typecheck` and `pnpm run test` in `web/`, after
`eval "$(scripts/web-toolchain.sh env)"`. Do not guess flags: `erp --help` and the subcommand help list what exists.

## Rules

1. **Tests per behaviour.** A change that adds or alters behaviour comes with a test for it, next to the code.
2. **Fail loudly.** An unsupported case returns a clear error. No silent defaults, no swallowed errors, no
   "unsupported means true".
3. **Keep the core small.** Business logic goes into addons, not into core crates.
4. **No copied code.** ERPForAll is a clean reimplementation: no Odoo source code is copied, and none may be
   contributed. Do not paste code from other projects unless its licence allows it and you say where it came from.
5. **Never claim what you did not run.** Report test and build results only if you ran them.
6. **No secrets** in the repository, in logs or in test output.
7. **DCO sign-off.** Every commit is signed off by the person who submits it (`git commit -s`). Agents do not sign
   off on their own; the human submitter does.
8. **One topic per pull request**, small enough to review.
9. Match the surrounding style. `cargo fmt` and clippy with `-D warnings` must pass.

## Where to look first

- Installing and running: [docs/install.md](docs/install.md). Production: [docs/deployment.md](docs/deployment.md).
- Layout, crates, writing an addon, the web client: [docs/development.md](docs/development.md).
- The `erp` program: `crates/erp-cli`. The server and its configuration: `crates/erp-server`.
- A model, a field or the ORM: `crates/erp-orm`, `crates/erp-fields`, `crates/erp-registry`.
- A view or the web client: `crates/erp-views`, `addons/web`, `web/`.
- Tests for a crate are in its `tests/` directory and in `#[cfg(test)]` modules; `crates/erp-test-models` holds the
  fixture addons many tests use.

Tools and skills for agents will be added to the repository later.
