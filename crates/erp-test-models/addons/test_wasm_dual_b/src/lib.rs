//! Fixture addon `test_wasm_dual_b`: extends `test.wasm.dual` of `test_wasm_dual_a`, overrides its
//! method and its compute, and is the crate compiled to the component. Identical source in both
//! modes.

use erp_addon_test_wasm_dual_a::{Dual, DualMethods};
use erp_sdk::prelude::*;
// Unconditional fixture infrastructure; portable model/ordinary-method source stays cfg-free.
mod adversarial_abi1;

erp_sdk::addon!(name = "test_wasm_dual_b", depends = ["test_wasm_dual_a"]);

// the leaf crate exports the component; natively this expands to nothing
erp_sdk::export_component!();

/// The text this layer adds, so a test can read the order of the layers in a result.
pub const MARK: &str = "b";

/// B's extension of `test.wasm.dual`.
#[extend(Dual)]
pub struct DualNote {
    /// A free note.
    #[field(char, string = "Note")]
    pub note: Option<Arc<str>>,
    /// Plain calendar date used by temporal bridge tests.
    #[field(date)]
    pub loan_date: Option<Date>,
    /// Naive UTC datetime used by temporal bridge tests.
    #[field(datetime)]
    pub returned_at: Option<DateTime>,
}

#[methods(Dual)]
impl DualNote {
    /// Read the old temporal values, then write new ones through typed field handles.
    #[method(wire)]
    async fn temporal_swap(
        this: &RecordSet<Dual>,
        tx: &mut Transaction,
        day: Option<i32>,
        micros: Option<i64>,
    ) -> Result<(Option<i32>, Option<i64>)> {
        let date = this.get(tx, Self::LOAN_DATE).await?;
        let datetime = this.get(tx, Self::RETURNED_AT).await?;
        this.set(tx, Self::LOAN_DATE, day.map(Date::from_days))
            .await?;
        this.set(tx, Self::RETURNED_AT, micros.map(DateTime::from_micros))
            .await?;
        Ok((date.map(Date::days), datetime.map(DateTime::micros)))
    }

    /// Today's date in the zone of the call, as a day count since 1970-01-01.
    #[method(wire)]
    async fn today(this: &RecordSet<Dual>, tx: &mut Transaction) -> Result<i32> {
        Ok(this.context_today(tx)?.days())
    }

    /// Read temporal columns in record order, including empty recordsets.
    #[method(wire)]
    async fn temporal_mapped(
        this: &RecordSet<Dual>,
        tx: &mut Transaction,
    ) -> Result<(Vec<Option<i32>>, Vec<Option<i64>>)> {
        let dates = this.mapped(tx, Self::LOAN_DATE).await?;
        let datetimes = this.mapped(tx, Self::RETURNED_AT).await?;
        Ok((
            dates.into_iter().map(|v| v.map(Date::days)).collect(),
            datetimes
                .into_iter()
                .map(|v| v.map(DateTime::micros))
                .collect(),
        ))
    }

    /// Appends B's mark to the label.
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

    /// Defined here, in the layer that runs as WebAssembly: upper-cases the text.
    #[method(wire)]
    async fn shout(this: &RecordSet<Dual>, tx: &mut Transaction, text: String) -> Result<String> {
        let _ = (this, tx);
        Ok(text.to_uppercase())
    }

    /// Calls `shout` on the same records through the whole override chain, which can run this
    /// addon again while this call is still waiting for the answer (re-entry).
    #[method(wire)]
    async fn nested(this: &RecordSet<Dual>, tx: &mut Transaction, text: String) -> Result<String> {
        let inner = this.shout(tx, text).await?;
        Ok(format!("[{inner}]"))
    }

    /// Never returns: the CPU limit has to stop it.
    #[method(wire)]
    async fn spin(this: &RecordSet<Dual>, tx: &mut Transaction) -> Result<i32> {
        let _ = (this, tx);
        let mut n: u64 = 0;
        loop {
            n = std::hint::black_box(n).wrapping_add(1);
        }
    }

    /// Reads the quantity of the record and appends it to the prefix, then writes it back
    /// doubled: the guest's `get` and `set` through the host's access-checked ORM.
    #[method(wire)]
    async fn double_qty_now(this: &RecordSet<Dual>, tx: &mut Transaction) -> Result<i32> {
        let one = this.ensure_one()?;
        let qty = one.get(tx, Dual::QTY).await?;
        one.set(tx, Dual::QTY, qty * 2).await?;
        Ok(qty)
    }

    /// Tries to reach a method that is not exposed over RPC, by name, through `call-method`.
    #[method(wire)]
    async fn call_hidden(this: &RecordSet<Dual>, tx: &mut Transaction) -> Result<String> {
        this.hidden_probe(tx).await
    }

    /// Writes the quantity, then calls `shout` on the same records: a result too large for the
    /// payload limit then fails the call after the write (the taint path).
    #[method(wire)]
    async fn write_then_shout(
        this: &RecordSet<Dual>,
        tx: &mut Transaction,
        text: String,
    ) -> Result<String> {
        this.set(tx, Dual::QTY, 99).await?;
        this.shout(tx, text).await
    }

    /// `double_qty` as the base computes it.
    #[compute(depends = ["qty", "note"], override)]
    fn _compute_double(batch: &mut Batch<'_, Dual>, sup: ComputeSuper) -> Result<()> {
        let _ = (batch, sup);
        Ok(())
    }

    /// Reacts to a change of `qty`.
    #[onchange("qty")]
    async fn _onchange_qty(this: &RecordSet<Dual>, tx: &mut Transaction) -> Result<OnchangeResult> {
        let _ = (this, tx);
        Ok(OnchangeResult::none())
    }
}

/// Separate lifecycle fixture so JSON precision does not obscure the WIT scalar tests.
#[model(name = "test.wasm.temporal", description = "Temporal JSON fixture")]
pub struct TemporalJson {
    /// Plain date.
    #[field(date)]
    pub loan_date: Option<Date>,
    /// UTC datetime.
    #[field(datetime)]
    pub returned_at: Option<DateTime>,
}
