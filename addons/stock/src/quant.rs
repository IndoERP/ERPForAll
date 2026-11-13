// SPDX-License-Identifier: MIT OR Apache-2.0
//! Stock balances (`stock.quant`) and serial positions (`stock.serial.position`): tables only.
//! Both are projections of the stock ledger (2), written by the
//! transition units; nothing here writes them.

use erp_sdk::DefaultContext;
use erp_sdk::prelude::*;

fn now(_ctx: &DefaultContext) -> Value {
    Value::DateTime(DateTime::now())
}

/// `stock.quant`: the balance of one key `(company, product, location, lot, package, owner)`.
///
/// One row per key: the key is `UNIQUE NULLS NOT DISTINCT` (2,.1). The
/// reserved counter is never negative and never clamped (1). The company is the company
/// of the ledger line that created the key, stored and required, never derived from the location
/// (B6). Quantities are exact `numeric` in the product unit. The product
/// declares its company check (its company comes through the template delegation); lot, location,
/// owner and assigned user are checked by the framework default.
///
/// The lot belongs to the quant's product: the composite foreign key `(lot_id, product_id)` →
/// `stock_lot (id, product_id)` (`stock_quant_lot_product`) refuses another product's lot for every
/// writer, ORM or SQL; an untracked quant (no lot) is not checked (S1).
#[model(
    name = "stock.quant",
    description = "Quants",
    rec_name = "product_id",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    keys = [key(
        name = "_key",
        fields = ["company_id", "product_id", "location_id", "lot_id", "package_id", "owner_id"],
        message = "A stock balance already exists for this company, product, location, lot, package and owner."
    )],
    constraints = [
        (
            "_reserved_nonneg",
            "CHECK (reserved_quantity >= 0)",
            "The reserved quantity of a stock balance cannot be negative."
        ),
        // named `finite_*` so PostgreSQL, which tests CHECKs in name order, reports a NaN here first
        (
            "_finite_quantity",
            "CHECK (quantity <> 'NaN' AND quantity <> 'Infinity' AND quantity <> '-Infinity')",
            "The quantity of a stock balance must be a finite number."
        ),
        (
            "_finite_reserved_quantity",
            "CHECK (reserved_quantity <> 'NaN' AND reserved_quantity <> 'Infinity' AND reserved_quantity <> '-Infinity')",
            "The reserved quantity of a stock balance must be a finite number."
        ),
        (
            "_finite_inventory_quantity",
            "CHECK (inventory_quantity <> 'NaN' AND inventory_quantity <> 'Infinity' AND inventory_quantity <> '-Infinity')",
            "The counted quantity of a stock balance must be a finite number."
        ),
        // the lot belongs to the row's product (raw until slice B6 declares composite keys);
        // needs `stock_lot_id_product`, created first because `lot.rs` sorts before this file
        (
            "_lot_product",
            "FOREIGN KEY (lot_id, product_id) REFERENCES stock_lot (id, product_id)",
            "The lot or serial number of a stock balance belongs to another product."
        ),
    ],
    // FIFO and LIFO removal order read without a sort or a heap visit (plan I3 W-I3.04); the
    // benefit is unmeasured (DEFERRED_PERFORMANCE)
    indexes = [index(
        name = "_removal_order",
        fields = ["company_id", "product_id", "location_id", "in_date", "id"],
        include = ["quantity", "reserved_quantity", "lot_id", "package_id", "owner_id"]
    )],
)]
pub struct StockQuant {
    /// Company of the ledger line that created the key (B6); leads the key, so it needs no
    /// index of its own.
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        ondelete = "restrict"
    )]
    pub company_id: Option<RecId>,
    /// Product.
    #[field(
        many2one,
        comodel = "product.product",
        required = true,
        ondelete = "restrict",
        index = true,
        check_company = true
    )]
    pub product_id: Option<RecId>,
    /// Exact location of the balance.
    #[field(
        many2one,
        comodel = "stock.location",
        required = true,
        ondelete = "restrict",
        index = true
    )]
    pub location_id: Option<RecId>,
    /// Lot or serial number; empty for untracked stock.
    #[field(
        many2one,
        comodel = "stock.lot",
        ondelete = "restrict",
        index = "btree_not_null",
        string = "Lot/Serial Number"
    )]
    pub lot_id: Option<RecId>,
    /// Package holding the stock; empty for loose stock.
    #[field(
        many2one,
        comodel = "stock.package",
        ondelete = "restrict",
        index = "btree_not_null",
        string = "Package"
    )]
    pub package_id: Option<RecId>,
    /// Owner of consigned stock.
    #[field(
        many2one,
        comodel = "res.partner",
        ondelete = "restrict",
        index = "btree_not_null",
        string = "Owner"
    )]
    pub owner_id: Option<RecId>,
    #[field(
        float,
        digits = "Product Unit",
        required = true,
        readonly = true,
        string = "Quantity"
    )]
    pub quantity: Decimal,
    /// Quantity reserved by open move lines on this key; never negative, never clamped.
    #[field(
        float,
        digits = "Product Unit",
        required = true,
        readonly = true,
        default = 0,
        string = "Reserved Quantity"
    )]
    pub reserved_quantity: Decimal,
    /// Incoming date, the FIFO order of the key.
    #[field(
        datetime,
        required = true,
        readonly = true,
        default_fn = now,
        string = "Incoming Date"
    )]
    pub in_date: Option<DateTime>,
    /// Counted quantity of an inventory count (row state, never written by transitions).
    #[field(float, digits = "Product Unit", string = "Counted")]
    pub inventory_quantity: Decimal,
    /// User assigned to count this balance.
    #[field(many2one, comodel = "res.users", string = "Assigned To")]
    pub user_id: Option<RecId>,
}

/// `stock.serial.position`: the summed quantity of one serial at one exact location across packages
/// and owners (1). `quantity <= 1` is the serial rule, with no lower bound (D5.2).
///
/// The serial belongs to the position's product: the composite foreign key `(lot_id, product_id)` →
/// `stock_lot (id, product_id)` (`stock_serial_position_lot_product`), without which one serial could
/// hold a position under two products at one location and escape `_max_one` (S1).
#[model(
    name = "stock.serial.position",
    description = "Serial Number Position",
    log_access = false,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    keys = [key(
        name = "_key",
        fields = ["company_id", "product_id", "lot_id", "location_id"],
        message = "A serial number has one position per company and location."
    )],
    constraints = [
        (
            "_max_one",
            "CHECK (quantity <= 1)",
            "A serial number cannot be more than once at one location."
        ),
        (
            "_finite_quantity",
            "CHECK (quantity <> 'NaN' AND quantity <> 'Infinity' AND quantity <> '-Infinity')",
            "The quantity of a serial position must be a finite number."
        ),
        // the serial belongs to the row's product (raw until slice B6); needs
        // `stock_lot_id_product`, created first because `lot.rs` sorts before this file
        (
            "_lot_product",
            "FOREIGN KEY (lot_id, product_id) REFERENCES stock_lot (id, product_id)",
            "The serial number of a serial position belongs to another product."
        ),
    ],
)]
pub struct StockSerialPosition {
    /// Company of the quant keys the position sums.
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        ondelete = "restrict"
    )]
    pub company_id: Option<RecId>,
    /// Serial-tracked product.
    #[field(
        many2one,
        comodel = "product.product",
        required = true,
        ondelete = "restrict",
        check_company = true
    )]
    pub product_id: Option<RecId>,
    /// The serial number.
    #[field(
        many2one,
        comodel = "stock.lot",
        required = true,
        ondelete = "restrict",
        index = true
    )]
    pub lot_id: Option<RecId>,
    /// Exact location.
    #[field(
        many2one,
        comodel = "stock.location",
        required = true,
        ondelete = "restrict",
        index = true
    )]
    pub location_id: Option<RecId>,
    /// Summed quantity of the serial at the location.
    #[field(float, digits = "Product Unit", required = true)]
    pub quantity: Decimal,
}
