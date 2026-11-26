// SPDX-License-Identifier: MIT OR Apache-2.0
//! Vendor On-Time Delivery Rate (`on_time_rate` and `purchase_receipt_done_count` on
//! `res.partner`), computed from completed purchase receipts (`purchase.stock.receipt.link`).

use std::collections::BTreeMap;

use erp_addon_base::ResPartner;
use erp_sdk::Operation;
use erp_sdk::prelude::*;

fn db(err: impl std::fmt::Display) -> Error {
    Error::db(format!("Vendor on-time delivery query failed: {err}"))
}

/// Pure exact-decimal on-time delivery percentage (`0.0`..=`100.0`, rounded to 1 decimal place).
pub fn compute_on_time_rate(on_time_count: i64, total_done_count: i64) -> Decimal {
    if total_done_count <= 0 {
        return Decimal::ZERO;
    }
    let clamped = on_time_count.clamp(0, total_done_count);
    ((Decimal::from(clamped) * Decimal::from(100)) / Decimal::from(total_done_count)).round_dp(1)
}

/// Extension of `res.partner` exposing vendor delivery reliability metrics.
#[extend(ResPartner, addon = "purchase_stock")]
pub struct ResPartnerPurchaseOnTime {
    /// Percentage of completed purchase receipts delivered on or before the scheduled date.
    #[field(
        float,
        digits = (16, 1),
        compute = "_compute_on_time_rate",
        groups = "purchase.group_purchase_user",
        string = "On-Time Delivery (%)"
    )]
    pub on_time_rate: Decimal,
    /// Total number of completed purchase receipts from this vendor.
    #[field(
        integer,
        compute = "_compute_on_time_rate",
        groups = "purchase.group_purchase_user",
        string = "Completed Receipts"
    )]
    pub purchase_receipt_done_count: i32,
}

#[methods(ResPartner, addon = "purchase_stock")]
impl ResPartnerPurchaseOnTime {
    #[compute(depends = [], depends_context = ["uid", "allowed_company_ids"])]
    async fn _compute_on_time_rate(
        this: &RecordSet<ResPartner>,
        tx: &mut Transaction,
        out: &mut Assign<'_, ResPartner>,
    ) -> Result<()> {
        tx.flush().await?;
        let env = this.env().clone();
        let rec_ids = this.as_records().rec_ids();
        let real_ids: Vec<i64> = rec_ids
            .iter()
            .filter_map(|r| r.as_real().or(r.origin()).map(|id| id.get()))
            .collect();
        let mut stats: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
        if !real_ids.is_empty() {
            // Restrict to purchase orders visible to the caller (ACL + allowed_company_ids).
            let visible_orders = env
                .records("purchase.order")?
                .search(
                    tx,
                    &Domain::condition("partner_id", Operator::In, real_ids.clone())?,
                )
                .await?;
            let order_ids = visible_orders.ids();
            if !order_ids.is_empty() {
                // Include linked receipts and any backorder pickings chained from them.
                let cand_rows = tx
                    .query(
                        "WITH RECURSIVE receipt_picks AS ( \
                             SELECT l.order_id::int8 AS order_id, l.picking_id::int8 AS picking_id \
                             FROM purchase_stock_receipt_link l \
                             WHERE l.order_id = ANY($1) \
                             UNION \
                             SELECT rp.order_id, p.id::int8 \
                             FROM stock_picking p \
                             JOIN receipt_picks rp ON p.backorder_id = rp.picking_id \
                         ) \
                         SELECT DISTINCT rp.picking_id, o.partner_id::int8, \
                                (p.date_done <= COALESCE(p.scheduled_date, p.date_done)) AS on_time \
                         FROM receipt_picks rp \
                         JOIN purchase_order o ON o.id = rp.order_id \
                         JOIN stock_picking p ON p.id = rp.picking_id \
                         WHERE p.state = 'done'",
                        &[&order_ids],
                    )
                    .await?;
                if !cand_rows.is_empty() {
                    let pick_ids: Vec<i64> = cand_rows
                        .iter()
                        .map(|r| r.try_get::<_, i64>(0).map_err(db))
                        .collect::<Result<_>>()?;
                    let allowed_picks: std::collections::BTreeSet<i64> = env
                        .records("stock.picking")?
                        .browse(pick_ids)?
                        .filtered_access(tx, Operation::Read)
                        .await?
                        .ids()
                        .into_iter()
                        .collect();
                    for row in cand_rows {
                        let pick_id: i64 = row.try_get(0).map_err(db)?;
                        if !allowed_picks.contains(&pick_id) {
                            continue;
                        }
                        let pid: i64 = row.try_get(1).map_err(db)?;
                        let on_time: bool = row.try_get(2).map_err(db)?;
                        let entry = stats.entry(pid).or_insert((0, 0));
                        if on_time {
                            entry.0 += 1;
                        }
                        entry.1 += 1;
                    }
                }
            }
        }
        for (i, rec_id) in rec_ids.iter().enumerate() {
            let (on_time, total) = rec_id
                .as_real()
                .or(rec_id.origin())
                .and_then(|id| stats.get(&id.get()).copied())
                .unwrap_or((0, 0));
            let rate = compute_on_time_rate(on_time, total);
            let done_i32 = i32::try_from(total).map_err(|_| {
                Error::invariant(format!(
                    "Vendor receipt done count {total} exceeds i32 range."
                ))
            })?;
            out.set(i, ResPartnerPurchaseOnTime::ON_TIME_RATE, rate)?;
            out.set(
                i,
                ResPartnerPurchaseOnTime::PURCHASE_RECEIPT_DONE_COUNT,
                done_i32,
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::compute_on_time_rate;
    use erp_sdk::Decimal;
    use std::str::FromStr;

    #[test]
    fn vendor_on_time_rate_is_exact_decimal() {
        assert_eq!(
            compute_on_time_rate(3, 4),
            Decimal::from_str("75.0").unwrap()
        );
        assert_eq!(
            compute_on_time_rate(2, 3),
            Decimal::from_str("66.7").unwrap()
        );
        assert_eq!(compute_on_time_rate(0, 0), Decimal::ZERO);
    }
}
