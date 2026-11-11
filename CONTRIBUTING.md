# Contributing to ERPForAll

Thank you for helping. This page covers the licence of contributions, the sign-off, and how to build and test.

## Licence of contributions

ERPForAll is licensed under `MIT OR Apache-2.0` at the user's option (see [`LICENSE-MIT`](LICENSE-MIT),
[`LICENSE-APACHE`](LICENSE-APACHE) and [`NOTICE`](NOTICE)). Unless you state otherwise, any contribution you
submit is licensed under the same terms, with no additional conditions ("inbound = outbound"). New source files
carry `SPDX-License-Identifier: MIT OR Apache-2.0`.

Do not contribute code you may not license this way: no code copied from other projects without a compatible
licence, and no dependency whose licence `deny.toml` does not allow (`cargo deny check` enforces it).

## Developer Certificate of Origin

Every commit carries a sign-off line saying you have the right to submit it under the project licence, as defined by
the [Developer Certificate of Origin 1.1](https://developercertificate.org/):

```sh
git commit -s
```

adds `Signed-off-by: Your Name <you@example.com>` using your git identity.

## Build and test

The layout of the repository and how a change flows are described in [docs/development.md](docs/development.md).

You need the Rust toolchain pinned in `rust-toolchain.toml` (rustup picks it up), PostgreSQL 18 for the database
tests, and for the web client the Node and pnpm versions pinned by `scripts/web-toolchain.sh`.

```sh
cargo build --workspace                          # build everything
cargo fmt --all --check                          # formatting
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo nextest run --locked --workspace           # tests (needs DATABASE_URL for database tests)
cargo deny --locked check                        # licences and advisories
scripts/web-toolchain.sh install                 # once: pinned Node and pnpm
eval "$(scripts/web-toolchain.sh env)"
pnpm --dir web install --frozen-lockfile
(cd web && pnpm run typecheck && pnpm run test)
```

`scripts/check.sh` runs the formatting, lint and build steps in one go; `scripts/check.sh --tests` adds the test suites.

## Pull requests

- This project is built with AI agents alongside people. Contributions made by agents follow the same rules as any other; the person who submits the change adds the DCO sign-off.
- One topic per pull request, with tests for the behaviour you change.
- Errors fail loudly: no swallowed errors, no silent defaults; an unsupported case returns a clear error.
- Keep the core small: business domains are addons, not core.
- Run the formatting, lint and test commands above before you push.

## Names

The names "ERPForAll" and "Elang" are not covered by the licence. A modified version you distribute must use a
different name.
