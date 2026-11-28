// SPDX-License-Identifier: MIT OR Apache-2.0
//! Direct, one-step receipts of approved purchase orders.

use std::collections::{BTreeMap, BTreeSet};

use erp_addon_purchase::order::PurchaseOrder;
use erp_addon_stock::StockPicking;
use erp_sdk::prelude::*;
use erp_sdk::{Env, ErrorKind, FieldOp, Operation};

#[model(
    name = "purchase.stock.receipt.link",
    description = "Direct Purchase Receipt Link",
    rec_name = "order_id",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    constraints = [
        ("_order", "UNIQUE (order_id)", "A purchase order has one direct receipt."),
        ("_picking", "UNIQUE (picking_id)", "A direct receipt belongs to one purchase order.")
    ],
)]
struct PurchaseReceiptLink {
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        ondelete = "restrict",
        index = true
    )]
    company_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "purchase.order",
        required = true,
        ondelete = "restrict"
    )]
    order_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "stock.picking",
        required = true,
        ondelete = "restrict"
    )]
    picking_id: Option<RecId>,
}

#[model(
    name = "purchase.stock.receipt.move.link",
    description = "Direct Purchase Receipt Move Link",
    rec_name = "purchase_line_id",
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    constraints = [
        ("_move", "UNIQUE (move_id)", "A receipt move has one purchase line.")
    ],
)]
struct PurchaseReceiptMoveLink {
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        ondelete = "restrict",
        index = true
    )]
    company_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "stock.move",
        required = true,
        ondelete = "restrict"
    )]
    move_id: Option<RecId>,
    #[field(
        many2one,
        comodel = "purchase.order.line",
        required = true,
        ondelete = "restrict",
        index = true
    )]
    purchase_line_id: Option<RecId>,
}

struct OrderDemand {
    order: i64,
    company: i64,
    vendor: i64,
    reference: String,
    lines: Vec<LineDemand>,
}

struct LineDemand {
    line: i64,
    product: i64,
    unit: Option<i64>,
    quantity: Decimal,
    scheduled: DateTime,
}

struct ReceiptPlan {
    order: OrderDemand,
    operation: i64,
    source: i64,
    destination: i64,
}

fn decode(error: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::db("The purchase receipt configuration could not be read.").with_source(error)
}

fn company_scope(env: &Env, company: i64) -> Env {
    let mut context = env.context().clone();
    let keys: Vec<String> = context
        .default_field_names()
        .map(|k| format!("default_{k}"))
        .collect();
    for k in keys {
        context.remove(&k);
    }
    context.set("allowed_company_ids", Value::list([Value::Int(company)]));
    env.with_context_replaced(context)
}

const DEMAND_ACCESS: &str = "Purchase receipt demand is not accessible.";

fn redact_demand(error: Error) -> Error {
    if matches!(error.kind(), ErrorKind::Access | ErrorKind::Missing) {
        return Error::access(DEMAND_ACCESS);
    }
    error
}

async fn authorize_fields(tx: &mut Transaction, records: &Records, fields: &[&str]) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    records
        .check_access(tx, Operation::Read)
        .await
        .map_err(redact_demand)?;
    for field in fields {
        if !tx
            .has_field_access(records.env(), records.spec().name, field, FieldOp::Read)
            .await?
        {
            return Err(Error::access(DEMAND_ACCESS));
        }
    }
    Ok(())
}

async fn authorize_demand(tx: &mut Transaction, orders: &Records) -> Result<()> {
    if orders.is_empty() {
        return Ok(());
    }
    orders.check_access(tx, Operation::Write).await?;
    authorize_fields(
        tx,
        orders,
        &["name", "company_id", "partner_id", "date_order"],
    )
    .await?;
    tx.flush().await?;
    // Enumerate the complete membership first. A caller-filtered search would drop protected lines.
    let memberships = tx
        .query(
            "SELECT id FROM purchase_order_line WHERE order_id = ANY($1) ORDER BY id",
            &[&orders.ids()],
        )
        .await?;
    let lines: Vec<i64> = memberships
        .iter()
        .map(|row| row.try_get(0))
        .collect::<std::result::Result<_, _>>()
        .map_err(decode)?;
    let env = orders.env();
    authorize_fields(
        tx,
        &env.records("purchase.order.line")?
            .browse(lines.iter().copied())?,
        &[
            "order_id",
            "company_id",
            "product_id",
            "uom_id",
            "product_qty",
            "date_planned",
            "sequence",
        ],
    )
    .await?;
    let source_refs = tx.query(
        "SELECT array_agg(DISTINCT l.product_id) FILTER (WHERE l.product_id IS NOT NULL),                 array_agg(DISTINCT l.uom_id) FILTER (WHERE l.uom_id IS NOT NULL)          FROM purchase_order_line l WHERE l.id = ANY($1)", &[&lines],
    ).await?;
    let refs = source_refs.first().ok_or_else(|| {
        Error::invariant("Purchase receipt reference lookup returned no summary.")
    })?;
    let products: Option<Vec<i64>> = refs.try_get(0).map_err(decode)?;
    let units: Option<Vec<i64>> = refs.try_get(1).map_err(decode)?;
    let products = products.unwrap_or_default();
    authorize_fields(
        tx,
        &env.records("product.product")?
            .browse(products.iter().copied())?,
        &["type", "uom_id"],
    )
    .await?;
    let fallback_rows = tx.query(
        "SELECT DISTINCT t.uom_id FROM purchase_order_line l          JOIN product_product p ON p.id = l.product_id JOIN product_template t ON t.id = p.product_tmpl_id          WHERE l.id = ANY($1) AND l.uom_id IS NULL AND t.uom_id IS NOT NULL",
        &[&lines],
    ).await?;
    let mut units = units.unwrap_or_default();
    for row in fallback_rows {
        units.push(row.try_get(0).map_err(decode)?);
    }
    units.sort_unstable();
    units.dedup();
    authorize_fields(tx, &env.records("uom.uom")?.browse(units)?, &[]).await?;
    let vendors = tx
        .query(
            "SELECT DISTINCT partner_id FROM purchase_order WHERE id = ANY($1)",
            &[&orders.ids()],
        )
        .await?;
    let vendors: Vec<i64> = vendors
        .iter()
        .map(|row| row.try_get(0))
        .collect::<std::result::Result<_, _>>()
        .map_err(decode)?;
    authorize_fields(
        tx,
        &env.records("res.partner")?.browse(vendors)?,
        &["property_stock_supplier"],
    )
    .await
}

async fn demands(tx: &mut Transaction, orders: &[i64]) -> Result<Vec<OrderDemand>> {
    let mut result = BTreeMap::new();
    let rows = tx
        .query(
            "SELECT o.id, o.company_id, o.partner_id, o.name, l.id, l.product_id, l.uom_id, \
                    l.product_qty, COALESCE(l.date_planned, o.date_order), t.type \
             FROM purchase_order o \
             JOIN purchase_order_line l ON l.order_id = o.id AND l.company_id = o.company_id \
             LEFT JOIN product_product p ON p.id = l.product_id \
             LEFT JOIN product_template t ON t.id = p.product_tmpl_id \
             WHERE o.id = ANY($1) \
             ORDER BY o.company_id, o.id, l.sequence, l.id",
            &[&orders],
        )
        .await?;
    for row in rows {
        let order: i64 = row.try_get(0).map_err(decode)?;
        let kind: Option<&str> = row.try_get(9).map_err(decode)?;
        match kind {
            Some("service") => continue,
            Some("consu") => {}
            None => {
                let reference: &str = row.try_get(3).map_err(decode)?;
                return Err(Error::user(format!(
                    "Purchase order {reference} has a line without a product."
                )));
            }
            Some(_) => {
                return Err(Error::user(
                    "Receipts of bundled products require a later release.",
                ));
            }
        }
        let quantity: Decimal = row.try_get(7).map_err(decode)?;
        if quantity == Decimal::ZERO {
            continue;
        }
        if quantity < Decimal::ZERO {
            return Err(Error::user(
                "Negative purchase quantities require the purchase return workflow.",
            ));
        }
        let demand = LineDemand {
            line: row.try_get(4).map_err(decode)?,
            product: row.try_get(5).map_err(decode)?,
            unit: row.try_get(6).map_err(decode)?,
            quantity,
            scheduled: row.try_get(8).map_err(decode)?,
        };
        match result.entry(order) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(OrderDemand {
                    order,
                    company: row.try_get(1).map_err(decode)?,
                    vendor: row.try_get(2).map_err(decode)?,
                    reference: row.try_get(3).map_err(decode)?,
                    lines: vec![demand],
                });
            }
            std::collections::btree_map::Entry::Occupied(mut slot) => {
                slot.get_mut().lines.push(demand)
            }
        }
    }
    let mut demands: Vec<OrderDemand> = result.into_values().collect();
    demands.sort_unstable_by_key(|demand| (demand.company, demand.order));
    Ok(demands)
}

async fn prepare(tx: &mut Transaction, orders: &Records) -> Result<Vec<ReceiptPlan>> {
    let mut plans = Vec::new();
    for order in demands(tx, &orders.ids()).await? {
        if !tx
            .query(
                "SELECT 1 FROM purchase_stock_receipt_link WHERE order_id = $1",
                &[&order.order],
            )
            .await?
            .is_empty()
        {
            return Err(Error::user(format!(
                "Purchase order {} already has a receipt. Replacing that receipt requires a later release.",
                order.reference
            )));
        }

        let warehouses = tx
            .query(
                "SELECT w.reception_steps, w.lot_stock_id, t.id, t.default_location_src_id, \
                    t.default_location_dest_id, t.code, t.active \
             FROM stock_warehouse w \
             LEFT JOIN stock_picking_type t ON t.id = w.in_type_id AND t.company_id = w.company_id \
             WHERE w.company_id = $1 AND w.active ORDER BY w.id LIMIT 2",
                &[&order.company],
            )
            .await?;
        let [warehouse] = warehouses.as_slice() else {
            return Err(Error::user(format!(
                "Purchase order {} needs exactly one active warehouse for direct receiving.",
                order.reference
            )));
        };
        let steps: &str = warehouse.try_get(0).map_err(decode)?;
        let stock: Option<i64> = warehouse.try_get(1).map_err(decode)?;
        let operation: Option<i64> = warehouse.try_get(2).map_err(decode)?;
        let source: Option<i64> = warehouse.try_get(3).map_err(decode)?;
        let destination: Option<i64> = warehouse.try_get(4).map_err(decode)?;
        let kind: Option<&str> = warehouse.try_get(5).map_err(decode)?;
        let active: Option<bool> = warehouse.try_get(6).map_err(decode)?;
        let (Some(operation), Some(source), Some(destination)) = (operation, source, destination)
        else {
            return Err(Error::user(format!(
                "Purchase order {} needs an incoming operation with source and destination locations.",
                order.reference
            )));
        };
        if steps != "one_step"
            || kind != Some("incoming")
            || active != Some(true)
            || stock != Some(destination)
        {
            return Err(Error::user(format!(
                "Purchase order {} needs the warehouse's active one-step incoming operation. Other receipt routes require a later release.",
                order.reference
            )));
        }
        let scope = company_scope(orders.env(), order.company);
        let vendor = scope.records("res.partner")?.browse([order.vendor])?;
        let source = match vendor.cell(tx, "property_stock_supplier").await? {
            Cell::Id(id) => id,
            Cell::Null | Cell::Bool(false) => source,
            _ => {
                return Err(Error::invariant(
                    "The vendor stock location is not a record identity.",
                ));
            }
        };
        let location = tx
            .query(
                "SELECT 1 FROM stock_location source CROSS JOIN stock_location destination \
             WHERE source.id = $1 AND source.active AND source.usage = 'supplier' \
               AND (source.company_id IS NULL OR source.company_id = $3) \
               AND destination.id = $2 AND destination.active AND destination.usage = 'internal' \
               AND destination.company_id = $3",
                &[&source, &destination, &order.company],
            )
            .await?;
        if location.is_empty() {
            return Err(Error::user(format!(
                "Purchase order {} needs a supplier source and an internal destination for its company.",
                order.reference
            )));
        }
        plans.push(ReceiptPlan {
            order,
            operation,
            source,
            destination,
        });
    }
    Ok(plans)
}

impl ReceiptPlan {
    async fn create(self, tx: &mut Transaction, env: &Env) -> Result<()> {
        let company = self.order.company;
        let env = company_scope(env, company);
        let shared = RawVals::new()
            .set_cell("company_id", Cell::Id(company))
            .set_cell("picking_type_id", Cell::Id(self.operation))
            .set_cell("location_id", Cell::Id(self.source))
            .set_cell("location_dest_id", Cell::Id(self.destination))
            .set_cell("origin", Cell::Str(self.order.reference.into()));
        let scheduled = self
            .order
            .lines
            .iter()
            .map(|line| line.scheduled)
            .min()
            .ok_or_else(|| Error::invariant("A direct purchase receipt has no planned line."))?;
        let picking = env
            .records("stock.picking")?
            .create(
                tx,
                vec![
                    shared
                        .clone()
                        .set_cell("partner_id", Cell::Id(self.order.vendor))
                        .set_cell("scheduled_date", Cell::DateTime(scheduled)),
                ],
            )
            .await?;
        let picking_id = match picking.ids().as_slice() {
            [id] => *id,
            _ => {
                return Err(Error::invariant(
                    "A purchase approval must create one direct receipt.",
                ));
            }
        };
        let lines: Vec<i64> = self.order.lines.iter().map(|line| line.line).collect();
        let vals = self
            .order
            .lines
            .into_iter()
            .map(|line| {
                let vals = shared
                    .clone()
                    .set_cell("picking_id", Cell::Id(picking_id))
                    .set_cell("product_id", Cell::Id(line.product))
                    .set_cell("product_uom_qty", Cell::Decimal(line.quantity))
                    .set_cell("date", Cell::DateTime(line.scheduled));
                match line.unit {
                    Some(unit) => vals.set_cell("uom_id", Cell::Id(unit)),
                    None => vals,
                }
            })
            .collect();
        let moves = env.records("stock.move")?.create(tx, vals).await?;
        let move_ids = moves.ids();
        if lines.len() != move_ids.len() {
            return Err(Error::invariant(
                "Receipt move creation did not preserve the purchase lines.",
            ));
        }
        let () = picking.call(tx, "action_confirm", ()).await?;
        tx.execute(
            "INSERT INTO purchase_stock_receipt_link (company_id, order_id, picking_id) VALUES ($1, $2, $3)",
            &[&company, &self.order.order, &picking_id],
        ).await?;
        tx.execute(
            "INSERT INTO purchase_stock_receipt_move_link (company_id, move_id, purchase_line_id) \
             SELECT $1, move_id, line_id FROM unnest($2::int8[], $3::int8[]) AS link(move_id, line_id)",
            &[&company, &move_ids, &lines],
        ).await?;
        tx.invalidate_records(
            &picking,
            Some(&["purchase_order_id", "purchase_order_count"]),
        )
        .await?;
        crate::received_quantity::refresh(tx, &env.records("purchase.order.line")?.browse(lines)?)
            .await?;
        let order = env.records("purchase.order")?.browse([self.order.order])?;
        tx.invalidate_records(&order, Some(&["direct_receipt_id", "receipt_count"]))
            .await
    }
}

#[extend(PurchaseOrder, addon = "purchase_stock")]
struct PurchaseOrderReceipt {
    /// The original direct transfer visible to the order reader.
    #[field(
        many2one,
        comodel = "stock.picking",
        compute = "_compute_direct_receipt_id",
        readonly = true,
        copy = false,
        string = "Receipt"
    )]
    pub direct_receipt_id: Option<RecId>,
    /// The immutable origin of a direct incoming transfer.
    #[field(
        one2many,
        comodel = "purchase.stock.receipt.link",
        inverse_name = "order_id",
        readonly = true,
        copy = false
    )]
    pub direct_receipt_link_ids: Vec<RecId>,
}

#[methods(PurchaseOrder, addon = "purchase_stock")]
impl PurchaseOrderReceipt {
    #[compute(depends = ["state", "company_id"], depends_context = ["uid", "allowed_company_ids"])]
    async fn _compute_direct_receipt_id(
        this: &RecordSet<PurchaseOrder>,
        tx: &mut Transaction,
        out: &mut Assign<'_, PurchaseOrder>,
    ) -> Result<()> {
        let links = readable_direct_documents(tx, this.as_records(), DirectLink::FromOrder).await?;
        for (index, link) in links.into_iter().enumerate() {
            out.set(index, PurchaseOrderReceipt::DIRECT_RECEIPT_ID, link)?;
        }
        Ok(())
    }

    /// Authorize the entire order before the base confirmation validates its lines.
    #[method(override)]
    #[rpc(effect = write, assistant = offer)]
    async fn button_confirm(
        this: &RecordSet<PurchaseOrder>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        let orders = this.as_records();
        tx.savepoint(async move |tx| {
            authorize_demand(tx, orders).await?;
            sup.call(tx, orders, ()).await
        })
        .await
    }

    /// Authorize the entire order before the base approval evaluates demand.
    #[method(override)]
    async fn button_approve(
        this: &RecordSet<PurchaseOrder>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        let orders = this.as_records();
        tx.savepoint(async move |tx| {
            authorize_demand(tx, orders).await?;
            sup.call(tx, orders, ()).await
        })
        .await
    }

    /// A single-step confirmation and a separately approved order use the same receipt hook.
    #[method(override)]
    async fn _approve(
        this: &RecordSet<PurchaseOrder>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        let orders = this.as_records().clone();
        orders.check_access(tx, Operation::Write).await?;
        tx.savepoint(async move |tx| {
            authorize_demand(tx, &orders).await?;
            let plans = prepare(tx, &orders).await?;
            let () = sup.call(tx, &orders, ()).await?;
            for plan in plans {
                plan.create(tx, orders.env()).await?;
            }
            Ok(())
        })
        .await
    }

    /// Cancel the open direct receipt as part of cancelling the associated purchase order. A done
    /// receipt refuses the cancel by name: stricter than the reference, which cancels the order
    /// and leaves the received goods on a done receipt of a cancelled order.
    #[method(override)]
    async fn _action_cancel(
        this: &RecordSet<PurchaseOrder>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        let orders = this.as_records().clone();
        orders.check_access(tx, Operation::Write).await?;
        tx.savepoint(async move |tx| {
            let rows = tx.query(
                "SELECT l.company_id, l.picking_id, p.state, \
                        EXISTS(SELECT 1 FROM stock_picking child WHERE child.backorder_id = p.id) \
                 FROM purchase_stock_receipt_link l JOIN stock_picking p ON p.id = l.picking_id \
                 WHERE l.order_id = ANY($1) ORDER BY l.company_id, l.picking_id",
                &[&orders.ids()],
            ).await?;
            for row in rows {
                let has_backorder: bool = row.try_get(3).map_err(decode)?;
                if has_backorder {
                    return Err(Error::user("Cancelling a purchase order with a receipt backorder requires a later release."));
                }
                let state: &str = row.try_get(2).map_err(decode)?;
                if state == "done" {
                    let picking: i64 = row.try_get(1).map_err(decode)?;
                    return Err(Error::user(done_receipt(tx, orders.env(), picking).await?));
                }
                if state == "cancel" {
                    continue;
                }
                let company: i64 = row.try_get(0).map_err(decode)?;
                let picking: i64 = row.try_get(1).map_err(decode)?;
                let receipt = company_scope(orders.env(), company).records("stock.picking")?.browse([picking])?;
                let () = receipt.call(tx, "action_cancel", ()).await?;
            }
            sup.call(tx, &orders, ()).await
        })
        .await
    }

    /// The orders whose cancel step refuses: those of the base and those with a done receipt or
    /// a receipt backorder.
    #[method(override)]
    async fn _cancel_blocked(
        this: &RecordSet<PurchaseOrder>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<Records> {
        let orders = this.as_records();
        let base: Records = sup.call(tx, orders, ()).await?;
        tx.flush().await?;
        let mut blocked: BTreeSet<i64> = base.ids().into_iter().collect();
        for row in tx
            .query(
                "SELECT DISTINCT l.order_id::int8 FROM purchase_stock_receipt_link l \
                 JOIN stock_picking p ON p.id = l.picking_id \
                 WHERE l.order_id = ANY($1) AND (p.state = 'done' \
                     OR EXISTS (SELECT 1 FROM stock_picking c WHERE c.backorder_id = p.id))",
                &[&orders.ids()],
            )
            .await?
        {
            blocked.insert(row.try_get::<_, i64>(0).map_err(decode)?);
        }
        orders.browse(blocked)
    }
}

/// The refusal of cancelling an order whose receipt `picking` is done, naming the order and,
/// when the caller may read it, the receipt.
async fn done_receipt(tx: &mut Transaction, env: &Env, picking: i64) -> Result<String> {
    let rows = tx
        .query(
            "SELECT o.name, p.name FROM purchase_stock_receipt_link l \
             JOIN purchase_order o ON o.id = l.order_id JOIN stock_picking p ON p.id = l.picking_id \
             WHERE l.picking_id = $1 ORDER BY o.id LIMIT 1",
            &[&picking],
        )
        .await?;
    let row = rows
        .first()
        .ok_or_else(|| Error::invariant(format!("receipt {picking} has no purchase order link")))?;
    let order: String = row.try_get(0).map_err(decode)?;
    let receipt: Option<String> = row.try_get(1).map_err(decode)?;
    let readable = !env
        .records("stock.picking")?
        .browse([picking])?
        .filtered_access(tx, Operation::Read)
        .await?
        .is_empty();
    let named = match receipt {
        Some(receipt) if readable => format!(" {receipt}"),
        _ => String::new(),
    };
    Ok(format!(
        "The purchase order {order} cannot be cancelled: its receipt{named} is done. Return the goods instead; the order keeps what was received."
    ))
}

/// The source order of the original transfer, subject to the reader's order access.
#[extend(StockPicking, addon = "purchase_stock")]
struct PurchasePickingOrigin {
    #[field(
        many2one,
        comodel = "purchase.order",
        compute = "_compute_purchase_order_id",
        readonly = true,
        copy = false,
        string = "Purchase Order"
    )]
    pub purchase_order_id: Option<RecId>,
}

#[methods(StockPicking, addon = "purchase_stock")]
impl PurchasePickingOrigin {
    #[compute(depends = ["company_id"], depends_context = ["uid", "allowed_company_ids"])]
    async fn _compute_purchase_order_id(
        this: &RecordSet<StockPicking>,
        tx: &mut Transaction,
        out: &mut Assign<'_, StockPicking>,
    ) -> Result<()> {
        let links =
            readable_direct_documents(tx, this.as_records(), DirectLink::FromPicking).await?;
        for (index, link) in links.into_iter().enumerate() {
            out.set(index, PurchasePickingOrigin::PURCHASE_ORDER_ID, link)?;
        }
        Ok(())
    }
}

enum DirectLink {
    FromOrder,
    FromPicking,
}

/// Resolve one immutable document link per input while preserving caller access on both ends.
async fn readable_direct_documents(
    tx: &mut Transaction,
    records: &Records,
    direction: DirectLink,
) -> Result<Vec<Option<RecId>>> {
    let sources = records.origin();
    sources.check_access(tx, Operation::Read).await?;
    let ids = sources.ids();
    let empty = Vec::<i64>::new();
    let (order_ids, picking_ids, target) = match direction {
        DirectLink::FromOrder => (&ids, &empty, "stock.picking"),
        DirectLink::FromPicking => (&empty, &ids, "purchase.order"),
    };
    let mut links = BTreeMap::new();
    if !ids.is_empty() {
        let rows = tx
            .query(
                "SELECT l.order_id, l.picking_id FROM purchase_stock_receipt_link l \
             JOIN purchase_order o ON o.id=l.order_id AND o.company_id=l.company_id \
             JOIN stock_picking p ON p.id=l.picking_id AND p.company_id=l.company_id \
             WHERE l.order_id=ANY($1) OR l.picking_id=ANY($2)
             UNION
             SELECT l.order_id, m.picking_id FROM stock_move m
             JOIN stock_move root ON root.id=m.backorder_root_move_id AND root.company_id=m.company_id
             JOIN purchase_stock_receipt_link l ON l.picking_id=root.picking_id AND l.company_id=m.company_id
             WHERE m.picking_id=ANY($2)",
                &[order_ids, picking_ids],
            )
            .await?;
        for row in rows {
            let order: i64 = row.try_get(0).map_err(decode)?;
            let picking: i64 = row.try_get(1).map_err(decode)?;
            match direction {
                DirectLink::FromOrder => links.insert(order, picking),
                DirectLink::FromPicking => links.insert(picking, order),
            };
        }
    }
    let allowed = records
        .env()
        .records(target)?
        .browse(links.values().copied())?
        .filtered_access(tx, Operation::Read)
        .await?;
    let visible: std::collections::BTreeSet<_> = allowed.ids().into_iter().collect();
    Ok(records
        .rec_ids()
        .iter()
        .map(|id| {
            id.as_real()
                .or(id.origin())
                .and_then(|id| links.get(&id.get()))
                .filter(|id| visible.contains(*id))
                .and_then(|id| RecId::from_real(*id))
        })
        .collect())
}
