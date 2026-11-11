//! Fixture addon `test_wasm_ovr_w`: names the addon a test runs as a WebAssembly layer. It extends
//! `test.wasm.ovr` with two computed fields whose computes are not Rust code here: a test gives
//! the ORM the computes and the layer's methods (`wasm_compute_impl`, `WasmMethodImpl`), as the
//! package lifecycle does for a package. Registries that do not provide them must not include this
//! addon.

use erp_addon_test_wasm_ovr_a::Ovr;
use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_wasm_ovr_w", depends = ["test_wasm_ovr_a"]);

/// The extension of `test.wasm.ovr` by the WebAssembly layer.
#[extend(Ovr)]
pub struct OvrW {
    /// Twice the quantity (computed, stored).
    #[field(
        integer,
        string = "Doubled",
        compute = "compute_doubled",
        store = true,
        compute_sudo = false
    )]
    pub doubled: i32,
    /// A text from the name and the quantity (computed, not stored).
    #[field(char, string = "Label", compute = "compute_label")]
    pub label: Option<Arc<str>>,
}
