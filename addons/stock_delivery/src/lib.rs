// SPDX-License-Identifier: MIT OR Apache-2.0
//! Addon `stock_delivery` for ERPForAll: carrier selection, shipping weight,
//! rate computation and dispatch tracking on stock transfers and sales orders.

use std::collections::BTreeMap;

use erp_addon_delivery::{ShipmentMetrics, load_carrier_pricing, rate_shipment};
use erp_addon_sale::SaleOrder;
use erp_addon_stock::picking::StockPicking;
use erp_sdk::{Operation, prelude::*};

erp_sdk::addon!(
    name = "stock_delivery",
    exact_only = true,
    depends = ["base", "stock", "sale", "sale_stock", "delivery"]
);

fn db_err(e: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::db("Shipping delivery data could not be read.").with_source(e)
}

async fn currency_scale(
    tx: &mut Transaction,
    currency_id: Option<i64>,
    company_id: i64,
) -> Result<u32> {
    let resolved_cid = match currency_id {
        Some(cid) => cid,
        None => {
            let rows = tx
                .query(
                    "SELECT currency_id::int8 FROM res_company WHERE id = $1",
                    &[&company_id],
                )
                .await?;
            rows.first()
                .ok_or_else(|| Error::missing("Company not found."))?
                .try_get::<_, Option<i64>>(0)
                .map_err(db_err)?
                .ok_or_else(|| Error::user("Company has no currency configured."))?
        }
    };
    let rows = tx
        .query(
            "SELECT decimal_places::int4 FROM res_currency WHERE id = $1",
            &[&resolved_cid],
        )
        .await?;
    let dp: i32 = rows
        .first()
        .ok_or_else(|| Error::missing("Currency not found."))?
        .try_get(0)
        .map_err(db_err)?;
    if !(0..=6).contains(&dp) {
        return Err(Error::validation(format!(
            "Unsupported currency decimal places '{dp}'."
        )));
    }
    u32::try_from(dp).map_err(|_| Error::internal("currency decimal_places conversion failed"))
}

#[extend(StockPicking, addon = "stock_delivery")]
pub struct StockPickingDelivery {
    /// Shipping carrier selected for this transfer.
    #[field(
        many2one,
        comodel = "delivery.carrier",
        check_company = true,
        string = "Carrier"
    )]
    pub carrier_id: Option<RecId>,
    /// Carrier tracking number returned or entered at dispatch.
    #[field(char, copy = false, string = "Tracking Reference")]
    pub carrier_tracking_ref: Option<Arc<str>>,
    /// Calculated shipping cost for this transfer.
    #[field(float, digits = "Product Price", default = 0, string = "Shipping Cost")]
    pub carrier_price: Decimal,
    /// Total weight of the moves on this transfer.
    #[field(
        float,
        digits = "Stock Weight",
        compute = "_compute_shipping_weight",
        store = true,
        string = "Weight for Shipping"
    )]
    pub shipping_weight: Decimal,
}

#[methods(StockPicking, addon = "stock_delivery")]
impl StockPickingDelivery {
    #[compute(depends = ["move_ids", "move_ids.product_uom_qty", "move_ids.quantity"])]
    async fn _compute_shipping_weight(
        this: &RecordSet<StockPicking>,
        tx: &mut Transaction,
        out: &mut Assign<'_, StockPicking>,
    ) -> Result<()> {
        let rec_ids = this.as_records().rec_ids();
        let real_ids: Vec<i64> = rec_ids
            .iter()
            .filter_map(|id| id.as_real().map(|r| r.get()))
            .collect();
        let mut weights: BTreeMap<i64, Decimal> = BTreeMap::new();
        if !real_ids.is_empty() {
            tx.flush().await?;
            let rows = tx
                .query(
                    "SELECT m.picking_id::int8, \
                            COALESCE(sum((CASE WHEN m.quantity_product_uom > 0 THEN m.quantity_product_uom ELSE m.product_qty END) * COALESCE(pt.weight, 0)), 0) \
                     FROM stock_move m \
                     JOIN product_product pp ON pp.id = m.product_id \
                     JOIN product_template pt ON pt.id = pp.product_tmpl_id \
                     WHERE m.picking_id = ANY($1) AND m.state <> 'cancel' \
                     GROUP BY m.picking_id",
                    &[&real_ids],
                )
                .await?;
            for row in rows {
                let pid: i64 = row.try_get(0).map_err(db_err)?;
                let w: Decimal = row.try_get(1).map_err(db_err)?;
                weights.insert(pid, w);
            }
        }
        for (idx, rec_id) in rec_ids.iter().enumerate() {
            let w = rec_id
                .as_real()
                .and_then(|id| weights.get(&id.get()).copied())
                .unwrap_or(Decimal::ZERO);
            out.set(idx, StockPickingDelivery::SHIPPING_WEIGHT, w)?;
        }
        Ok(())
    }

    /// Compute the carrier's exact shipping cost from the transfer's moves.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_compute_shipping_cost(
        this: &RecordSet<StockPicking>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let records = this.as_records().clone();
        records.check_access(tx, Operation::Write).await?;
        let picking_id = records.ensure_one()?.int()?;
        let uid = records.env().uid();
        tx.flush().await?;
        let p_rows = tx
            .query(
                "SELECT carrier_id::int8, state, company_id::int8 \
                 FROM stock_picking WHERE id = $1 FOR UPDATE",
                &[&picking_id],
            )
            .await?;
        let prow = p_rows
            .first()
            .ok_or_else(|| Error::missing("Transfer not found."))?;
        let carrier_id: Option<i64> = prow.try_get(0).map_err(db_err)?;
        let state: String = prow.try_get(1).map_err(db_err)?;
        let company_id: i64 = prow.try_get(2).map_err(db_err)?;
        if state == "cancel" {
            return Err(Error::user(
                "Cannot compute shipping cost on a cancelled transfer.",
            ));
        }
        let Some(carrier_id) = carrier_id else {
            return Err(Error::user(
                "Select a carrier before computing shipping cost.",
            ));
        };
        let carrier = load_carrier_pricing(tx, carrier_id).await?;
        let scale = currency_scale(tx, None, company_id).await?;
        let m_rows = tx
            .query(
                "SELECT COALESCE(sum(qty * COALESCE(pt.weight, 0)), 0), \
                        COALESCE(sum(qty * COALESCE(pt.volume, 0)), 0), \
                        COALESCE(sum(qty * COALESCE(pt.list_price, 0)), 0), \
                        COALESCE(sum(qty), 0) \
                 FROM ( \
                   SELECT product_id, \
                          CASE WHEN quantity_product_uom > 0 THEN quantity_product_uom ELSE product_qty END AS qty \
                   FROM stock_move \
                   WHERE picking_id = $1 AND state <> 'cancel' \
                 ) m \
                 JOIN product_product pp ON pp.id = m.product_id \
                 JOIN product_template pt ON pt.id = pp.product_tmpl_id",
                &[&picking_id],
            )
            .await?;
        let row = m_rows
            .first()
            .ok_or_else(|| Error::internal("Transfer shipment metrics returned no row."))?;
        let metrics = ShipmentMetrics {
            weight: row.try_get(0).map_err(db_err)?,
            volume: row.try_get(1).map_err(db_err)?,
            order_price: row.try_get(2).map_err(db_err)?,
            quantity: row.try_get(3).map_err(db_err)?,
        };
        let price = rate_shipment(&carrier, &metrics, scale)?;
        tx.execute(
            "UPDATE stock_picking \
             SET carrier_price = $2, shipping_weight = $3, \
                 write_uid = $4, write_date = (now() AT TIME ZONE 'UTC') \
             WHERE id = $1",
            &[&picking_id, &price, &metrics.weight, &uid],
        )
        .await?;
        records
            .modified_external(tx, &["carrier_price", "shipping_weight"])
            .await?;
        Ok(())
    }

    /// Dispatch transfer to the configured carrier, computing shipping price and assigning a
    /// tracking reference if one has not been provided yet.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_send_to_shipper(
        this: &RecordSet<StockPicking>,
        tx: &mut Transaction,
    ) -> Result<()> {
        Self::action_compute_shipping_cost(this, tx).await?;
        let records = this.as_records().clone();
        let picking_id = records.ensure_one()?.int()?;
        let uid = records.env().uid();
        let rows = tx
            .query(
                "SELECT state, carrier_tracking_ref FROM stock_picking WHERE id = $1 FOR UPDATE",
                &[&picking_id],
            )
            .await?;
        let row = rows
            .first()
            .ok_or_else(|| Error::missing("Transfer not found."))?;
        let state: String = row.try_get(0).map_err(db_err)?;
        if state == "cancel" {
            return Err(Error::user("Cannot dispatch a cancelled transfer."));
        }
        let existing: Option<String> = row.try_get(1).map_err(db_err)?;
        if existing.as_deref().is_none_or(|s| s.trim().is_empty()) {
            let tracking = format!("TRK-{picking_id:06}");
            tx.execute(
                "UPDATE stock_picking \
                 SET carrier_tracking_ref = $2, write_uid = $3, write_date = (now() AT TIME ZONE 'UTC') \
                 WHERE id = $1",
                &[&picking_id, &tracking, &uid],
            )
            .await?;
            records
                .modified_external(tx, &["carrier_tracking_ref"])
                .await?;
        }
        Ok(())
    }
}

#[extend(SaleOrder, addon = "stock_delivery")]
pub struct SaleOrderDelivery {
    /// Selected shipping carrier for this quotation / sales order.
    #[field(
        many2one,
        comodel = "delivery.carrier",
        check_company = true,
        string = "Delivery Method"
    )]
    pub carrier_id: Option<RecId>,
    /// Last computed shipping cost for the selected carrier.
    #[field(float, digits = "Product Price", default = 0, string = "Shipping Cost")]
    pub delivery_price: Decimal,
}

#[methods(SaleOrder, addon = "stock_delivery")]
impl SaleOrderDelivery {
    /// Compute the carrier shipping charge from the order lines and add or update the
    /// delivery service line on the sales order.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_add_shipping(
        this: &RecordSet<SaleOrder>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let records = this.as_records().clone();
        records.check_access(tx, Operation::Write).await?;
        let order_id = records.ensure_one()?.int()?;
        let env = records.env().clone();
        let uid = env.uid();
        tx.savepoint(async move |tx| {
            tx.flush().await?;
            let order_rows = tx
                .query(
                    "SELECT state, locked, carrier_id::int8, currency_id::int8, company_id::int8 \
                     FROM sale_order WHERE id = $1 FOR UPDATE",
                    &[&order_id],
                )
                .await?;
            let Some(orow) = order_rows.first() else {
                return Err(Error::missing("Sales order not found."));
            };
            let state: String = orow.try_get(0).map_err(db_err)?;
            let locked: bool = orow.try_get(1).map_err(db_err)?;
            if !matches!(state.as_str(), "draft" | "sent") || locked {
                return Err(Error::user(
                    "Shipping lines can only be added to unlocked draft or sent quotations.",
                ));
            }
            let carrier_id: Option<i64> = orow.try_get(2).map_err(db_err)?;
            let currency_id: Option<i64> = orow.try_get(3).map_err(db_err)?;
            let company_id: i64 = orow.try_get(4).map_err(db_err)?;
            let Some(carrier_id) = carrier_id else {
                return Err(Error::user("Select a delivery method first."));
            };
            let carrier = load_carrier_pricing(tx, carrier_id).await?;
            let scale = currency_scale(tx, currency_id, company_id).await?;
            let carrier_prod_rows = tx
                .query(
                    "SELECT product_id::int8, name FROM delivery_carrier WHERE id = $1",
                    &[&carrier_id],
                )
                .await?;
            let cprow = carrier_prod_rows
                .first()
                .ok_or_else(|| Error::missing("Delivery carrier not found."))?;
            let delivery_product_id: i64 = cprow.try_get(0).map_err(db_err)?;
            let carrier_name: String = cprow.try_get(1).map_err(db_err)?;

            let m_rows = tx
                .query(
                    "SELECT COALESCE(sum(base_qty * COALESCE(pt.weight, 0)), 0), \
                            COALESCE(sum(base_qty * COALESCE(pt.volume, 0)), 0), \
                            COALESCE(sum(price_subtotal), 0), \
                            COALESCE(sum(base_qty), 0) \
                     FROM ( \
                       SELECT l.product_id, \
                              l.price_subtotal, \
                              CASE WHEN lu.factor IS NOT NULL AND pu.factor IS NOT NULL AND pu.factor <> 0 \
                                   THEN l.product_uom_qty * lu.factor / pu.factor \
                                   ELSE l.product_uom_qty END AS base_qty \
                       FROM sale_order_line l \
                       JOIN product_product pp ON pp.id = l.product_id \
                       JOIN product_template pt ON pt.id = pp.product_tmpl_id \
                       LEFT JOIN uom_uom lu ON lu.id = l.product_uom_id \
                       LEFT JOIN uom_uom pu ON pu.id = pt.uom_id \
                       WHERE l.order_id = $1 AND l.product_id <> $2 AND l.display_type IS NULL \
                     ) lines \
                     JOIN product_product pp ON pp.id = lines.product_id \
                     JOIN product_template pt ON pt.id = pp.product_tmpl_id",
                    &[&order_id, &delivery_product_id],
                )
                .await?;
            let mrow = m_rows
                .first()
                .ok_or_else(|| Error::internal("Sales order shipment metrics returned no row."))?;
            let metrics = ShipmentMetrics {
                weight: mrow.try_get(0).map_err(db_err)?,
                volume: mrow.try_get(1).map_err(db_err)?,
                order_price: mrow.try_get(2).map_err(db_err)?,
                quantity: mrow.try_get(3).map_err(db_err)?,
            };
            let price = rate_shipment(&carrier, &metrics, scale)?;

            // Update existing delivery line if present, otherwise create one.
            let existing = tx
                .query(
                    "SELECT id::int8 FROM sale_order_line \
                     WHERE order_id = $1 AND product_id = $2 LIMIT 1",
                    &[&order_id, &delivery_product_id],
                )
                .await?;
            if let Some(erow) = existing.first() {
                let lid: i64 = erow.try_get(0).map_err(db_err)?;
                env.records("sale.order.line")?
                    .browse(vec![lid])?
                    .write(
                        tx,
                        RawVals::new()
                            .set_cell("price_unit", Cell::Decimal(price))
                            .set_cell("product_uom_qty", Cell::Decimal(Decimal::ONE)),
                    )
                    .await?;
            } else {
                let mut vals = RawVals::new();
                vals.insert("order_id".into(), Cell::Id(order_id));
                vals.insert("product_id".into(), Cell::Id(delivery_product_id));
                vals.insert("name".into(), Cell::Str(carrier_name.into()));
                vals.insert("product_uom_qty".into(), Cell::Decimal(Decimal::ONE));
                vals.insert("price_unit".into(), Cell::Decimal(price));
                env.records("sale.order.line")?
                    .create(tx, vec![vals])
                    .await?;
            }

            tx.execute(
                "UPDATE sale_order \
                 SET delivery_price = $2, write_uid = $3, write_date = (now() AT TIME ZONE 'UTC') \
                 WHERE id = $1",
                &[&order_id, &price, &uid],
            )
            .await?;
            records.modified_external(tx, &["delivery_price"]).await?;
            Ok(())
        })
        .await
    }
}
