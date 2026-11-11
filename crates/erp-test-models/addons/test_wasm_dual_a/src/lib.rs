//! Fixture addon `test_wasm_dual_a`: defines `test.wasm.dual`. The source is the same for the
//! native build and the `wasm32-wasip2` component; no `cfg` appears in it.
//! `test_wasm_dual_b` extends the model without this crate knowing it.

use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_wasm_dual_a", depends = ["base"]);

/// The text this layer adds, so a test can read the order of the layers in a result.
pub const MARK: &str = "a";

/// `test.wasm.dual`.
#[model(name = "test.wasm.dual", description = "WASM dual-mode fixture")]
pub struct Dual {
    /// Title.
    #[field(char, required = true)]
    pub name: Option<Arc<str>>,
    /// Quantity.
    #[field(integer, default = 1)]
    pub qty: i32,
    /// Twice the quantity.
    #[field(integer, compute = "_compute_double", store = true)]
    pub double_qty: i32,
}

#[methods(Dual)]
impl Dual {
    /// A label for the record.
    #[method(wire)]
    async fn label(this: &RecordSet<Dual>, tx: &mut Transaction, prefix: String) -> Result<String> {
        let _ = (this, tx);
        Ok(format!("{prefix}{MARK}"))
    }

    /// Never exposed over RPC (no `#[rpc]`): only native code may call it. The security tests
    /// check that a guest cannot reach it by name.
    #[method(wire)]
    async fn hidden_probe(this: &RecordSet<Dual>, tx: &mut Transaction) -> Result<String> {
        let _ = (this, tx);
        Ok("hidden".to_owned())
    }

    /// `double_qty` from `qty`.
    #[compute(depends = ["qty"])]
    fn _compute_double(batch: &mut Batch<'_, Dual>) -> Result<()> {
        let _ = batch;
        Ok(())
    }

    /// The quantity cannot be negative.
    #[constrains("qty")]
    async fn _check_qty(this: &RecordSet<Dual>, tx: &mut Transaction) -> Result<()> {
        let _ = (this, tx);
        Ok(())
    }
}
