// SPDX-License-Identifier: MIT OR Apache-2.0
//! A line created with a product and without a price takes it from the product's vendor price
//! for the order's vendor (the seller service of `product`, never a second selection), else from
//! the product cost, converted to the line's unit and the order's currency. Subtotal, tax and
//! total come from the tax computation of `account` (`erp_addon_account::tax::compute`) and are
//! written after every create, write and delete together with the order's totals. The amounts,
//! the selected vendor price and the received quantity are never written by a client.

use std::collections::{BTreeMap, BTreeSet};

use erp_addon_account::tax::compute::{self, AmountType, BaseLine, TaxDef, TaxKind};
use erp_addon_base::money::{CURRENCY_ROUNDING_MODE, RoundingTable, convert};
use erp_addon_product::{SellerInfo, SellerQuery};
use erp_addon_uom::conversion::{price_exact, quantity_rounded};
use erp_addon_uom::models::load_unit;
use erp_sdk::Command;
use erp_sdk::money::{Increment, RoundingMode};
use erp_sdk::prelude::*;

use crate::order::refuse_context_defaults;

/// Fields only this module writes: the amounts, the quantity in the product unit, the vendor
/// price the line was priced from, and the billed quantities with their private bill links.
pub(crate) const COMPUTED: [&str; 8] = [
    "price_subtotal",
    "price_tax",
    "price_total",
    "product_uom_qty",
    "selected_seller_id",
    "direct_bill_line_ids",
    "qty_invoiced",
    "qty_to_invoice",
];

/// The received quantity and its method, filled by the receipt slices.
pub(crate) const RECEIVED: [&str; 2] = ["qty_received", "qty_received_method"];

/// Related fields of the order that only its buttons set; a write through the line would reach the order.
const ORDER_OPERATION: [&str; 2] = ["state", "date_approve"];

/// The stored copies of the order's company and vendor: the company rule of the line reads the
/// first, so neither is ever written through a line; the order's fields are the only source.
const ORDER_COPIES: [&str; 2] = ["company_id", "partner_id"];

/// A change of these fields prices a line again unless its price was set by hand
/// (the `technical_price_unit` guard at `:547`).
const REPRICE: [&str; 3] = ["product_qty", "uom_id", "product_id"];

const MICROS_PER_DAY: i64 = 86_400_000_000;

/// `purchase.order.line`: product, description, quantity, unit, planned date, price, discount,
/// taxes and the computed amounts.
#[model(
    name = "purchase.order.line",
    description = "Purchase Order Line",
    order = "order_id, sequence, id",
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny", parent = "order_id"),
)]
pub struct PurchaseOrderLine {
    /// The order the line belongs to; deleting the order deletes its lines.
    #[field(
        many2one,
        comodel = "purchase.order",
        required = true,
        ondelete = "cascade",
        index = true,
        string = "Order Reference"
    )]
    pub order_id: Option<RecId>,
    /// The position of the line on its order.
    #[field(integer, default = 10, string = "Sequence")]
    pub sequence: i32,
    /// The product bought; required to confirm the order.
    #[field(
        many2one,
        comodel = "product.product",
        ondelete = "restrict",
        index = "btree_not_null",
        string = "Product"
    )]
    pub product_id: Option<RecId>,
    /// The text the vendor reads; the product's purchase description when none is given.
    #[field(text, string = "Description")]
    pub name: Option<Arc<str>>,
    /// The quantity ordered, in the line's unit.
    #[field(
        float,
        digits = "Product Unit",
        required = true,
        default = 1,
        string = "Quantity"
    )]
    pub product_qty: Decimal,
    /// The quantity in the product's own unit.
    #[field(
        float,
        digits = "Product Unit",
        readonly = true,
        copy = false,
        string = "Total Quantity"
    )]
    pub product_uom_qty: Decimal,
    /// The unit of the quantity and the price; the product's unit when none is given.
    #[field(many2one, comodel = "uom.uom", ondelete = "restrict", string = "Unit")]
    pub uom_id: Option<RecId>,
    /// The date the goods are expected: the order deadline plus the vendor's lead time.
    #[field(datetime, index = true, copy = false, string = "Expected Arrival")]
    pub date_planned: Option<DateTime>,
    /// The price of one unit, in the order currency, before the discount.
    #[field(
        float,
        digits = "Product Price",
        required = true,
        default = 0,
        string = "Unit Price"
    )]
    pub price_unit: Decimal,
    /// The price the line was last priced at; differs from the unit price once a person set it.
    /// The default 0 lets the line form send it with the untouched default price (see `fill_line`).
    #[field(float, digits = "Product Price", copy = false, default = 0)]
    pub technical_price_unit: Decimal,
    /// Discount in percent.
    #[field(float, digits = "Discount", default = 0, string = "Discount (%)")]
    pub discount: Decimal,
    /// The taxes of the line; the product's vendor taxes of the order company when none are given.
    #[field(
        many2many,
        comodel = "account.tax",
        relation = "account_tax_purchase_order_line_rel",
        column1 = "purchase_order_line_id",
        column2 = "account_tax_id",
        check_company = true,
        string = "Taxes"
    )]
    pub tax_ids: Vec<RecId>,
    /// Cost Center / Analytic Account charged by this purchase line (checked against Budget AVC).
    #[field(
        many2one,
        comodel = "account.analytic.account",
        check_company = true,
        ondelete = "set null",
        index = "btree_not_null",
        string = "Cost Center"
    )]
    pub analytic_account_id: Option<RecId>,
    /// Optional Construction in Progress (CIP) asset accumulating costs from this purchase line.
    #[field(
        many2one,
        comodel = "account.asset",
        check_company = true,
        ondelete = "set null",
        index = "btree_not_null",
        string = "CIP Asset"
    )]
    pub asset_id: Option<RecId>,
    /// The vendor price row the line was priced from, if any.
    #[field(
        many2one,
        comodel = "product.supplierinfo",
        readonly = true,
        copy = false,
        ondelete = "set null",
        string = "Selected Vendor Price"
    )]
    pub selected_seller_id: Option<RecId>,
    /// Amount without taxes, in the order currency.
    #[field(
        monetary,
        currency_field = "currency_id",
        readonly = true,
        copy = false,
        string = "Subtotal"
    )]
    pub price_subtotal: Decimal,
    /// The taxes of the line, in the order currency.
    #[field(
        monetary,
        currency_field = "currency_id",
        readonly = true,
        copy = false,
        string = "Tax"
    )]
    pub price_tax: Decimal,
    /// Amount with taxes, in the order currency.
    #[field(
        monetary,
        currency_field = "currency_id",
        readonly = true,
        copy = false,
        string = "Total"
    )]
    pub price_total: Decimal,
    /// The order's company.
    #[field(
        many2one,
        comodel = "res.company",
        related = "order_id.company_id",
        store = true,
        copy = false,
        index = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// The order's state.
    #[field(selection, related = "order_id.state", string = "Status")]
    pub state: Option<Arc<str>>,
    /// The order's vendor.
    #[field(
        many2one,
        comodel = "res.partner",
        related = "order_id.partner_id",
        store = true,
        copy = false,
        index = "btree_not_null",
        string = "Partner"
    )]
    pub partner_id: Option<RecId>,
    /// The order's currency.
    #[field(
        many2one,
        comodel = "res.currency",
        related = "order_id.currency_id",
        string = "Currency"
    )]
    pub currency_id: Option<RecId>,
    /// The order's deadline.
    #[field(datetime, related = "order_id.date_order", string = "Order Deadline")]
    pub date_order: Option<DateTime>,
    /// The order's approval date.
    #[field(
        datetime,
        related = "order_id.date_approve",
        string = "Confirmation Date"
    )]
    pub date_approve: Option<DateTime>,
    /// How the received quantity is known; another addon adds its value with `selection_add`.
    #[field(
        selection,
        selection = [("manual", "Manual")],
        default = "manual",
        readonly = true,
        copy = false,
        string = "Received Qty Method"
    )]
    pub qty_received_method: Option<Arc<str>>,
    /// The quantity received, in the line's unit; zero until the receipt slices fill it.
    #[field(
        float,
        digits = "Product Unit",
        readonly = true,
        copy = false,
        default = 0,
        string = "Received Qty"
    )]
    pub qty_received: Decimal,
    /// The private links to the vendor bill items made from this line (Accounting reads them, a
    /// buyer only sees the quantities below).
    #[field(
        one2many,
        comodel = "purchase.direct.bill.line",
        inverse_name = "purchase_line_id",
        readonly = true,
        copy = false,
        // no `groups`: the order's bill count, which a buyer reads, depends on this field
        string = "Vendor Bill Items"
    )]
    pub direct_bill_line_ids: Vec<RecId>,
    /// The quantity on linked draft and posted vendor bills less linked vendor credit notes, in
    /// the line's unit ([`crate::billing`]).
    #[field(
        float,
        digits = "Product Unit",
        compute = "_compute_qty_invoiced",
        store = true,
        string = "Billed Qty"
    )]
    pub qty_invoiced: Decimal,
    /// What is left to bill on a confirmed order under the product's bill control: the ordered or
    /// received quantity less the billed one ([`crate::billing`]).
    #[field(
        float,
        digits = "Product Unit",
        compute = "_compute_qty_to_invoice",
        store = true,
        string = "Quantity to Bill"
    )]
    pub qty_to_invoice: Decimal,
    /// Non-blocking notice from the line product. Computed elevated, but empty when the caller's own
    /// rights cannot read the product.
    #[field(
        text,
        compute = "_compute_purchase_line_warn_msg",
        compute_sudo = true,
        readonly = true,
        copy = false,
        string = "Product notice"
    )]
    pub purchase_line_warn_msg: Option<Arc<str>>,
}

#[methods(PurchaseOrderLine, addon = "purchase")]
impl PurchaseOrderLine {
    /// Refuses the computed and received fields, fills unit, description, taxes, price, discount
    /// and planned date from the product and the order's vendor, then writes the amounts and the
    /// order totals.
    #[method(override)]
    async fn create(
        this: &RecordSet<PurchaseOrderLine>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let env = this.env().clone();
        refuse_context_defaults(&env, &COMPUTED, "purchase order line")?;
        refuse_context_defaults(&env, &RECEIVED, "purchase order line")?;
        refuse_context_defaults(&env, &ORDER_OPERATION, "purchase order line")?;
        refuse_context_defaults(&env, &ORDER_COPIES, "purchase order line")?;
        let mut order_ids = BTreeSet::new();
        for vals in &vals_list {
            refuse_line_fields(vals, false)?;
            order_ids.insert(id_of(vals.get("order_id"), "order_id")?.ok_or_else(|| {
                Error::validation("A purchase order line needs its order (order_id).")
            })?);
        }
        let heads = order_heads(tx, &env, &order_ids).await?;
        refuse_closed(&heads, &order_ids, "add lines to", false)?;
        let before = approved_totals(tx, &env, &heads).await?;
        for vals in &mut vals_list {
            let order = id_of(vals.get("order_id"), "order_id")?.unwrap_or_default();
            fill_line(tx, &env, &heads[&order], vals).await?;
        }
        let lines = sup.call(tx, this.as_records(), (vals_list,)).await?;
        sync_lines(tx, &env, &lines).await?;
        refuse_unapproved_raise(tx, &env, &heads, &before).await?;
        let confirmed_orders: Vec<i64> = heads
            .iter()
            .filter(|(_, h)| h.state == "purchase")
            .map(|(id, _)| *id)
            .collect();
        if !confirmed_orders.is_empty() {
            crate::order::sync_order_commitments(tx, &env, &confirmed_orders).await?;
        }
        Ok(lines)
    }

    #[constrains("asset_id")]
    async fn _check_asset_is_active_cip(
        this: &RecordSet<PurchaseOrderLine>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let asset_ids: Vec<i64> = this
            .mapped(tx, PurchaseOrderLine::ASSET_ID)
            .await?
            .into_iter()
            .flatten()
            .map(|id| id.get())
            .collect();
        if asset_ids.is_empty() {
            return Ok(());
        }
        let bad = tx
            .query(
                "SELECT name::text, COALESCE(asset_type, 'normal')::text, COALESCE(state, 'draft')::text \
                 FROM account_asset \
                 WHERE id = ANY($1::int8[]) \
                   AND (COALESCE(asset_type, 'normal') <> 'cip' \
                        OR COALESCE(state, 'draft') NOT IN ('draft', 'cip')) \
                 LIMIT 1",
                &[&asset_ids],
            )
            .await?;
        if let Some(row) = bad.first() {
            let name: String = row.try_get(0).map_err(db_error)?;
            let atype: String = row.try_get(1).map_err(db_error)?;
            let state: String = row.try_get(2).map_err(db_error)?;
            return Err(Error::validation(format!(
                "Asset '{name}' (type '{atype}', status '{state}') cannot be linked on a purchase order line: select an active Construction in Progress (CIP) asset."
            )));
        }
        Ok(())
    }

    /// Refuses the computed and received fields and a move to another order, and edits of lines of
    /// cancelled or locked orders; prices again the lines whose quantity or unit changed and whose
    /// price was not set by hand; then the amounts and the order totals.
    #[method(override)]
    async fn write(
        this: &RecordSet<PurchaseOrderLine>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        refuse_line_fields(&vals, true)?;
        if vals.contains("order_id") {
            return Err(Error::validation(
                "A purchase order line cannot move to another order; delete it and add a new one.",
            ));
        }
        let lines = this.as_records().clone();
        if lines.is_empty() {
            return sup.call(tx, &lines, (vals,)).await;
        }
        refuse_consequences(tx, &lines, &vals).await?;
        if !vals.contains("product_id") && !vals.contains("uom_id") {
            return write_lines(tx, &lines, vals, sup).await;
        }
        tx.savepoint(async move |tx| {
            write_lines(tx, &lines, vals, sup).await?;
            crate::direct_bill_integrity::check_purchase_items(tx, &lines).await
        })
        .await
    }

    /// Lines of confirmed, cancelled or locked orders stay; the order totals follow.
    #[method(override)]
    async fn unlink(
        this: &RecordSet<PurchaseOrderLine>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        let lines = this.as_records().clone();
        if lines.is_empty() {
            return sup.call(tx, &lines, ()).await;
        }
        let env = this.env().clone();
        let order_ids: BTreeSet<i64> = line_orders(tx, &lines).await?.into_values().collect();
        let heads = order_heads(tx, &env, &order_ids).await?;
        refuse_closed(&heads, &order_ids, "delete lines of", true)?;
        sup.call(tx, &lines, ()).await?;
        sync_orders(tx, &env, &order_ids).await
    }

    /// Notify installed purchase review extensions when their relational source changes.
    #[method]
    async fn _invalidate_purchase_bill_review(
        _this: &RecordSet<PurchaseOrderLine>,
        tx: &mut Transaction,
    ) -> Result<()> {
        tx.invalidate_model("account.move.line", Some(crate::bill_source_review::FIELDS))
            .await
    }
}

/// Per line: description, order number, product, unit, received quantity, billed quantity (draft
/// and posted vendor bills; a linked bill is always a vendor bill) and ordered quantity.
const LINE_CONSEQUENCES: &str = "SELECT l.id::int8, COALESCE(l.name, ''), o.name, l.product_id::int8, \
    l.uom_id::int8, COALESCE(l.qty_received, 0)::numeric, COALESCE(b.billed, 0)::numeric, \
    COALESCE(l.product_qty, 0)::numeric \
    FROM purchase_order_line l JOIN purchase_order o ON o.id = l.order_id \
    LEFT JOIN (SELECT k.purchase_line_id, SUM(i.quantity) AS billed FROM purchase_direct_bill_line k \
        JOIN account_move_line i ON i.id = k.bill_line_id JOIN account_move m ON m.id = i.move_id \
        WHERE m.state IN ('draft', 'posted') GROUP BY k.purchase_line_id) b \
    ON b.purchase_line_id = l.id WHERE l.id = ANY($1) ORDER BY l.id";

/// Refuses, on lines with received or billed goods, a new product or unit and a quantity below
/// what was received or billed, naming the line and its order. Stricter than the reference, which
/// checks neither on a write: the receipts and bills keep the line's product, unit and quantity.
async fn refuse_consequences(tx: &mut Transaction, lines: &Records, vals: &RawVals) -> Result<()> {
    let id_of = |field: &str| match vals.get(field) {
        Some(Cell::Id(id)) => Some(*id),
        _ => None,
    };
    let quantity = match vals.get("product_qty") {
        Some(cell) => Some(amount(cell, "product_qty")?),
        None => None,
    };
    let identity = vals.contains("product_id") || vals.contains("uom_id");
    if !identity && quantity.is_none() {
        return Ok(());
    }
    // the write right first: the refusal names the line and its order
    lines.check_access(tx, erp_sdk::Operation::Write).await?;
    tx.flush().await?;
    for row in tx.query(LINE_CONSEQUENCES, &[&lines.ids()]).await? {
        let line: String = row.try_get(1).map_err(db_error)?;
        // the first line of the description names the line, when it has one
        let line = match line.lines().next() {
            Some(first) if !first.trim().is_empty() => format!(" {first}"),
            _ => String::new(),
        };
        let order: String = row.try_get(2).map_err(db_error)?;
        let product: Option<i64> = row.try_get(3).map_err(db_error)?;
        let unit: Option<i64> = row.try_get(4).map_err(db_error)?;
        let received: Decimal = row.try_get(5).map_err(db_error)?;
        let billed: Decimal = row.try_get(6).map_err(db_error)?;
        let ordered: Decimal = row.try_get(7).map_err(db_error)?;
        if received.is_zero() && billed.is_zero() {
            continue;
        }
        let moved = (vals.contains("product_id") && id_of("product_id") != product)
            || (vals.contains("uom_id") && id_of("uom_id") != unit);
        if moved {
            return Err(Error::user(format!(
                "The purchase order line{line} of {order} has received or billed goods: its product and unit can no longer change."
            )));
        }
        // a raise or an unchanged quantity passes, even while still below the floor
        let floor = received.max(billed);
        if let Some(asked) = quantity.filter(|asked| *asked < floor && *asked < ordered) {
            let hint = if received >= billed {
                "return the goods first"
            } else {
                "correct the bill first"
            };
            return Err(Error::user(format!(
                "The quantity of the purchase order line{line} of {order} cannot go below what was received or billed ({} asked, {} received, {} billed); {hint}.",
                asked.normalize(),
                received.normalize(),
                billed.normalize()
            )));
        }
    }
    Ok(())
}

fn db_error(error: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::db("Reading what a purchase order line received and billed failed.").with_source(error)
}

/// The totals, in the order currency, of the approved orders of `heads`, before a line change.
/// The order currency of an approved order is frozen, so these totals compare like with like.
async fn approved_totals(
    tx: &mut Transaction,
    env: &Env,
    heads: &BTreeMap<i64, OrderHead>,
) -> Result<BTreeMap<i64, Decimal>> {
    let approved: Vec<i64> = heads
        .iter()
        .filter(|(_, head)| head.state == "purchase")
        .map(|(id, _)| *id)
        .collect();
    if approved.is_empty() {
        return Ok(BTreeMap::new());
    }
    let orders = env
        .records("purchase.order")?
        .browse(approved.iter().copied())?;
    let cells = orders.cells(tx, "amount_total").await?;
    orders
        .ids()
        .into_iter()
        .zip(cells.iter())
        .map(|(id, cell)| Ok((id, amount(cell, "amount_total")?)))
        .collect()
}

/// A change of lines may not take the total of an approved order above what the caller may
/// approve: the approval read the total as it was, so a raise past the limit needs a user who
/// may approve it (V11). A raise is a higher total in the order currency; the question
/// itself converts into the company currency. A lower total, and a raise inside the limit, pass.
async fn refuse_unapproved_raise(
    tx: &mut Transaction,
    env: &Env,
    heads: &BTreeMap<i64, OrderHead>,
    before: &BTreeMap<i64, Decimal>,
) -> Result<()> {
    if before.is_empty() {
        return Ok(());
    }
    let orders = env
        .records("purchase.order")?
        .browse(before.keys().copied())?;
    let after = orders.cells(tx, "amount_total").await?;
    let raised: Vec<i64> = before
        .iter()
        .zip(after.iter())
        .map(|((id, old), cell)| Ok((*id, amount(cell, "amount_total")? > *old)))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter_map(|(id, raised)| raised.then_some(id))
        .collect();
    if raised.is_empty() {
        return Ok(());
    }
    // one answer per order, in id order, as the approval question gives them
    let allowed: Vec<bool> = orders
        .browse(raised.iter().copied())?
        .call(tx, "_approval_allowed", ())
        .await?;
    let refused: Vec<String> = raised
        .iter()
        .zip(allowed)
        .filter(|(_, ok)| !ok)
        .filter_map(|(id, _)| heads.get(id).map(|h| h.name.clone()))
        .collect();
    if refused.is_empty() {
        return Ok(());
    }
    Err(Error::access(format!(
        "These purchase orders are approved, and this change takes their total above what you may \
         approve; ask a purchase administrator to make it: {}.",
        refused.join(", ")
    )))
}

/// Apply the ordinary purchase-line changes, including the product's dependent defaults.
async fn write_lines(
    tx: &mut Transaction,
    lines: &Records,
    vals: RawVals,
    sup: Super<(RawVals,), ()>,
) -> Result<()> {
    let env = lines.env().clone();
    let orders = line_orders(tx, lines).await?;
    let order_ids: BTreeSet<i64> = orders.values().copied().collect();
    let heads = order_heads(tx, &env, &order_ids).await?;
    refuse_closed(&heads, &order_ids, "change lines of", false)?;
    let written: BTreeSet<String> = vals
        .iter()
        .map(|(name, _)| name)
        .chain(vals.iter_commands().map(|(name, _)| name))
        .map(str::to_owned)
        .collect();
    let reprice = !written.contains("price_unit") && REPRICE.iter().any(|f| written.contains(*f));
    let manual = if reprice {
        manual_prices(tx, lines).await?
    } else {
        BTreeSet::new()
    };
    let before = approved_totals(tx, &env, &heads).await?;
    sup.call(tx, lines, (vals,)).await?;
    if written.contains("product_id") {
        for id in lines.ids() {
            refill_for_product(tx, &env, &heads[&orders[&id]], id, &written).await?;
        }
    }
    if reprice {
        for id in lines.ids() {
            if !manual.contains(&id) {
                reprice_line(tx, &env, &heads[&orders[&id]], id).await?;
            }
        }
    }
    sync_lines(tx, &env, lines).await?;
    refuse_unapproved_raise(tx, &env, &heads, &before).await?;
    if [
        "product_id",
        "uom_id",
        "product_qty",
        "price_unit",
        "discount",
        "analytic_account_id",
        "asset_id",
    ]
    .iter()
    .any(|field| written.contains(*field))
    {
        let confirmed_orders: Vec<i64> = heads
            .iter()
            .filter(|(_, h)| h.state == "purchase")
            .map(|(id, _)| *id)
            .collect();
        if !confirmed_orders.is_empty() {
            crate::order::sync_order_commitments(tx, &env, &confirmed_orders).await?;
        }
    }
    if reprice
        || [
            "product_id",
            "uom_id",
            "product_qty",
            "price_unit",
            "discount",
        ]
        .iter()
        .any(|field| written.contains(*field))
    {
        invalidate_bill_comparisons(tx).await?;
    }
    Ok(())
}

/// Refresh transaction-local bill review fields after changing their purchase or receipt source.
///
/// # Errors
///
/// Propagates ORM cache invalidation errors.
pub async fn invalidate_bill_comparisons(tx: &mut Transaction) -> Result<()> {
    let lines = tx.env().records("purchase.order.line")?;
    lines
        .call::<(), ()>(tx, "_invalidate_purchase_bill_review", ())
        .await
}

/// After a new product on an existing line: unit, vendor taxes and description of the new product,
/// each unless the same write gave it (the price follows through `reprice_line`, unless it was
/// set by hand).
async fn refill_for_product(
    tx: &mut Transaction,
    env: &Env,
    head: &OrderHead,
    id: i64,
    written: &BTreeSet<String>,
) -> Result<()> {
    let line = env.records("purchase.order.line")?.browse([id])?;
    let Some(product) = id_of(Some(&line.cell(tx, "product_id").await?), "product_id")? else {
        return Ok(());
    };
    let company_env = env.with_company(head.company)?;
    let record = company_env.records("product.product")?.browse([product])?;
    let mut set = RawVals::new();
    if !written.contains("uom_id") {
        let unit = id_of(Some(&record.cell(tx, "uom_id").await?), "uom_id")?
            .ok_or_else(|| Error::invariant(format!("product {product} has no unit")))?;
        set = set.set_cell("uom_id", Cell::Id(unit));
    }
    if !written.contains("name") {
        set = set.set_cell(
            "name",
            description(tx, &record)
                .await?
                .map_or(Cell::Null, Cell::Str),
        );
    }
    if !written.contains("tax_ids") {
        let taxes = vendor_taxes(tx, &company_env, &record, head.company).await?;
        set = set.set_commands("tax_ids", [Command::Set(taxes)]);
    }
    // the vendor price row and discount of the former product are no longer this line's; a line
    // priced again takes the new ones, a hand-priced line has none
    set = set.set_cell("selected_seller_id", Cell::Null);
    if !written.contains("discount") {
        set = set.set_cell("discount", Cell::Decimal(Decimal::ZERO));
    }
    line.write_base(tx, set).await
}

/// The lines of `orders` priced again from their vendor (the order's vendor changed), unless a
/// person set the price; then the amounts and totals follow.
pub(crate) async fn reprice_orders(
    tx: &mut Transaction,
    env: &Env,
    orders: &BTreeSet<i64>,
) -> Result<()> {
    let ids: Vec<i64> = orders.iter().copied().collect();
    let lines = env
        .sudo(true)
        .records("purchase.order.line")?
        .search(tx, &Domain::condition("order_id", Operator::In, ids)?)
        .await?;
    if lines.is_empty() {
        return Ok(());
    }
    let owners = line_orders(tx, &lines).await?;
    let heads = order_heads(tx, env, orders).await?;
    let manual = manual_prices(tx, &lines).await?;
    for id in lines.ids() {
        if manual.contains(&id) {
            // a hand-priced line keeps its price, not the former vendor's row and discount
            env.records("purchase.order.line")?
                .browse([id])?
                .write_base(
                    tx,
                    RawVals::new()
                        .set_cell("selected_seller_id", Cell::Null)
                        .set_cell("discount", Cell::Decimal(Decimal::ZERO)),
                )
                .await?;
        } else {
            reprice_line(tx, env, &heads[&owners[&id]], id).await?;
        }
    }
    sync_lines(tx, env, &lines).await
}

/// What a line reads of its order.
struct OrderHead {
    name: String,
    state: String,
    locked: bool,
    company: i64,
    currency: i64,
    partner: i64,
    date_order: DateTime,
}

fn id_of(cell: Option<&Cell>, field: &str) -> Result<Option<i64>> {
    match cell {
        None | Some(Cell::Null | Cell::Bool(false)) => Ok(None),
        Some(Cell::Id(id)) => Ok(Some(*id)),
        Some(Cell::Int(id)) => Ok(Some(i64::from(*id))),
        Some(other) => Err(Error::validation(format!(
            "{field} takes a record id, not {other:?}."
        ))),
    }
}

pub(crate) fn amount(cell: &Cell, field: &str) -> Result<Decimal> {
    match cell {
        Cell::Null | Cell::Bool(false) => Ok(Decimal::ZERO),
        Cell::Decimal(value) => Ok(*value),
        Cell::Int(value) => Ok(Decimal::from(*value)),
        Cell::Float(value) => Decimal::try_from(*value)
            .map_err(|e| Error::validation(format!("{field} {value} is not exact: {e}"))),
        other => Err(Error::validation(format!(
            "{field} takes a number, not {other:?}."
        ))),
    }
}

fn value_amount(value: Option<&Value>, field: &str) -> Result<Decimal> {
    match value {
        None | Some(Value::False) => Ok(Decimal::ZERO),
        Some(Value::Decimal(d)) => Ok(*d),
        Some(Value::Int(i)) => Ok(Decimal::from(*i)),
        Some(Value::Float(f)) => Decimal::try_from(*f)
            .map_err(|e| Error::internal(format!("vendor price {field} {f}: {e}"))),
        Some(other) => Err(Error::internal(format!(
            "vendor price {field} is {other:?}, not a number"
        ))),
    }
}

fn value_id(value: Option<&Value>, field: &str) -> Result<i64> {
    match value {
        Some(Value::Int(id)) if *id > 0 => Ok(*id),
        other => Err(Error::internal(format!(
            "vendor price {field} is {other:?}, not a record id"
        ))),
    }
}

/// Refuses the fields only this module or a later slice writes, by name.
fn refuse_line_fields(vals: &RawVals, on_write: bool) -> Result<()> {
    if let Some(field) = ORDER_COPIES.iter().find(|f| vals.contains(f)) {
        return Err(Error::validation(format!(
            "The field {field} of a purchase order line is a copy of the one of its order and \
             cannot be written on the line; change it on the order."
        )));
    }
    // the line form sends the last price with a new line; a later write never sets it
    if on_write && vals.contains("technical_price_unit") {
        return Err(Error::validation(
            "The field technical_price_unit of a purchase order line follows its price and cannot \
             be written; change the unit price instead.",
        ));
    }
    // a value or x2many commands: the bill links are a one2many
    if let Some(field) = COMPUTED
        .iter()
        .find(|f| vals.contains(f) || vals.commands(f).is_some())
    {
        return Err(Error::validation(format!(
            "The field {field} of a purchase order line is computed from the line and its order, \
             not written."
        )));
    }
    if let Some(field) = ORDER_OPERATION.iter().find(|f| vals.contains(f)) {
        return Err(Error::validation(format!(
            "The field {field} of a purchase order line is the one of its order, set by the order's \
             buttons, not written."
        )));
    }
    if let Some(field) = RECEIVED.iter().find(|f| vals.contains(f)) {
        return Err(Error::user(format!(
            "The received quantity of a purchase order line ({field}) is filled by receipts; \
             receiving goods is not available yet, so it cannot be written."
        )));
    }
    Ok(())
}

/// The heads of `ids`, read as the caller (who must read the orders of the lines it edits).
async fn order_heads(
    tx: &mut Transaction,
    env: &Env,
    ids: &BTreeSet<i64>,
) -> Result<BTreeMap<i64, OrderHead>> {
    let orders = env.records("purchase.order")?.browse(ids.iter().copied())?;
    let mut columns = BTreeMap::new();
    for field in [
        "name",
        "state",
        "locked",
        "company_id",
        "currency_id",
        "partner_id",
        "date_order",
    ] {
        columns.insert(field, orders.cells(tx, field).await?);
    }
    let mut heads = BTreeMap::new();
    for (i, id) in orders.ids().into_iter().enumerate() {
        let cell = |field: &str| -> Result<&Cell> {
            columns
                .get(field)
                .and_then(|c| c.get(i))
                .ok_or_else(|| Error::invariant(format!("order column {field} is short")))
        };
        let text = |field: &str| -> Result<String> {
            match cell(field)? {
                Cell::Str(s) => Ok(s.to_string()),
                other => Err(Error::invariant(format!("order {id} {field} is {other:?}"))),
            }
        };
        let required = |field: &str| -> Result<i64> {
            id_of(Some(cell(field)?), field)?
                .ok_or_else(|| Error::invariant(format!("order {id} has no {field}")))
        };
        heads.insert(
            id,
            OrderHead {
                name: text("name")?,
                state: text("state")?,
                locked: matches!(cell("locked")?, Cell::Bool(true)),
                company: required("company_id")?,
                currency: required("currency_id")?,
                partner: required("partner_id")?,
                date_order: match cell("date_order")? {
                    Cell::DateTime(at) => *at,
                    other => return Err(Error::invariant(format!("order {id} date {other:?}"))),
                },
            },
        );
    }
    Ok(heads)
}

/// Refuses line changes on cancelled or locked orders, and with `confirmed` on purchase orders
/// too, naming the orders (V5, V3).
fn refuse_closed(
    heads: &BTreeMap<i64, OrderHead>,
    ids: &BTreeSet<i64>,
    what: &str,
    confirmed: bool,
) -> Result<()> {
    let closed: Vec<String> = ids
        .iter()
        .filter_map(|id| heads.get(id))
        .filter(|h| h.state == "cancel" || h.locked || (confirmed && h.state == "purchase"))
        .map(|h| {
            let lock = if h.locked { ", locked" } else { "" };
            format!("{} ({}{lock})", h.name, h.state)
        })
        .collect();
    if closed.is_empty() {
        return Ok(());
    }
    Err(Error::user(format!(
        "You cannot {what} these purchase orders: {}.",
        closed.join(", ")
    )))
}

/// The order of each line.
async fn line_orders(tx: &mut Transaction, lines: &Records) -> Result<BTreeMap<i64, i64>> {
    let cells = lines.cells(tx, "order_id").await?;
    let mut orders = BTreeMap::new();
    for (id, cell) in lines.ids().into_iter().zip(cells.iter()) {
        let order = id_of(Some(cell), "order_id")?
            .ok_or_else(|| Error::invariant(format!("purchase order line {id} has no order")))?;
        orders.insert(id, order);
    }
    Ok(orders)
}

/// The lines whose unit price differs from the price they were last given: set by a person.
async fn manual_prices(tx: &mut Transaction, lines: &Records) -> Result<BTreeSet<i64>> {
    let prices = lines.cells(tx, "price_unit").await?;
    let technical = lines.cells(tx, "technical_price_unit").await?;
    let mut manual = BTreeSet::new();
    for (i, id) in lines.ids().into_iter().enumerate() {
        let get = |cells: &[Cell]| cells.get(i).cloned().unwrap_or(Cell::Null);
        if amount(&get(&prices), "price_unit")? != amount(&get(&technical), "technical_price_unit")?
        {
            manual.insert(id);
        }
    }
    Ok(manual)
}

/// The vendor price of `product` for the order's vendor, quantity and unit at the deadline's
/// date, through the product seller service under the order's company.
async fn seller(
    tx: &mut Transaction,
    env: &Env,
    head: &OrderHead,
    product: i64,
    quantity: Decimal,
    unit: i64,
) -> Result<Option<SellerInfo>> {
    // the UTC day of the deadline, whoever asks: a stored price never depends on the reader
    let date = head.date_order.date();
    let query = SellerQuery::default()
        .with_partner(head.partner)?
        .with_quantity(Some(quantity.abs()))
        .with_date(date)
        .with_unit(unit)?
        .with_force_uom(true);
    env.with_company(head.company)?
        .records("product.product")?
        .browse([product])?
        .call(tx, "_select_seller_exact", (query,))
        .await
}

/// The price, discount, vendor price row and lead time of a line of `product` in `unit`: the
/// vendor price when one applies, else the product cost; converted to `unit` and the order's
/// currency at the deadline's date.
async fn price_of(
    tx: &mut Transaction,
    env: &Env,
    head: &OrderHead,
    product: i64,
    quantity: Decimal,
    unit: i64,
) -> Result<(Decimal, Decimal, Option<i64>, i64)> {
    let company_env = env.with_company(head.company)?;
    let digits =
        erp_addon_base::decimal_precision::precision_get(&company_env, tx, "Product Price").await?;
    let precision = Increment::from_places(digits)?;
    let target = load_unit(&company_env, tx, unit).await?;
    let (price, from_unit, currency, discount, row, delay) =
        match seller(tx, env, head, product, quantity, unit).await? {
            Some(info) => {
                let v = info.values();
                (
                    value_amount(v.get("price"), "price")?,
                    value_id(v.get("uom_id"), "uom_id")?,
                    value_id(v.get("currency_id"), "currency_id")?,
                    value_amount(v.get("discount"), "discount")?,
                    Some(value_id(v.get("supplierinfo"), "supplierinfo")?),
                    // the reference's lead time is required with a default of 1 day
                    // : never guessed
                    match v.get("delay") {
                        Some(Value::Int(days)) => *days,
                        _ => {
                            // the column is required, so no row reaches here without one
                            return Err(Error::invariant(format!(
                                "vendor price {} has no lead time",
                                value_id(v.get("supplierinfo"), "supplierinfo")?
                            )));
                        }
                    },
                )
            }
            None => {
                let record = company_env.records("product.product")?.browse([product])?;
                // the cost hook, so a valuation addon supplies it per product
                let costs: Vec<Decimal> = record.call(tx, "_get_standard_price", ()).await?;
                let [cost] = costs.as_slice() else {
                    return Err(Error::programming(
                        "_get_standard_price must return one value per product.",
                    ));
                };
                let cost = *cost;
                let from_unit = id_of(Some(&record.cell(tx, "uom_id").await?), "uom_id")?
                    .ok_or_else(|| Error::invariant(format!("product {product} has no unit")))?;
                // the reference's cost currency is the product company's or the active company's
                // currency, never empty;
                // it is read under the order's company, so an empty one is a broken product
                let currency = id_of(
                    Some(&record.cell(tx, "cost_currency_id").await?),
                    "cost_currency_id",
                )?
                .ok_or_else(|| {
                    Error::invariant(format!("product {product} has no cost currency"))
                })?;
                // no vendor price: no lead time, the line is expected at the deadline
                // (an empty seller info)
                (cost, from_unit, currency, Decimal::ZERO, None, 0)
            }
        };
    let source = load_unit(&company_env, tx, from_unit).await?;
    let in_unit =
        price_exact(&source, &target, price)?.quantize(&precision, CURRENCY_ROUNDING_MODE)?;
    let in_currency = if currency == head.currency {
        in_unit
    } else {
        let at = head.date_order.date();
        let rec = |id: i64| {
            RecId::from_real(id).ok_or_else(|| Error::invariant(format!("record id {id}")))
        };
        convert(
            &company_env,
            tx,
            in_unit,
            rec(currency)?,
            rec(head.currency)?,
            rec(head.company)?,
            at,
        )
        .await?
    };
    Ok((in_currency, discount, row, delay))
}

/// The deadline moved by `days` days.
fn planned(head: &OrderHead, days: i64) -> Result<DateTime> {
    days.checked_mul(MICROS_PER_DAY)
        .and_then(|delta| head.date_order.micros().checked_add(delta))
        .map(DateTime::from_micros)
        .ok_or_else(|| Error::validation(format!("A lead time of {days} days is out of range.")))
}

/// Unit, description, taxes, price, discount, vendor price row and planned date of a new line,
/// each only when not given.
async fn fill_line(
    tx: &mut Transaction,
    env: &Env,
    head: &OrderHead,
    vals: &mut RawVals,
) -> Result<()> {
    let Some(product) = id_of(vals.get("product_id"), "product_id")? else {
        if !vals.contains("date_planned") {
            vals.insert("date_planned".into(), Cell::DateTime(head.date_order));
        }
        return Ok(());
    };
    let company_env = env.with_company(head.company)?;
    let record = company_env.records("product.product")?.browse([product])?;
    let unit = match id_of(vals.get("uom_id"), "uom_id")? {
        Some(unit) => unit,
        None => {
            let unit = id_of(Some(&record.cell(tx, "uom_id").await?), "uom_id")?
                .ok_or_else(|| Error::invariant(format!("product {product} has no unit")))?;
            vals.insert("uom_id".into(), Cell::Id(unit));
            unit
        }
    };
    if !vals.contains("name")
        && let Some(text) = description(tx, &record).await?
    {
        vals.insert("name".into(), Cell::Str(text));
    }
    if vals.commands("tax_ids").is_none() && !vals.contains("tax_ids") {
        let taxes = vendor_taxes(tx, &company_env, &record, head.company).await?;
        vals.insert_commands("tax_ids".into(), vec![Command::Set(taxes)]);
    }
    // a price is set by hand unless it equals the price the line was last given: the line form
    // sends its untouched default price with that technical price (the reference's rule,
    // ); a given price without it is the caller's (a copy, an import)
    let priced = match (vals.get("price_unit"), vals.get("technical_price_unit")) {
        (None, _) => false,
        (Some(price), Some(last)) => {
            amount(price, "price_unit")? != amount(last, "technical_price_unit")?
        }
        (Some(_), None) => true,
    };
    // the form also sends the default discount 0; with an untouched price the vendor's applies
    let discounted = match vals.get("discount") {
        None => false,
        Some(discount) => priced || amount(discount, "discount")? != Decimal::ZERO,
    };
    if !priced || !discounted || !vals.contains("date_planned") {
        let quantity = match vals.get("product_qty") {
            Some(cell) => amount(cell, "product_qty")?,
            None => Decimal::ONE,
        };
        let (price, discount, row, delay) =
            price_of(tx, env, head, product, quantity, unit).await?;
        if !priced {
            vals.insert("price_unit".into(), Cell::Decimal(price));
            vals.insert("technical_price_unit".into(), Cell::Decimal(price));
        }
        if !discounted {
            vals.insert("discount".into(), Cell::Decimal(discount));
        }
        if !vals.contains("date_planned") {
            vals.insert("date_planned".into(), Cell::DateTime(planned(head, delay)?));
        }
        vals.insert(
            "selected_seller_id".into(),
            row.map_or(Cell::Null, Cell::Id),
        );
    }
    if !vals.contains("technical_price_unit") {
        // a given price counts as the last price the line was given, so the quantity can reprice it
        let price = vals.get("price_unit").cloned().unwrap_or(Cell::Null);
        vals.insert("technical_price_unit".into(), price);
    }
    Ok(())
}

/// The product's purchase description, if it has one; the product itself is shown next to it
/// (attribute values without variants come with the order
/// lines slice).
async fn description(tx: &mut Transaction, product: &Records) -> Result<Option<Arc<str>>> {
    Ok(match product.cell(tx, "description_purchase").await? {
        Cell::Str(text) if !text.trim().is_empty() => Some(text),
        _ => None,
    })
}

/// The product's vendor taxes that belong to `company` or a parent of it
/// (fiscal positions come with the order defaults slice).
async fn vendor_taxes(
    tx: &mut Transaction,
    env: &Env,
    product: &Records,
    company: i64,
) -> Result<Vec<RecId>> {
    let proposed: Vec<i64> = match product.cell(tx, "supplier_taxes_id").await? {
        Cell::Ids(ids) => ids
            .iter()
            .filter_map(|id| id.as_real().map(|id| id.get()))
            .collect(),
        _ => Vec::new(),
    };
    if proposed.is_empty() {
        return Ok(Vec::new());
    }
    Ok(env
        .records("account.tax")?
        .search(
            tx,
            &Domain::and([
                Domain::condition("id", Operator::In, proposed)?,
                Domain::condition("company_id", Operator::ParentOf, company)?,
            ]),
        )
        .await?
        .ids()
        .into_iter()
        .filter_map(RecId::from_real)
        .collect())
}

/// Prices line `id` again from its current product, quantity and unit.
async fn reprice_line(tx: &mut Transaction, env: &Env, head: &OrderHead, id: i64) -> Result<()> {
    let line = env.records("purchase.order.line")?.browse([id])?;
    let Some(product) = id_of(Some(&line.cell(tx, "product_id").await?), "product_id")? else {
        return Ok(());
    };
    let Some(unit) = id_of(Some(&line.cell(tx, "uom_id").await?), "uom_id")? else {
        return Ok(());
    };
    let quantity = amount(&line.cell(tx, "product_qty").await?, "product_qty")?;
    let (price, discount, row, _) = price_of(tx, env, head, product, quantity, unit).await?;
    line.write_base(
        tx,
        RawVals::new()
            .set_cell("price_unit", Cell::Decimal(price))
            .set_cell("technical_price_unit", Cell::Decimal(price))
            .set_cell("discount", Cell::Decimal(discount))
            .set_cell("selected_seller_id", row.map_or(Cell::Null, Cell::Id)),
    )
    .await
}

/// The taxes of `ids` as the tax computation takes them. A group of taxes and a tax with
/// a negative factor (reverse charge) are not supported yet and refused as on invoices; other unsupported kinds are refused by the computation.
async fn load_taxes(
    tx: &mut Transaction,
    env: &Env,
    ids: &BTreeSet<i64>,
) -> Result<BTreeMap<i64, TaxDef>> {
    if ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let taxes = env.records("account.tax")?.browse(ids.iter().copied())?;
    let mut columns = BTreeMap::new();
    for field in [
        "name",
        "amount_type",
        "amount",
        "sequence",
        "price_include",
        "include_base_amount",
        "is_base_affected",
        "has_negative_factor",
    ] {
        columns.insert(field, taxes.cells(tx, field).await?);
    }
    let mut defs = BTreeMap::new();
    for (i, id) in taxes.ids().into_iter().enumerate() {
        let cell = |field: &str| -> Result<Cell> {
            columns
                .get(field)
                .and_then(|c| c.get(i))
                .cloned()
                .ok_or_else(|| Error::invariant(format!("tax column {field} is short")))
        };
        let flag = |field: &str| -> Result<bool> { Ok(matches!(cell(field)?, Cell::Bool(true))) };
        let name = || -> Result<String> {
            Ok(match cell("name")? {
                Cell::Str(name) => name.to_string(),
                _ => format!("#{id}"),
            })
        };
        let kind = match cell("amount_type")? {
            Cell::Str(kind) if kind.as_ref() == "group" => {
                return Err(Error::user(format!(
                    "Tax {} is a group of taxes: purchase lines do not support groups of \
                     taxes yet; use their child taxes on the line.",
                    name()?
                )));
            }
            Cell::Str(kind) => AmountType::parse(&kind)?,
            other => return Err(Error::invariant(format!("tax {id} amount type {other:?}"))),
        };
        if flag("has_negative_factor")? {
            return Err(Error::user(format!(
                "Tax {} has a negative distribution (reverse charge), which purchase lines do \
                 not support yet.",
                name()?
            )));
        }
        defs.insert(
            id,
            TaxDef {
                id,
                sequence: match cell("sequence")? {
                    Cell::Int(seq) => seq,
                    _ => {
                        return Err(Error::validation(format!(
                            "Tax {} has no sequence; give it one.",
                            match cell("name")? {
                                Cell::Str(name) => name.to_string(),
                                _ => format!("#{id}"),
                            }
                        )));
                    }
                },
                kind: TaxKind::Simple(kind),
                amount: amount(&cell("amount")?, "amount")?,
                price_include: flag("price_include")?,
                include_base_amount: flag("include_base_amount")?,
                is_base_affected: flag("is_base_affected")?,
            },
        );
    }
    Ok(defs)
}

/// Writes subtotal, tax, total and the quantity in the product unit of `lines`, then the totals
/// of their orders.
async fn sync_lines(tx: &mut Transaction, env: &Env, lines: &Records) -> Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    let sudo = env.sudo(true);
    let lines = sudo.records("purchase.order.line")?.browse(lines.ids())?;
    let mut columns = BTreeMap::new();
    for field in [
        "order_id",
        "product_id",
        "product_qty",
        "uom_id",
        "price_unit",
        "discount",
        "tax_ids",
        "currency_id",
    ] {
        columns.insert(field, lines.cells(tx, field).await?);
    }
    let get = |field: &str, i: usize| -> Cell {
        columns
            .get(field)
            .and_then(|c| c.get(i))
            .cloned()
            .unwrap_or(Cell::Null)
    };
    let ids = lines.ids();
    let mut tax_ids = BTreeSet::new();
    let mut currencies = BTreeSet::new();
    let mut orders = BTreeSet::new();
    for i in 0..ids.len() {
        if let Cell::Ids(taxes) = get("tax_ids", i) {
            tax_ids.extend(taxes.iter().filter_map(|t| t.as_real().map(|t| t.get())));
        }
        if let Some(currency) = id_of(Some(&get("currency_id", i)), "currency_id")? {
            currencies.insert(currency);
        }
        if let Some(order) = id_of(Some(&get("order_id", i)), "order_id")? {
            orders.insert(order);
        }
    }
    let defs = load_taxes(tx, &sudo, &tax_ids).await?;
    let currency_ids: Vec<RecId> = currencies
        .iter()
        .filter_map(|c| RecId::from_real(*c))
        .collect();
    let table = RoundingTable::load(&sudo, tx, &currency_ids).await?;
    let digits =
        erp_addon_base::decimal_precision::precision_get(&sudo, tx, "Product Unit").await?;
    let qty_precision = Increment::from_places(digits)?;
    for (i, id) in ids.iter().enumerate() {
        let currency = id_of(Some(&get("currency_id", i)), "currency_id")?
            .and_then(RecId::from_real)
            .ok_or_else(|| Error::invariant(format!("purchase order line {id} has no currency")))?;
        let taxes: Vec<TaxDef> = match get("tax_ids", i) {
            Cell::Ids(taxes) => taxes
                .iter()
                .filter_map(|t| t.as_real())
                .map(|t| {
                    defs.get(&t.get()).cloned().ok_or_else(|| {
                        Error::invariant(format!(
                            "tax {} of purchase order line {id} was not loaded",
                            t.get()
                        ))
                    })
                })
                .collect::<Result<_>>()?,
            _ => Vec::new(),
        };
        let quantity = amount(&get("product_qty", i), "product_qty")?;
        let result = compute::compute(&BaseLine {
            price_unit: amount(&get("price_unit", i), "price_unit")?,
            quantity,
            discount: amount(&get("discount", i), "discount")?,
            taxes: &taxes,
            increment: table.get(currency)?.increment(),
        })?;
        let tax = result
            .total_included
            .checked_sub(result.total_excluded)
            .ok_or_else(out_of_range)?;
        let in_product_unit = product_quantity(
            tx,
            &sudo,
            &get("product_id", i),
            &get("uom_id", i),
            quantity,
            &qty_precision,
        )
        .await?;
        sudo.records("purchase.order.line")?
            .browse([*id])?
            .write_base(
                tx,
                RawVals::new()
                    .set_cell("price_subtotal", Cell::Decimal(result.total_excluded))
                    .set_cell("price_tax", Cell::Decimal(tax))
                    .set_cell("price_total", Cell::Decimal(result.total_included))
                    .set_cell("product_uom_qty", Cell::Decimal(in_product_unit)),
            )
            .await?;
    }
    sync_orders(tx, env, &orders).await
}

/// `quantity` in `unit` expressed in the product's unit, rounded up at Product Unit precision.
async fn product_quantity(
    tx: &mut Transaction,
    env: &Env,
    product: &Cell,
    unit: &Cell,
    quantity: Decimal,
    precision: &Increment,
) -> Result<Decimal> {
    let (Some(product), Some(unit)) = (
        id_of(Some(product), "product_id")?,
        id_of(Some(unit), "uom_id")?,
    ) else {
        return Ok(quantity);
    };
    let main = id_of(
        Some(
            &env.records("product.product")?
                .browse([product])?
                .cell(tx, "uom_id")
                .await?,
        ),
        "uom_id",
    )?
    .unwrap_or(unit);
    if main == unit {
        return Ok(quantity);
    }
    let source = load_unit(env, tx, unit).await?;
    let target = load_unit(env, tx, main).await?;
    Ok(quantity_rounded(
        &source,
        &target,
        quantity,
        precision,
        RoundingMode::AwayFromZero,
    )?)
}

fn out_of_range() -> Error {
    Error::validation("An amount of the purchase order is out of range.")
}

/// Writes the untaxed amount, taxes, total, total in the company currency and earliest planned
/// date of `orders` from their lines. Taxes are rounded
/// per line, the only rounding the tax computation has.
pub(crate) async fn sync_orders(
    tx: &mut Transaction,
    env: &Env,
    orders: &BTreeSet<i64>,
) -> Result<()> {
    if orders.is_empty() {
        return Ok(());
    }
    let sudo = env.sudo(true);
    let ids: Vec<i64> = orders.iter().copied().collect();
    let lines = sudo
        .records("purchase.order.line")?
        .search(
            tx,
            &Domain::condition("order_id", Operator::In, ids.clone())?,
        )
        .await?;
    let owners = lines.cells(tx, "order_id").await?;
    let subtotals = lines.cells(tx, "price_subtotal").await?;
    let taxes = lines.cells(tx, "price_tax").await?;
    let dates = lines.cells(tx, "date_planned").await?;
    let mut sums: BTreeMap<i64, (Decimal, Decimal, Option<DateTime>)> = ids
        .iter()
        .map(|id| (*id, (Decimal::ZERO, Decimal::ZERO, None)))
        .collect();
    for i in 0..owners.len() {
        let Some(order) = id_of(owners.get(i), "order_id")? else {
            continue;
        };
        let entry = sums
            .entry(order)
            .or_insert((Decimal::ZERO, Decimal::ZERO, None));
        entry.0 = entry
            .0
            .checked_add(amount(
                subtotals.get(i).unwrap_or(&Cell::Null),
                "price_subtotal",
            )?)
            .ok_or_else(out_of_range)?;
        entry.1 = entry
            .1
            .checked_add(amount(taxes.get(i).unwrap_or(&Cell::Null), "price_tax")?)
            .ok_or_else(out_of_range)?;
        if let Some(Cell::DateTime(at)) = dates.get(i) {
            entry.2 = Some(entry.2.map_or(*at, |seen: DateTime| seen.min(*at)));
        }
    }
    let records = sudo
        .records("purchase.order")?
        .browse(ids.iter().copied())?;
    let companies = records.cells(tx, "company_id").await?;
    let currencies = records.cells(tx, "currency_id").await?;
    let deadlines = records.cells(tx, "date_order").await?;
    for (i, id) in ids.iter().enumerate() {
        let (untaxed, tax, earliest) = sums[id];
        let total = untaxed.checked_add(tax).ok_or_else(out_of_range)?;
        let company = id_of(companies.get(i), "company_id")?.and_then(RecId::from_real);
        let currency = id_of(currencies.get(i), "currency_id")?.and_then(RecId::from_real);
        let (Some(company), Some(currency)) = (company, currency) else {
            return Err(Error::invariant(format!(
                "purchase order {id} has no company or currency"
            )));
        };
        let deadline = match deadlines.get(i) {
            Some(Cell::DateTime(at)) => Some(*at),
            _ => None,
        };
        let total_cc = in_company_currency(tx, &sudo, total, currency, company, deadline).await?;
        sudo.records("purchase.order")?
            .browse([*id])?
            .write_base(
                tx,
                RawVals::new()
                    .set_cell("amount_untaxed", Cell::Decimal(untaxed))
                    .set_cell("amount_tax", Cell::Decimal(tax))
                    .set_cell("amount_total", Cell::Decimal(total))
                    .set_cell("amount_total_cc", Cell::Decimal(total_cc))
                    .set_cell("date_planned", earliest.map_or(Cell::Null, Cell::DateTime)),
            )
            .await?;
    }
    Ok(())
}

/// `total` in `currency` expressed in the currency of `company` at the deadline's date (today
/// without one), with the rates of that company (BUG-132).
pub(crate) async fn in_company_currency(
    tx: &mut Transaction,
    env: &Env,
    total: Decimal,
    currency: RecId,
    company: RecId,
    deadline: Option<DateTime>,
) -> Result<Decimal> {
    let company_currency = id_of(
        Some(
            &env.records("res.company")?
                .browse([company.get()])?
                .cell(tx, "currency_id")
                .await?,
        ),
        "currency_id",
    )?
    .and_then(RecId::from_real)
    .ok_or_else(|| Error::invariant(format!("company {company:?} has no currency")))?;
    if company_currency == currency {
        return Ok(total);
    }
    // the UTC day of the deadline (or the company's business date without one), never the host clock
    let date = match deadline {
        Some(at) => at.date(),
        None => erp_addon_base::company_calendar::business_date(tx, company.get()).await?,
    };
    convert(env, tx, total, currency, company_currency, company, date).await
}

/// The lines of `orders` priced again in their (changed) currency, deadline or company.
pub(crate) async fn resync_orders(
    tx: &mut Transaction,
    env: &Env,
    orders: &BTreeSet<i64>,
) -> Result<()> {
    let ids: Vec<i64> = orders.iter().copied().collect();
    let lines = env
        .sudo(true)
        .records("purchase.order.line")?
        .search(tx, &Domain::condition("order_id", Operator::In, ids)?)
        .await?;
    if lines.is_empty() {
        return sync_orders(tx, env, orders).await;
    }
    sync_lines(tx, env, &lines).await
}

/// The orders of `orders` that have a line without a product, by number.
pub(crate) async fn orders_without_product(
    tx: &mut Transaction,
    env: &Env,
    orders: &[i64],
) -> Result<BTreeSet<i64>> {
    let lines = env
        .sudo(true)
        .records("purchase.order.line")?
        .search(
            tx,
            &Domain::and([
                Domain::condition("order_id", Operator::In, orders.to_vec())?,
                Domain::condition("product_id", Operator::Eq, false)?,
            ]),
        )
        .await?;
    Ok(line_orders(tx, &lines).await?.into_values().collect())
}
