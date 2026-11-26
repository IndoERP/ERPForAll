// SPDX-License-Identifier: MIT OR Apache-2.0
//! Create one ordinary draft customer invoice from one confirmed order.

use std::collections::BTreeMap;

use erp_sdk::money::{Increment, RoundingMode};
use erp_sdk::prelude::*;
use erp_sdk::settings::ParameterDecl;
use erp_sdk::{Command, Context, Operation, ValueMap};

use crate::SaleOrder;
use crate::invoice_lineage::readable_fields;

const LIMIT_KEY: &str = "sale.max_invoice_source_rows";

fn parameters() -> Result<Vec<ParameterDecl>> {
    Ok(vec![ParameterDecl::integer(
        LIMIT_KEY,
        "Source lines or tax links in one Sales invoice",
        10_000,
        (1, 100_000),
    )?])
}
erp_sdk::system_parameters!(parameters);

fn decode(error: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::db("Reading the sales invoice sources failed.").with_source(error)
}

fn too_many(limit: i64) -> Error {
    Error::user(format!(
        "Creating this invoice exceeds {LIMIT_KEY} ({limit})."
    ))
}

#[methods(SaleOrder, addon = "sale")]
impl SaleOrder {
    /// Create and open an unposted customer invoice for remaining order- or delivery-policy quantities.
    #[method]
    #[rpc(effect = write)]
    async fn action_create_draft_invoice(
        this: &RecordSet<SaleOrder>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let order = this.as_records();
        let order_id = order.ensure_one()?.int()?;
        order.check_access(tx, Operation::Write).await?;
        tx.savepoint(async |tx| create_draft(tx, order, order_id).await)
            .await
    }
}

async fn create_draft(tx: &mut Transaction, order: &Records, order_id: i64) -> Result<Value> {
    tx.flush().await?;
    order.lock_for_update(tx, true).await?;
    if order.cell(tx, "state").await? != Cell::Str("sale".into()) {
        return Err(Error::user(
            "Confirm the sales order before creating its invoice.",
        ));
    }
    if order.cell(tx, "invoicing_closed").await? == Cell::Bool(true) {
        return Err(Error::user("Reopen invoicing on the sales order first."));
    }
    let company = order.cell(tx, "company_id").await?;
    let Cell::Id(company_id) = company else {
        return Err(Error::invariant("The sales order requires a company."));
    };
    let caller = order.env();
    if !caller.su()
        && !tx
            .companies(caller)
            .await?
            .iter()
            .any(|company| company.get() == company_id)
    {
        return Err(Error::access(
            "Select the sales order's company before creating its invoice.",
        ));
    }
    // Carry only the admitted company and principal into Accounting, so a caller cannot
    // inject defaults or internal accounting validation switches into the derived document.
    let env = caller.with_context_replaced(Context::new().with_company_id(company_id));
    let invoices = env.records("account.move")?;
    invoices.check_access(tx, Operation::Create).await?;
    for field in ["partner_invoice_id", "currency_id"] {
        order
            .related(tx, field)
            .await?
            .check_access(tx, Operation::Read)
            .await?;
    }
    let limit = erp_sdk::params::get_int(tx, &crate::ADDON, LIMIT_KEY).await?;
    let items = invoice_items(tx, &env, order_id, limit).await?;
    let vals = RawVals::new()
        .set_cell("move_type", Cell::Str("out_invoice".into()))
        .set_cell("company_id", company)
        .set_cell("ref", order.cell(tx, "name").await?)
        .set_cell("partner_id", order.cell(tx, "partner_invoice_id").await?)
        .set_cell("currency_id", order.cell(tx, "currency_id").await?)
        .set_commands("invoice_line_ids", items);
    let invoice = invoices.create(tx, vec![vals]).await?;
    invoice.check_access(tx, Operation::Read).await?;
    let views = tx.query(
        "SELECT res_id::int8 FROM ir_model_data WHERE module='account' AND name='view_invoice_form' AND model='ir.ui.view'",
        &[],
    ).await?;
    let [view] = views.as_slice() else {
        return Err(Error::invariant("The customer invoice form is missing."));
    };
    let view_id: i64 = view.try_get(0).map_err(decode)?;
    Ok(Value::Map(
        ValueMap::from([
            ("type".into(), Value::str("ir.actions.act_window")),
            ("name".into(), Value::str("Customer Invoice")),
            ("res_model".into(), Value::str("account.move")),
            ("res_id".into(), Value::Int(invoice.ensure_one()?.int()?)),
            (
                "views".into(),
                Value::list([Value::list([Value::Int(view_id), Value::str("form")])]),
            ),
            ("target".into(), Value::str("new")),
        ])
        .into(),
    ))
}

async fn invoice_items(
    tx: &mut Transaction,
    env: &Env,
    order: i64,
    limit: i64,
) -> Result<Vec<Command>> {
    let candidates = tx
        .query(
            "SELECT id::int8 FROM sale_order_line WHERE order_id=$1 ORDER BY sequence,id LIMIT $2",
            &[&order, &(limit + 1)],
        )
        .await?;
    let ids = candidates
        .iter()
        .map(|row| row.try_get::<_, i64>(0).map_err(decode))
        .collect::<Result<Vec<_>>>()?;
    let lines = env.records("sale.order.line")?.browse(ids)?;
    lines.check_access(tx, Operation::Read).await?;
    readable_fields(
        tx,
        &lines,
        &[
            "sequence",
            "product_id",
            "name",
            "discount",
            "price_unit",
            "product_uom_id",
            "tax_ids",
            "qty_to_invoice",
            "invoice_policy",
            "is_downpayment",
            "display_type",
            "analytic_account_id",
        ],
    )
    .await?;
    if lines.len() as i64 > limit {
        return Err(too_many(limit));
    }
    lines.lock_for_update(tx, true).await?;
    let policies = lines.cells(tx, "invoice_policy").await?;
    let quantities = lines.cells(tx, "qty_to_invoice").await?;
    let kinds = lines.cells(tx, "display_type").await?;
    let advances = lines.cells(tx, "is_downpayment").await?;
    let digits = erp_addon_base::decimal_precision::precision_get(env, tx, "Product Unit").await?;
    let precision = Increment::from_places(digits).map_err(|e| Error::validation(e.to_string()))?;
    let mut eligible = Vec::new();
    for (index, id) in lines.ids().into_iter().enumerate() {
        if !matches!(kinds[index], Cell::Null) {
            continue;
        }
        if !matches!(&policies[index], Cell::Str(policy) if matches!(policy.as_ref(), "order" | "delivery"))
        {
            return Err(Error::validation(
                "Choose an invoicing policy for the sales line.",
            ));
        }
        let Cell::Decimal(qty) = quantities[index] else {
            return Err(Error::invariant("A sales line quantity must be numeric."));
        };
        let qty = precision
            .quantize(qty, RoundingMode::HalfAwayFromZero)
            .map_err(|e| Error::validation(e.to_string()))?;
        if qty.is_zero() {
            continue;
        }
        if advances[index] == Cell::Bool(true) {
            if qty < Decimal::ZERO {
                eligible.push((id, qty));
            }
            continue;
        }
        if qty < Decimal::ZERO {
            return Err(Error::user(
                "This order needs a credit invoice. Use Accounting for negative lines.",
            ));
        }
        eligible.push((id, qty));
    }
    if eligible.is_empty() {
        return Err(Error::user(
            "This order has no remaining quantities to invoice.",
        ));
    }
    let ids = eligible.iter().map(|(id, _)| *id).collect::<Vec<_>>();
    let taxes = source_taxes(tx, env, &ids, limit).await?;
    let source = lines.browse(ids)?;
    for field in ["product_id", "product_uom_id", "analytic_account_id"] {
        source
            .related(tx, field)
            .await?
            .check_access(tx, Operation::Read)
            .await?;
    }
    let products = source.cells(tx, "product_id").await?;
    let units = source.cells(tx, "product_uom_id").await?;
    let sequences = source.cells(tx, "sequence").await?;
    let descriptions = source.cells(tx, "name").await?;
    let prices = source.cells(tx, "price_unit").await?;
    let discounts = source.cells(tx, "discount").await?;
    let analytics = source.cells(tx, "analytic_account_id").await?;
    Ok(eligible
        .into_iter()
        .enumerate()
        .map(|(index, (id, qty))| {
            let vals = RawVals::new()
                .set_cell("sale_line_id", Cell::Id(id))
                .set_cell("product_id", products[index].clone())
                .set_cell("product_uom_id", units[index].clone())
                .set_cell("name", descriptions[index].clone())
                .set_cell("sequence", sequences[index].clone())
                .set_cell("price_unit", prices[index].clone())
                .set_cell("discount", discounts[index].clone())
                .set_cell("analytic_account_id", analytics[index].clone())
                .set_cell("quantity", Cell::Decimal(qty))
                .set_commands(
                    "tax_ids",
                    [Command::set(taxes.get(&id).into_iter().flatten().copied())],
                );
            Command::create(vals)
        })
        .collect())
}

async fn source_taxes(
    tx: &mut Transaction,
    env: &Env,
    ids: &[i64],
    limit: i64,
) -> Result<BTreeMap<i64, Vec<RecId>>> {
    // The regular many-to-many reader omits hidden taxes. Admit every association so
    // an invoice cannot silently lose a tax the caller lacks permission to read.
    let rows=tx.query("SELECT sale_order_line_id::int8, account_tax_id::int8 FROM account_tax_sale_order_line_rel
        WHERE sale_order_line_id=ANY($1) ORDER BY sale_order_line_id,account_tax_id LIMIT $2", &[&ids,&(limit+1)]).await?;
    let mut by_line: BTreeMap<i64, Vec<RecId>> = BTreeMap::new();
    for row in &rows {
        let line: i64 = row.try_get(0).map_err(decode)?;
        let tax: i64 = row.try_get(1).map_err(decode)?;
        by_line.entry(line).or_default().push(
            RecId::from_real(tax).ok_or_else(|| Error::invariant("Invalid sales tax identity."))?,
        );
    }
    env.records("account.tax")?
        .browse_ids(by_line.values().flatten().copied())
        .check_access(tx, Operation::Read)
        .await?;
    if rows.len() as i64 > limit {
        return Err(too_many(limit));
    }
    Ok(by_line)
}
