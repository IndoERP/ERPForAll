// SPDX-License-Identifier: MIT OR Apache-2.0
//! Profit margins on sales order lines and orders (`purchase_price`, `margin`, `margin_percent`).

use std::collections::{BTreeMap, BTreeSet};

use erp_addon_base::money::{CURRENCY_ROUNDING_MODE, RoundingTable};
use erp_addon_product::ProductProduct;
use erp_addon_product::product::ProductProductMethods;
use erp_addon_uom::conversion::price_exact;
use erp_addon_uom::models::load_unit;
use erp_sdk::prelude::*;

use crate::line::SaleOrderLine;
use crate::order::SaleOrder;

/// Pure exact-decimal margin calculation: returns `(margin, margin_percent)`.
pub fn compute_line_margin(
    subtotal: Decimal,
    quantity: Decimal,
    purchase_price: Decimal,
) -> (Decimal, Decimal) {
    let cost_total = purchase_price * quantity;
    let margin = (subtotal - cost_total).round_dp(2);
    let pct = if subtotal > Decimal::ZERO {
        ((margin * Decimal::from(100)) / subtotal).round_dp(2)
    } else {
        Decimal::ZERO
    };
    (margin, pct)
}

/// Pure exact-decimal order margin percentage helper.
pub fn compute_order_margin_percent(margin: Decimal, amount_untaxed: Decimal) -> Decimal {
    if amount_untaxed > Decimal::ZERO {
        ((margin * Decimal::from(100)) / amount_untaxed).round_dp(2)
    } else {
        Decimal::ZERO
    }
}

#[methods(SaleOrderLine, addon = "sale")]
impl SaleOrderLine {
    /// Default unit cost from the product's standard cost (Hook N6, company-scoped), converted
    /// to the line's UoM and order currency.
    #[compute(depends = [
        "product_id",
        "product_uom_id",
        "order_id.company_id",
        "order_id.currency_id",
        "order_id.currency_rate",
    ])]
    async fn _compute_purchase_price(
        this: &RecordSet<SaleOrderLine>,
        tx: &mut Transaction,
        out: &mut Assign<'_, SaleOrderLine>,
    ) -> Result<()> {
        if this.is_empty() {
            return Ok(());
        }
        tx.flush().await?;
        let products = this.mapped(tx, SaleOrderLine::PRODUCT_ID).await?;
        let line_uoms = this.mapped(tx, SaleOrderLine::PRODUCT_UOM_ID).await?;
        let orders = this.mapped(tx, SaleOrderLine::ORDER_ID).await?;
        let env = this.env().clone();

        // Batch-read order company, currency, and rate.
        let order_ids: Vec<RecId> = orders
            .iter()
            .flatten()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut order_meta: BTreeMap<RecId, (Option<RecId>, Option<RecId>, Decimal)> =
            BTreeMap::new();
        let mut cur_ids: BTreeSet<RecId> = BTreeSet::new();
        if !order_ids.is_empty() {
            let ord_recs = env.records("sale.order")?.browse_ids(order_ids.clone());
            let companies = ord_recs.cells(tx, "company_id").await?;
            let currencies = ord_recs.cells(tx, "currency_id").await?;
            let rates = ord_recs.cells(tx, "currency_rate").await?;
            for (((oid, comp), cur), rate) in order_ids
                .into_iter()
                .zip(companies)
                .zip(currencies)
                .zip(rates)
            {
                let cid = match comp {
                    Cell::Id(id) => RecId::from_real(id),
                    Cell::Null => None,
                    other => {
                        return Err(Error::programming(format!(
                            "sale.order.company_id returned {other:?}"
                        )));
                    }
                };
                let cur_id = match cur {
                    Cell::Id(id) => RecId::from_real(id),
                    Cell::Null => None,
                    other => {
                        return Err(Error::programming(format!(
                            "sale.order.currency_id returned {other:?}"
                        )));
                    }
                };
                if let Some(c) = cur_id {
                    cur_ids.insert(c);
                }
                let r = match rate {
                    Cell::Decimal(d) if d > Decimal::ZERO => d,
                    Cell::Decimal(_) | Cell::Null => Decimal::ONE,
                    other => {
                        return Err(Error::programming(format!(
                            "sale.order.currency_rate returned {other:?}"
                        )));
                    }
                };
                order_meta.insert(oid, (cid, cur_id, r));
            }
        }
        let cur_vec: Vec<RecId> = cur_ids.into_iter().collect();
        let rounding = if cur_vec.is_empty() {
            None
        } else {
            Some(RoundingTable::load(&env, tx, &cur_vec).await?)
        };

        // Batch-read product base UoMs and company-scoped standard costs (Hook N6).
        let prod_ids: Vec<RecId> = products
            .iter()
            .flatten()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut prod_base_uom: BTreeMap<RecId, Option<i64>> = BTreeMap::new();
        if !prod_ids.is_empty() {
            let prod_recs = env.records("product.product")?.browse_ids(prod_ids.clone());
            let uoms = prod_recs.cells(tx, "uom_id").await?;
            for (pid, cell) in prod_ids.into_iter().zip(uoms) {
                let u = match cell {
                    Cell::Id(id) => Some(id),
                    Cell::Null => None,
                    other => {
                        return Err(Error::programming(format!(
                            "product.product.uom_id returned {other:?}"
                        )));
                    }
                };
                prod_base_uom.insert(pid, u);
            }
        }

        let mut by_company: BTreeMap<Option<i64>, BTreeSet<RecId>> = BTreeMap::new();
        for (prod, order) in products.iter().zip(&orders) {
            if let Some(pid) = prod {
                let cid = order
                    .as_ref()
                    .and_then(|o| order_meta.get(o))
                    .and_then(|(c, _, _)| *c)
                    .map(RecId::get);
                by_company.entry(cid).or_default().insert(*pid);
            }
        }
        let mut std_costs: BTreeMap<(Option<i64>, RecId), Decimal> = BTreeMap::new();
        for (cid, pids) in by_company {
            let scoped_env = match cid {
                Some(c) => env.with_company(c)?,
                None => env.clone(),
            };
            let pid_vec: Vec<RecId> = pids.into_iter().collect();
            let recs = scoped_env
                .records("product.product")?
                .browse_ids(pid_vec.clone());
            let typed = RecordSet::<ProductProduct>::try_from(recs)?;
            let costs = typed._get_standard_price(tx).await?;
            for (pid, cost) in pid_vec.into_iter().zip(costs) {
                std_costs.insert((cid, pid), cost);
            }
        }

        let price_digits =
            erp_addon_base::decimal_precision::precision_get(&env, tx, "Product Price").await?;
        let price_inc = erp_sdk::money::Increment::from_places(price_digits)?;

        let mut unit_cache = BTreeMap::new();
        for (index, ((prod, line_uom), order)) in
            products.iter().zip(&line_uoms).zip(&orders).enumerate()
        {
            let Some(pid) = prod else {
                out.set(index, SaleOrderLine::PURCHASE_PRICE, Decimal::ZERO)?;
                continue;
            };
            let (cid_rec, cur_rec, rate) = order
                .as_ref()
                .and_then(|o| order_meta.get(o))
                .copied()
                .unwrap_or((None, None, Decimal::ONE));
            let cid = cid_rec.map(RecId::get);
            let mut unit_cost = std_costs
                .get(&(cid, *pid))
                .copied()
                .unwrap_or(Decimal::ZERO);

            if let (Some(base_uom), Some(target_uom)) = (
                prod_base_uom.get(pid).copied().flatten(),
                line_uom.map(RecId::get),
            ) && base_uom != target_uom
            {
                if let std::collections::btree_map::Entry::Vacant(e) = unit_cache.entry(base_uom) {
                    e.insert(load_unit(&env, tx, base_uom).await?);
                }
                if let std::collections::btree_map::Entry::Vacant(e) = unit_cache.entry(target_uom)
                {
                    e.insert(load_unit(&env, tx, target_uom).await?);
                }
                let src = &unit_cache[&base_uom];
                let dst = &unit_cache[&target_uom];
                unit_cost = price_exact(src, dst, unit_cost)?
                    .quantize(&price_inc, CURRENCY_ROUNDING_MODE)?;
            }

            let converted = unit_cost
                .checked_mul(rate)
                .ok_or_else(|| Error::validation("Margin cost conversion overflowed."))?;
            let cost = if let (Some(table), Some(cur)) = (&rounding, cur_rec) {
                table
                    .get(cur)?
                    .increment()
                    .quantize(converted, CURRENCY_ROUNDING_MODE)?
            } else {
                converted.round_dp(2)
            };
            out.set(index, SaleOrderLine::PURCHASE_PRICE, cost)?;
        }
        Ok(())
    }

    /// Exact-decimal line margin and margin percentage.
    #[compute(depends = ["price_subtotal", "product_uom_qty", "purchase_price", "display_type", "is_downpayment"])]
    async fn _compute_margin(
        this: &RecordSet<SaleOrderLine>,
        tx: &mut Transaction,
        out: &mut Assign<'_, SaleOrderLine>,
    ) -> Result<()> {
        let subtotals = this.mapped(tx, SaleOrderLine::PRICE_SUBTOTAL).await?;
        let qtys = this.mapped(tx, SaleOrderLine::PRODUCT_UOM_QTY).await?;
        let costs = this.mapped(tx, SaleOrderLine::PURCHASE_PRICE).await?;
        let kinds = this.mapped(tx, SaleOrderLine::DISPLAY_TYPE).await?;
        let dps = this.mapped(tx, SaleOrderLine::IS_DOWNPAYMENT).await?;
        for (index, ((((sub, qty), cost), kind), is_dp)) in subtotals
            .into_iter()
            .zip(qtys)
            .zip(costs)
            .zip(kinds)
            .zip(dps)
            .enumerate()
        {
            if kind.is_some() || is_dp {
                out.set(index, SaleOrderLine::MARGIN, Decimal::ZERO)?;
                out.set(index, SaleOrderLine::MARGIN_PERCENT, Decimal::ZERO)?;
            } else {
                let (m, pct) = compute_line_margin(sub, qty, cost);
                out.set(index, SaleOrderLine::MARGIN, m)?;
                out.set(index, SaleOrderLine::MARGIN_PERCENT, pct)?;
            }
        }
        Ok(())
    }
}

#[methods(SaleOrder, addon = "sale")]
impl SaleOrder {
    /// Total exact-decimal profit margin across all accountable product lines.
    #[compute(depends = ["order_line.margin", "order_line.price_subtotal", "order_line.purchase_price", "amount_untaxed", "currency_id"])]
    async fn _compute_margin(
        this: &RecordSet<SaleOrder>,
        tx: &mut Transaction,
        out: &mut Assign<'_, SaleOrder>,
    ) -> Result<()> {
        if this.is_empty() {
            return Ok(());
        }
        tx.flush().await?;
        let untaxed = this.mapped(tx, SaleOrder::AMOUNT_UNTAXED).await?;
        let currencies = this.mapped(tx, SaleOrder::CURRENCY_ID).await?;
        let ids = this.as_records().ids();
        let cur_vec: Vec<RecId> = currencies
            .iter()
            .flatten()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let rounding = if cur_vec.is_empty() {
            None
        } else {
            Some(RoundingTable::load(this.env(), tx, &cur_vec).await?)
        };
        let rows = if ids.is_empty() {
            Vec::new()
        } else {
            tx.query(
                "SELECT order_id::int8, \
                        COALESCE(SUM(COALESCE(price_subtotal, 0) - COALESCE(purchase_price, 0) * COALESCE(product_uom_qty, 0)), 0) \
                 FROM sale_order_line \
                 WHERE order_id = ANY($1) AND display_type IS NULL AND NOT COALESCE(is_downpayment, false) \
                 GROUP BY order_id",
                &[&ids],
            )
            .await?
        };
        let mut by_order = BTreeMap::new();
        for row in rows {
            let oid: i64 = row
                .try_get(0)
                .map_err(|e| Error::db(format!("margin sum: {e}")))?;
            let total: Decimal = row
                .try_get(1)
                .map_err(|e| Error::db(format!("margin sum: {e}")))?;
            by_order.insert(oid, total);
        }
        for (index, ((oid, unt), cur)) in ids.iter().zip(untaxed).zip(currencies).enumerate() {
            let raw = by_order.get(oid).copied().unwrap_or(Decimal::ZERO);
            let margin = if let (Some(table), Some(c)) = (&rounding, cur) {
                table
                    .get(c)?
                    .increment()
                    .quantize(raw, CURRENCY_ROUNDING_MODE)?
            } else {
                raw.round_dp(2)
            };
            let pct = compute_order_margin_percent(margin, unt);
            out.set(index, SaleOrder::MARGIN, margin)?;
            out.set(index, SaleOrder::MARGIN_PERCENT, pct)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{compute_line_margin, compute_order_margin_percent};
    use erp_sdk::Decimal;
    use std::str::FromStr;

    #[test]
    fn margin_calculation_is_exact_decimal() {
        let (margin, pct) = compute_line_margin(
            Decimal::from_str("250.00").unwrap(),
            Decimal::from(5),
            Decimal::from_str("32.50").unwrap(),
        );
        // cost = 162.50, margin = 87.50, pct = 87.50 / 250.00 = 35.00%
        assert_eq!(margin, Decimal::from_str("87.50").unwrap());
        assert_eq!(pct, Decimal::from_str("35.00").unwrap());

        let order_pct = compute_order_margin_percent(
            Decimal::from_str("87.50").unwrap(),
            Decimal::from_str("250.00").unwrap(),
        );
        assert_eq!(order_pct, Decimal::from_str("35.00").unwrap());
        assert_eq!(
            compute_order_margin_percent(Decimal::ZERO, Decimal::ZERO),
            Decimal::ZERO
        );
    }
}
