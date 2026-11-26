// SPDX-License-Identifier: MIT OR Apache-2.0
//! An invoice is a draft entry whose `move_type` is one of [`INVOICE_TYPES`]. The user edits its
//! product lines (`invoice_line_ids`); after every create and every relevant write of a draft
//! invoice [`sync_lines`] rebuilds the rest of `line_ids` from them: one tax line per tax
//! repartition line and account, one receivable or payable line that balances the entry, and the
//! header totals. A change of the accounting date of a draft runs [`sync_lines_redated`], which
//! also converts the balances of a foreign-currency invoice again at the new date. Posting checks
//! live in [`post`] and are called by the posting service.
//!
//! Not in this cut (each refused or left visible, never guessed): receipts, early payment discount
//! and cash rounding of payment terms, product account mappings
//! (product lines take the journal's default account), fiscal positions, cash
//! rounding, payments (the amount due is the total, the payment state `not_paid`).

mod onchange;
pub mod post;
mod settings;
mod sync;

pub(crate) use sync::fallback_account as partner_account;
pub use sync::{sync_lines, sync_lines_redated};

use erp_sdk::Command;
use erp_sdk::prelude::*;

use crate::moves::id_cell;

/// The move types this module treats as invoices.
pub const INVOICE_TYPES: [&str; 4] = ["out_invoice", "out_refund", "in_invoice", "in_refund"];

/// Fields whose change on a draft invoice rebuilds its tax, receivable/payable and total values.
const SYNC_FIELDS: [&str; 9] = [
    "partner_id",
    "currency_id",
    "move_type",
    "journal_id",
    "invoice_date",
    "invoice_date_due",
    "invoice_payment_term_id",
    "date",
    "company_id",
];

/// True for the move types of [`INVOICE_TYPES`].
pub fn is_invoice(move_type: &str) -> bool {
    INVOICE_TYPES.contains(&move_type)
}

/// True for customer documents (invoice, credit note).
pub fn is_sale(move_type: &str) -> bool {
    matches!(move_type, "out_invoice" | "out_refund")
}

/// True for credit notes.
pub fn is_refund(move_type: &str) -> bool {
    matches!(move_type, "out_refund" | "in_refund")
}

/// `direction_sign`: -1 for a customer invoice and a vendor credit note, +1 for the other types.
/// A product line's amount in currency is its subtotal times this sign, so a
/// customer invoice credits its income and debits its receivable.
pub fn direction_sign(move_type: &str) -> Decimal {
    if matches!(move_type, "out_invoice" | "in_refund") {
        -Decimal::ONE
    } else {
        Decimal::ONE
    }
}

/// The journal type an invoice of `move_type` belongs to.
pub fn journal_type(move_type: &str) -> &'static str {
    if is_sale(move_type) {
        "sale"
    } else {
        "purchase"
    }
}

fn text_of(cell: Option<&Cell>) -> Option<Arc<str>> {
    match cell {
        Some(Cell::Str(text)) => Some(text.clone()),
        _ => None,
    }
}

/// The move type a new entry will get: the given one, else the `default_move_type` context key.
fn new_move_type(env: &Env, vals: &RawVals) -> Result<Option<Arc<str>>> {
    if let Some(given) = vals.get("move_type") {
        return Ok(text_of(Some(given)));
    }
    match env.context().get("default_move_type") {
        None | Some(Value::False | Value::Bool(false)) => Ok(None),
        Some(Value::Str(kind)) => Ok(Some(kind.clone())),
        Some(other) => Err(Error::programming(format!(
            "The context key default_move_type takes a move type, not {other:?}."
        ))),
    }
}

/// Fills what a new draft invoice needs before the entry defaults run: its move type, a sale or
/// purchase journal, the accounting date from the invoice date and the account
/// of new product lines. Does nothing for other entries.
pub(crate) async fn prepare_create(
    tx: &mut Transaction,
    env: &Env,
    vals: &mut RawVals,
) -> Result<()> {
    let Some(move_type) = new_move_type(env, vals)?.filter(|t| is_invoice(t)) else {
        return Ok(());
    };
    vals.insert("move_type".into(), Cell::Str(move_type.clone()));
    let currency = match id_cell(vals.get("currency_id"), "currency_id")? {
        Some(currency) => Some(currency),
        None => match env.context().get("default_currency_id") {
            None | Some(Value::False | Value::Bool(false)) => None,
            Some(Value::Int(currency)) => {
                vals.insert("currency_id".into(), Cell::Id(currency));
                Some(currency)
            }
            Some(other) => {
                return Err(Error::validation(format!(
                    "The context key default_currency_id takes a currency id, not {other:?}."
                )));
            }
        },
    };
    let journal = match id_cell(vals.get("journal_id"), "journal_id")? {
        Some(journal) => journal,
        None => {
            let company = match id_cell(vals.get("company_id"), "company_id")? {
                Some(company) => company,
                None => env.company_id()?.ok_or_else(|| {
                    Error::user("No company is active: give the invoice a company.")
                })?,
            };
            let journal = invoice_journal(tx, env, company, &move_type, currency).await?;
            vals.insert("journal_id".into(), Cell::Id(journal));
            journal
        }
    };
    date_from_invoice_date(vals);
    fill_line_accounts(tx, env, vals, journal).await
}

/// Fills the partner's default payment terms and the taxes of the new product lines of a draft
/// invoice, after the entry defaults have decided its company (so both are those of the company
/// the entry gets). Does nothing for other entries.
pub(crate) async fn fill_create_taxes(
    tx: &mut Transaction,
    env: &Env,
    vals: &mut RawVals,
) -> Result<()> {
    let Some(kind) = text_of(vals.get("move_type")).filter(|t| is_invoice(t)) else {
        return Ok(());
    };
    let Some(company) = id_cell(vals.get("company_id"), "company_id")? else {
        return Ok(());
    };
    let context = TaxContext {
        company,
        sale: is_sale(&kind),
    };
    // The partner's default payment terms, decided here because the entry's company is known only now.
    if !vals.contains("invoice_payment_term_id")
        && !env.context().contains("default_invoice_payment_term_id")
        && let Some(partner) = id_cell(vals.get("partner_id"), "partner_id")?
        && let Some(term) = partner_terms(tx, env, company, context.sale, partner).await?
    {
        vals.insert("invoice_payment_term_id".into(), Cell::Id(term));
    }
    fill_taxes(tx, env, vals, &context).await
}

/// The payment terms a partner has for `company`: the customer terms of a customer document, the
/// vendor terms of a vendor bill.
pub(super) async fn partner_terms(
    tx: &mut Transaction,
    env: &Env,
    company: i64,
    sale: bool,
    partner: i64,
) -> Result<Option<i64>> {
    let field = if sale {
        "property_payment_term_id"
    } else {
        "property_supplier_payment_term_id"
    };
    let cell = env
        .with_company(company)?
        .records("res.partner")?
        .browse([partner])?
        .cell(tx, field)
        .await?;
    id_cell(Some(&cell), field)
}

/// Prepares a write of draft entries: fills the account of new product lines and the accounting
/// date from the invoice date. True when the write touches invoices in a way that needs
/// [`sync_lines`] afterwards.
pub(crate) async fn prepare_write(
    tx: &mut Transaction,
    records: &Records,
    vals: &mut RawVals,
) -> Result<bool> {
    let lines = ["line_ids", "invoice_line_ids"]
        .iter()
        .any(|f| vals.commands(f).is_some());
    if !lines && !SYNC_FIELDS.iter().any(|f| vals.contains(f)) {
        return Ok(false);
    }
    let invoices = match text_of(vals.get("move_type")) {
        Some(kind) => is_invoice(&kind),
        None => records
            .cells(tx, "move_type")
            .await?
            .iter()
            .any(|c| text_of(Some(c)).is_some_and(|k| is_invoice(&k))),
    };
    if !invoices {
        return Ok(false);
    }
    date_from_invoice_date(vals);
    if has_new_lines(vals) {
        let journal = match id_cell(vals.get("journal_id"), "journal_id")? {
            Some(journal) => journal,
            None => {
                let journals = records.cells(tx, "journal_id").await?;
                match journals.as_slice() {
                    [one] => id_cell(Some(one), "journal_id")?
                        .ok_or_else(|| Error::invariant("an invoice has no journal"))?,
                    _ => {
                        return Err(Error::user("Add invoice lines to one invoice at a time."));
                    }
                }
            }
        };
        fill_line_accounts(tx, records.env(), vals, journal).await?;
    }
    if needs_taxes(vals) {
        let context = write_tax_context(tx, records, vals).await?;
        fill_taxes(tx, records.env(), vals, &context).await?;
    }
    // a new partner brings its payment terms, when the invoices agree on kind and company
    if !vals.contains("invoice_payment_term_id")
        && let Some(partner) = id_cell(vals.get("partner_id"), "partner_id")?
    {
        let kinds: Vec<bool> = records
            .cells(tx, "move_type")
            .await?
            .iter()
            .filter_map(|c| text_of(Some(c)))
            .map(|k| is_sale(&k))
            .collect();
        let mut companies = Vec::new();
        for cell in records.cells(tx, "company_id").await? {
            companies.extend(id_cell(Some(&cell), "company_id")?);
        }
        companies.sort_unstable();
        companies.dedup();
        if let (Some(&sale), [company]) = (kinds.first(), companies.as_slice())
            && kinds.iter().all(|k| *k == sale)
            && let Some(term) = partner_terms(tx, records.env(), *company, sale, partner).await?
        {
            vals.insert("invoice_payment_term_id".into(), Cell::Id(term));
        }
    }
    Ok(true)
}

/// The accounting date follows the invoice date when only the invoice date is given
/// (partial: the vendor bill date rules come later).
fn date_from_invoice_date(vals: &mut RawVals) {
    if let Some(Cell::Date(date)) = vals.get("invoice_date").cloned()
        && !vals.contains("date")
    {
        vals.insert("date".into(), Cell::Date(date));
    }
}

fn has_new_lines(vals: &RawVals) -> bool {
    ["line_ids", "invoice_line_ids"].iter().any(|f| {
        vals.commands(f)
            .is_some_and(|cmds| cmds.iter().any(|c| matches!(c, Command::Create(_))))
    })
}

/// The first eligible journal in journal order, preferring a matching foreign currency.
/// Both searches run under the same caller and company scope.
async fn invoice_journal(
    tx: &mut Transaction,
    env: &Env,
    company: i64,
    move_type: &str,
    currency: Option<i64>,
) -> Result<i64> {
    let kind = journal_type(move_type);
    let journals = env.records("account.journal")?;
    let eligible = Domain::and([
        Domain::condition("type", Operator::Eq, kind)?,
        Domain::condition("company_id", Operator::ParentOf, company)?,
    ]);
    let first = SearchOptions::new().limit(1);
    if let Some(currency) = currency {
        let company_currency = env
            .records("res.company")?
            .browse([company])?
            .cell(tx, "currency_id")
            .await?;
        if id_cell(Some(&company_currency), "currency_id")? != Some(currency) {
            let matching = Domain::and([
                eligible.clone(),
                Domain::condition("currency_id", Operator::Eq, currency)?,
            ]);
            if let Some(&journal) = journals
                .search_with(tx, &matching, &first)
                .await?
                .ids()
                .first()
            {
                return Ok(journal);
            }
        }
    }
    let found = journals.search_with(tx, &eligible, &first).await?;
    found.ids().first().copied().ok_or_else(|| {
        Error::user(format!(
            "No {kind} journal is available for company {company}: create one in Accounting, \
             Configuration, Journals."
        ))
    })
}

/// What decides the default taxes of a line: the entry's company and whether it is a sale
/// document (customer taxes) or a purchase document (vendor taxes).
pub(crate) struct TaxContext {
    company: i64,
    sale: bool,
}

const LINE_FIELDS: [&str; 2] = ["line_ids", "invoice_line_ids"];

/// A line command that sets or changes the product without giving `tax_ids`.
fn takes_product_taxes(line: &RawVals) -> bool {
    !line.contains("tax_ids")
        && line.commands("tax_ids").is_none()
        && matches!(line.get("product_id"), Some(Cell::Id(_)))
}

fn needs_taxes(vals: &RawVals) -> bool {
    LINE_FIELDS.iter().any(|f| {
        vals.commands(f).is_some_and(|cmds| {
            cmds.iter().any(|c| match c {
                Command::Create(line) | Command::Update(_, line) => takes_product_taxes(line),
                _ => false,
            })
        })
    })
}

/// The type and company of the invoice a write changes: the given ones, else the record's (one
/// invoice at a time).
async fn write_tax_context(
    tx: &mut Transaction,
    records: &Records,
    vals: &RawVals,
) -> Result<TaxContext> {
    let types = match text_of(vals.get("move_type")) {
        Some(kind) => vec![kind],
        None => records
            .cells(tx, "move_type")
            .await?
            .iter()
            .filter_map(|c| text_of(Some(c)))
            .collect(),
    };
    let companies = match id_cell(vals.get("company_id"), "company_id")? {
        Some(company) => vec![company],
        None => {
            let mut found = Vec::new();
            for cell in records.cells(tx, "company_id").await? {
                found.extend(id_cell(Some(&cell), "company_id")?);
            }
            found.sort_unstable();
            found.dedup();
            found
        }
    };
    let (Some(kind), [company]) = (types.first(), companies.as_slice()) else {
        return Err(Error::user("Change the lines of one invoice at a time."));
    };
    Ok(TaxContext {
        company: *company,
        sale: is_sale(kind),
    })
}

/// The tax context of the invoice(s) the journal items `lines` belong to: `None` when none is an
/// invoice line.
///
/// # Errors
///
/// A user error when the lines belong to invoices of different companies or sides.
pub(crate) async fn line_tax_context(
    tx: &mut Transaction,
    lines: &Records,
) -> Result<Option<TaxContext>> {
    let entries = lines.cells(tx, "move_id").await?;
    let ids: Vec<RecId> = entries
        .iter()
        .filter_map(|cell| match cell {
            Cell::Id(id) => RecId::from_real(*id),
            _ => None,
        })
        .collect();
    let moves = lines.env().records("account.move")?.browse_ids(ids);
    let kinds = moves.cells(tx, "move_type").await?;
    let companies = moves.cells(tx, "company_id").await?;
    let mut found: Vec<(i64, bool)> = Vec::new();
    for (kind, company) in kinds.iter().zip(&companies) {
        let Some(kind) = text_of(Some(kind)).filter(|k| is_invoice(k)) else {
            continue;
        };
        let company = id_cell(Some(company), "company_id")?
            .ok_or_else(|| Error::invariant("an invoice has no company"))?;
        found.push((company, is_sale(&kind)));
    }
    found.sort_unstable();
    found.dedup();
    match found.as_slice() {
        [] => Ok(None),
        [(company, sale)] => Ok(Some(TaxContext {
            company: *company,
            sale: *sale,
        })),
        _ => Err(Error::user(
            "Change the product of the lines of one invoice at a time.",
        )),
    }
}

/// A line that names a product and gives no `tax_ids` takes the product's taxes of the entry's
/// company: a new line, and an existing line whose product is changed (the
/// former product's taxes would not fit). A given set, the empty one included, is kept as given;
/// a line without a product takes none. Shortcut: no fiscal position mapping, because an invoice
/// has no fiscal position field yet.
async fn fill_taxes(
    tx: &mut Transaction,
    env: &Env,
    vals: &mut RawVals,
    context: &TaxContext,
) -> Result<()> {
    for field in LINE_FIELDS {
        let Some(mut commands) = vals.remove_commands(field) else {
            continue;
        };
        for command in &mut commands {
            match command {
                Command::Create(line) if is_product_line(line) => {
                    fill_line_taxes(tx, env, line, context).await?;
                }
                Command::Update(_, line) => fill_line_taxes(tx, env, line, context).await?,
                _ => {}
            }
        }
        vals.insert_commands(field.into(), commands);
    }
    Ok(())
}

fn is_product_line(line: &RawVals) -> bool {
    match line.get("display_type") {
        None | Some(Cell::Null) => true,
        Some(Cell::Str(kind)) => kind.as_ref() == "product",
        Some(_) => false,
    }
}

pub(crate) async fn fill_line_taxes(
    tx: &mut Transaction,
    env: &Env,
    line: &mut RawVals,
    context: &TaxContext,
) -> Result<()> {
    if !takes_product_taxes(line) {
        return Ok(());
    }
    let Some(product) = id_cell(line.get("product_id"), "product_id")? else {
        return Ok(());
    };
    let company_env = env.with_company(context.company)?;
    let field = if context.sale {
        "taxes_id"
    } else {
        "supplier_taxes_id"
    };
    let proposed: Vec<i64> = match company_env
        .records("product.product")?
        .browse([product])?
        .cell(tx, field)
        .await?
    {
        Cell::Ids(ids) => ids
            .iter()
            .filter_map(|id| id.as_real().map(|id| id.get()))
            .collect(),
        _ => Vec::new(),
    };
    let taxes: Vec<RecId> = if proposed.is_empty() {
        Vec::new()
    } else {
        company_env
            .records("account.tax")?
            .search(
                tx,
                &Domain::and([
                    Domain::condition("id", Operator::In, proposed)?,
                    Domain::condition("company_id", Operator::ParentOf, context.company)?,
                ]),
            )
            .await?
            .ids()
            .into_iter()
            .filter_map(RecId::from_real)
            .collect()
    };
    line.insert_commands("tax_ids".into(), vec![Command::Set(taxes)]);
    Ok(())
}

/// New product lines without an account take the journal's default account (
/// partial: product accounts are not available yet); without one the save is refused
/// with what to configure.
async fn fill_line_accounts(
    tx: &mut Transaction,
    env: &Env,
    vals: &mut RawVals,
    journal: i64,
) -> Result<()> {
    let mut default: Option<Option<i64>> = None;
    for field in ["line_ids", "invoice_line_ids"] {
        let Some(mut commands) = vals.remove_commands(field) else {
            continue;
        };
        for command in &mut commands {
            let Command::Create(line) = command else {
                continue;
            };
            let product = match line.get("display_type") {
                None | Some(Cell::Null) => true,
                Some(Cell::Str(kind)) => kind.as_ref() == "product",
                Some(_) => false,
            };
            if !product || id_cell(line.get("account_id"), "account_id")?.is_some() {
                continue;
            }
            let account = match default {
                Some(account) => account,
                None => {
                    let cell = env
                        .records("account.journal")?
                        .browse([journal])?
                        .cell(tx, "default_account_id")
                        .await?;
                    let account = id_cell(Some(&cell), "default_account_id")?;
                    default = Some(account);
                    account
                }
            };
            let account = account.ok_or_else(|| {
                Error::user(format!(
                    "Choose an account on each invoice line, or give journal {journal} a default \
                     account in Accounting, Configuration, Journals (accounts set on products are not \
                     available yet)."
                ))
            })?;
            line.insert("account_id".into(), Cell::Id(account));
        }
        vals.insert_commands(field.into(), commands);
    }
    Ok(())
}
