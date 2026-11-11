//! Isolated named-precision mutation fixtures; no business/default data models.
use erp_sdk::prelude::*;
erp_sdk::addon!(name = "test_exact_numeric", exact_only = true);

/// Stored and precomputed precision outputs plus a numeric upsert arbiter.
#[model(name = "exact_numeric.item", addon = "test_exact_numeric", keys = [key(name = "_code_key", fields = ["code"], message = "Code already exists.")])]
pub struct NumericItem {
    #[field(char, required)]
    pub code: Option<Arc<str>>,
    #[field(float, digits = "Product Price")]
    pub amount: Decimal,
    #[field(float, digits = 0)]
    pub input: Decimal,
    #[field(float, digits = 0)]
    pub late_input: Decimal,
    #[field(float, digits = "Product Price", compute = "_early", store, precompute)]
    pub early: Decimal,
    #[field(float, digits = "Product Price", compute = "_late", store)]
    pub late: Decimal,
    #[field(many2one, comodel = "exact_numeric.item")]
    pub parent_id: Option<RecId>,
    #[field(one2many, comodel = "exact_numeric.item", inverse_name = "parent_id")]
    pub children: Vec<RecId>,
    #[field(
        float,
        digits = "Product Price",
        related = "parent_id.amount",
        readonly = false
    )]
    pub related_amount: Decimal,
}

#[methods(NumericItem, addon = "test_exact_numeric")]
impl NumericItem {
    #[compute(depends = ["input"])]
    fn _early(batch: &mut Batch<'_, NumericItem>) -> Result<()> {
        for i in batch.indices() {
            let input = batch.get(i, NumericItem::INPUT)?;
            batch.set(i, NumericItem::EARLY, input)?;
        }
        Ok(())
    }
    #[compute(depends = ["late_input"])]
    fn _late(batch: &mut Batch<'_, NumericItem>) -> Result<()> {
        for i in batch.indices() {
            let input = batch.get(i, NumericItem::LATE_INPUT)?;
            batch.set(i, NumericItem::LATE, input)?;
        }
        Ok(())
    }
}
