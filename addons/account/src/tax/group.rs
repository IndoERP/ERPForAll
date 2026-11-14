// SPDX-License-Identifier: MIT OR Apache-2.0
//! Tax groups are company-owned labels for tax totals.
use erp_sdk::ValueMap;
use erp_sdk::prelude::*;

/// A tax total grouping, extensible by localization addons.
#[model(name = "account.tax.group", description = "Tax group", order = "sequence, id", company_access = direct(field = "company_id", branch = "ancestor", empty = "deny"), company_match = "parent_of")]
pub struct AccountTaxGroup {
    /// Translated group label.
    #[field(char, required = true, translate = true)]
    pub name: Option<Arc<str>>,
    /// Display order, editable on the group.
    #[field(integer, default = 10)]
    pub sequence: i32,
    /// Owning company.
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        ondelete = "restrict"
    )]
    pub company_id: Option<RecId>,
    /// Empty means the group is usable for any country of its company.
    #[field(many2one, comodel = "res.country")]
    pub country_id: Option<RecId>,
    /// Jurisdiction code for display.
    #[field(char, related = "country_id.code")]
    pub country_code: Option<Arc<str>>,
    /// Subtotal label shown before this group's taxes.
    #[field(char, translate = true)]
    pub preceding_subtotal: Option<Arc<str>>,
    /// Receipt label owned by this group.
    #[field(char)]
    pub pos_receipt_label: Option<Arc<str>>,
}
#[methods(AccountTaxGroup, addon = "account")]
impl AccountTaxGroup {
    #[method(override)]
    async fn default_get(
        this: &RecordSet<AccountTaxGroup>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<ValueMap> {
        let requested = super::defaults::requested(&names, false);
        let values = sup.call(tx, this.as_records(), (requested,)).await?;
        super::defaults::complete(this.as_records(), tx, &names, values, false).await
    }
    #[method(override)]
    async fn create(
        this: &RecordSet<AccountTaxGroup>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        super::admit(
            this.as_records(),
            tx,
            &vals_list,
            erp_sdk::Operation::Create,
        )
        .await?;
        for vals in &mut vals_list {
            super::defaults::for_create(this.as_records(), tx, vals, false).await?;
            super::company_defaults(this.as_records(), tx, vals, false).await?;
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }
}
