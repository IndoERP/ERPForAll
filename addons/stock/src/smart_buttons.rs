// SPDX-License-Identifier: MIT OR Apache-2.0
//! Cross-document smart buttons owned by `stock`:
//! - `stock.picking`: Returns (`action_view_returns`), Backorders (`backorder_count` / `action_view_backorders`)
//! - `product.template`: On Hand (`qty_on_hand` / `action_open_quants`), Stock Moves (`stock_move_count` / `action_open_stock_moves`), Reordering Rules (`reordering_rule_count` / `action_open_orderpoints`)
//! - `stock.warehouse`: Operation Types (`picking_type_count` / `action_view_picking_types`), Transfers (`picking_count` / `action_view_pickings`)
//! - `stock.location`: Current Stock (`quant_count` / `action_view_quants`), Transfers (`picking_count` / `action_view_pickings`)
//! - `res.partner`: Transfers (`picking_count` / `action_view_pickings`)
//!
//! Counts and sums are one grouped read per button whose search predicate carries the caller's
//! read access, so hidden records are never counted. An action opens the one readable record in
//! its form, or more by a domain on the relation field, never a list of ids.

use std::collections::BTreeMap;
use std::sync::Arc;

use erp_addon_base::ResPartner;
use erp_addon_product::ProductTemplate;
use erp_sdk::prelude::*;
use erp_sdk::{ErrorKind, Operation, ValueMap};

use crate::location::StockLocation;
use crate::picking::StockPicking;
use crate::warehouse::StockWarehouse;

/// The quants that count as on hand: those in internal locations.
const INTERNAL: (&str, &str) = ("location_id.usage", "internal");

fn row_of(id: &RecId) -> Option<i64> {
    id.as_real().or(id.origin()).map(|id| id.get())
}

fn source_ids(sources: &Records) -> Vec<i64> {
    sources.rec_ids().iter().filter_map(row_of).collect()
}

/// The groups of the `model` records matching `domain` by `groupby`, with `aggregate`: the
/// group keys as ids, then the aggregate. A caller refused read on `model` sees none of its
/// records, as `Records::filtered_access` treats a model-level refusal.
async fn grouped(
    tx: &mut Transaction,
    sources: &Records,
    model: &str,
    domain: Domain,
    groupby: &[&str],
    aggregate: &str,
) -> Result<Vec<(Vec<Option<i64>>, GroupValue)>> {
    let read = sources
        .env()
        .records(model)?
        ._read_group(
            tx,
            ReadGroup::new(domain)
                .groupby(groupby.iter().copied())
                .aggregates([aggregate]),
        )
        .await;
    let groups = match read {
        Ok(groups) => groups,
        Err(error) if error.kind() == ErrorKind::Access => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut out = Vec::new();
    for mut row in groups.rows() {
        let value = row
            .pop()
            .ok_or_else(|| Error::internal("A smart button group has no aggregate."))?;
        let keys = row
            .iter()
            .map(|key| key.record_id().map(|id| id.get()))
            .collect();
        out.push((keys, value));
    }
    Ok(out)
}

fn count_of(value: &GroupValue) -> Result<i64> {
    match value {
        GroupValue::Int(count) => Ok(*count),
        _ => Err(Error::internal("A smart button count is not an integer.")),
    }
}

fn per_source(sources: &Records, found: &BTreeMap<i64, i64>) -> Result<Vec<i32>> {
    sources
        .rec_ids()
        .iter()
        .map(|id| {
            let n = row_of(id)
                .and_then(|id| found.get(&id))
                .copied()
                .unwrap_or(0);
            i32::try_from(n).map_err(|_| Error::programming("record count does not fit an i32"))
        })
        .collect()
}

/// Per source record, in record order, how many readable `model` records have the source in
/// `link` (a many2one or a path ending in one) and match `extra`.
async fn counts(
    tx: &mut Transaction,
    sources: &Records,
    model: &str,
    link: &str,
    extra: &[(&str, &str)],
) -> Result<Vec<i32>> {
    let rows = grouped(
        tx,
        sources,
        model,
        linked(link, source_ids(sources), extra)?,
        &[link],
        "__count",
    )
    .await?;
    let mut found = BTreeMap::new();
    for (keys, value) in &rows {
        if let Some(Some(source)) = keys.first() {
            *found.entry(*source).or_insert(0) += count_of(value)?;
        }
    }
    per_source(sources, &found)
}

/// `link in ids` and every `extra` condition.
fn linked(link: &str, ids: Vec<i64>, extra: &[(&str, &str)]) -> Result<Domain> {
    let mut parts = vec![Domain::field(link)?.in_(ids)];
    for (field, value) in extra {
        parts.push(Domain::field(field)?.eq(*value));
    }
    Ok(Domain::and(parts))
}

fn leaf(field: &str, op: &str, value: Value) -> Value {
    Value::list([Value::str(field), Value::str(op), value])
}

/// The one record of a button's action, after the caller's read check.
async fn the_one(this: &Records, tx: &mut Transaction) -> Result<i64> {
    this.check_access(tx, Operation::Read).await?;
    this.ensure_one()?;
    this.rec_ids()
        .first()
        .and_then(row_of)
        .ok_or_else(|| Error::validation("Save the record before opening its linked documents."))
}

/// A current-window action over the `model` records matching `domain` (a client domain list):
/// exactly one the caller may read opens straight in its form; none or more open the list by the
/// domain, never by an id list.
async fn window(
    tx: &mut Transaction,
    sources: &Records,
    name: &str,
    model: &str,
    domain: Vec<Value>,
) -> Result<Value> {
    let filter = erp_sdk::web::domain_from_value(&Value::list(domain.clone()))?;
    let found = sources
        .env()
        .records(model)?
        .search_with(tx, &filter, &erp_sdk::SearchOptions::new().limit(2))
        .await;
    let ids = match found {
        Ok(found) => found.ids(),
        // a caller refused read on the model gets the list, which says so
        Err(error) if error.kind() == ErrorKind::Access => Vec::new(),
        Err(error) => return Err(error),
    };
    let (views, target) = if let [id] = ids.as_slice() {
        (
            Value::list([Value::list([Value::False, Value::str("form")])]),
            ("res_id".to_owned(), Value::Int(*id)),
        )
    } else {
        (
            Value::list([
                Value::list([Value::False, Value::str("list")]),
                Value::list([Value::False, Value::str("form")]),
            ]),
            ("domain".to_owned(), Value::list(domain)),
        )
    };
    let context = ValueMap::from([("create".to_owned(), Value::Bool(false))]);
    Ok(Value::Map(Arc::new(ValueMap::from([
        ("type".to_owned(), Value::str("ir.actions.act_window")),
        ("name".to_owned(), Value::str(name)),
        ("res_model".to_owned(), Value::str(model)),
        ("target".to_owned(), Value::str("current")),
        ("views".to_owned(), views),
        target,
        ("context".to_owned(), Value::Map(Arc::new(context))),
    ]))))
}

/// `stock.picking` smart buttons for returns and backorders.
#[methods(StockPicking, addon = "stock")]
impl StockPicking {
    #[compute(depends = ["state", "company_id"], depends_context = ["uid", "allowed_company_ids"])]
    async fn _compute_backorder_count(
        this: &RecordSet<StockPicking>,
        tx: &mut Transaction,
        out: &mut Assign<'_, StockPicking>,
    ) -> Result<()> {
        let records = this.as_records();
        let found = counts(tx, records, "stock.picking", "backorder_id", &[]).await?;
        for (index, count) in found.into_iter().enumerate() {
            out.set(index, StockPicking::BACKORDER_COUNT, count)?;
        }
        Ok(())
    }

    /// Open the backorder transfers split from this transfer.
    #[method]
    #[rpc(effect = read)]
    async fn action_view_backorders(
        this: &RecordSet<StockPicking>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let id = the_one(this.as_records(), tx).await?;
        window(
            tx,
            this.as_records(),
            "Backorders",
            "stock.picking",
            vec![leaf("backorder_id", "=", Value::Int(id))],
        )
        .await
    }

    /// Open the return transfers created from this transfer.
    #[method]
    #[rpc(effect = read)]
    async fn action_view_returns(
        this: &RecordSet<StockPicking>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let id = the_one(this.as_records(), tx).await?;
        window(
            tx,
            this.as_records(),
            "Returns",
            "stock.picking",
            vec![leaf("return_id", "=", Value::Int(id))],
        )
        .await
    }
}

/// `product.template` inventory smart buttons: On Hand, Forecasted, Moves, Reordering Rules.
#[extend(ProductTemplate, addon = "stock")]
pub struct ProductTemplateStockButtons {
    /// On-hand quantity summed across readable internal stock quants of this product's variants.
    #[field(
        float,
        digits = "Product Unit",
        compute = "_compute_stock_counters",
        string = "On Hand"
    )]
    pub qty_on_hand: Decimal,
    /// Open incoming quantity into internal locations across this product's variants.
    #[field(
        float,
        digits = "Product Unit",
        compute = "_compute_stock_counters",
        string = "Incoming"
    )]
    pub qty_incoming: Decimal,
    /// Open outgoing quantity leaving internal locations across this product's variants.
    #[field(
        float,
        digits = "Product Unit",
        compute = "_compute_stock_counters",
        string = "Outgoing"
    )]
    pub qty_outgoing: Decimal,
    /// Forecasted stock (`qty_on_hand + qty_incoming - qty_outgoing`).
    #[field(
        float,
        digits = "Product Unit",
        compute = "_compute_stock_counters",
        string = "Forecasted"
    )]
    pub virtual_available: Decimal,
    /// Inventory routes assigned specifically to this product (e.g. Replenish on Order / MTO).
    #[field(
        many2many,
        comodel = "stock.route",
        relation = "stock_route_product",
        column1 = "product_id",
        column2 = "route_id",
        groups = "stock.group_adv_location",
        string = "Routes"
    )]
    pub route_ids: Vec<RecId>,
    /// Readable stock moves touching this product's variants.
    #[field(integer, compute = "_compute_stock_counters", string = "Stock Moves")]
    pub stock_move_count: i32,
    /// Readable reordering rules defined for this product's variants.
    #[field(
        integer,
        compute = "_compute_stock_counters",
        string = "Reordering Rules"
    )]
    pub reordering_rule_count: i32,
}

/// The path from a variant-level record to its product template.
const TEMPLATE: &str = "product_id.product_tmpl_id";

#[methods(ProductTemplate, addon = "stock")]
impl ProductTemplateStockButtons {
    #[compute(depends = ["company_id", "is_storable"], depends_context = ["uid", "allowed_company_ids"])]
    async fn _compute_stock_counters(
        this: &RecordSet<ProductTemplate>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ProductTemplate>,
    ) -> Result<()> {
        let records = this.as_records();
        let tmpl_ids = source_ids(records);
        let sums = grouped(
            tx,
            records,
            "stock.quant",
            linked(TEMPLATE, tmpl_ids.clone(), &[INTERNAL])?,
            &[TEMPLATE],
            "quantity:sum",
        )
        .await?;
        let mut on_hand: BTreeMap<i64, Decimal> = BTreeMap::new();
        for (keys, value) in &sums {
            let GroupValue::Decimal(quantity) = value else {
                return Err(Error::internal(
                    "The on-hand quantity sum is not a decimal.",
                ));
            };
            if let Some(Some(template)) = keys.first() {
                *on_hand.entry(*template).or_insert(Decimal::ZERO) += *quantity;
            }
        }
        let mut incoming: BTreeMap<i64, Decimal> = BTreeMap::new();
        let mut outgoing: BTreeMap<i64, Decimal> = BTreeMap::new();
        if !tmpl_ids.is_empty() {
            tx.flush().await?;
            let env = records.env();
            let is_su = env.su();
            let allowed_companies: Vec<i64> = if is_su {
                Vec::new()
            } else {
                tx.companies(env)
                    .await?
                    .into_iter()
                    .map(|c| c.get())
                    .collect()
            };
            let cand_rows = tx
                .query(
                    "SELECT m.id::int8 \
                     FROM stock_move m \
                     JOIN product_product pp ON pp.id = m.product_id \
                     WHERE pp.product_tmpl_id = ANY($1) \
                       AND m.state NOT IN ('draft', 'done', 'cancel') \
                       AND ($2::bool OR m.company_id = ANY($3))",
                    &[&tmpl_ids, &is_su, &allowed_companies],
                )
                .await?;
            if !cand_rows.is_empty() {
                let decode = |e| Error::internal(format!("forecasted move read: {e}"));
                let cand_ids: Vec<i64> = cand_rows
                    .iter()
                    .map(|r| r.try_get::<_, i64>(0).map_err(decode))
                    .collect::<Result<_>>()?;
                let visible_ids = env
                    .records("stock.move")?
                    .browse(cand_ids)?
                    .filtered_access(tx, Operation::Read)
                    .await?
                    .ids();
                if !visible_ids.is_empty() {
                    let flow_rows = tx
                        .query(
                            "SELECT pp.product_tmpl_id::int8, \
                                    COALESCE(sum(CASE WHEN ld.usage = 'internal' AND ls.usage <> 'internal' THEN m.product_qty ELSE 0 END), 0), \
                                    COALESCE(sum(CASE WHEN ls.usage = 'internal' AND ld.usage <> 'internal' THEN m.product_qty ELSE 0 END), 0) \
                             FROM stock_move m \
                             JOIN product_product pp ON pp.id = m.product_id \
                             JOIN stock_location ls ON ls.id = m.location_id \
                             JOIN stock_location ld ON ld.id = m.location_dest_id \
                             WHERE m.id = ANY($1) \
                             GROUP BY pp.product_tmpl_id",
                            &[&visible_ids],
                        )
                        .await?;
                    for row in flow_rows {
                        let tid: i64 = row.try_get(0).map_err(decode)?;
                        let inc: Decimal = row.try_get(1).map_err(decode)?;
                        let out_q: Decimal = row.try_get(2).map_err(decode)?;
                        incoming.insert(tid, inc);
                        outgoing.insert(tid, out_q);
                    }
                }
            }
        }
        let move_counts = counts(tx, records, "stock.move", TEMPLATE, &[]).await?;
        let rule_counts = counts(tx, records, "stock.warehouse.orderpoint", TEMPLATE, &[]).await?;
        for (index, rec_id) in records.rec_ids().iter().enumerate() {
            let tid = row_of(rec_id);
            let quantity = tid
                .and_then(|id| on_hand.get(&id))
                .copied()
                .unwrap_or(Decimal::ZERO);
            let inc = tid
                .and_then(|id| incoming.get(&id))
                .copied()
                .unwrap_or(Decimal::ZERO);
            let out_q = tid
                .and_then(|id| outgoing.get(&id))
                .copied()
                .unwrap_or(Decimal::ZERO);
            let forecast = quantity + inc - out_q;
            out.set(index, ProductTemplateStockButtons::QTY_ON_HAND, quantity)?;
            out.set(index, ProductTemplateStockButtons::QTY_INCOMING, inc)?;
            out.set(index, ProductTemplateStockButtons::QTY_OUTGOING, out_q)?;
            out.set(
                index,
                ProductTemplateStockButtons::VIRTUAL_AVAILABLE,
                forecast,
            )?;
            out.set(
                index,
                ProductTemplateStockButtons::STOCK_MOVE_COUNT,
                move_counts[index],
            )?;
            out.set(
                index,
                ProductTemplateStockButtons::REORDERING_RULE_COUNT,
                rule_counts[index],
            )?;
        }
        Ok(())
    }

    /// Open open/forecasted stock moves for this product.
    #[method]
    #[rpc(effect = read)]
    async fn action_open_forecasted(
        this: &RecordSet<ProductTemplate>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let id = the_one(this.as_records(), tx).await?;
        window(
            tx,
            this.as_records(),
            "Forecasted Moves",
            "stock.move",
            vec![
                Value::str("&"),
                leaf(TEMPLATE, "=", Value::Int(id)),
                leaf(
                    "state",
                    "not in",
                    Value::list([Value::str("done"), Value::str("cancel")]),
                ),
            ],
        )
        .await
    }

    /// Open internal stock quants on hand for this product.
    #[method]
    #[rpc(effect = read)]
    async fn action_open_quants(
        this: &RecordSet<ProductTemplate>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let id = the_one(this.as_records(), tx).await?;
        window(
            tx,
            this.as_records(),
            "Stock On Hand",
            "stock.quant",
            vec![
                Value::str("&"),
                leaf(TEMPLATE, "=", Value::Int(id)),
                leaf(INTERNAL.0, "=", Value::str(INTERNAL.1)),
            ],
        )
        .await
    }

    /// Open stock move history for this product.
    #[method]
    #[rpc(effect = read)]
    async fn action_open_stock_moves(
        this: &RecordSet<ProductTemplate>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let id = the_one(this.as_records(), tx).await?;
        window(
            tx,
            this.as_records(),
            "Stock Moves",
            "stock.move",
            vec![leaf(TEMPLATE, "=", Value::Int(id))],
        )
        .await
    }

    /// Open reordering rules for this product.
    #[method]
    #[rpc(effect = read)]
    async fn action_open_orderpoints(
        this: &RecordSet<ProductTemplate>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let id = the_one(this.as_records(), tx).await?;
        window(
            tx,
            this.as_records(),
            "Reordering Rules",
            "stock.warehouse.orderpoint",
            vec![leaf(TEMPLATE, "=", Value::Int(id))],
        )
        .await
    }
}

/// The path from a transfer to its warehouse.
const PICKING_WAREHOUSE: &str = "picking_type_id.warehouse_id";

/// `stock.warehouse` smart buttons: Operation Types and Transfers.
#[methods(StockWarehouse, addon = "stock")]
impl StockWarehouse {
    #[compute(depends = ["company_id"], depends_context = ["uid", "allowed_company_ids"])]
    async fn _compute_warehouse_counts(
        this: &RecordSet<StockWarehouse>,
        tx: &mut Transaction,
        out: &mut Assign<'_, StockWarehouse>,
    ) -> Result<()> {
        let records = this.as_records();
        let type_counts = counts(tx, records, "stock.picking.type", "warehouse_id", &[]).await?;
        let picking_counts = counts(tx, records, "stock.picking", PICKING_WAREHOUSE, &[]).await?;
        for index in 0..records.len() {
            out.set(
                index,
                StockWarehouse::PICKING_TYPE_COUNT,
                type_counts[index],
            )?;
            out.set(index, StockWarehouse::PICKING_COUNT, picking_counts[index])?;
        }
        Ok(())
    }

    #[method]
    #[rpc(effect = read)]
    async fn action_view_picking_types(
        this: &RecordSet<StockWarehouse>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let id = the_one(this.as_records(), tx).await?;
        window(
            tx,
            this.as_records(),
            "Operation Types",
            "stock.picking.type",
            vec![leaf("warehouse_id", "=", Value::Int(id))],
        )
        .await
    }

    #[method]
    #[rpc(effect = read)]
    async fn action_view_pickings(
        this: &RecordSet<StockWarehouse>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let id = the_one(this.as_records(), tx).await?;
        window(
            tx,
            this.as_records(),
            "Transfers",
            "stock.picking",
            vec![leaf(PICKING_WAREHOUSE, "=", Value::Int(id))],
        )
        .await
    }
}

/// `stock.location` smart buttons: Current Stock and Transfers.
#[methods(StockLocation, addon = "stock")]
impl StockLocation {
    #[compute(depends = ["company_id", "usage"], depends_context = ["uid", "allowed_company_ids"])]
    async fn _compute_location_counts(
        this: &RecordSet<StockLocation>,
        tx: &mut Transaction,
        out: &mut Assign<'_, StockLocation>,
    ) -> Result<()> {
        let records = this.as_records();
        let quant_counts = counts(tx, records, "stock.quant", "location_id", &[]).await?;
        // A transfer counts once for each of its two ends that is one of these locations, and
        // once only when both ends are the same location.
        let ids = source_ids(records);
        let pairs = grouped(
            tx,
            records,
            "stock.picking",
            Domain::or([
                Domain::field("location_id")?.in_(ids.clone()),
                Domain::field("location_dest_id")?.in_(ids.clone()),
            ]),
            &["location_id", "location_dest_id"],
            "__count",
        )
        .await?;
        let mut found: BTreeMap<i64, i64> = BTreeMap::new();
        for (keys, value) in &pairs {
            let count = count_of(value)?;
            let (from, to) = match keys.as_slice() {
                [from, to] => (*from, *to),
                _ => return Err(Error::internal("A transfer group has no location pair.")),
            };
            if let Some(from) = from.filter(|id| ids.contains(id)) {
                *found.entry(from).or_insert(0) += count;
            }
            if let Some(to) = to.filter(|id| ids.contains(id) && Some(*id) != from) {
                *found.entry(to).or_insert(0) += count;
            }
        }
        let picking_counts = per_source(records, &found)?;
        for index in 0..records.len() {
            out.set(index, StockLocation::QUANT_COUNT, quant_counts[index])?;
            out.set(index, StockLocation::PICKING_COUNT, picking_counts[index])?;
        }
        Ok(())
    }

    #[method]
    #[rpc(effect = read)]
    async fn action_view_quants(
        this: &RecordSet<StockLocation>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let id = the_one(this.as_records(), tx).await?;
        window(
            tx,
            this.as_records(),
            "Current Stock",
            "stock.quant",
            vec![leaf("location_id", "=", Value::Int(id))],
        )
        .await
    }

    #[method]
    #[rpc(effect = read)]
    async fn action_view_pickings(
        this: &RecordSet<StockLocation>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let id = the_one(this.as_records(), tx).await?;
        window(
            tx,
            this.as_records(),
            "Transfers",
            "stock.picking",
            vec![
                Value::str("|"),
                leaf("location_id", "=", Value::Int(id)),
                leaf("location_dest_id", "=", Value::Int(id)),
            ],
        )
        .await
    }
}

/// `res.partner` smart button for Transfers.
#[extend(ResPartner, addon = "stock")]
pub struct PartnerStockPickings {
    /// Readable transfers linked to this contact.
    #[field(integer, compute = "_compute_picking_count", string = "Transfers")]
    pub picking_count: i32,
}

#[methods(ResPartner, addon = "stock")]
impl PartnerStockPickings {
    #[compute(depends = ["company_id"], depends_context = ["uid", "allowed_company_ids"])]
    async fn _compute_picking_count(
        this: &RecordSet<ResPartner>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ResPartner>,
    ) -> Result<()> {
        let records = this.as_records();
        let found = counts(tx, records, "stock.picking", "partner_id", &[]).await?;
        for (index, count) in found.into_iter().enumerate() {
            out.set(index, PartnerStockPickings::PICKING_COUNT, count)?;
        }
        Ok(())
    }

    #[method]
    #[rpc(effect = read)]
    async fn action_view_pickings(
        this: &RecordSet<ResPartner>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let id = the_one(this.as_records(), tx).await?;
        window(
            tx,
            this.as_records(),
            "Transfers",
            "stock.picking",
            vec![leaf("partner_id", "=", Value::Int(id))],
        )
        .await
    }
}
