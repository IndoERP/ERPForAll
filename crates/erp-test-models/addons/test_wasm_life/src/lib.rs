//! Fixture addon `test_wasm_life`: defines its own model and one method, and is compiled to a
//! component that `erp-modules` installs into a running database from a `.erpaddon` package.
//! Only the field kinds and attributes the runtime install
//! reads from `describe()` are used.

use erp_sdk::prelude::*;

erp_sdk::addon!(name = "test_wasm_life", depends = ["base"]);

// the leaf crate exports the component; natively this expands to nothing
erp_sdk::export_component!();

/// The text this addon adds, so a test can read it in a result.
pub const MARK: &str = "life";

/// `test.wasm.life`.
#[model(name = "test.wasm.life", description = "WASM lifecycle fixture")]
pub struct Life {
    /// Title.
    #[field(char, required = true)]
    pub name: Option<Arc<str>>,
    /// Quantity.
    #[field(integer)]
    pub qty: i32,
    /// Nonstored ordinary compute used to test package-only input/output admission.
    #[field(integer, compute = "_compute_copy", compute_sudo = false)]
    pub copied_qty: i32,
}

#[methods(Life)]
impl Life {
    /// Copies the declared ordinary input; there is no native stub in host inventory.
    #[compute(depends = ["qty"])]
    fn _compute_copy(batch: &mut Batch<'_, Life>) -> Result<()> {
        for i in batch.indices() {
            let qty = batch.get(i, Self::QTY)?;
            batch.set(i, Self::COPIED_QTY, qty)?;
        }
        Ok(())
    }

    /// Upper-cases the text and appends the mark.
    #[method(wire)]
    async fn shout(this: &RecordSet<Life>, tx: &mut Transaction, text: String) -> Result<String> {
        let _ = (this, tx);
        Ok(format!("{}{MARK}", text.to_uppercase()))
    }
}
