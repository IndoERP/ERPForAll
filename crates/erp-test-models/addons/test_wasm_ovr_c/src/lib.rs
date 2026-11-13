//! Fixture addon `test_wasm_ovr_c`: the native top layer of the chain C -> WebAssembly (W) -> A.

use erp_addon_test_wasm_ovr_a::{Ovr, note};
use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_wasm_ovr_c", depends = ["test_wasm_ovr_w"]);

/// C's extension of `test.wasm.ovr`.
#[extend(Ovr)]
pub struct OvrC {
    /// A free flag.
    #[field(boolean)]
    pub flagged: bool,
}

#[methods(Ovr)]
impl OvrC {
    /// Records that the native top layer ran, then the layers below.
    #[method(override)]
    async fn create(
        this: &RecordSet<Ovr>,
        tx: &mut Transaction,
        vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        note("c:create");
        sup.call(tx, this.as_records(), (vals_list,)).await
    }

    /// Records that the native top layer ran, then the layers below.
    #[method(override)]
    async fn write(
        this: &RecordSet<Ovr>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        note("c:write");
        sup.call(tx, this.as_records(), (vals,)).await
    }

    /// Records that the native top layer ran, then the layers below.
    #[method(override)]
    async fn unlink(this: &RecordSet<Ovr>, tx: &mut Transaction, sup: Super) -> Result<()> {
        note("c:unlink");
        sup.call(tx, this.as_records(), ()).await
    }
}
