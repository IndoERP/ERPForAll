// SPDX-License-Identifier: MIT OR Apache-2.0
//! Orderpoint replenishment behavior: horizon-bounded forecasted quantities (`stock.horizon_days`),
//! automatic sequence numbering, `Order Once`, and `Snooze`.

use std::collections::BTreeMap;

use erp_addon_base::ir_config_setting::read_values;
use erp_sdk::sequence::draw_by_code;
use erp_sdk::settings::SettingsScope;
use erp_sdk::{CompanyId, Operation, ValueMap, prelude::*};

use crate::orderpoint::StockWarehouseOrderpoint;

const HORIZON_KEY: &str = "stock.horizon_days";

fn db_err(e: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::db("A replenishment rule could not be read.").with_source(e)
}

/// Read the company's configured replenishment horizon in days (`stock.horizon_days`).
pub(crate) async fn company_horizon_days(tx: &mut Transaction, company_id: i64) -> Result<i64> {
    let env = tx.env().sudo(true).with_company(company_id)?;
    let wire_company = u64::try_from(company_id)
        .ok()
        .and_then(|id| erp_sdk::views::wire::WireId::try_from(id).ok())
        .ok_or_else(|| Error::validation("Invalid company identifier."))?;
    let read = read_values(
        &env,
        tx,
        SettingsScope::Company {
            company_id: wire_company,
        },
        vec![HORIZON_KEY.to_owned()],
    )
    .await?;
    let json = erp_sdk::serde_json::to_value(&read)
        .map_err(|e| Error::internal(format!("horizon setting read: {e}")))?;
    let entry = json["values"]
        .as_array()
        .and_then(|arr| arr.iter().find(|v| v["key"] == HORIZON_KEY))
        .ok_or_else(|| Error::internal(format!("setting '{HORIZON_KEY}' missing from response")))?;
    let days = entry["value"]
        .as_i64()
        .filter(|d| (1..=3650).contains(d))
        .ok_or_else(|| {
            Error::validation(format!(
                "setting '{HORIZON_KEY}' must be an integer between 1 and 3650"
            ))
        })?;
    Ok(days)
}

async fn next_orderpoint_name(env: &Env, tx: &mut Transaction, company_id: i64) -> Result<String> {
    let company = CompanyId::new(company_id)?;
    let today = env.context_today()?;
    draw_by_code(
        tx,
        &env.sudo(true),
        "stock.orderpoint",
        company,
        Some(today),
    )
    .await
}

/// Pure exact-decimal helper computing `qty_to_order` from `forecast`, `min_qty`, `max_qty`,
/// optional `manual_qty`, and optional rounding multiple factor.
pub fn compute_qty_to_order(
    forecast: Decimal,
    min_qty: Decimal,
    max_qty: Decimal,
    manual_qty: Decimal,
    multiple: Option<Decimal>,
) -> Decimal {
    if manual_qty > Decimal::ZERO {
        return manual_qty;
    }
    if forecast >= min_qty {
        return Decimal::ZERO;
    }
    let target = if max_qty >= min_qty { max_qty } else { min_qty };
    let raw = target - forecast;
    if raw <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    if let Some(step) = multiple
        && step > Decimal::ZERO
    {
        let ratio = (raw / step).ceil();
        return ratio * step;
    }
    raw
}

#[methods(StockWarehouseOrderpoint, addon = "stock")]
impl StockWarehouseOrderpoint {
    #[method(override)]
    async fn default_get(
        this: &RecordSet<StockWarehouseOrderpoint>,
        tx: &mut Transaction,
        fields: Vec<String>,
        sup: Super,
    ) -> Result<ValueMap> {
        let mut out: ValueMap = sup.call(tx, this.as_records(), (fields.clone(),)).await?;
        let company_id = match out.get("company_id") {
            Some(Value::Int(id)) => Some(*id),
            _ => this.env().company_id()?,
        };
        if let Some(cid) = company_id {
            if fields.iter().any(|f| f == "company_id") && !out.contains_key("company_id") {
                out.insert("company_id".into(), Value::Int(cid));
            }
            if fields.iter().any(|f| f == "name") && !out.contains_key("name") {
                out.insert("name".into(), Value::str("New"));
            }
            let wh_rows = tx
                .query(
                    "SELECT id::int8, lot_stock_id::int8 FROM stock_warehouse \
                     WHERE company_id = $1 AND active IS TRUE ORDER BY sequence, id LIMIT 1",
                    &[&cid],
                )
                .await?;
            if let Some(row) = wh_rows.first() {
                let wh_id: i64 = row.try_get(0).map_err(db_err)?;
                let loc_id: i64 = row.try_get(1).map_err(db_err)?;
                if fields.iter().any(|f| f == "warehouse_id") && !out.contains_key("warehouse_id") {
                    out.insert("warehouse_id".into(), Value::Int(wh_id));
                }
                if fields.iter().any(|f| f == "location_id") && !out.contains_key("location_id") {
                    out.insert("location_id".into(), Value::Int(loc_id));
                }
            }
        }
        Ok(out)
    }

    #[method(override)]
    async fn create(
        this: &RecordSet<StockWarehouseOrderpoint>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        this.as_records()
            .check_access(tx, Operation::Create)
            .await?;
        let env = this.env();
        let active_company = env.company_id()?;
        for vals in &mut vals_list {
            let company_id = match vals.get("company_id") {
                Some(Cell::Id(id)) => *id,
                Some(Cell::Int(id)) => i64::from(*id),
                _ => {
                    let cid = active_company.ok_or_else(|| {
                        Error::validation("A company is required to create a replenishment rule.")
                    })?;
                    vals.insert("company_id".into(), Cell::Id(cid));
                    cid
                }
            };
            let needs_name = match vals.get("name") {
                None | Some(Cell::Null) => true,
                Some(Cell::Str(s)) => s.trim().is_empty() || s.as_ref() == "New",
                _ => false,
            };
            if needs_name {
                let name = next_orderpoint_name(env, tx, company_id).await?;
                vals.insert("name".into(), Cell::Str(name.into()));
            }
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }

    /// Compute on-hand, horizon-bounded forecasted quantity, and suggested quantity to order.
    #[compute(
        depends = [
            "product_id",
            "location_id",
            "company_id",
            "product_min_qty",
            "product_max_qty",
            "qty_to_order_manual",
            "replenishment_uom_id"
        ],
        depends_context = ["allowed_company_ids"]
    )]
    async fn _compute_quantities(
        this: &RecordSet<StockWarehouseOrderpoint>,
        tx: &mut Transaction,
        out: &mut Assign<'_, StockWarehouseOrderpoint>,
    ) -> Result<()> {
        let records = this.as_records();
        let ids = records.ids();
        let mut computed: BTreeMap<i64, (Decimal, Decimal, Decimal)> = BTreeMap::new();
        if !ids.is_empty() {
            tx.flush().await?;
            let rows = tx
                .query(
                    "SELECT op.id::int8, op.company_id::int8, op.product_id::int8, op.location_id::int8, \
                            op.product_min_qty, op.product_max_qty, COALESCE(op.qty_to_order_manual, 0), \
                            u.relative_factor \
                     FROM stock_warehouse_orderpoint op \
                     LEFT JOIN uom_uom u ON u.id = op.replenishment_uom_id \
                     WHERE op.id = ANY($1) ORDER BY op.id",
                    &[&ids],
                )
                .await?;
            let mut horizons: BTreeMap<i64, i64> = BTreeMap::new();
            for row in rows {
                let id: i64 = row.try_get(0).map_err(db_err)?;
                let company_id: i64 = row.try_get(1).map_err(db_err)?;
                let product_id: i64 = row.try_get(2).map_err(db_err)?;
                let location_id: i64 = row.try_get(3).map_err(db_err)?;
                let min_qty: Decimal = row.try_get(4).map_err(db_err)?;
                let max_qty: Decimal = row.try_get(5).map_err(db_err)?;
                let manual_qty: Decimal = row.try_get(6).map_err(db_err)?;
                let mult: Option<Decimal> = row.try_get(7).map_err(db_err)?;

                let horizon = match horizons.get(&company_id) {
                    Some(&h) => h,
                    None => {
                        let h = company_horizon_days(tx, company_id).await?;
                        horizons.insert(company_id, h);
                        h
                    }
                };
                let on_hand: Decimal = tx
                    .query(
                        "SELECT COALESCE(sum(quantity), 0) FROM stock_quant \
                         WHERE company_id = $1 AND product_id = $2 AND location_id = $3",
                        &[&company_id, &product_id, &location_id],
                    )
                    .await?[0]
                    .try_get(0)
                    .map_err(db_err)?;
                let incoming: Decimal = tx
                    .query(
                        "SELECT COALESCE(sum(product_qty), 0) FROM stock_move \
                         WHERE company_id = $1 AND product_id = $2 AND location_dest_id = $3 \
                           AND location_id <> $3 \
                           AND state NOT IN ('draft', 'done', 'cancel') \
                           AND date <= (now() AT TIME ZONE 'UTC') + ($4::int8 * INTERVAL '1 day')",
                        &[&company_id, &product_id, &location_id, &horizon],
                    )
                    .await?[0]
                    .try_get(0)
                    .map_err(db_err)?;
                let outgoing: Decimal = tx
                    .query(
                        "SELECT COALESCE(sum(product_qty), 0) FROM stock_move \
                         WHERE company_id = $1 AND product_id = $2 AND location_id = $3 \
                           AND location_dest_id <> $3 \
                           AND state NOT IN ('draft', 'done', 'cancel') \
                           AND date <= (now() AT TIME ZONE 'UTC') + ($4::int8 * INTERVAL '1 day')",
                        &[&company_id, &product_id, &location_id, &horizon],
                    )
                    .await?[0]
                    .try_get(0)
                    .map_err(db_err)?;
                let forecast = on_hand + incoming - outgoing;
                let to_order = compute_qty_to_order(forecast, min_qty, max_qty, manual_qty, mult);
                computed.insert(id, (on_hand, forecast, to_order));
            }
        }
        for (index, rec_id) in records.rec_ids().iter().enumerate() {
            let (on_hand, forecast, to_order) = rec_id
                .as_real()
                .and_then(|id| computed.get(&id.get()).copied())
                .unwrap_or((Decimal::ZERO, Decimal::ZERO, Decimal::ZERO));
            out.set(index, StockWarehouseOrderpoint::QTY_ON_HAND, on_hand)?;
            out.set(index, StockWarehouseOrderpoint::QTY_FORECAST, forecast)?;
            out.set(index, StockWarehouseOrderpoint::QTY_TO_ORDER, to_order)?;
        }
        Ok(())
    }

    /// Execute manual replenishment (`Order Once`) for this reordering rule, creating a receipt
    /// transfer (or delegating to `action_replenish_buy` if a Buy route is selected).
    #[method]
    #[rpc(effect = write)]
    pub async fn action_replenish(
        this: &RecordSet<StockWarehouseOrderpoint>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let records = this.as_records().clone();
        records.check_access(tx, Operation::Write).await?;
        let id = records.ensure_one()?.int()?;
        let to_order = match records.cell(tx, "qty_to_order").await? {
            Cell::Decimal(v) => v,
            _ => Decimal::ZERO,
        };
        if to_order <= Decimal::ZERO {
            return Err(Error::user(
                "Nothing to replenish: forecasted quantity already meets the minimum.",
            ));
        }
        let row = tx
            .query(
                "SELECT op.name, op.company_id::int8, op.product_id::int8, op.location_id::int8, \
                        op.warehouse_id::int8, op.route_id::int8, pt.uom_id::int8, \
                        w.in_type_id::int8 \
                 FROM stock_warehouse_orderpoint op \
                 JOIN product_product pp ON pp.id = op.product_id \
                 JOIN product_template pt ON pt.id = pp.product_tmpl_id \
                 JOIN stock_warehouse w ON w.id = op.warehouse_id \
                 WHERE op.id = $1",
                &[&id],
            )
            .await?;
        let Some(r) = row.first() else {
            return Err(Error::missing("Replenishment rule not found."));
        };
        let op_name: String = r.try_get(0).map_err(db_err)?;
        let company_id: i64 = r.try_get(1).map_err(db_err)?;
        let product_id: i64 = r.try_get(2).map_err(db_err)?;
        let location_id: i64 = r.try_get(3).map_err(db_err)?;
        let _warehouse_id: i64 = r.try_get(4).map_err(db_err)?;
        let route_id: Option<i64> = r.try_get(5).map_err(db_err)?;
        let uom_id: i64 = r.try_get(6).map_err(db_err)?;
        let in_type_id: Option<i64> = r.try_get(7).map_err(db_err)?;

        // Check if the chosen route uses a `buy` rule and purchase_stock exposes `action_replenish_buy`
        if let Some(rid) = route_id {
            let has_buy: i64 = tx
                .query(
                    "SELECT count(*)::int8 FROM stock_rule \
                     WHERE route_id = $1 AND action = 'buy' AND active IS TRUE",
                    &[&rid],
                )
                .await?[0]
                .try_get(0)
                .map_err(db_err)?;
            if has_buy > 0 {
                return records
                    .call::<(), Value>(tx, "action_replenish_buy", ())
                    .await;
            }
        }

        // Otherwise create a replenishment incoming transfer into `location_id`.
        let vendor_loc: i64 = tx
            .query(
                "SELECT id::int8 FROM stock_location WHERE usage = 'supplier' AND active IS TRUE ORDER BY id LIMIT 1",
                &[],
            )
            .await?
            .first()
            .ok_or_else(|| Error::user("No active Vendors location found for replenishment."))?
            .try_get(0)
            .map_err(db_err)?;
        let picking_type_id = in_type_id.ok_or_else(|| {
            Error::user("Warehouse has no incoming operation type for replenishment.")
        })?;
        let env = records.env();
        let mut pick_vals = RawVals::new();
        pick_vals.insert("company_id".into(), Cell::Id(company_id));
        pick_vals.insert("picking_type_id".into(), Cell::Id(picking_type_id));
        pick_vals.insert("location_id".into(), Cell::Id(vendor_loc));
        pick_vals.insert("location_dest_id".into(), Cell::Id(location_id));
        pick_vals.insert("origin".into(), Cell::Str(op_name.clone().into()));
        let created_pick = env
            .records("stock.picking")?
            .create(tx, vec![pick_vals])
            .await?;
        tx.flush().await?;
        let pick_id =
            created_pick.ids().into_iter().next().ok_or_else(|| {
                Error::invariant("Created replenishment transfer returned no id.")
            })?;

        let mut mv_vals = RawVals::new();
        mv_vals.insert("company_id".into(), Cell::Id(company_id));
        mv_vals.insert("picking_id".into(), Cell::Id(pick_id));
        mv_vals.insert("picking_type_id".into(), Cell::Id(picking_type_id));
        mv_vals.insert("product_id".into(), Cell::Id(product_id));
        mv_vals.insert("product_uom_qty".into(), Cell::Decimal(to_order));
        mv_vals.insert("uom_id".into(), Cell::Id(uom_id));
        mv_vals.insert("location_id".into(), Cell::Id(vendor_loc));
        mv_vals.insert("location_dest_id".into(), Cell::Id(location_id));
        mv_vals.insert("origin".into(), Cell::Str(op_name.into()));
        env.records("stock.move")?.create(tx, vec![mv_vals]).await?;
        tx.flush().await?;
        created_pick
            .call::<(), ()>(tx, "action_confirm", ())
            .await?;

        // Reset manual override once ordered
        records
            .write(
                tx,
                RawVals::new().set_cell("qty_to_order_manual", Cell::Decimal(Decimal::ZERO)),
            )
            .await?;

        Ok(Value::Map(
            ValueMap::from([
                ("type".into(), Value::str("ir.actions.act_window")),
                ("name".into(), Value::str("Replenishment Transfer")),
                ("res_model".into(), Value::str("stock.picking")),
                ("target".into(), Value::str("current")),
                ("res_id".into(), Value::Int(pick_id)),
                (
                    "views".into(),
                    Value::list([Value::list([Value::False, Value::str("form")])]),
                ),
            ])
            .into(),
        ))
    }

    /// Snooze this replenishment rule for 7 days.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_snooze(
        this: &RecordSet<StockWarehouseOrderpoint>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let records = this.as_records();
        records.check_access(tx, Operation::Write).await?;
        let ids = records.ids();
        if ids.is_empty() {
            return Ok(());
        }
        let companies = this
            .mapped(tx, StockWarehouseOrderpoint::COMPANY_ID)
            .await?;
        let mut date_by_company: BTreeMap<i64, Date> = BTreeMap::new();
        for (id, comp) in ids.into_iter().zip(companies) {
            let cid = comp
                .ok_or_else(|| Error::invariant("Replenishment rule has no company."))?
                .get();
            let target_date = if let Some(&d) = date_by_company.get(&cid) {
                d
            } else {
                let today = erp_addon_base::company_calendar::business_date(tx, cid).await?;
                let d = Date::from_days(today.days() + 7);
                date_by_company.insert(cid, d);
                d
            };
            records
                .browse([id])?
                .write(
                    tx,
                    RawVals::new().set_cell("snoozed_until", Cell::Date(target_date)),
                )
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).expect("decimal")
    }

    #[test]
    fn orderpoint_qty_to_order_respects_min_max_and_multiple() {
        assert_eq!(
            compute_qty_to_order(dec("12"), dec("10"), dec("30"), dec("0"), None),
            dec("0")
        );
        assert_eq!(
            compute_qty_to_order(dec("4"), dec("10"), dec("25"), dec("0"), None),
            dec("21")
        );
        // Rounded up to multiple of 6 -> 24
        assert_eq!(
            compute_qty_to_order(dec("4"), dec("10"), dec("25"), dec("0"), Some(dec("6"))),
            dec("24")
        );
        // Manual override takes priority
        assert_eq!(
            compute_qty_to_order(dec("50"), dec("10"), dec("25"), dec("7.5"), None),
            dec("7.5")
        );
    }
}
