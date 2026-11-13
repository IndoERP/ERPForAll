// SPDX-License-Identifier: MIT OR Apache-2.0
//! Putaway rules: the table only. Rule selection, capacity checks and the
//! company-change refusal belong to. Observed in.

use erp_sdk::prelude::*;

/// `stock.putaway.rule`: where goods arriving at a location are stored.
#[model(
    name = "stock.putaway.rule",
    description = "Putaway Rule",
    order = "sequence, product_id",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct StockPutawayRule {
    /// Product the rule applies to; empty with an empty category means every product.
    #[field(
        many2one,
        comodel = "product.product",
        ondelete = "cascade",
        index = "btree_not_null",
        check_company = true,
        string = "Product"
    )]
    pub product_id: Option<RecId>,
    /// Product category the rule applies to.
    #[field(
        many2one,
        comodel = "product.category",
        ondelete = "cascade",
        index = "btree_not_null",
        string = "Product Category"
    )]
    pub category_id: Option<RecId>,
    /// Location the goods arrive in.
    #[field(
        many2one,
        comodel = "stock.location",
        required = true,
        ondelete = "cascade",
        index = true,
        check_company = true,
        string = "When product arrives in"
    )]
    pub location_in_id: Option<RecId>,
    /// Location the goods are stored in.
    #[field(
        many2one,
        comodel = "stock.location",
        required = true,
        ondelete = "cascade",
        check_company = true,
        string = "Store to sublocation"
    )]
    pub location_out_id: Option<RecId>,
    /// Priority: lower first.
    #[field(integer, default = 0, string = "Priority")]
    pub sequence: i32,
    /// Owning company.
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        index = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Package types the rule applies to; empty means any.
    #[field(
        many2many,
        comodel = "stock.package.type",
        relation = "stock_package_type_stock_putaway_rule_rel",
        column1 = "stock_putaway_rule_id",
        column2 = "stock_package_type_id",
        check_company = true,
        string = "Package Type"
    )]
    pub package_type_ids: Vec<RecId>,
    #[field(
        many2one,
        comodel = "stock.storage.category",
        ondelete = "cascade",
        check_company = true,
        string = "Storage Category"
    )]
    pub storage_category_id: Option<RecId>,
    /// Archived rules are ignored.
    #[field(boolean, default = true)]
    pub active: bool,
    /// How a sublocation is chosen below the target.
    #[field(
        selection,
        selection = [
            ("no", "No"),
            ("last_used", "Last Used"),
            ("closest_location", "Closest Location")
        ],
        default = "no"
    )]
    pub sublocation: Option<Arc<str>>,
}
