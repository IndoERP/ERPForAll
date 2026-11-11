# Vendored crates

Third-party crates carried in this repository as path crates, wired by `[patch.crates-io]` in the workspace
`Cargo.toml`. They are not workspace members: no workspace lints, their own edition. Each keeps its upstream
licence file and `Cargo.toml` licence field. Local modifications are kept as patch files next to the crate or in
the history of the crate's directory.

| Directory | Crate |
|---|---|
| `vendor/hyper` | `hyper` |
| `vendor/tokio-postgres` | `tokio-postgres` |
| `vendor/html5ever-bounded` | a bounded-input fork of `html5ever` |
