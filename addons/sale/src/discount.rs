// SPDX-License-Identifier: MIT OR Apache-2.0
//! Sales Order Discount wizard (`sale.order.discount`): line percentage discount, global
//! percentage discount line, or fixed discount amount line.

use std::collections::BTreeMap;

use erp_addon_base::money::{CURRENCY_ROUNDING_MODE, RoundingTable};
use erp_sdk::prelude::*;
use erp_sdk::serde_json::json;
use erp_sdk::{Command, Operation};

use crate::order::{SaleOrder, strip_default_context};

/// Pure exact-decimal calculation for a global percentage discount on an order's untaxed amount.
pub fn compute_global_discount(untaxed_amount: Decimal, percent: Decimal) -> Result<Decimal> {
    if percent <= Decimal::ZERO || percent > Decimal::from(100) {
        return Err(Error::user(
            "Discount percentage must be greater than 0 and at most 100.",
        ));
    }
    if untaxed_amount <= Decimal::ZERO {
        return Err(Error::user(
            "Add at least one positive order line before applying a percentage discount.",
        ));
    }
    let raw = untaxed_amount
        .checked_mul(percent)
        .and_then(|v| v.checked_div(Decimal::from(100)))
        .ok_or_else(|| Error::validation("Discount calculation overflowed."))?;
    Ok(raw.round_dp(2))
}

/// `sale.order.discount`: transient dialog opened from a Sales Order to apply discounts.
#[model(
    name = "sale.order.discount",
    description = "Apply Discount",
    kind = "transient",
    transient_max_age_seconds = 3600,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct SaleOrderDiscount {
    /// Target quotation or sales order.
    #[field(
        many2one,
        comodel = "sale.order",
        required = true,
        ondelete = "cascade",
        string = "Sales Order"
    )]
    pub sale_order_id: Option<RecId>,
    /// Order company.
    #[field(
        many2one,
        comodel = "res.company",
        related = "sale_order_id.company_id",
        store = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Order currency.
    #[field(
        many2one,
        comodel = "res.currency",
        related = "sale_order_id.currency_id",
        store = true,
        string = "Currency"
    )]
    pub currency_id: Option<RecId>,
    /// How the discount is applied.
    #[field(
        selection,
        selection = [
            ("sol_discount", "On All Order Lines (%)"),
            ("so_discount", "Global Discount (%)"),
            ("amount", "Fixed Amount"),
        ],
        required = true,
        default = "sol_discount",
        string = "Discount Type"
    )]
    pub discount_type: Option<Arc<str>>,
    /// Discount percentage (used when `discount_type` is `sol_discount` or `so_discount`).
    #[field(float, digits = (16, 2), default = 10, string = "Discount (%)")]
    pub discount_percentage: Decimal,
    /// Fixed discount amount (used when `discount_type` is `amount`).
    #[field(monetary, currency_field = "currency_id", string = "Amount")]
    pub discount_amount: Decimal,
}

#[methods(SaleOrderDiscount, addon = "sale")]
impl SaleOrderDiscount {
    #[method]
    #[rpc(effect = write)]
    pub async fn action_apply_discount(
        this: &RecordSet<SaleOrderDiscount>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let wizard = this.ensure_one()?;
        wizard
            .as_records()
            .check_access(tx, Operation::Write)
            .await?;
        let order_id = wizard
            .get(tx, SaleOrderDiscount::SALE_ORDER_ID)
            .await?
            .ok_or_else(|| Error::validation("Choose a sales order."))?
            .get();
        let kind = wizard
            .get(tx, SaleOrderDiscount::DISCOUNT_TYPE)
            .await?
            .unwrap_or_else(|| Arc::from("sol_discount"));
        let pct = wizard
            .get(tx, SaleOrderDiscount::DISCOUNT_PERCENTAGE)
            .await?;
        let fixed_amt = wizard.get(tx, SaleOrderDiscount::DISCOUNT_AMOUNT).await?;

        let env = strip_default_context(wizard.env());
        let order_rec = env.records("sale.order")?.browse([order_id])?;
        order_rec.check_access(tx, Operation::Write).await?;

        let msg = tx
            .savepoint(async |tx| {
                tx.flush().await?;
                order_rec.lock_for_update(tx, true).await?;

                let state = match order_rec.cell(tx, "state").await? {
                    Cell::Str(s) => s.to_string(),
                    other => {
                        return Err(Error::invariant(format!(
                            "sale.order.state returned {other:?}"
                        )));
                    }
                };
                if state != "draft" && state != "sent" {
                    return Err(Error::user(
                        "Discounts can only be applied to draft or sent quotations.",
                    ));
                }
                if order_rec.cell(tx, "locked").await? == Cell::Bool(true) {
                    return Err(Error::user(
                        "This sales order is locked and cannot be discounted.",
                    ));
                }
                let currency_id = match order_rec.cell(tx, "currency_id").await? {
                    Cell::Id(id) => id,
                    other => {
                        return Err(Error::invariant(format!(
                            "Sales order {order_id} has no valid currency ({other:?})."
                        )));
                    }
                };
                let cur_rec = RecId::from_real(currency_id)
                    .ok_or_else(|| Error::invariant("Invalid order currency id."))?;
                let rounding = RoundingTable::load(&env, tx, &[cur_rec]).await?;
                let inc = rounding.get(cur_rec)?.increment();

                let msg = match kind.as_ref() {
                    "sol_discount" => {
                        if pct < Decimal::ZERO || pct > Decimal::from(100) {
                            return Err(Error::user(
                                "Discount percentage must be between 0 and 100.",
                            ));
                        }
                        let lines = env
                            .records("sale.order.line")?
                            .search(
                                tx,
                                &Domain::and([
                                    Domain::condition("order_id", Operator::Eq, order_id)?,
                                    Domain::condition("display_type", Operator::Eq, false)?,
                                ]),
                            )
                            .await?;
                        if lines.is_empty() {
                            return Err(Error::user(
                                "Add at least one product line before applying a line discount.",
                            ));
                        }
                        let mut vals = RawVals::new();
                        vals.insert("discount".into(), Cell::Decimal(pct));
                        lines.write(tx, vals).await?;
                        format!("Applied {pct}% discount to all order lines.")
                    }
                    "so_discount" => {
                        if pct <= Decimal::ZERO || pct > Decimal::from(100) {
                            return Err(Error::user(
                                "Discount percentage must be greater than 0 and at most 100.",
                            ));
                        }
                        let line_rows = tx
                            .query(
                                "SELECT l.id::int8, COALESCE(l.price_subtotal, 0), r.account_tax_id::int8 \
                                 FROM sale_order_line l \
                                 LEFT JOIN account_tax_sale_order_line_rel r ON r.sale_order_line_id = l.id \
                                 WHERE l.order_id = $1 AND l.display_type IS NULL \
                                   AND NOT COALESCE(l.is_downpayment, false) \
                                 ORDER BY l.id, r.account_tax_id",
                                &[&order_id],
                            )
                            .await?;
                        let mut per_line: BTreeMap<i64, (Decimal, Vec<i64>)> = BTreeMap::new();
                        for row in line_rows {
                            let lid: i64 = row
                                .try_get(0)
                                .map_err(|e| Error::db(format!("discount lines: {e}")))?;
                            let sub: Decimal = row
                                .try_get(1)
                                .map_err(|e| Error::db(format!("discount lines: {e}")))?;
                            let tid: Option<i64> = row
                                .try_get(2)
                                .map_err(|e| Error::db(format!("discount lines: {e}")))?;
                            let entry = per_line.entry(lid).or_insert((sub, Vec::new()));
                            if let Some(t) = tid {
                                entry.1.push(t);
                            }
                        }
                        let mut by_tax: BTreeMap<Vec<i64>, Decimal> = BTreeMap::new();
                        for (sub, taxes) in per_line.into_values() {
                            *by_tax.entry(taxes).or_insert(Decimal::ZERO) += sub;
                        }
                        let uom_id = default_unit_uom_id(tx).await?;
                        let mut new_lines = Vec::new();
                        let mut total_disc = Decimal::ZERO;
                        for (taxes, base) in by_tax {
                            if base <= Decimal::ZERO {
                                continue;
                            }
                            let raw_disc = compute_global_discount(base, pct)?;
                            let disc = inc.quantize(raw_disc, CURRENCY_ROUNDING_MODE)?;
                            if disc.is_zero() {
                                continue;
                            }
                            total_disc += disc;
                            let mut lvals = RawVals::new();
                            lvals.insert("order_id".into(), Cell::Id(order_id));
                            lvals.insert(
                                "name".into(),
                                Cell::Str(format!("Global Discount ({pct}%)").into()),
                            );
                            lvals.insert("product_uom_id".into(), Cell::Id(uom_id));
                            lvals.insert("product_uom_qty".into(), Cell::Decimal(Decimal::ONE));
                            lvals.insert("price_unit".into(), Cell::Decimal(-disc));
                            lvals.insert_commands(
                                "tax_ids".into(),
                                vec![Command::set(
                                    taxes.iter().copied().filter_map(RecId::from_real),
                                )],
                            );
                            new_lines.push(lvals);
                        }
                        if new_lines.is_empty() {
                            return Err(Error::user(
                                "Add at least one positive order line before applying a percentage discount.",
                            ));
                        }
                        env.records("sale.order.line")?
                            .create(tx, new_lines)
                            .await?;
                        format!("Added global discount line (-{total_disc}).")
                    }
                    "amount" => {
                        if fixed_amt <= Decimal::ZERO {
                            return Err(Error::user(
                                "Fixed discount amount must be greater than zero.",
                            ));
                        }
                        let disc = inc.quantize(fixed_amt, CURRENCY_ROUNDING_MODE)?;
                        let uom_id = default_unit_uom_id(tx).await?;
                        let mut lvals = RawVals::new();
                        lvals.insert("order_id".into(), Cell::Id(order_id));
                        lvals.insert("name".into(), Cell::Str("Fixed Discount".into()));
                        lvals.insert("product_uom_id".into(), Cell::Id(uom_id));
                        lvals.insert("product_uom_qty".into(), Cell::Decimal(Decimal::ONE));
                        lvals.insert("price_unit".into(), Cell::Decimal(-disc));
                        lvals.insert_commands("tax_ids".into(), vec![Command::clear()]);
                        env.records("sale.order.line")?
                            .create(tx, vec![lvals])
                            .await?;
                        format!("Added fixed discount line (-{disc}).")
                    }
                    other => {
                        return Err(Error::validation(format!("Unknown discount type: {other}")));
                    }
                };

                wizard.as_records().unlink(tx).await?;
                Ok(msg)
            })
            .await?;

        Ok(Value::Json(Arc::new(json!({
            "type": "ir.actions.client",
            "tag": "display_notification",
            "params": {
                "message": msg,
                "type": "success",
                "next": {"type": "ir.actions.act_window_close"},
            },
        }))))
    }
}

/// Resolves the seeded default unit of measure (`uom.product_uom_unit`) without arbitrary
/// fallbacks to unrelated units.
pub(crate) async fn default_unit_uom_id(tx: &mut Transaction) -> Result<i64> {
    erp_addon_product::defaults::default_unit_id(tx).await
}

#[methods(SaleOrder, addon = "sale")]
impl SaleOrder {
    /// Opens the Discount wizard dialog for this sales order.
    #[method]
    #[rpc(effect = read)]
    pub async fn action_open_discount_wizard(
        this: &RecordSet<SaleOrder>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let rec = this.ensure_one()?;
        rec.as_records().check_access(tx, Operation::Write).await?;
        let oid = rec.as_records().ids()[0];
        Ok(Value::Json(Arc::new(json!({
            "type": "ir.actions.act_window",
            "name": "Discount",
            "res_model": "sale.order.discount",
            "target": "new",
            "views": [[false, "form"]],
            "context": {"default_sale_order_id": oid},
        }))))
    }
}

#[cfg(test)]
mod tests {
    use super::compute_global_discount;
    use erp_sdk::Decimal;
    use std::str::FromStr;

    #[test]
    fn global_discount_uses_exact_decimals_and_rejects_invalid_percent() {
        let untaxed = Decimal::from_str("250.50").unwrap();
        let disc = compute_global_discount(untaxed, Decimal::from_str("15").unwrap()).unwrap();
        assert_eq!(disc, Decimal::from_str("37.58").unwrap());
        assert!(compute_global_discount(untaxed, Decimal::from_str("120").unwrap()).is_err());
        assert!(compute_global_discount(untaxed, Decimal::ZERO).is_err());
    }
}
