//! Fixture addon `test_wasm_dual_c`: always native. It overrides the methods of `test_wasm_dual_a`
//! and `test_wasm_dual_b` from above, so a chain runs native (C) -> WebAssembly (B) -> native (A).

use erp_addon_test_wasm_dual_a::Dual;
use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_wasm_dual_c", depends = ["test_wasm_dual_b"]);

/// The text this layer adds, so a test can read the order of the layers in a result.
pub const MARK: &str = "c";

/// C's extension of `test.wasm.dual`.
#[extend(Dual)]
pub struct DualNative {
    /// A free flag.
    #[field(boolean)]
    pub flagged: bool,
}

#[methods(Dual)]
impl DualNative {
    /// Appends C's mark after what the layers below return.
    #[method(override, wire)]
    async fn label(
        this: &RecordSet<Dual>,
        tx: &mut Transaction,
        prefix: String,
        sup: Super,
    ) -> Result<String> {
        let inner = sup.call(tx, this, (prefix,)).await?;
        Ok(format!("{inner}{MARK}"))
    }

    /// Appends C's mark after the result of the WebAssembly definition below.
    #[method(override, wire)]
    async fn shout(
        this: &RecordSet<Dual>,
        tx: &mut Transaction,
        text: String,
        sup: Super,
    ) -> Result<String> {
        let inner = sup.call(tx, this, (text,)).await?;
        Ok(format!("{inner}{MARK}"))
    }
}

/// The addon below, named so this crate links it (its registrations must be in the program).
pub fn below() -> &'static str {
    erp_addon_test_wasm_dual_b::ADDON.name
}
