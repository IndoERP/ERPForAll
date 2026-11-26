// SPDX-License-Identifier: MIT OR Apache-2.0
//! Addon `stock_fleet` for ERPForAll: transport dispatch vehicles, categories,
//! dock scheduling and exact-decimal batch weight/volume capacity utilization.

use std::collections::BTreeMap;

use erp_addon_stock::StockPickingBatch;
use erp_sdk::{Operation, prelude::*};

erp_sdk::addon!(
    name = "stock_fleet",
    exact_only = true,
    depends = ["base", "stock"]
);

fn db_err(e: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::db("Fleet dispatch data could not be read.").with_source(e)
}

/// Compute exact percentage utilization (`load / capacity * 100`, rounded to 2 decimal places).
/// Returns `0` when capacity is not configured (`<= 0`).
pub fn capacity_percent(load: Decimal, capacity: Decimal) -> Decimal {
    if capacity <= Decimal::ZERO || load <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    ((load * Decimal::from(100)) / capacity).round_dp(2)
}

/// `fleet.vehicle.category`: standard load limits for a class of delivery vehicles.
#[model(
    name = "fleet.vehicle.category",
    description = "Vehicle Category",
    order = "sequence, name",
    constraints = [
        ("_nonneg_weight", "CHECK (max_weight >= 0)", "Max weight cannot be negative."),
        ("_nonneg_volume", "CHECK (max_volume >= 0)", "Max volume cannot be negative."),
    ]
)]
pub struct FleetVehicleCategory {
    /// Category name (e.g. `Delivery Van`, `Box Truck`).
    #[field(char, required = true, string = "Category")]
    pub name: Option<Arc<str>>,
    /// Display order.
    #[field(integer, default = 10)]
    pub sequence: i32,
    /// Maximum payload weight in kilograms.
    #[field(
        float,
        digits = "Stock Weight",
        default = 0,
        string = "Max Weight (kg)"
    )]
    pub max_weight: Decimal,
    /// Maximum payload volume in cubic meters.
    #[field(float, digits = 0, default = 0, string = "Max Volume (m³)")]
    pub max_volume: Decimal,
}

/// `fleet.vehicle`: one dispatch vehicle assignable to batch transfers.
#[model(
    name = "fleet.vehicle",
    description = "Fleet Vehicle",
    order = "name, id",
    company_access = direct(field = "company_id", branch = "exact", empty = "allow"),
    constraints = [
        ("_nonneg_weight", "CHECK (max_weight >= 0)", "Max weight cannot be negative."),
        ("_nonneg_volume", "CHECK (max_volume >= 0)", "Max volume cannot be negative."),
    ]
)]
pub struct FleetVehicle {
    /// Vehicle name.
    #[field(char, required = true, string = "Vehicle")]
    pub name: Option<Arc<str>>,
    /// Registration plate number.
    #[field(char, string = "License Plate")]
    pub license_plate: Option<Arc<str>>,
    /// Vehicle class.
    #[field(many2one, comodel = "fleet.vehicle.category", string = "Category")]
    pub category_id: Option<RecId>,
    /// Assigned driver contact.
    #[field(many2one, comodel = "res.partner", string = "Driver")]
    pub driver_id: Option<RecId>,
    /// Owning company (optional).
    #[field(many2one, comodel = "res.company", string = "Company")]
    pub company_id: Option<RecId>,
    /// Maximum payload weight in kg (falls back to category limit when 0).
    #[field(
        float,
        digits = "Stock Weight",
        default = 0,
        string = "Max Weight (kg)"
    )]
    pub max_weight: Decimal,
    /// Maximum payload volume in m³ (falls back to category limit when 0).
    #[field(float, digits = 0, default = 0, string = "Max Volume (m³)")]
    pub max_volume: Decimal,
    /// Active flag.
    #[field(boolean, default = true)]
    pub active: bool,
}

#[extend(StockPickingBatch, addon = "stock_fleet")]
pub struct StockPickingBatchFleet {
    /// Vehicle assigned to dispatch this batch.
    #[field(
        many2one,
        comodel = "fleet.vehicle",
        check_company = true,
        string = "Vehicle"
    )]
    pub vehicle_id: Option<RecId>,
    /// Vehicle category (auto-used for capacity limits when set).
    #[field(
        many2one,
        comodel = "fleet.vehicle.category",
        string = "Vehicle Category"
    )]
    pub vehicle_category_id: Option<RecId>,
    /// Driver assigned to this dispatch.
    #[field(many2one, comodel = "res.partner", string = "Driver")]
    pub driver_id: Option<RecId>,
    /// Staging / loading dock location.
    #[field(
        many2one,
        comodel = "stock.location",
        check_company = true,
        string = "Dock Location"
    )]
    pub dock_id: Option<RecId>,
    /// Total weight of all transfers in this batch (kg).
    #[field(
        float,
        digits = "Stock Weight",
        compute = "_compute_dispatch_load",
        store = true,
        string = "Total Weight (kg)"
    )]
    pub dispatch_weight: Decimal,
    /// Total volume of all transfers in this batch (m³).
    #[field(
        float,
        digits = 0,
        compute = "_compute_dispatch_load",
        store = true,
        string = "Total Volume (m³)"
    )]
    pub dispatch_volume: Decimal,
    /// Percentage of vehicle weight capacity used.
    #[field(
        float,
        digits = 0,
        compute = "_compute_dispatch_load",
        store = true,
        string = "Weight Load (%)"
    )]
    pub weight_utilization: Decimal,
    /// Percentage of vehicle volume capacity used.
    #[field(
        float,
        digits = 0,
        compute = "_compute_dispatch_load",
        store = true,
        string = "Volume Load (%)"
    )]
    pub volume_utilization: Decimal,
}

async fn compute_batch_totals(
    tx: &mut Transaction,
    batch_ids: &[i64],
) -> Result<BTreeMap<i64, (Decimal, Decimal, Decimal, Decimal)>> {
    let mut out = BTreeMap::new();
    if batch_ids.is_empty() {
        return Ok(out);
    }
    let rows = tx
        .query(
            "SELECT b.id::int8, \
                    COALESCE(load.w, 0), \
                    COALESCE(load.v, 0), \
                    COALESCE(NULLIF(v.max_weight, 0), NULLIF(vc.max_weight, 0), NULLIF(bc.max_weight, 0), 0), \
                    COALESCE(NULLIF(v.max_volume, 0), NULLIF(vc.max_volume, 0), NULLIF(bc.max_volume, 0), 0) \
             FROM stock_picking_batch b \
             LEFT JOIN fleet_vehicle v ON v.id = b.vehicle_id \
             LEFT JOIN fleet_vehicle_category vc ON vc.id = v.category_id \
             LEFT JOIN fleet_vehicle_category bc ON bc.id = b.vehicle_category_id \
             LEFT JOIN ( \
               SELECT p.batch_id, \
                      sum((CASE WHEN m.quantity_product_uom > 0 THEN m.quantity_product_uom ELSE m.product_qty END) * COALESCE(pt.weight, 0)) AS w, \
                      sum((CASE WHEN m.quantity_product_uom > 0 THEN m.quantity_product_uom ELSE m.product_qty END) * COALESCE(pt.volume, 0)) AS v \
               FROM stock_picking p \
               JOIN stock_picking_batch b2 ON b2.id = p.batch_id AND p.company_id = b2.company_id \
               JOIN stock_move m ON m.picking_id = p.id AND m.state <> 'cancel' \
               JOIN product_product pp ON pp.id = m.product_id \
               JOIN product_template pt ON pt.id = pp.product_tmpl_id \
               WHERE p.batch_id = ANY($1) \
               GROUP BY p.batch_id \
             ) load ON load.batch_id = b.id \
             WHERE b.id = ANY($1)",
            &[&batch_ids],
        )
        .await?;
    for row in rows {
        let bid: i64 = row.try_get(0).map_err(db_err)?;
        let w: Decimal = row.try_get(1).map_err(db_err)?;
        let v: Decimal = row.try_get(2).map_err(db_err)?;
        let cap_w: Decimal = row.try_get(3).map_err(db_err)?;
        let cap_v: Decimal = row.try_get(4).map_err(db_err)?;
        out.insert(
            bid,
            (w, v, capacity_percent(w, cap_w), capacity_percent(v, cap_v)),
        );
    }
    Ok(out)
}

#[methods(StockPickingBatch, addon = "stock_fleet")]
impl StockPickingBatchFleet {
    #[compute(depends = ["picking_ids", "vehicle_id", "vehicle_category_id"])]
    async fn _compute_dispatch_load(
        this: &RecordSet<StockPickingBatch>,
        tx: &mut Transaction,
        out: &mut Assign<'_, StockPickingBatch>,
    ) -> Result<()> {
        let rec_ids = this.as_records().rec_ids();
        let real_ids: Vec<i64> = rec_ids
            .iter()
            .filter_map(|id| id.as_real().map(|r| r.get()))
            .collect();
        let totals = compute_batch_totals(tx, &real_ids).await?;
        for (idx, rec_id) in rec_ids.iter().enumerate() {
            let (w, v, wp, vp) = rec_id
                .as_real()
                .and_then(|id| totals.get(&id.get()).copied())
                .unwrap_or((Decimal::ZERO, Decimal::ZERO, Decimal::ZERO, Decimal::ZERO));
            out.set(idx, StockPickingBatchFleet::DISPATCH_WEIGHT, w)?;
            out.set(idx, StockPickingBatchFleet::DISPATCH_VOLUME, v)?;
            out.set(idx, StockPickingBatchFleet::WEIGHT_UTILIZATION, wp)?;
            out.set(idx, StockPickingBatchFleet::VOLUME_UTILIZATION, vp)?;
        }
        Ok(())
    }

    /// Recompute batch dispatch load and verify it fits within the assigned vehicle capacity.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_recompute_dispatch(
        this: &RecordSet<StockPickingBatch>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let records = this.as_records().clone();
        records.check_access(tx, Operation::Write).await?;
        let bid = records.ensure_one()?.int()?;
        let uid = records.env().uid();
        tx.savepoint(async move |tx| {
            tx.flush().await?;
            let state_rows = tx
                .query(
                    "SELECT state FROM stock_picking_batch WHERE id = $1 FOR UPDATE",
                    &[&bid],
                )
                .await?;
            let state: String = state_rows
                .first()
                .ok_or_else(|| Error::missing("Batch transfer not found."))?
                .try_get(0)
                .map_err(db_err)?;
            if matches!(state.as_str(), "done" | "cancel") {
                return Err(Error::user(
                    "Cannot recompute dispatch on a completed or cancelled batch.",
                ));
            }
            let totals = compute_batch_totals(tx, &[bid]).await?;
            let (w, v, wp, vp) = totals.get(&bid).copied().unwrap_or((
                Decimal::ZERO,
                Decimal::ZERO,
                Decimal::ZERO,
                Decimal::ZERO,
            ));
            if wp > Decimal::from(100) || vp > Decimal::from(100) {
                return Err(Error::user(format!(
                    "Batch load exceeds vehicle capacity (Weight: {wp}%, Volume: {vp}%)."
                )));
            }
            tx.execute(
                "UPDATE stock_picking_batch \
                 SET dispatch_weight = $2, dispatch_volume = $3, \
                     weight_utilization = $4, volume_utilization = $5, \
                     write_uid = $6, write_date = (now() AT TIME ZONE 'UTC') \
                 WHERE id = $1",
                &[&bid, &w, &v, &wp, &vp, &uid],
            )
            .await?;
            records
                .modified_external(
                    tx,
                    &[
                        "dispatch_weight",
                        "dispatch_volume",
                        "weight_utilization",
                        "volume_utilization",
                    ],
                )
                .await?;
            Ok(())
        })
        .await
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
    fn computes_exact_vehicle_capacity_utilization() {
        assert_eq!(capacity_percent(dec("250"), dec("1000")), dec("25.00"));
        assert_eq!(capacity_percent(dec("1250"), dec("1000")), dec("125.00"));
        assert_eq!(capacity_percent(dec("50"), dec("0")), Decimal::ZERO);
    }
}
