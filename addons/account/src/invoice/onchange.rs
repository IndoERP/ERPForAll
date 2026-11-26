// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::{BTreeMap, BTreeSet};

use erp_addon_base::money::{RoundingTable, convert};
use erp_sdk::Command;
use erp_sdk::prelude::*;

use super::sync::{distribute, load_taxes};
use super::{is_invoice, is_refund, is_sale};
use crate::moves::{AccountMove, AccountMoveLine};
use crate::payterm::{self, compute as terms_compute};
use crate::tax::compute::{self, BaseLine, TaxDef};

fn amount(cell: &Cell, field: &str) -> Result<Decimal> {
    match cell {
        Cell::Null | Cell::Bool(false) => Ok(Decimal::ZERO),
        Cell::Decimal(value) => Ok(*value),
        Cell::Int(value) => Ok(Decimal::from(*value)),
        Cell::Float(value) => Decimal::try_from(*value)
            .map_err(|e| Error::validation(format!("{field} {value} is not an exact amount: {e}"))),
        other => Err(Error::invariant(format!(
            "{field} holds {other:?} instead of an amount"
        ))),
    }
}

fn id_of(cell: &Cell, field: &str) -> Result<Option<i64>> {
    match cell {
        Cell::Null | Cell::Bool(false) => Ok(None),
        Cell::Id(id) => Ok(RecId::from_raw(*id)
            .and_then(|rec| rec.as_real().or(rec.origin()))
            .map(|rec| rec.get())),
        Cell::Int(id) => Ok(Some(i64::from(*id))),
        other => Err(Error::validation(format!(
            "{field} takes a record id, not {other:?}."
        ))),
    }
}

fn ids_of(cell: &Cell) -> Vec<i64> {
    match cell {
        Cell::Ids(ids) => ids
            .iter()
            .filter_map(|id| id.as_real().or(id.origin()).map(|id| id.get()))
            .collect(),
        _ => Vec::new(),
    }
}

fn date_of(cell: &Cell, field: &str) -> Result<Option<Date>> {
    match cell {
        Cell::Null | Cell::Bool(false) => Ok(None),
        Cell::Date(date) => Ok(Some(*date)),
        other => Err(Error::invariant(format!(
            "{field} holds {other:?} instead of a date"
        ))),
    }
}

fn rec(id: i64) -> Result<RecId> {
    RecId::from_real(id).ok_or_else(|| Error::invariant(format!("record id {id}")))
}

fn add_total(a: Decimal, b: Decimal) -> Result<Decimal> {
    a.checked_add(b)
        .ok_or_else(|| Error::validation("An invoice total exceeds the range of amounts."))
}

struct MoveHead {
    move_type: Option<Arc<str>>,
    company: i64,
    company_currency: i64,
    currency: i64,
    journal: Option<i64>,
    term: Option<i64>,
    invoice_date: Option<Date>,
    due: Option<Date>,
    date: Option<Date>,
}

/// Read header context of a virtual or persisted `account.move` for live `onchange`.
async fn move_head_for_onchange(
    tx: &mut Transaction,
    env: &Env,
    entry: &Records,
) -> Result<Option<MoveHead>> {
    let move_type = if !entry.is_empty() {
        match entry.cell(tx, "move_type").await? {
            Cell::Str(kind) => Some(kind),
            _ => None,
        }
    } else {
        None
    }
    .or_else(|| match env.context().get("default_move_type") {
        Some(Value::Str(kind)) => Some(kind.clone()),
        _ => None,
    });
    let company = if !entry.is_empty() {
        match id_of(&entry.cell(tx, "company_id").await?, "company_id")? {
            Some(c) => Some(c),
            None => env.company_id()?,
        }
    } else {
        env.company_id()?
    };
    let Some(company) = company else {
        return Ok(None);
    };
    let company_currency = id_of(
        &env.records("res.company")?
            .browse([company])?
            .cell(tx, "currency_id")
            .await?,
        "currency_id",
    )?;
    let Some(company_currency) = company_currency else {
        return Ok(None);
    };
    let currency = if !entry.is_empty() {
        id_of(&entry.cell(tx, "currency_id").await?, "currency_id")?
    } else {
        None
    }
    .unwrap_or(company_currency);
    let journal = if !entry.is_empty() {
        id_of(&entry.cell(tx, "journal_id").await?, "journal_id")?
    } else {
        None
    };
    let term = if !entry.is_empty() {
        id_of(
            &entry.cell(tx, "invoice_payment_term_id").await?,
            "invoice_payment_term_id",
        )?
    } else {
        None
    };
    let invoice_date = if !entry.is_empty() {
        date_of(&entry.cell(tx, "invoice_date").await?, "invoice_date")?
    } else {
        None
    };
    let due = if !entry.is_empty() {
        date_of(
            &entry.cell(tx, "invoice_date_due").await?,
            "invoice_date_due",
        )?
    } else {
        None
    };
    let date = if !entry.is_empty() {
        date_of(&entry.cell(tx, "date").await?, "date")?
    } else {
        None
    };
    Ok(Some(MoveHead {
        move_type,
        company,
        company_currency,
        currency,
        journal,
        term,
        invoice_date,
        due,
        date,
    }))
}

fn is_product_line_display(cell: &Cell) -> bool {
    match cell {
        Cell::Null | Cell::Bool(false) => true,
        Cell::Str(kind) => kind.as_ref() == "product",
        _ => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum TaxBucketKey {
    Repartition(i64, Option<i64>),
    Tax(i64, Option<i64>),
}

/// Update `price_subtotal` and `price_total` on an in-memory `account.move.line` record.
async fn preview_line_amounts(
    tx: &mut Transaction,
    line: &Records,
    company: i64,
    currency: i64,
    refund: bool,
) -> Result<(Decimal, Decimal)> {
    if !is_product_line_display(&line.cell(tx, "display_type").await?) {
        line.write_base(
            tx,
            RawVals::new()
                .set_cell("price_subtotal", Cell::Decimal(Decimal::ZERO))
                .set_cell("price_total", Cell::Decimal(Decimal::ZERO)),
        )
        .await?;
        return Ok((Decimal::ZERO, Decimal::ZERO));
    }
    let company_env = line.env().with_company(company)?;
    let tax_ids = ids_of(&line.cell(tx, "tax_ids").await?);
    let tax_set: BTreeSet<i64> = tax_ids.iter().copied().collect();
    let (defs, repartitions) = load_taxes(tx, &company_env, &tax_set).await?;
    let table = RoundingTable::load(&company_env, tx, &[rec(currency)?]).await?;
    let increment = table.get(rec(currency)?)?.increment();
    let taxes: Vec<TaxDef> = tax_ids
        .iter()
        .map(|t| {
            defs.get(t)
                .map(|(def, _)| def.clone())
                .ok_or_else(|| Error::invariant(format!("tax {t} was not loaded")))
        })
        .collect::<Result<_>>()?;
    let result = compute::compute(&BaseLine {
        price_unit: amount(&line.cell(tx, "price_unit").await?, "price_unit")?,
        quantity: amount(&line.cell(tx, "quantity").await?, "quantity")?,
        discount: amount(&line.cell(tx, "discount").await?, "discount")?,
        taxes: &taxes,
        increment,
    })?;
    let mut effective_line_tax = Decimal::ZERO;
    for tax in &result.taxes {
        let own_reps = repartitions
            .iter()
            .filter(|r| r.tax == tax.tax_id && r.refund == refund);
        let distributed = distribute(tax.amount, own_reps, increment)?;
        if distributed.is_empty() {
            effective_line_tax = add_total(effective_line_tax, tax.amount)?;
        } else {
            for (_, part) in distributed {
                effective_line_tax = add_total(effective_line_tax, part)?;
            }
        }
    }
    let effective_price_total = add_total(result.total_excluded, effective_line_tax)?;
    line.write_base(
        tx,
        RawVals::new()
            .set_cell("price_subtotal", Cell::Decimal(result.total_excluded))
            .set_cell("price_total", Cell::Decimal(effective_price_total)),
    )
    .await?;
    Ok((result.total_excluded, effective_price_total))
}

#[methods(AccountMoveLine, addon = "account")]
impl AccountMoveLine {
    /// Proposes the product's name, unit, sale or purchase price (converted to the invoice
    /// currency when needed), default journal account, taxes of the invoice company, and updates
    /// line subtotal/total in memory before Save.
    #[onchange("product_id")]
    async fn _onchange_product_invoice(
        this: &RecordSet<AccountMoveLine>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        let line = this.as_records();
        let entry = line.related(tx, "move_id").await?;
        let Some(head) = move_head_for_onchange(tx, this.env(), &entry).await? else {
            return Ok(OnchangeResult::none());
        };
        let Some(move_type) = head.move_type.as_ref().filter(|k| is_invoice(k)) else {
            return Ok(OnchangeResult::none());
        };
        let refund = is_refund(move_type);
        let Some(product) = id_of(&line.cell(tx, "product_id").await?, "product_id")? else {
            preview_line_amounts(tx, line, head.company, head.currency, refund).await?;
            return Ok(OnchangeResult::none());
        };
        let company_env = this.env().with_company(head.company)?;
        let sale = is_sale(move_type);
        let record = company_env.records("product.product")?.browse([product])?;
        let mut assign = RawVals::new();
        if let Cell::Str(name) = record.cell(tx, "display_name").await? {
            assign.insert("name".into(), Cell::Str(name));
        }
        if let Some(uom) = id_of(&record.cell(tx, "uom_id").await?, "uom_id")? {
            assign.insert("product_uom_id".into(), Cell::Id(uom));
        }
        let price_field = if sale { "lst_price" } else { "standard_price" };
        let mut price = amount(&record.cell(tx, price_field).await?, price_field)?;
        if head.currency != head.company_currency {
            let at = match head.invoice_date.or(head.date) {
                Some(d) => d,
                None => erp_addon_base::company_calendar::business_date(tx, head.company).await?,
            };
            price = convert(
                &company_env,
                tx,
                price,
                rec(head.company_currency)?,
                rec(head.currency)?,
                rec(head.company)?,
                at,
            )
            .await?;
        }
        assign.insert("price_unit".into(), Cell::Decimal(price));
        let taxes_field = if sale {
            "taxes_id"
        } else {
            "supplier_taxes_id"
        };
        let proposed = ids_of(&record.cell(tx, taxes_field).await?);
        let taxes = if proposed.is_empty() {
            Vec::new()
        } else {
            company_env
                .records("account.tax")?
                .search(
                    tx,
                    &Domain::and([
                        Domain::condition("id", Operator::In, proposed)?,
                        Domain::condition("company_id", Operator::ParentOf, head.company)?,
                    ]),
                )
                .await?
                .ids()
                .into_iter()
                .map(rec)
                .collect::<Result<Vec<_>>>()?
        };
        assign.insert_commands("tax_ids".into(), vec![Command::Set(taxes)]);
        if id_of(&line.cell(tx, "account_id").await?, "account_id")?.is_none()
            && let Some(journal) = head.journal
            && let Some(account) = id_of(
                &company_env
                    .records("account.journal")?
                    .browse([journal])?
                    .cell(tx, "default_account_id")
                    .await?,
                "default_account_id",
            )?
        {
            assign.insert("account_id".into(), Cell::Id(account));
        }
        line.write_base(tx, assign).await?;
        preview_line_amounts(tx, line, head.company, head.currency, refund).await?;
        Ok(OnchangeResult::none())
    }

    /// Recomputes `price_subtotal` and `price_total` in memory when quantity, price, discount,
    /// taxes, unit or display type change on a draft invoice line.
    #[onchange(
        "quantity",
        "price_unit",
        "discount",
        "tax_ids",
        "product_uom_id",
        "display_type"
    )]
    async fn _onchange_line_amounts(
        this: &RecordSet<AccountMoveLine>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        let line = this.as_records();
        let entry = line.related(tx, "move_id").await?;
        let Some(head) = move_head_for_onchange(tx, this.env(), &entry).await? else {
            return Ok(OnchangeResult::none());
        };
        let Some(move_type) = head.move_type.as_deref().filter(|k| is_invoice(k)) else {
            return Ok(OnchangeResult::none());
        };
        preview_line_amounts(tx, line, head.company, head.currency, is_refund(move_type)).await?;
        Ok(OnchangeResult::none())
    }
}

async fn recompute_move_totals(
    tx: &mut Transaction,
    env: &Env,
    entry: &Records,
) -> Result<OnchangeResult> {
    if let Cell::Str(state) = entry.cell(tx, "state").await?
        && state.as_ref() != "draft"
    {
        return Ok(OnchangeResult::none());
    }
    let Some(head) = move_head_for_onchange(tx, env, entry).await? else {
        return Ok(OnchangeResult::none());
    };
    let move_type = head.move_type.as_deref().unwrap_or("entry");
    if move_type == "entry" {
        let lines = entry.related(tx, "line_ids").await?;
        let mut total = Decimal::ZERO;
        for line in lines.iter() {
            // Avoid virtual generated `debit` on in-memory records; derive debit side from stored
            // `balance` and `amount_currency`.
            let balance = amount(&line.cell(tx, "balance").await?, "balance")?;
            let amt_curr = amount(&line.cell(tx, "amount_currency").await?, "amount_currency")?;
            if balance > Decimal::ZERO {
                let add = if !amt_curr.is_zero() {
                    amt_curr
                } else {
                    balance
                };
                total = add_total(total, add)?;
            } else if balance.is_zero() && amt_curr > Decimal::ZERO {
                total = add_total(total, amt_curr)?;
            }
        }
        entry
            .write_base(
                tx,
                RawVals::new().set_cell("amount_total", Cell::Decimal(total)),
            )
            .await?;
        return Ok(OnchangeResult::none());
    }
    if !is_invoice(move_type) {
        return Ok(OnchangeResult::none());
    }
    let company_env = env.with_company(head.company)?;
    let lines = entry.related(tx, "invoice_line_ids").await?;
    let mut tax_ids = BTreeSet::new();
    for line in lines.iter() {
        if is_product_line_display(&line.cell(tx, "display_type").await?) {
            tax_ids.extend(ids_of(&line.cell(tx, "tax_ids").await?));
        }
    }
    let (taxes, repartitions) = load_taxes(tx, &company_env, &tax_ids).await?;
    let table = RoundingTable::load(&company_env, tx, &[rec(head.currency)?]).await?;
    let increment = table.get(rec(head.currency)?)?.increment();
    let refund = is_refund(move_type);

    let mut untaxed = Decimal::ZERO;
    let mut by_key: BTreeMap<TaxBucketKey, Decimal> = BTreeMap::new();
    for line in lines.iter() {
        if !is_product_line_display(&line.cell(tx, "display_type").await?) {
            line.write_base(
                tx,
                RawVals::new()
                    .set_cell("price_subtotal", Cell::Decimal(Decimal::ZERO))
                    .set_cell("price_total", Cell::Decimal(Decimal::ZERO)),
            )
            .await?;
            continue;
        }
        let line_tax_ids = ids_of(&line.cell(tx, "tax_ids").await?);
        let defs: Vec<TaxDef> = line_tax_ids
            .iter()
            .map(|t| {
                taxes
                    .get(t)
                    .map(|(def, _)| def.clone())
                    .ok_or_else(|| Error::invariant(format!("tax {t} was not loaded")))
            })
            .collect::<Result<_>>()?;
        let result = compute::compute(&BaseLine {
            price_unit: amount(&line.cell(tx, "price_unit").await?, "price_unit")?,
            quantity: amount(&line.cell(tx, "quantity").await?, "quantity")?,
            discount: amount(&line.cell(tx, "discount").await?, "discount")?,
            taxes: &defs,
            increment,
        })?;
        untaxed = add_total(untaxed, result.total_excluded)?;
        let base_account = id_of(&line.cell(tx, "account_id").await?, "account_id")?;
        let mut effective_line_tax = Decimal::ZERO;
        for tax in &result.taxes {
            let own_reps = repartitions
                .iter()
                .filter(|r| r.tax == tax.tax_id && r.refund == refund);
            let distributed = distribute(tax.amount, own_reps, increment)?;
            if distributed.is_empty() {
                effective_line_tax = add_total(effective_line_tax, tax.amount)?;
                let slot = by_key
                    .entry(TaxBucketKey::Tax(tax.tax_id, base_account))
                    .or_insert(Decimal::ZERO);
                *slot = add_total(*slot, tax.amount)?;
            } else {
                for (rep, part) in distributed {
                    effective_line_tax = add_total(effective_line_tax, part)?;
                    let key = TaxBucketKey::Repartition(rep.id, rep.account.or(base_account));
                    let slot = by_key.entry(key).or_insert(Decimal::ZERO);
                    *slot = add_total(*slot, part)?;
                }
            }
        }
        let effective_price_total = add_total(result.total_excluded, effective_line_tax)?;
        line.write_base(
            tx,
            RawVals::new()
                .set_cell("price_subtotal", Cell::Decimal(result.total_excluded))
                .set_cell("price_total", Cell::Decimal(effective_price_total)),
        )
        .await?;
    }
    let mut tax_total = Decimal::ZERO;
    for part in by_key.values() {
        if !part.is_zero() {
            tax_total = add_total(tax_total, *part)?;
        }
    }
    let amount_total = add_total(untaxed, tax_total)?;
    let mut vals = RawVals::new()
        .set_cell("amount_untaxed", Cell::Decimal(untaxed))
        .set_cell("amount_tax", Cell::Decimal(tax_total))
        .set_cell("amount_total", Cell::Decimal(amount_total))
        .set_cell("amount_residual", Cell::Decimal(amount_total));

    let date_ref = head.invoice_date.or(head.date);
    let due = if let (Some(term), Some(date_ref)) = (head.term, date_ref) {
        let terms = payterm::load_terms(tx, &company_env, &[term]).await?;
        let term_lines = terms
            .get(&term)
            .ok_or_else(|| Error::invariant(format!("payment term {term} was not loaded")))?;
        let mut max_due: Option<Date> = None;
        for line in term_lines {
            let d = terms_compute::due_date(line, date_ref)?;
            max_due = Some(max_due.map_or(d, |seen| seen.max(d)));
        }
        max_due
    } else {
        match (head.due, date_ref) {
            (Some(due), Some(start)) if due < start => Some(start),
            (Some(due), _) => Some(due),
            (None, start) => start,
        }
    };
    if let Some(due) = due {
        vals.insert("invoice_date_due".into(), Cell::Date(due));
    }
    entry.write_base(tx, vals).await?;
    Ok(OnchangeResult::none())
}

#[methods(AccountMove, addon = "account")]
impl AccountMove {
    /// Live `onchange` when `partner_id` changes on a draft invoice: proposes the partner's
    /// company-scoped payment term and refreshes totals and due date before Save.
    #[onchange("partner_id")]
    async fn _onchange_partner_payment_term(
        this: &RecordSet<AccountMove>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        let entry = this.as_records();
        if let Cell::Str(state) = entry.cell(tx, "state").await?
            && state.as_ref() != "draft"
        {
            return Ok(OnchangeResult::none());
        }
        if let Some(head) = move_head_for_onchange(tx, this.env(), entry).await?
            && let Some(move_type) = head.move_type.as_deref().filter(|k| is_invoice(k))
            && let Some(partner) = id_of(&entry.cell(tx, "partner_id").await?, "partner_id")?
            && let Some(term) =
                super::partner_terms(tx, this.env(), head.company, is_sale(move_type), partner)
                    .await?
        {
            entry
                .write_base(
                    tx,
                    RawVals::new().set_cell("invoice_payment_term_id", Cell::Id(term)),
                )
                .await?;
        }
        recompute_move_totals(tx, this.env(), entry).await
    }

    /// Live `onchange` for `account.move`: recomputes draft invoice line amounts, tax totals
    /// (with repartition rounding), `amount_untaxed`, `amount_tax`, `amount_total`,
    /// `amount_residual` and `invoice_date_due` (and `amount_total` on plain entries) before Save.
    #[onchange(
        "invoice_line_ids",
        "line_ids",
        "currency_id",
        "invoice_payment_term_id",
        "invoice_date",
        "date"
    )]
    async fn _onchange_move_totals(
        this: &RecordSet<AccountMove>,
        tx: &mut Transaction,
    ) -> Result<OnchangeResult> {
        recompute_move_totals(tx, this.env(), this.as_records()).await
    }
}
