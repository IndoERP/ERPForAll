// SPDX-License-Identifier: MIT OR Apache-2.0
//! Down-payment and invoicing wizard (`sale.advance.payment.inv`) for confirmed sales orders.

use erp_addon_base::money::{CURRENCY_ROUNDING_MODE, RoundingTable};
use erp_sdk::prelude::*;
use erp_sdk::serde_json::json;
use erp_sdk::{Command, Operation, ValueMap};

use crate::discount::default_unit_uom_id;
use crate::order::{SaleOrder, strip_default_context};

/// Pure exact-decimal helper computing a down-payment amount from the order total.
pub fn compute_down_payment_amount(
    order_total: Decimal,
    method: &str,
    percentage: Decimal,
    fixed_amount: Decimal,
) -> Result<Decimal> {
    match method {
        "percentage" => {
            if percentage <= Decimal::ZERO || percentage > Decimal::from(100) {
                return Err(Error::user(
                    "Down payment percentage must be greater than 0 and at most 100.",
                ));
            }
            if order_total <= Decimal::ZERO {
                return Err(Error::user(
                    "Cannot compute a percentage down payment on a zero-total order.",
                ));
            }
            let raw = order_total
                .checked_mul(percentage)
                .and_then(|v| v.checked_div(Decimal::from(100)))
                .ok_or_else(|| Error::validation("Down payment calculation overflowed."))?;
            Ok(raw.round_dp(2))
        }
        "fixed" => {
            if fixed_amount <= Decimal::ZERO {
                return Err(Error::user(
                    "Down payment fixed amount must be greater than zero.",
                ));
            }
            Ok(fixed_amount.round_dp(2))
        }
        other => Err(Error::validation(format!(
            "Unsupported down payment method: {other}"
        ))),
    }
}

/// `sale.advance.payment.inv`: transient wizard to create a regular invoice or a down-payment
/// invoice (percentage or fixed amount) from a confirmed sales order.
#[model(
    name = "sale.advance.payment.inv",
    description = "Sales Advance Payment Invoice",
    kind = "transient",
    transient_max_age_seconds = 3600,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct SaleAdvancePaymentInv {
    /// Confirmed sales order to invoice.
    #[field(
        many2one,
        comodel = "sale.order",
        required = true,
        ondelete = "cascade",
        string = "Sales Order"
    )]
    pub sale_order_id: Option<RecId>,
    /// Company from the order.
    #[field(
        many2one,
        comodel = "res.company",
        related = "sale_order_id.company_id",
        store = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Currency from the order.
    #[field(
        many2one,
        comodel = "res.currency",
        related = "sale_order_id.currency_id",
        store = true,
        string = "Currency"
    )]
    pub currency_id: Option<RecId>,
    /// Regular invoice vs down payment.
    #[field(
        selection,
        selection = [
            ("delivered", "Regular Invoice"),
            ("percentage", "Down Payment (percentage)"),
            ("fixed", "Down Payment (fixed amount)"),
        ],
        required = true,
        default = "delivered",
        string = "Create Invoice"
    )]
    pub advance_payment_method: Option<Arc<str>>,
    /// Percentage of the order total when `advance_payment_method == 'percentage'`.
    #[field(float, digits = (16, 2), default = 0, string = "Down Payment (%)")]
    pub amount: Decimal,
    /// Fixed amount when `advance_payment_method == 'fixed'`.
    #[field(
        monetary,
        currency_field = "currency_id",
        default = 0,
        string = "Down Payment Amount"
    )]
    pub fixed_amount: Decimal,
}

#[methods(SaleAdvancePaymentInv, addon = "sale")]
impl SaleAdvancePaymentInv {
    /// Executes the chosen invoicing method and opens the created draft customer invoice.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_create_invoices(
        this: &RecordSet<SaleAdvancePaymentInv>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let wizard = this.ensure_one()?;
        wizard
            .as_records()
            .check_access(tx, Operation::Write)
            .await?;
        let order_id = wizard
            .get(tx, SaleAdvancePaymentInv::SALE_ORDER_ID)
            .await?
            .ok_or_else(|| Error::validation("Choose a sales order to invoice."))?
            .get();
        let method = wizard
            .get(tx, SaleAdvancePaymentInv::ADVANCE_PAYMENT_METHOD)
            .await?
            .unwrap_or_else(|| Arc::from("delivered"));
        let pct = wizard.get(tx, SaleAdvancePaymentInv::AMOUNT).await?;
        let fixed = wizard.get(tx, SaleAdvancePaymentInv::FIXED_AMOUNT).await?;

        let clean_env = strip_default_context(wizard.env());
        let order_raw = clean_env.records("sale.order")?.browse([order_id])?;
        order_raw.check_access(tx, Operation::Write).await?;

        if method.as_ref() == "delivered" {
            return tx
                .savepoint(async |tx| {
                    wizard.as_records().unlink(tx).await?;
                    order_raw.call(tx, "action_create_draft_invoice", ()).await
                })
                .await;
        }

        let inv_id = tx
            .savepoint(async |tx| {
                tx.flush().await?;
                order_raw.lock_for_update(tx, true).await?;

                if order_raw.cell(tx, "state").await? != Cell::Str("sale".into()) {
                    return Err(Error::user(
                        "Confirm the sales order before creating a down payment invoice.",
                    ));
                }
                if order_raw.cell(tx, "invoicing_closed").await? == Cell::Bool(true) {
                    return Err(Error::user("Reopen invoicing on the sales order first."));
                }
                let company = order_raw.cell(tx, "company_id").await?;
                let Cell::Id(company_id) = company else {
                    return Err(Error::invariant("The sales order requires a company."));
                };
                if !clean_env.su()
                    && !tx
                        .companies(&clean_env)
                        .await?
                        .iter()
                        .any(|c| c.get() == company_id)
                {
                    return Err(Error::access(
                        "Select the sales order's company before creating its down payment invoice.",
                    ));
                }
                for field in ["partner_invoice_id", "currency_id"] {
                    order_raw
                        .related(tx, field)
                        .await?
                        .check_access(tx, Operation::Read)
                        .await?;
                }
                let order_name = match order_raw.cell(tx, "name").await? {
                    Cell::Str(s) => s.to_string(),
                    other => {
                        return Err(Error::invariant(format!(
                            "sale.order.name returned {other:?}"
                        )));
                    }
                };
                let order_total = match order_raw.cell(tx, "amount_total").await? {
                    Cell::Decimal(d) => d,
                    Cell::Null => Decimal::ZERO,
                    other => {
                        return Err(Error::invariant(format!(
                            "sale.order.amount_total returned {other:?}"
                        )));
                    }
                };
                let currency_cell = order_raw.cell(tx, "currency_id").await?;
                let Cell::Id(currency_id) = currency_cell else {
                    return Err(Error::invariant("The sales order requires a currency."));
                };
                let was_locked = match order_raw.cell(tx, "locked").await? {
                    Cell::Bool(b) => b,
                    other => {
                        return Err(Error::invariant(format!(
                            "sale.order.locked returned {other:?}"
                        )));
                    }
                };

                let cur_rec = RecId::from_real(currency_id)
                    .ok_or_else(|| Error::invariant("Invalid order currency id."))?;
                let rounding = RoundingTable::load(&clean_env, tx, &[cur_rec]).await?;
                let inc = rounding.get(cur_rec)?.increment();
                let raw_dp = compute_down_payment_amount(order_total, method.as_ref(), pct, fixed)?;
                let dp_amount = inc.quantize(raw_dp, CURRENCY_ROUNDING_MODE)?;

                let env = clean_env.with_company(company_id)?;
                let invoices = env.records("account.move")?;
                invoices.check_access(tx, Operation::Create).await?;

                // Align down-payment tax_ids between the tracking SO line and the DP invoice line
                // so final regular invoice deduction cancels out taxes cleanly.
                let tax_rows = tx
                    .query(
                        "SELECT l.id::int8, r.account_tax_id::int8 \
                         FROM sale_order_line l \
                         LEFT JOIN account_tax_sale_order_line_rel r ON r.sale_order_line_id = l.id \
                         WHERE l.order_id = $1 AND l.display_type IS NULL \
                           AND NOT COALESCE(l.is_downpayment, false) \
                         ORDER BY l.id, r.account_tax_id",
                        &[&order_id],
                    )
                    .await?;
                let mut line_taxes = std::collections::BTreeMap::<i64, Vec<i64>>::new();
                for row in tax_rows {
                    let lid: i64 = row
                        .try_get(0)
                        .map_err(|e| Error::db(format!("down payment tax lookup: {e}")))?;
                    let tid: Option<i64> = row
                        .try_get(1)
                        .map_err(|e| Error::db(format!("down payment tax lookup: {e}")))?;
                    let entry = line_taxes.entry(lid).or_default();
                    if let Some(t) = tid {
                        entry.push(t);
                    }
                }
                let distinct_sets: std::collections::BTreeSet<Vec<i64>> =
                    line_taxes.into_values().collect();
                let dp_taxes: Vec<i64> = if distinct_sets.len() == 1 {
                    distinct_sets.into_iter().next().unwrap_or_default()
                } else {
                    Vec::new()
                };

                let uom_id = default_unit_uom_id(tx).await?;
                let desc = if method.as_ref() == "percentage" {
                    format!("Down payment of {pct}% ({order_name})")
                } else {
                    format!("Down payment ({order_name})")
                };

                // Record a 0-qty down-payment tracking line on the sales order (including on locked
                // confirmed orders, atomically inside this savepoint under FOR UPDATE).
                if was_locked {
                    order_raw
                        .write_base(tx, RawVals::new().set_cell("locked", Cell::Bool(false)))
                        .await?;
                }
                let mut sol_vals = RawVals::new();
                sol_vals.insert("order_id".into(), Cell::Id(order_id));
                sol_vals.insert("name".into(), Cell::Str(desc.clone().into()));
                sol_vals.insert("product_uom_qty".into(), Cell::Decimal(Decimal::ZERO));
                sol_vals.insert("product_uom_id".into(), Cell::Id(uom_id));
                sol_vals.insert("price_unit".into(), Cell::Decimal(dp_amount));
                sol_vals.insert_commands(
                    "tax_ids".into(),
                    vec![Command::set(
                        dp_taxes.iter().copied().filter_map(RecId::from_real),
                    )],
                );
                let dp_line = env
                    .records("sale.order.line")?
                    .create(tx, vec![sol_vals])
                    .await?;
                dp_line
                    .write_base(
                        tx,
                        RawVals::new().set_cell("is_downpayment", Cell::Bool(true)),
                    )
                    .await?;
                if was_locked {
                    order_raw
                        .write_base(tx, RawVals::new().set_cell("locked", Cell::Bool(true)))
                        .await?;
                }
                let dp_line_id = dp_line.ensure_one()?.int()?;

                let inv_line = RawVals::new()
                    .set_cell("sale_line_id", Cell::Id(dp_line_id))
                    .set_cell("product_uom_id", Cell::Id(uom_id))
                    .set_cell("name", Cell::Str(desc.into()))
                    .set_cell("quantity", Cell::Decimal(Decimal::ONE))
                    .set_cell("price_unit", Cell::Decimal(dp_amount))
                    .set_commands(
                        "tax_ids",
                        [Command::set(
                            dp_taxes.iter().copied().filter_map(RecId::from_real),
                        )],
                    );

                let inv_vals = RawVals::new()
                    .set_cell("move_type", Cell::Str("out_invoice".into()))
                    .set_cell("company_id", company)
                    .set_cell("ref", Cell::Str(order_name.into()))
                    .set_cell(
                        "partner_id",
                        order_raw.cell(tx, "partner_invoice_id").await?,
                    )
                    .set_cell("currency_id", currency_cell)
                    .set_commands("invoice_line_ids", [Command::create(inv_line)]);

                let invoice = invoices.create(tx, vec![inv_vals]).await?;
                invoice.check_access(tx, Operation::Read).await?;
                let inv_id = invoice.ensure_one()?.int()?;
                wizard.as_records().unlink(tx).await?;
                Ok(inv_id)
            })
            .await?;

        let views = tx
            .query(
                "SELECT res_id::int8 FROM ir_model_data \
                 WHERE module='account' AND name='view_invoice_form' AND model='ir.ui.view'",
                &[],
            )
            .await?;
        let [view] = views.as_slice() else {
            return Err(Error::invariant("The customer invoice form is missing."));
        };
        let view_id: i64 = view
            .try_get(0)
            .map_err(|e| Error::db(format!("invoice form view lookup: {e}")))?;

        Ok(Value::Map(
            ValueMap::from([
                ("type".into(), Value::str("ir.actions.act_window")),
                ("name".into(), Value::str("Down Payment Invoice")),
                ("res_model".into(), Value::str("account.move")),
                ("res_id".into(), Value::Int(inv_id)),
                (
                    "views".into(),
                    Value::list([Value::list([Value::Int(view_id), Value::str("form")])]),
                ),
                ("target".into(), Value::str("current")),
            ])
            .into(),
        ))
    }
}

#[methods(SaleOrder, addon = "sale")]
impl SaleOrder {
    /// Opens the Create Invoice / Down Payment wizard for a confirmed order.
    #[method]
    #[rpc(effect = read)]
    pub async fn action_open_invoice_wizard(
        this: &RecordSet<SaleOrder>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let rec = this.ensure_one()?;
        rec.as_records().check_access(tx, Operation::Write).await?;
        let oid = rec.as_records().ids()[0];
        Ok(Value::Json(Arc::new(json!({
            "type": "ir.actions.act_window",
            "name": "Create Invoice",
            "res_model": "sale.advance.payment.inv",
            "target": "new",
            "views": [[false, "form"]],
            "context": {"default_sale_order_id": oid},
        }))))
    }
}

#[cfg(test)]
mod tests {
    use super::compute_down_payment_amount;
    use erp_sdk::Decimal;
    use std::str::FromStr;

    #[test]
    fn down_payment_computation_is_exact_and_bounded() {
        let pct = compute_down_payment_amount(
            Decimal::from_str("250.00").unwrap(),
            "percentage",
            Decimal::from(30),
            Decimal::ZERO,
        )
        .unwrap();
        assert_eq!(pct, Decimal::from_str("75.00").unwrap());

        let fixed = compute_down_payment_amount(
            Decimal::from_str("250.00").unwrap(),
            "fixed",
            Decimal::ZERO,
            Decimal::from_str("45.50").unwrap(),
        )
        .unwrap();
        assert_eq!(fixed, Decimal::from_str("45.50").unwrap());

        assert!(
            compute_down_payment_amount(
                Decimal::from(100),
                "percentage",
                Decimal::from(120),
                Decimal::ZERO,
            )
            .is_err()
        );
        assert!(
            compute_down_payment_amount(Decimal::from(100), "fixed", Decimal::ZERO, Decimal::ZERO,)
                .is_err()
        );
    }
}
