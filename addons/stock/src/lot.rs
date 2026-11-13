// SPDX-License-Identifier: MIT OR Apache-2.0
//! Lots and serial numbers (`stock.lot`) and their name scope rows (`stock.lot.name.scope`):
//! tables only. Name generation, the company default and the statement that
//! keeps the scope rows are's.

use erp_sdk::prelude::*;

/// `stock.lot`: a lot or serial number of one product.
///
/// `(product, company, name)` is unique with `NULLS NOT DISTINCT`, so two lots without a company
/// clash too (4); a lot without a company clashing with any company's lot is the scope
/// row's rule ([`StockLotNameScope`]).
///
/// `(id, product_id)` is unique too (`stock_lot_id_product`): it is the target of the composite
/// foreign keys `(lot_id, product_id)` of [`crate::quant::StockQuant`] and
/// [`crate::quant::StockSerialPosition`], so a quant or serial position can only name a lot of its own
/// product, and a lot's product cannot change while one names it (S1). A raw
/// table constraint until declared composite foreign keys exist (slice B6).
#[model(
    name = "stock.lot",
    description = "Lot/Serial",
    order = "name, id",
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "exact", empty = "allow"),
    keys = [key(
        name = "_name_unique",
        fields = ["product_id", "company_id", "name"],
        message = "The combination of lot/serial number and product must be unique within a company including when no company is defined."
    )],
    // target of the quant and serial position keys, created first because `lot.rs` sorts before
    // `quant.rs` in registry order; changing it needs a migration that drops those keys first
    constraints = [(
        "_id_product",
        "UNIQUE (id, product_id)",
        "A lot is unique with its product."
    )],
)]
pub struct StockLot {
    /// Lot or serial number.
    #[field(char, required = true, index = "trigram", string = "Lot/Serial Number")]
    pub name: Option<Arc<str>>,
    /// Internal reference, when it differs from the manufacturer's number.
    #[field(char, string = "Internal Reference")]
    pub r#ref: Option<Arc<str>>,
    /// Product the lot belongs to.
    #[field(
        many2one,
        comodel = "product.product",
        required = true,
        ondelete = "restrict",
        index = true,
        check_company = true
    )]
    pub product_id: Option<RecId>,
    /// Company of the lot; empty for a lot shared by every company.
    #[field(many2one, comodel = "res.company", index = true)]
    pub company_id: Option<RecId>,
    /// Description.
    #[field(html, string = "Description")]
    pub note: Option<Arc<str>>,
}

/// `stock.lot.name.scope`: per product and lot name, how many lots without a company
/// (`global_count`) and with one (`company_count`) carry it (4). The check allows either
/// no shared lot, or one shared lot and no company lot. Rows are kept at zero counts and are written
/// only by the ordered statement of the lot write (amendment B4: no trigger).
#[model(
    name = "stock.lot.name.scope",
    description = "Lot Name Scope",
    log_access = false,
    keys = [key(
        name = "_key",
        fields = ["product_id", "name"],
        message = "A lot name has one scope row per product."
    )],
    constraints = [
        (
            "_rule",
            "CHECK (global_count = 0 OR (global_count = 1 AND company_count = 0))",
            "A lot without a company cannot share its product and name with another lot."
        ),
        (
            "_counts_nonneg",
            "CHECK (global_count >= 0 AND company_count >= 0)",
            "Lot name counts cannot be negative."
        ),
    ],
)]
pub struct StockLotNameScope {
    /// Product. A product with lots cannot be deleted, so only zero-count rows follow it.
    #[field(
        many2one,
        comodel = "product.product",
        required = true,
        ondelete = "cascade"
    )]
    pub product_id: Option<RecId>,
    /// Lot name.
    #[field(char, required = true)]
    pub name: Option<Arc<str>>,
    /// Lots without a company carrying this name.
    #[field(integer, required = true, default = 0)]
    pub global_count: i32,
    /// Lots with a company carrying this name.
    #[field(integer, required = true, default = 0)]
    pub company_count: i32,
}
