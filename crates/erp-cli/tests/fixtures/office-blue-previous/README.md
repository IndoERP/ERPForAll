# Previous Office Blue header inputs

These are our own exact XML/manifests from repository commit
`b40e68f3a1e6b6748ae8f87970dd1a33a71bf6d1` (before the state-header change):

- `library-views.xml`: `addons/sample_library/views/sample_library_views.xml`
- `library-manifest.toml`: `addons/sample_library/manifest.toml` (1.0.1)
- `wasm-views.xml`: `examples/wasm/sample_library_wasm/views/sample_library_wasm_views.xml`
- `wasm-manifest.toml`: `examples/wasm/sample_library_wasm/manifest.toml` (1.3.4)

The coordinated-upgrade test installs this old base and a package built from the old
view/manifest plus unchanged compiled method code, then upgrades to the current base
and package in one maintenance train. These are owned addon fixtures, not upstream
Community/Enterprise source or a deployed database backup. `scripts/wasm-poc.sh` builds
the previous package for this purpose. No broad duplicate-header admission is introduced.
