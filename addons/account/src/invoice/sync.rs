// SPDX-License-Identifier: MIT OR Apache-2.0
//! Reads are one query per field for all the invoices together. New lines are created in one call
//! and stale ones deleted in one call. A changed line is written with one write per line (product,
//! tax and receivable/payable lines) and the header once per invoice whose totals, payment state
//! or due date changed; lines and headers whose stored values already match are not written.

use std::collections::{BTreeMap, BTreeSet};

use erp_addon_base::money::{MAX_CURRENCIES_PER_CALL, RoundingTable};
use erp_sdk::money::Increment;
use erp_sdk::prelude::*;

use super::settings;
use super::{direction_sign, is_invoice, is_refund, is_sale, journal_type};
use crate::moves::{id_cell, unchecked};
use crate::payterm::{self, compute as terms_compute};
use crate::tax::compute::{self, AmountType, BaseLine, TaxDef, TaxKind};

fn real(id: i64) -> Result<RecId> {
    RecId::from_real(id)
        .ok_or_else(|| Error::invariant(format!("record id {id} is not a stored record id")))
}

/// The cell of row `i` of a column read for a set of records; a column shorter than the set is a
/// broken read, never an empty value.
fn nth<'a>(cells: &'a [Cell], i: usize, column: &str) -> Result<&'a Cell> {
    cells
        .get(i)
        .ok_or_else(|| Error::invariant(format!("the {column} column is short")))
}

fn text(cell: &Cell, field: &str) -> Result<Option<Arc<str>>> {
    match cell {
        Cell::Null | Cell::Bool(false) => Ok(None),
        Cell::Str(text) => Ok(Some(text.clone())),
        other => Err(Error::invariant(format!(
            "{field} holds {other:?} instead of a text"
        ))),
    }
}

fn flag(cell: &Cell, field: &str) -> Result<bool> {
    match cell {
        Cell::Null => Ok(false),
        Cell::Bool(value) => Ok(*value),
        other => Err(Error::invariant(format!(
            "{field} holds {other:?} instead of true or false"
        ))),
    }
}

fn date(cell: &Cell, field: &str) -> Result<Option<Date>> {
    match cell {
        Cell::Null | Cell::Bool(false) => Ok(None),
        Cell::Date(date) => Ok(Some(*date)),
        other => Err(Error::invariant(format!(
            "{field} holds {other:?} instead of a date"
        ))),
    }
}

/// An ordering number; an empty one sorts as 0 (a sequence is not a required field).
fn int(cell: &Cell, field: &str) -> Result<i32> {
    match cell {
        Cell::Null | Cell::Bool(false) => Ok(0),
        Cell::Int(value) => Ok(*value),
        other => Err(Error::invariant(format!(
            "{field} holds {other:?} instead of a whole number"
        ))),
    }
}

fn id(cell: &Cell, field: &str) -> Result<Option<i64>> {
    id_cell(Some(cell), field)
}

fn ids(cell: &Cell, field: &str) -> Result<Vec<i64>> {
    match cell {
        Cell::Null | Cell::Bool(false) => Ok(Vec::new()),
        Cell::Ids(ids) => ids
            .iter()
            .map(|id| {
                id.as_real().map(|id| id.get()).ok_or_else(|| {
                    Error::invariant(format!(
                        "{field} holds {id:?}, which is not a stored record"
                    ))
                })
            })
            .collect(),
        other => Err(Error::invariant(format!(
            "{field} holds {other:?} instead of a list of records"
        ))),
    }
}

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

/// A stored header amount; `None` when nothing is stored yet, which differs from a stored zero.
fn stored_amount(cell: &Cell, field: &str) -> Result<Option<Decimal>> {
    match cell {
        Cell::Null | Cell::Bool(false) => Ok(None),
        other => amount(other, field).map(Some),
    }
}

fn total(a: Decimal, b: Decimal) -> Result<Decimal> {
    a.checked_add(b)
        .ok_or_else(|| Error::validation("An invoice total exceeds the range of amounts."))
}

/// The header of a draft invoice.
struct Invoice {
    id: i64,
    move_type: Arc<str>,
    company: i64,
    currency: i64,
    company_currency: i64,
    term: Option<i64>,
    journal: i64,
    partner: Option<i64>,
    invoice_date: Option<Date>,
    due: Option<Date>,
    date: Option<Date>,
    stored: Totals,
}

/// The header values the sync keeps, as stored before the sync.
#[derive(Default)]
struct Totals {
    untaxed: Option<Decimal>,
    tax: Option<Decimal>,
    total: Option<Decimal>,
    residual: Option<Decimal>,
    payment_state: Option<Arc<str>>,
}

/// A journal item of an invoice, as the sync reads it.
struct Line {
    id: i64,
    move_id: i64,
    kind: Arc<str>,
    account: Option<i64>,
    partner: Option<i64>,
    quantity: Decimal,
    price_unit: Decimal,
    discount: Decimal,
    taxes: Vec<i64>,
    repartition: Option<i64>,
    currency: Option<i64>,
    amount_currency: Decimal,
    balance: Decimal,
    subtotal: Decimal,
    price_total: Decimal,
    maturity: Option<Date>,
}

/// One tax repartition line of kind `tax`.
pub(super) struct Repartition {
    pub(super) id: i64,
    pub(super) tax: i64,
    pub(super) refund: bool,
    pub(super) factor: Decimal,
    pub(super) account: Option<i64>,
    pub(super) sequence: i32,
}

/// The draft invoices among `move_ids`, with their headers.
async fn load_invoices(tx: &mut Transaction, env: &Env, move_ids: &[i64]) -> Result<Vec<Invoice>> {
    if move_ids.is_empty() {
        return Ok(Vec::new());
    }
    let moves = env
        .records("account.move")?
        .browse(move_ids.iter().copied())?;
    let types = moves.cells(tx, "move_type").await?;
    let states = moves.cells(tx, "state").await?;
    let mut chosen = Vec::new();
    let mut kinds = Vec::new();
    for (i, id) in moves.ids().into_iter().enumerate() {
        let kind = text(nth(&types, i, "move_type")?, "move_type")?;
        if let Some(kind) = kind.filter(|k| is_invoice(k))
            && text(nth(&states, i, "state")?, "state")?.as_deref() == Some("draft")
        {
            chosen.push(id);
            kinds.push(kind);
        }
    }
    if chosen.is_empty() {
        return Ok(Vec::new());
    }
    let moves = env
        .records("account.move")?
        .browse(chosen.iter().copied())?;
    let companies = moves.cells(tx, "company_id").await?;
    let currencies = moves.cells(tx, "currency_id").await?;
    let company_currencies = moves.cells(tx, "company_currency_id").await?;
    let terms = moves.cells(tx, "invoice_payment_term_id").await?;
    let journals = moves.cells(tx, "journal_id").await?;
    let partners = moves.cells(tx, "partner_id").await?;
    let invoice_dates = moves.cells(tx, "invoice_date").await?;
    let dues = moves.cells(tx, "invoice_date_due").await?;
    let dates = moves.cells(tx, "date").await?;
    let untaxed = moves.cells(tx, "amount_untaxed").await?;
    let taxes = moves.cells(tx, "amount_tax").await?;
    let totals = moves.cells(tx, "amount_total").await?;
    let residuals = moves.cells(tx, "amount_residual").await?;
    let payment_states = moves.cells(tx, "payment_state").await?;
    let mut invoices = Vec::with_capacity(chosen.len());
    for (i, (id, move_type)) in chosen.into_iter().zip(kinds).enumerate() {
        let missing = |field: &str| Error::invariant(format!("draft invoice {id} has no {field}"));
        invoices.push(Invoice {
            id,
            move_type,
            company: id_cell(Some(nth(&companies, i, "company_id")?), "company_id")?
                .ok_or_else(|| missing("company"))?,
            currency: id_cell(Some(nth(&currencies, i, "currency_id")?), "currency_id")?
                .ok_or_else(|| missing("currency"))?,
            company_currency: id_cell(
                Some(nth(&company_currencies, i, "company_currency_id")?),
                "company_currency_id",
            )?
            .ok_or_else(|| missing("company currency"))?,
            term: id_cell(
                Some(nth(&terms, i, "invoice_payment_term_id")?),
                "invoice_payment_term_id",
            )?,
            journal: id_cell(Some(nth(&journals, i, "journal_id")?), "journal_id")?
                .ok_or_else(|| missing("journal"))?,
            partner: id_cell(Some(nth(&partners, i, "partner_id")?), "partner_id")?,
            invoice_date: date(nth(&invoice_dates, i, "invoice_date")?, "invoice_date")?,
            due: date(nth(&dues, i, "invoice_date_due")?, "invoice_date_due")?,
            date: date(nth(&dates, i, "date")?, "date")?,
            stored: Totals {
                untaxed: stored_amount(nth(&untaxed, i, "amount_untaxed")?, "amount_untaxed")?,
                tax: stored_amount(nth(&taxes, i, "amount_tax")?, "amount_tax")?,
                total: stored_amount(nth(&totals, i, "amount_total")?, "amount_total")?,
                residual: stored_amount(nth(&residuals, i, "amount_residual")?, "amount_residual")?,
                payment_state: text(nth(&payment_states, i, "payment_state")?, "payment_state")?,
            },
        });
    }
    Ok(invoices)
}

/// An invoice's journal must be of its kind: sale for customer documents, purchase for vendor
/// documents.
async fn check_journals(tx: &mut Transaction, env: &Env, invoices: &[Invoice]) -> Result<()> {
    let wanted: BTreeSet<i64> = invoices.iter().map(|i| i.journal).collect();
    let journals = env
        .records("account.journal")?
        .browse(wanted.iter().copied())?;
    let cells = journals.cells(tx, "type").await?;
    let mut types: BTreeMap<i64, Option<Arc<str>>> = BTreeMap::new();
    for (i, journal) in journals.ids().into_iter().enumerate() {
        types.insert(journal, text(nth(&cells, i, "journal type")?, "type")?);
    }
    for invoice in invoices {
        let needed = journal_type(&invoice.move_type);
        let actual = types.get(&invoice.journal).cloned().flatten();
        if actual.as_deref() != Some(needed) {
            return Err(Error::validation(format!(
                "Invoice {} of type {} needs a {needed} journal; journal {} is of type {}.",
                invoice.id,
                invoice.move_type,
                invoice.journal,
                actual.as_deref().unwrap_or("none")
            )));
        }
    }
    Ok(())
}

/// The rounding increment of each invoice currency and company currency.
async fn load_increments(
    tx: &mut Transaction,
    env: &Env,
    invoices: &[Invoice],
) -> Result<BTreeMap<i64, Increment>> {
    let currencies: Vec<i64> = invoices
        .iter()
        .flat_map(|i| [i.currency, i.company_currency])
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut increments = BTreeMap::new();
    for chunk in currencies.chunks(MAX_CURRENCIES_PER_CALL) {
        let ids = chunk
            .iter()
            .map(|id| real(*id))
            .collect::<Result<Vec<_>>>()?;
        let table = RoundingTable::load(env, tx, &ids).await?;
        for id in ids {
            increments.insert(id.get(), table.get(id)?.increment().clone());
        }
    }
    Ok(increments)
}

/// Every journal item of the invoices.
async fn load_lines(tx: &mut Transaction, env: &Env, invoices: &[Invoice]) -> Result<Vec<Line>> {
    let move_ids: Vec<i64> = invoices.iter().map(|i| i.id).collect();
    let found = env
        .records("account.move.line")?
        .search(tx, &Domain::condition("move_id", Operator::In, move_ids)?)
        .await?;
    let mut columns = BTreeMap::new();
    for field in [
        "move_id",
        "display_type",
        "account_id",
        "partner_id",
        "quantity",
        "price_unit",
        "discount",
        "tax_ids",
        "tax_repartition_line_id",
        "currency_id",
        "amount_currency",
        "balance",
        "price_subtotal",
        "price_total",
        "date_maturity",
    ] {
        columns.insert(field, found.cells(tx, field).await?);
    }
    let cell = |field: &str, i: usize| -> Result<&Cell> {
        columns
            .get(field)
            .and_then(|c| c.get(i))
            .ok_or_else(|| Error::invariant(format!("journal item column {field} is short")))
    };
    let mut lines = Vec::with_capacity(found.len());
    for (i, line_id) in found.ids().into_iter().enumerate() {
        lines.push(Line {
            id: line_id,
            move_id: id(cell("move_id", i)?, "move_id")?
                .ok_or_else(|| Error::invariant(format!("journal item {line_id} has no entry")))?,
            // the field is required with the default `product` (`moves/line.rs`), so an empty
            // cell is a product line; any other kind of cell is refused by `text`
            kind: text(cell("display_type", i)?, "display_type")?
                .unwrap_or_else(|| "product".into()),
            account: id(cell("account_id", i)?, "account_id")?,
            partner: id(cell("partner_id", i)?, "partner_id")?,
            quantity: amount(cell("quantity", i)?, "quantity")?,
            price_unit: amount(cell("price_unit", i)?, "price_unit")?,
            discount: amount(cell("discount", i)?, "discount")?,
            taxes: ids(cell("tax_ids", i)?, "tax_ids")?,
            repartition: id(
                cell("tax_repartition_line_id", i)?,
                "tax_repartition_line_id",
            )?,
            currency: id(cell("currency_id", i)?, "currency_id")?,
            amount_currency: amount(cell("amount_currency", i)?, "amount_currency")?,
            balance: amount(cell("balance", i)?, "balance")?,
            subtotal: amount(cell("price_subtotal", i)?, "price_subtotal")?,
            price_total: amount(cell("price_total", i)?, "price_total")?,
            maturity: date(cell("date_maturity", i)?, "date_maturity")?,
        });
    }
    Ok(lines)
}

/// The taxes of `tax_ids` as the computation needs them, with their labels, and their `tax`
/// repartition lines. Groups of taxes are refused until child-tax expansion is implemented.
pub(super) async fn load_taxes(
    tx: &mut Transaction,
    env: &Env,
    tax_ids: &BTreeSet<i64>,
) -> Result<(BTreeMap<i64, (TaxDef, Arc<str>)>, Vec<Repartition>)> {
    if tax_ids.is_empty() {
        return Ok((BTreeMap::new(), Vec::new()));
    }
    let taxes = env
        .records("account.tax")?
        .browse(tax_ids.iter().copied())?;
    let mut columns = BTreeMap::new();
    for field in [
        "amount_type",
        "amount",
        "sequence",
        "price_include",
        "include_base_amount",
        "is_base_affected",
        "tax_label",
    ] {
        columns.insert(field, taxes.cells(tx, field).await?);
    }
    let cell = |field: &str, i: usize| -> Result<&Cell> {
        columns
            .get(field)
            .and_then(|c| c.get(i))
            .ok_or_else(|| Error::invariant(format!("tax column {field} is short")))
    };
    let mut defs = BTreeMap::new();
    for (i, tax_id) in taxes.ids().into_iter().enumerate() {
        let label = text(cell("tax_label", i)?, "tax_label")?
            .ok_or_else(|| Error::invariant(format!("tax {tax_id} has no label")))?;
        let kind = match text(cell("amount_type", i)?, "amount_type")?.as_deref() {
            Some("group") => {
                return Err(Error::user(format!(
                    "Tax {label} is a group of taxes: groups need their child taxes, which are not \
                     available yet."
                )));
            }
            Some(kind) => AmountType::parse(kind)?,
            None => return Err(Error::invariant(format!("tax {tax_id} has no amount type"))),
        };
        let def = TaxDef {
            id: tax_id,
            sequence: int(cell("sequence", i)?, "sequence")?,
            kind: TaxKind::Simple(kind),
            amount: amount(cell("amount", i)?, "amount")?,
            price_include: flag(cell("price_include", i)?, "price_include")?,
            include_base_amount: flag(cell("include_base_amount", i)?, "include_base_amount")?,
            is_base_affected: flag(cell("is_base_affected", i)?, "is_base_affected")?,
        };
        defs.insert(tax_id, (def, label));
    }
    let found = env
        .records("account.tax.repartition.line")?
        .search(
            tx,
            &Domain::and([
                Domain::condition(
                    "tax_id",
                    Operator::In,
                    tax_ids.iter().copied().collect::<Vec<_>>(),
                )?,
                Domain::condition("repartition_type", Operator::Eq, "tax")?,
            ]),
        )
        .await?;
    let owners = found.cells(tx, "tax_id").await?;
    let documents = found.cells(tx, "document_type").await?;
    let factors = found.cells(tx, "factor_percent").await?;
    let accounts = found.cells(tx, "account_id").await?;
    let sequences = found.cells(tx, "sequence").await?;
    let mut repartitions = Vec::with_capacity(found.len());
    for (i, rep_id) in found.ids().into_iter().enumerate() {
        let document = text(nth(&documents, i, "document_type")?, "document_type")?;
        let refund = match document.as_deref() {
            Some("refund") => true,
            Some("invoice") => false,
            other => {
                return Err(Error::invariant(format!(
                    "repartition line {rep_id} is for the document type {other:?}, which is \
                     neither invoice nor refund"
                )));
            }
        };
        repartitions.push(Repartition {
            id: rep_id,
            tax: id(nth(&owners, i, "tax_id")?, "tax_id")?
                .ok_or_else(|| Error::invariant(format!("repartition line {rep_id} has no tax")))?,
            refund,
            factor: amount(nth(&factors, i, "factor_percent")?, "factor_percent")?,
            account: id(nth(&accounts, i, "account_id")?, "account_id")?,
            sequence: int(nth(&sequences, i, "sequence")?, "sequence")?,
        });
    }
    repartitions.sort_by_key(|r| (r.tax, r.sequence, r.id));
    Ok((defs, repartitions))
}

/// Splits a tax amount over the tax's repartition lines of the document kind by their factor; the
/// rounding difference goes to the first line so the parts add up to the rounded whole.
pub(super) fn distribute<'a>(
    amount: Decimal,
    repartitions: impl Iterator<Item = &'a Repartition>,
    increment: &Increment,
) -> Result<Vec<(&'a Repartition, Decimal)>> {
    let round = |value: Decimal| -> Result<Decimal> {
        Ok(increment.quantize(value, erp_addon_base::money::CURRENCY_ROUNDING_MODE)?)
    };
    let range = || Error::validation("A tax amount exceeds the range of amounts.");
    let hundred = Decimal::ONE_HUNDRED;
    let mut parts = Vec::new();
    let mut factors = Decimal::ZERO;
    let mut shared = Decimal::ZERO;
    for rep in repartitions {
        let part = round(
            amount
                .checked_mul(rep.factor)
                .and_then(|v| v.checked_div(hundred))
                .ok_or_else(range)?,
        )?;
        factors = total(factors, rep.factor)?;
        shared = total(shared, part)?;
        parts.push((rep, part));
    }
    let whole = round(
        amount
            .checked_mul(factors)
            .and_then(|v| v.checked_div(hundred))
            .ok_or_else(range)?,
    )?;
    if let Some(first) = parts.first_mut() {
        first.1 = total(first.1, whole.checked_sub(shared).ok_or_else(range)?)?;
    }
    Ok(parts)
}

/// Rebuilds the tax lines, the receivable/payable line and the totals of the draft invoices among
/// `move_ids` from their product lines. Other entries and posted documents are left alone.
///
/// # Errors
///
/// A user error naming what to configure when a journal, account or tax is missing or not yet
/// supported; the errors of the line writes.
pub async fn sync_lines(tx: &mut Transaction, env: &Env, move_ids: &[i64]) -> Result<()> {
    sync(tx, env, move_ids, false).await
}

/// [`sync_lines`] for draft invoices whose accounting date changed: the product and tax lines of
/// an invoice in another currency than its company's get their amount in currency written again
/// even when it is unchanged, so the line write converts each balance at the new date
/// (`moves/line.rs`, the write override); the receivable/payable line then balances the new sums.
/// Invoices in their company's currency are synced as by [`sync_lines`].
///
/// # Errors
///
/// As [`sync_lines`]; a currency without a rate at the new date is refused by the line write.
pub async fn sync_lines_redated(tx: &mut Transaction, env: &Env, move_ids: &[i64]) -> Result<()> {
    sync(tx, env, move_ids, true).await
}

/// The invoices whose currency is not their company's.
async fn foreign_invoices(
    tx: &mut Transaction,
    env: &Env,
    invoices: &[Invoice],
) -> Result<BTreeSet<i64>> {
    let wanted: BTreeSet<i64> = invoices.iter().map(|i| i.company).collect();
    let companies = env.records("res.company")?.browse(wanted.iter().copied())?;
    let cells = companies.cells(tx, "currency_id").await?;
    let mut currencies = BTreeMap::new();
    for (i, company) in companies.ids().into_iter().enumerate() {
        let currency = id(nth(&cells, i, "company currency")?, "currency_id")?
            .ok_or_else(|| Error::invariant(format!("company {company} has no currency")))?;
        currencies.insert(company, currency);
    }
    let mut foreign = BTreeSet::new();
    for invoice in invoices {
        let currency = currencies.get(&invoice.company).ok_or_else(|| {
            Error::invariant(format!(
                "the currency of company {} was not read",
                invoice.company
            ))
        })?;
        if invoice.currency != *currency {
            foreign.insert(invoice.id);
        }
    }
    Ok(foreign)
}

async fn sync(tx: &mut Transaction, env: &Env, move_ids: &[i64], redated: bool) -> Result<()> {
    let invoices = load_invoices(tx, env, move_ids).await?;
    if invoices.is_empty() {
        return Ok(());
    }
    let reconverted = if redated {
        foreign_invoices(tx, env, &invoices).await?
    } else {
        BTreeSet::new()
    };
    check_journals(tx, env, &invoices).await?;
    let increments = load_increments(tx, env, &invoices).await?;
    let lines = load_lines(tx, env, &invoices).await?;
    let tax_ids: BTreeSet<i64> = lines
        .iter()
        .filter(|l| l.kind.as_ref() == "product")
        .flat_map(|l| l.taxes.iter().copied())
        .collect();
    let (taxes, repartitions) = load_taxes(tx, env, &tax_ids).await?;
    let items = unchecked(&env.records("account.move.line")?);
    let mut creates = Vec::new();
    let mut deletes = Vec::new();
    let mut totals = Vec::with_capacity(invoices.len());
    for invoice in &invoices {
        let sign = direction_sign(&invoice.move_type);
        let refund = is_refund(&invoice.move_type);
        let reconvert = reconverted.contains(&invoice.id);
        let increment = increments
            .get(&invoice.currency)
            .ok_or_else(|| Error::invariant("an invoice currency has no loaded rounding"))?;
        let own: Vec<&Line> = lines.iter().filter(|l| l.move_id == invoice.id).collect();
        let mut untaxed = Decimal::ZERO;
        let mut by_key: BTreeMap<(i64, i64), (Decimal, Arc<str>)> = BTreeMap::new();
        for line in own.iter().filter(|l| l.kind.as_ref() == "product") {
            let defs = line
                .taxes
                .iter()
                .map(|t| {
                    taxes
                        .get(t)
                        .map(|(def, _)| def.clone())
                        .ok_or_else(|| Error::invariant(format!("tax {t} was not loaded")))
                })
                .collect::<Result<Vec<_>>>()?;
            let result = compute::compute(&BaseLine {
                price_unit: line.price_unit,
                quantity: line.quantity,
                discount: line.discount,
                taxes: &defs,
                increment,
            })?;
            untaxed = total(untaxed, result.total_excluded)?;
            let base_account = line.account.ok_or_else(|| {
                Error::invariant(format!("invoice line {} has no account", line.id))
            })?;
            let mut effective_line_tax = Decimal::ZERO;
            for tax in &result.taxes {
                let label = taxes
                    .get(&tax.tax_id)
                    .map(|(_, label)| label.clone())
                    .ok_or_else(|| {
                        Error::invariant(format!("tax {} was not loaded", tax.tax_id))
                    })?;
                let own_reps = repartitions
                    .iter()
                    .filter(|r| r.tax == tax.tax_id && r.refund == refund);
                for (rep, part) in distribute(tax.amount, own_reps, increment)? {
                    effective_line_tax = total(effective_line_tax, part)?;
                    let key = (rep.id, rep.account.unwrap_or(base_account));
                    let slot = by_key.entry(key).or_insert((Decimal::ZERO, label.clone()));
                    slot.0 = total(slot.0, part)?;
                }
            }
            let effective_price_total = total(result.total_excluded, effective_line_tax)?;
            let mut update = RawVals::new();
            let amount_currency = sign * result.total_excluded;
            amounts_in(&mut update, line, invoice, amount_currency, reconvert);
            if line.subtotal != result.total_excluded {
                update.insert(
                    "price_subtotal".into(),
                    Cell::Decimal(result.total_excluded),
                );
            }
            if line.price_total != effective_price_total {
                update.insert("price_total".into(), Cell::Decimal(effective_price_total));
            }
            if line.partner != invoice.partner {
                update.insert(
                    "partner_id".into(),
                    invoice.partner.map_or(Cell::Null, Cell::Id),
                );
            }
            if !update.is_empty() {
                // shortcut: one write per changed line; a batched multi-row update needs an ORM
                // bulk-write API, add it when invoices with hundreds of lines show up in a profile
                items.browse([line.id])?.write(tx, update).await?;
            }
        }
        // tax lines: one per (repartition line, account); zero amounts make no line
        let mut tax_total = Decimal::ZERO;
        let mut kept = BTreeSet::new();
        for ((rep, account), (amount, label)) in &by_key {
            if amount.is_zero() {
                continue;
            }
            tax_total = total(tax_total, *amount)?;
            let amount_currency = sign * *amount;
            let existing = own.iter().find(|l| {
                l.kind.as_ref() == "tax"
                    && l.repartition == Some(*rep)
                    && l.account == Some(*account)
                    && !kept.contains(&l.id)
            });
            match existing {
                Some(line) => {
                    kept.insert(line.id);
                    let mut update = RawVals::new();
                    amounts_in(&mut update, line, invoice, amount_currency, reconvert);
                    if line.partner != invoice.partner {
                        update.insert(
                            "partner_id".into(),
                            invoice.partner.map_or(Cell::Null, Cell::Id),
                        );
                    }
                    if !update.is_empty() {
                        items.browse([line.id])?.write(tx, update).await?;
                    }
                }
                None => creates.push(
                    RawVals::new()
                        .set_cell("move_id", Cell::Id(invoice.id))
                        .set_cell("display_type", Cell::Str("tax".into()))
                        .set_cell("name", Cell::Str(label.clone()))
                        .set_cell("account_id", Cell::Id(*account))
                        .set_cell("tax_repartition_line_id", Cell::Id(*rep))
                        .set_cell("partner_id", invoice.partner.map_or(Cell::Null, Cell::Id))
                        .set_cell("amount_currency", Cell::Decimal(amount_currency)),
                ),
            }
        }
        deletes.extend(
            own.iter()
                .filter(|l| l.kind.as_ref() == "tax" && !kept.contains(&l.id))
                .map(|l| l.id),
        );
        totals.push((invoice, untaxed, tax_total));
    }
    if !deletes.is_empty() {
        items.browse(deletes.drain(..))?.unlink(tx).await?;
    }
    if !creates.is_empty() {
        items.create(tx, std::mem::take(&mut creates)).await?;
    }
    let term_dues = sync_terms(tx, env, &items, &lines, &totals, &increments).await?;
    // the header totals, written past the entry's write override (they trigger no sync); a
    // header whose stored values already match is not written
    let moves = env.records("account.move")?;
    for (invoice, untaxed, tax) in &totals {
        let amount_total = total(*untaxed, *tax)?;
        let due = term_dues
            .get(&invoice.id)
            .copied()
            .or_else(|| due_date(invoice))
            .filter(|due| invoice.due != Some(*due));
        if due.is_none() && header_matches(&invoice.stored, *untaxed, *tax, amount_total) {
            continue;
        }
        let mut vals = RawVals::new()
            .set_cell("amount_untaxed", Cell::Decimal(*untaxed))
            .set_cell("amount_tax", Cell::Decimal(*tax))
            .set_cell("amount_total", Cell::Decimal(amount_total))
            .set_cell("amount_residual", Cell::Decimal(amount_total))
            .set_cell("payment_state", Cell::Str(UNPAID.into()));
        if let Some(due) = due {
            vals.insert("invoice_date_due".into(), Cell::Date(due));
        }
        moves.browse([invoice.id])?.write_base(tx, vals).await?;
    }
    Ok(())
}

/// The payment state of every invoice until payments exist.
const UNPAID: &str = "not_paid";

/// True when the stored header already holds the values the sync would write: the three totals,
/// the amount due (the total until payments exist) and the payment state.
fn header_matches(stored: &Totals, untaxed: Decimal, tax: Decimal, amount_total: Decimal) -> bool {
    stored.untaxed == Some(untaxed)
        && stored.tax == Some(tax)
        && stored.total == Some(amount_total)
        && stored.residual == Some(amount_total)
        && stored.payment_state.as_deref() == Some(UNPAID)
}

/// Puts the amount in currency into `update` when it or the line's currency differs from the
/// invoice's; a currency change writes both, so the line write converts the balance at the new
/// currency's rate (a line keeps its currency otherwise, `moves/line.rs` create). With
/// `reconvert` the amount is written even when unchanged, so the line write converts the balance
/// again at the entry's present date.
fn amounts_in(
    update: &mut RawVals,
    line: &Line,
    invoice: &Invoice,
    amount_currency: Decimal,
    reconvert: bool,
) {
    let currency_changed = line.currency != Some(invoice.currency);
    if currency_changed {
        update.insert("currency_id".into(), Cell::Id(invoice.currency));
    }
    if reconvert || currency_changed || line.amount_currency != amount_currency {
        update.insert("amount_currency".into(), Cell::Decimal(amount_currency));
    }
}

/// The due date until payment terms exist: the one set by hand unless it is empty or before the
/// invoice date, else the invoice date, else the accounting date (partial).
fn due_date(invoice: &Invoice) -> Option<Date> {
    let start = invoice.invoice_date.or(invoice.date);
    match (invoice.due, start) {
        (Some(due), Some(start)) if due < start => Some(start),
        (Some(due), _) => Some(due),
        (None, start) => start,
    }
}

/// One receivable (customer documents) or payable (vendor documents) line per invoice that
/// balances the entry in both currencies; none when the entry is already zero (
/// partial: one installment, no payment terms). Its account is the one an existing line carries
/// when it is of the right type. Otherwise the candidates are the accounts of that type shared
/// with the invoice company or one of its parent companies
/// (step 4; the partner properties of
/// steps 2-3 do not exist yet): none is a refusal naming what to create, a single one is used,
/// and among several the one whose code the company chose in its Accounting settings
/// ([`settings`]) is used; several without a valid choice are refused listing their codes, never
/// picked by order.
async fn sync_terms(
    tx: &mut Transaction,
    env: &Env,
    items: &Records,
    lines: &[Line],
    totals: &[(&Invoice, Decimal, Decimal)],
    increments: &BTreeMap<i64, Increment>,
) -> Result<BTreeMap<i64, Date>> {
    tx.flush().await?;
    let move_ids: Vec<i64> = totals.iter().map(|(i, _, _)| i.id).collect();
    let rows = tx
        .query(
            "SELECT move_id, COALESCE(SUM(balance), 0)::text, COALESCE(SUM(amount_currency), 0)::text \
             FROM account_move_line WHERE move_id = ANY($1::int8[]) AND display_type <> 'payment_term' \
             GROUP BY move_id",
            &[&move_ids],
        )
        .await?;
    let parse = |text: String| -> Result<Decimal> {
        text.parse()
            .map_err(|e| Error::db(format!("An entry sum {text:?} could not be read: {e}")))
    };
    let mut sums: BTreeMap<i64, (Decimal, Decimal)> = BTreeMap::new();
    for row in &rows {
        let id: i64 = row
            .try_get(0)
            .map_err(|e| Error::db(format!("An entry id could not be read: {e}")))?;
        let balance: String = row
            .try_get(1)
            .map_err(|e| Error::db(format!("An entry balance could not be read: {e}")))?;
        let amount: String = row
            .try_get(2)
            .map_err(|e| Error::db(format!("An entry amount could not be read: {e}")))?;
        sums.insert(id, (parse(balance)?, parse(amount)?));
    }
    let term_accounts: Vec<i64> = lines
        .iter()
        .filter(|l| l.kind.as_ref() == "payment_term")
        .filter_map(|l| l.account)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let account_model = env.records("account.account")?;
    let known = account_model.browse(term_accounts.iter().copied())?;
    let cells = known.cells(tx, "account_type").await?;
    let mut types: BTreeMap<i64, Option<Arc<str>>> = BTreeMap::new();
    for (i, account) in known.ids().into_iter().enumerate() {
        types.insert(
            account,
            text(nth(&cells, i, "account type")?, "account_type")?,
        );
    }
    let term_ids: Vec<i64> = totals
        .iter()
        .filter_map(|(i, _, _)| i.term)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let payment_terms = payterm::load_terms(tx, env, &term_ids).await?;
    let digits = if term_ids.is_empty() {
        0
    } else {
        payterm::percent_digits(tx, env).await?
    };
    let mut term_dues: BTreeMap<i64, Date> = BTreeMap::new();
    let mut fallback: BTreeMap<(i64, &'static str), i64> = BTreeMap::new();
    let mut creates = Vec::new();
    let mut deletes = Vec::new();
    for (invoice, _, _) in totals {
        let (balance, amount) = sums.get(&invoice.id).copied().unwrap_or_default();
        let terms: Vec<&Line> = lines
            .iter()
            .filter(|l| l.move_id == invoice.id && l.kind.as_ref() == "payment_term")
            .collect();
        if balance.is_zero() && amount.is_zero() {
            deletes.extend(terms.iter().map(|l| l.id));
            continue;
        }
        let wanted: &'static str = if is_sale(&invoice.move_type) {
            "asset_receivable"
        } else {
            "liability_payable"
        };
        let current = terms
            .first()
            .and_then(|l| l.account)
            .filter(|a| types.get(a).cloned().flatten().as_deref() == Some(wanted));
        let account = match current {
            Some(account) => account,
            None => match fallback.get(&(invoice.company, wanted)) {
                Some(account) => *account,
                None => {
                    let account = fallback_account(tx, env, invoice.company, wanted).await?;
                    fallback.insert((invoice.company, wanted), account);
                    account
                }
            },
        };
        let (term_balance, term_amount) = (-balance, -amount);
        let wanted_lines = installments(
            invoice,
            &payment_terms,
            digits,
            increments,
            term_balance,
            term_amount,
        )?;
        if let Some(last) = wanted_lines.iter().filter_map(|w| w.date).max()
            && invoice.term.is_some()
        {
            term_dues.insert(invoice.id, last);
        }
        let mut existing = terms.iter();
        for wanted_line in &wanted_lines {
            let due = wanted_line.date;
            match existing.next() {
                Some(term) => {
                    let same = term.balance == wanted_line.balance
                        && term.amount_currency == wanted_line.amount_currency
                        && term.currency == Some(invoice.currency)
                        && term.account == Some(account)
                        && term.partner == invoice.partner
                        && term.maturity == due;
                    if !same {
                        let vals = RawVals::new()
                            .set_cell("currency_id", Cell::Id(invoice.currency))
                            .set_cell("account_id", Cell::Id(account))
                            .set_cell("partner_id", invoice.partner.map_or(Cell::Null, Cell::Id))
                            .set_cell("date_maturity", due.map_or(Cell::Null, Cell::Date))
                            .set_cell("balance", Cell::Decimal(wanted_line.balance))
                            .set_cell(
                                "amount_currency",
                                Cell::Decimal(wanted_line.amount_currency),
                            );
                        items.browse([term.id])?.write(tx, vals).await?;
                    }
                }
                None => creates.push(
                    RawVals::new()
                        .set_cell("move_id", Cell::Id(invoice.id))
                        .set_cell("display_type", Cell::Str("payment_term".into()))
                        .set_cell("account_id", Cell::Id(account))
                        .set_cell("partner_id", invoice.partner.map_or(Cell::Null, Cell::Id))
                        .set_cell("date_maturity", due.map_or(Cell::Null, Cell::Date))
                        .set_cell("balance", Cell::Decimal(wanted_line.balance))
                        .set_cell(
                            "amount_currency",
                            Cell::Decimal(wanted_line.amount_currency),
                        ),
                ),
            }
        }
        deletes.extend(existing.map(|l| l.id));
    }
    if !deletes.is_empty() {
        items.browse(deletes)?.unlink(tx).await?;
    }
    if !creates.is_empty() {
        items.create(tx, creates).await?;
    }
    Ok(term_dues)
}

/// The receivable or payable lines an invoice needs for the amounts to collect, one per instalment
/// of its payment terms. Without terms, or while the invoice has no date
/// to count from, a single line due on [`due_date`]: the invoice then keeps the lines it had.
fn installments(
    invoice: &Invoice,
    payment_terms: &BTreeMap<i64, Vec<terms_compute::TermLine>>,
    digits: u32,
    increments: &BTreeMap<i64, Increment>,
    balance: Decimal,
    amount_currency: Decimal,
) -> Result<Vec<Wanted>> {
    let single = || {
        vec![Wanted {
            date: due_date(invoice),
            balance,
            amount_currency,
        }]
    };
    let (Some(term), Some(date_ref)) = (invoice.term, invoice.invoice_date.or(invoice.date)) else {
        return Ok(single());
    };
    let lines = payment_terms
        .get(&term)
        .ok_or_else(|| Error::invariant(format!("payment term {term} was not loaded")))?;
    terms_compute::check_percent_lines(lines, digits)?;
    let increment = |currency: i64| {
        increments
            .get(&currency)
            .ok_or_else(|| Error::invariant("an invoice currency has no loaded rounding"))
    };
    let sign = if balance < Decimal::ZERO || (balance.is_zero() && amount_currency < Decimal::ZERO)
    {
        -Decimal::ONE
    } else {
        Decimal::ONE
    };
    Ok(terms_compute::instalments(
        lines,
        date_ref,
        balance,
        amount_currency,
        sign,
        increment(invoice.company_currency)?,
        increment(invoice.currency)?,
    )?
    .into_iter()
    .map(|i| Wanted {
        date: Some(i.date),
        balance: i.balance,
        amount_currency: i.amount_currency,
    })
    .collect())
}

/// A receivable or payable line the invoice needs: its due date and its amounts.
struct Wanted {
    date: Option<Date>,
    balance: Decimal,
    amount_currency: Decimal,
}

/// The receivable or payable account (`wanted` is the account type) of an invoice or a payment of
/// `company` that has none yet: see [`sync_terms`].
pub(crate) async fn fallback_account(
    tx: &mut Transaction,
    env: &Env,
    company: i64,
    wanted: &'static str,
) -> Result<i64> {
    let path = env
        .records("res.company")?
        .browse([company])?
        .cell(tx, "parent_path")
        .await?;
    let companies = company_with_parents(company, text(&path, "parent_path")?.as_deref())?;
    let found = env
        .records("account.account")?
        .search(
            tx,
            &Domain::and([
                Domain::condition("account_type", Operator::Eq, wanted)?,
                Domain::condition("company_ids", Operator::In, companies)?,
            ]),
        )
        .await?;
    let found_ids = found.ids();
    if found_ids.len() < 2 {
        // none or one: nothing to choose, so no setting is read
        let candidates: Vec<(i64, Option<Arc<str>>)> =
            found_ids.into_iter().map(|id| (id, None)).collect();
        return choose_account(wanted, company, &candidates, None);
    }
    // an account's code is the one of the active company's root company (`ledger/account.rs`)
    let codes = env
        .with_company(company)?
        .records("account.account")?
        .browse(found_ids.iter().copied())?
        .cells(tx, "code")
        .await?;
    let mut candidates = Vec::with_capacity(found_ids.len());
    for (i, account) in found_ids.into_iter().enumerate() {
        candidates.push((account, text(nth(&codes, i, "account code")?, "code")?));
    }
    let configured = settings::configured_code(tx, company, wanted).await?;
    choose_account(wanted, company, &candidates, configured.as_deref())
}

/// `company` and its parent companies, from its `parent_path` (`1/4/9/` for company 9 under 4
/// under 1); a company without a path has no parent.
fn company_with_parents(company: i64, path: Option<&str>) -> Result<Vec<i64>> {
    let Some(path) = path.filter(|p| !p.is_empty()) else {
        return Ok(vec![company]);
    };
    let unreadable = || {
        Error::invariant(format!(
            "the parent path {path:?} of company {company} is not a list of company ids ending \
             with the company"
        ))
    };
    let mut companies = Vec::new();
    for part in path.strip_suffix('/').unwrap_or(path).split('/') {
        companies.push(part.parse::<i64>().map_err(|_| unreadable())?);
    }
    if companies.last() != Some(&company) {
        return Err(unreadable());
    }
    Ok(companies)
}

/// Chooses the receivable or payable account (`wanted` is the account type) among `candidates`
/// (account id and code): the only one, else the one whose code is `configured`. No candidate and
/// several candidates without a valid choice are refused with what to do; an account is never
/// picked by its position among several.
fn choose_account(
    wanted: &str,
    company: i64,
    candidates: &[(i64, Option<Arc<str>>)],
    configured: Option<&str>,
) -> Result<i64> {
    let choice = settings::choice_of(wanted)?;
    let kind = choice.kind;
    match candidates {
        [] => Err(Error::user(format!(
            "Company {company} has no {kind} account: create one in Accounting, Configuration, \
             Chart of Accounts before saving this invoice."
        ))),
        [(only, _)] => Ok(*only),
        several => {
            let configured = configured.filter(|code| !code.is_empty());
            let matching: Vec<i64> = several
                .iter()
                .filter(|(_, code)| code.is_some() && code.as_deref() == configured)
                .map(|(id, _)| *id)
                .collect();
            if let [one] = matching.as_slice() {
                return Ok(*one);
            }
            let codes = several
                .iter()
                .map(|(id, code)| match code {
                    Some(code) => code.to_string(),
                    None => format!("account {id} without a code"),
                })
                .collect::<Vec<_>>()
                .join(", ");
            let problem = match configured {
                None => "none is chosen".to_owned(),
                Some(code) if matching.is_empty() => {
                    format!("the chosen code {code} is not one of them")
                }
                Some(code) => format!("the chosen code {code} is the code of more than one"),
            };
            Err(Error::user(format!(
                "Company {company} has several {kind} accounts ({codes}) and {problem}: enter the \
                 code of the one to use in the Accounting settings, block \"{block}\", \
                 \"{label}\".",
                block = settings::BLOCK,
                label = choice.label,
            )))
        }
    }
}

#[cfg(test)]
#[path = "test_sync.rs"]
mod tests;
