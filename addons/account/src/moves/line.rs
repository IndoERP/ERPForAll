// SPDX-License-Identifier: MIT OR Apache-2.0
//! `account.move.line`: journal items; the items of a posted or cancelled entry keep their
//! accounting fields.

use std::collections::{BTreeMap, BTreeSet};

use erp_addon_base::money::{
    CURRENCY_ROUNDING_MODE, MAX_CURRENCIES_PER_CALL, RateBook, RoundingTable,
};
use erp_sdk::GeneratedInput;
use erp_sdk::Operation;
use erp_sdk::money::{Increment, RateDateRule};
use erp_sdk::prelude::*;

use super::post::LockScope;
use super::{
    TEXT_LINE_TYPES, check_balanced, id_cell, refuse_state, skip_balance_check, unchecked,
};

/// A journal item. `balance` is the only stored amount (company currency); `debit` and `credit`
/// are virtual columns over it and `is_storno`. Header fields read through the entry
/// (`move_name`, `ref`, the company currency) are not stored on the line.
#[model(
    name = "account.move.line",
    description = "Journal Item",
    order = "date desc, move_id desc, id",
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    owner_only_stored_fields = [],
    constraints = [
        (
            "_check_accountable_required_fields",
            "CHECK (display_type IN ('line_section', 'line_subsection', 'line_note') OR account_id IS NOT NULL)",
            "A journal item that carries an amount needs an account."
        ),
        (
            "_check_non_accountable_fields_null",
            "CHECK (display_type NOT IN ('line_section', 'line_subsection', 'line_note') OR (COALESCE(balance, 0) = 0 AND COALESCE(amount_currency, 0) = 0 AND account_id IS NULL))",
            "A section or note line has no amount and no account."
        ),
    ],
    // the grouped reads of the journal overview: lines of a journal by entry state, and posted
    // lines of an account by date
    indexes = [
        ("_journal_state", "(journal_id, parent_state)"),
        (
            "_posted_account_date",
            "(account_id, date) WHERE parent_state = 'posted'"
        ),
        (
            "_posted_company_date",
            "(company_id, date) WHERE parent_state = 'posted'"
        ),
    ],
)]
pub struct AccountMoveLine {
    /// The entry; a line never moves to another entry and is deleted with it.
    #[field(
        many2one,
        comodel = "account.move",
        required = true,
        ondelete = "cascade",
        string = "Journal Entry"
    )]
    pub move_id: Option<RecId>,
    /// The entry's company.
    #[field(
        many2one,
        comodel = "res.company",
        related = "move_id.company_id",
        store = true,
        index = false,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// The entry's journal.
    #[field(
        many2one,
        comodel = "account.journal",
        related = "move_id.journal_id",
        store = true,
        index = false,
        string = "Journal"
    )]
    pub journal_id: Option<RecId>,
    /// The entry's accounting date.
    #[field(date, related = "move_id.date", store = true, string = "Date")]
    pub date: Option<Date>,
    /// The entry's state, written with it by the posting service. Its values repeat the entry's,
    /// so a form can read it in its modifiers.
    #[field(
        selection,
        selection = [("draft", "Draft"), ("posted", "Posted"), ("cancel", "Cancelled")],
        related = "move_id.state",
        store = true,
        readonly = true,
        string = "Status"
    )]
    pub parent_state: Option<Arc<str>>,
    /// The entry's number, read through the entry.
    #[field(char, related = "move_id.name", string = "Number")]
    pub move_name: Option<Arc<str>>,
    /// The entry's reference, read through the entry.
    #[field(char, related = "move_id.ref", string = "Reference")]
    pub r#ref: Option<Arc<str>>,
    /// The company currency, read through the entry.
    #[field(
        many2one,
        comodel = "res.currency",
        related = "move_id.company_currency_id",
        string = "Company Currency"
    )]
    pub company_currency_id: Option<RecId>,
    /// The account; required on every line that carries an amount.
    #[field(
        many2one,
        comodel = "account.account",
        check_company = true,
        ondelete = "restrict",
        index = false,
        string = "Account"
    )]
    pub account_id: Option<RecId>,
    /// Partner of the line.
    #[field(
        many2one,
        comodel = "res.partner",
        check_company = true,
        ondelete = "restrict",
        index = false,
        string = "Partner"
    )]
    pub partner_id: Option<RecId>,
    /// Optional analytic account / cost center for controlling; generates an analytic item on posting.
    #[field(
        many2one,
        comodel = "account.analytic.account",
        check_company = true,
        ondelete = "set null",
        index = "btree_not_null",
        string = "Cost Center"
    )]
    pub analytic_account_id: Option<RecId>,
    /// Optional CIP or Fixed Asset receiving construction costs from this line.
    #[field(
        many2one,
        comodel = "account.asset",
        check_company = true,
        ondelete = "set null",
        index = "btree_not_null",
        string = "Asset / CIP"
    )]
    pub asset_id: Option<RecId>,
    /// Counterpart group legal entity for intercompany reconciliation & consolidation elimination.
    #[field(
        many2one,
        comodel = "res.company",
        ondelete = "set null",
        index = "btree_not_null",
        string = "Trading Partner"
    )]
    pub trading_partner_company_id: Option<RecId>,
    /// Label.
    #[field(char, string = "Label")]
    pub name: Option<Arc<str>>,
    /// Order inside the entry.
    #[field(integer, default = 10)]
    pub sequence: i32,
    /// What the line is: an accounting line (`product`), an invoice's tax or receivable/payable
    /// line (`tax`, `payment_term`, kept by the invoice line sync), or a section, subsection or note.
    #[field(
        selection,
        selection = [
            ("product", "Product"),
            ("tax", "Tax"),
            ("payment_term", "Payment Term"),
            ("line_section", "Section"),
            ("line_subsection", "Subsection"),
            ("line_note", "Note"),
        ],
        required = true,
        default = "product",
        string = "Display Type"
    )]
    pub display_type: Option<Arc<str>>,
    /// Currency of the line; the entry's when none is given.
    #[field(
        many2one,
        comodel = "res.currency",
        required = true,
        index = false,
        string = "Currency"
    )]
    pub currency_id: Option<RecId>,
    /// Amount in the line currency; equal to the balance in the company currency. In another
    /// currency the amount that is not given is converted from the other one at the entry date.
    #[field(
        monetary,
        currency_field = "currency_id",
        string = "Amount in Currency"
    )]
    pub amount_currency: Decimal,
    /// Signed amount in the company currency: debit minus credit.
    #[field(monetary, currency_field = "company_currency_id", string = "Balance")]
    pub balance: Decimal,
    /// Negative amounts kept on their side instead of swapping debit and credit.
    #[field(boolean, copy = false, string = "Storno")]
    pub is_storno: bool,
    /// Debit, generated from the balance.
    #[field(
        monetary,
        currency_field = "company_currency_id",
        generated = "least(balance, 0) if is_storno else greatest(balance, 0)",
        input_adapted = true,
        string = "Debit"
    )]
    pub debit: Decimal,
    /// Credit, generated from the balance.
    #[field(
        monetary,
        currency_field = "company_currency_id",
        generated = "-greatest(balance, 0) if is_storno else -least(balance, 0)",
        input_adapted = true,
        string = "Credit"
    )]
    pub credit: Decimal,
    /// Due date of the line.
    #[field(date, string = "Due Date")]
    pub date_maturity: Option<Date>,
    /// The product of an invoice line.
    #[field(
        many2one,
        comodel = "product.product",
        ondelete = "restrict",
        index = "btree_not_null",
        string = "Product"
    )]
    pub product_id: Option<RecId>,
    /// Unit of the quantity.
    #[field(many2one, comodel = "uom.uom", ondelete = "restrict", string = "Unit")]
    pub product_uom_id: Option<RecId>,
    /// Quantity of an invoice line.
    #[field(float, digits = "Product Unit", default = 1, string = "Quantity")]
    pub quantity: Decimal,
    /// Unit price, in the line currency.
    #[field(float, digits = "Product Price", string = "Unit Price")]
    pub price_unit: Decimal,
    /// Discount in percent of the unit price.
    #[field(float, digits = "Discount", default = 0, string = "Discount (%)")]
    pub discount: Decimal,
    /// Taxes applied to an invoice line.
    #[field(
        many2many,
        comodel = "account.tax",
        relation = "account_move_line_account_tax_rel",
        column1 = "account_move_line_id",
        column2 = "account_tax_id",
        check_company = true,
        string = "Taxes"
    )]
    pub tax_ids: Vec<RecId>,
    /// The repartition line a tax line was made from.
    #[field(
        many2one,
        comodel = "account.tax.repartition.line",
        ondelete = "restrict",
        index = "btree_not_null",
        string = "Originator Tax Distribution Line"
    )]
    pub tax_repartition_line_id: Option<RecId>,
    /// The tax of a tax line, from its repartition line.
    #[field(
        many2one,
        comodel = "account.tax",
        related = "tax_repartition_line_id.tax_id",
        store = true,
        index = false,
        string = "Originator Tax"
    )]
    pub tax_line_id: Option<RecId>,
    /// Line amount without taxes, in the line currency.
    #[field(
        monetary,
        currency_field = "currency_id",
        readonly = true,
        string = "Subtotal"
    )]
    pub price_subtotal: Decimal,
    /// Line amount with taxes, in the line currency.
    #[field(
        monetary,
        currency_field = "currency_id",
        readonly = true,
        string = "Total"
    )]
    pub price_total: Decimal,
    /// The settlement of the item once it and every item matched with it are settled; written
    /// only by the reconciliation service (`crate::reconcile`).
    #[field(
        many2one,
        comodel = "account.full.reconcile",
        ondelete = "set null",
        readonly = true,
        copy = false,
        index = "btree_not_null",
        string = "Matching"
    )]
    pub full_reconcile_id: Option<RecId>,
    /// The matches in which the item is the credit side.
    #[field(
        one2many,
        comodel = "account.partial.reconcile",
        inverse_name = "credit_move_id",
        readonly = true,
        copy = false,
        string = "Matched Debits"
    )]
    pub matched_debit_ids: Vec<RecId>,
    /// The matches in which the item is the debit side.
    #[field(
        one2many,
        comodel = "account.partial.reconcile",
        inverse_name = "debit_move_id",
        readonly = true,
        copy = false,
        string = "Matched Credits"
    )]
    pub matched_credit_ids: Vec<RecId>,
    /// What is still due on the item, in the company currency.
    #[field(
        monetary,
        currency_field = "company_currency_id",
        compute = "_compute_amount_residual",
        string = "Residual Amount"
    )]
    pub amount_residual: Decimal,
    /// What is still due on the item, in its currency.
    #[field(
        monetary,
        currency_field = "currency_id",
        compute = "_compute_amount_residual",
        string = "Residual Amount in Currency"
    )]
    pub amount_residual_currency: Decimal,
    /// Both residuals are zero.
    #[field(boolean, compute = "_compute_amount_residual", string = "Reconciled")]
    pub reconciled: bool,
    /// The settlement id, else `P` and the smallest match id of the item's matches, else empty.
    #[field(char, compute = "_compute_amount_residual", string = "Matching #")]
    pub matching_number: Option<Arc<str>>,
}

/// An amount cell as an exact decimal; empty is zero.
fn amount_of(cell: Option<&Cell>, field: &str) -> Result<Option<Decimal>> {
    match cell {
        None => Ok(None),
        Some(Cell::Null | Cell::Bool(false)) => Ok(Some(Decimal::ZERO)),
        Some(Cell::Decimal(value)) => Ok(Some(*value)),
        Some(Cell::Int(value)) => Ok(Some(Decimal::from(*value))),
        Some(other) => Err(Error::validation(format!(
            "{field} takes an exact amount, not {other:?}."
        ))),
    }
}

fn is_text_line(vals: &RawVals) -> bool {
    matches!(vals.get("display_type"), Some(Cell::Str(kind)) if TEXT_LINE_TYPES.contains(&kind.as_ref()))
}

/// The balance a row's debit and credit stand. `stored`
/// is the persisted (debit, credit) of a written line: a side written as zero keeps the other
/// side's stored amount, a non-zero side clears the other one (the reference's inverses).
fn balance_of(
    debit: Option<Decimal>,
    credit: Option<Decimal>,
    stored: Option<(Decimal, Decimal)>,
) -> Result<Decimal> {
    let overflow = || Error::validation("The debit and credit of a journal item are too large.");
    let (stored_debit, stored_credit) = stored.unwrap_or((Decimal::ZERO, Decimal::ZERO));
    match (debit, credit) {
        (Some(debit), Some(credit)) if !debit.is_zero() && !credit.is_zero() => {
            Err(Error::validation(format!(
                "A journal item cannot have both a debit and a credit (debit {debit}, credit \
                 {credit}): use two lines."
            )))
        }
        (Some(debit), Some(credit)) => debit.checked_sub(credit).ok_or_else(overflow),
        (Some(debit), None) if debit.is_zero() => Ok(-stored_credit),
        (Some(debit), None) => Ok(debit),
        (None, Some(credit)) if credit.is_zero() => Ok(stored_debit),
        (None, Some(credit)) => Ok(-credit),
        (None, None) => Err(Error::internal(
            "a journal item's debit and credit were translated without either",
        )),
    }
}

/// Translates a written debit or credit into the balance; a balance given with them must agree.
fn adapt_row(row: &mut RawVals, stored: Option<(Decimal, Decimal)>) -> Result<()> {
    let debit = amount_of(row.remove("debit").as_ref(), "debit")?;
    let credit = amount_of(row.remove("credit").as_ref(), "credit")?;
    if debit.is_none() && credit.is_none() {
        return Ok(());
    }
    let balance = balance_of(debit, credit, stored)?;
    match amount_of(row.get("balance"), "balance")? {
        Some(given) if given != balance => Err(Error::validation(format!(
            "The balance {given} of a journal item contradicts its debit and credit (balance \
             {balance})."
        ))),
        _ => {
            row.insert("balance".into(), Cell::Decimal(balance));
            Ok(())
        }
    }
}

/// The amounts of a company-currency line agree: the written one sets the other.
fn align_company_amounts(vals: &mut RawVals) -> Result<()> {
    let balance = amount_of(vals.get("balance"), "balance")?;
    let amount = amount_of(vals.get("amount_currency"), "amount_currency")?;
    match (balance, amount) {
        (Some(balance), Some(amount)) if balance != amount => Err(Error::validation(format!(
            "In the company currency the amount in currency equals the balance (balance \
             {balance}, amount in currency {amount})."
        ))),
        (Some(balance), None) => {
            vals.insert("amount_currency".into(), Cell::Decimal(balance));
            Ok(())
        }
        (None, Some(amount)) => {
            vals.insert("balance".into(), Cell::Decimal(amount));
            Ok(())
        }
        _ => Ok(()),
    }
}

/// One amount to convert between a line currency and the company currency of an entry.
struct Conversion {
    company: i64,
    date: Date,
    company_currency: i64,
    from: i64,
    to: i64,
    amount: Decimal,
}

fn real_id(id: i64) -> Result<RecId> {
    RecId::from_real(id)
        .ok_or_else(|| Error::invariant(format!("record id {id} is not a stored record id")))
}

/// Converts each amount through the `spot` rate strictly before its entry date for the entry's
/// company, rounded half away from zero on the target currency's increment (
/// 025, 032, 034;/D3). One rate book per (company, date) and chunk of currencies,
/// never one per line; a currency without any rate fails with base's `UserError`.
async fn convert_amounts(
    env: &Env,
    tx: &mut Transaction,
    conversions: &[Conversion],
) -> Result<Vec<Decimal>> {
    if conversions.is_empty() {
        return Ok(Vec::new());
    }
    let targets: Vec<i64> = conversions
        .iter()
        .map(|c| c.to)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut increments: BTreeMap<i64, Increment> = BTreeMap::new();
    for chunk in targets.chunks(MAX_CURRENCIES_PER_CALL) {
        let ids = chunk
            .iter()
            .map(|id| real_id(*id))
            .collect::<Result<Vec<_>>>()?;
        let table = RoundingTable::load(env, tx, &ids).await?;
        for id in ids {
            increments.insert(id.get(), table.get(id)?.increment().clone());
        }
    }
    // the company currency is one side of every conversion, the other side is foreign
    type Group<'a> = BTreeMap<i64, Vec<(usize, &'a Conversion)>>;
    let mut groups: BTreeMap<(i64, Date, i64), Group<'_>> = BTreeMap::new();
    for (index, c) in conversions.iter().enumerate() {
        let foreign = if c.from == c.company_currency {
            c.to
        } else {
            c.from
        };
        groups
            .entry((c.company, c.date, c.company_currency))
            .or_default()
            .entry(foreign)
            .or_default()
            .push((index, c));
    }
    let mut amounts = vec![None; conversions.len()];
    for ((company, date, company_currency), by_foreign) in groups {
        let by_foreign: Vec<_> = by_foreign.into_iter().collect();
        // one slot of each book is the company currency
        for chunk in by_foreign.chunks(MAX_CURRENCIES_PER_CALL - 1) {
            let mut ids = vec![real_id(company_currency)?];
            for (id, _) in chunk {
                ids.push(real_id(*id)?);
            }
            let book = RateBook::load(
                env,
                tx,
                real_id(company)?,
                date,
                RateDateRule::StrictlyBefore,
                &ids,
            )
            .await?;
            for (index, c) in chunk.iter().flat_map(|(_, items)| items) {
                let increment = increments.get(&c.to).ok_or_else(|| {
                    Error::invariant(format!("currency {} has no loaded rounding", c.to))
                })?;
                let exact = book
                    .snapshot(real_id(c.from)?, real_id(c.to)?)?
                    .convert_exact(c.amount)?;
                let slot = amounts
                    .get_mut(*index)
                    .ok_or_else(|| Error::invariant("a conversion index is out of range"))?;
                *slot = Some(exact.quantize(increment, CURRENCY_ROUNDING_MODE)?);
            }
        }
    }
    amounts
        .into_iter()
        .map(|amount| amount.ok_or_else(|| Error::invariant("a currency conversion was skipped")))
        .collect()
}

/// The fields of an item only the reconciliation service writes.
const RECONCILE_OWNED: [&str; 3] = [
    "full_reconcile_id",
    "matched_debit_ids",
    "matched_credit_ids",
];

fn refuse_line_inputs(vals: &RawVals) -> Result<()> {
    refuse_state(vals, "parent_state")?;
    let owned = RECONCILE_OWNED
        .iter()
        .find(|f| vals.contains(f) || vals.commands(f).is_some());
    if let Some(field) = owned {
        return Err(Error::user(format!(
            "The field {field} of a journal item is set by matching items; use Reconcile or \
             Unreconcile instead."
        )));
    }
    Ok(())
}

async fn check_storno_enabled(tx: &mut Transaction, move_ids: &[i64]) -> Result<()> {
    if move_ids.is_empty() {
        return Ok(());
    }
    let blocked: i64 = tx
        .query(
            "SELECT count(*)::int8 FROM account_move m \
             JOIN res_company c ON c.id = m.company_id \
             WHERE m.id = ANY($1::int8[]) AND COALESCE(c.account_storno, false) IS FALSE",
            &[&move_ids],
        )
        .await?[0]
        .try_get(0)
        .map_err(|e| Error::db(format!("storno setting read: {e}")))?;
    if blocked > 0 {
        return Err(Error::validation(
            "Storno journal items require enabling the storno setting ('Storno Accounting') in Company Accounting settings.",
        ));
    }
    Ok(())
}

/// The fields of a matched item whose change would move what its matches settle: the amounts,
/// the account, the currency and the partner, which its open-item row copies. Observed: the
/// reference drops the matches of such an item; modified: the change is refused until the item is
/// unmatched, on a draft as on a posted entry, for every caller.
const MATCHED_ITEM_FIELDS: [&str; 7] = [
    "account_id",
    "balance",
    "debit",
    "credit",
    "amount_currency",
    "currency_id",
    "partner_id",
];

/// Refuses a change of [`MATCHED_ITEM_FIELDS`] on an item of `ids` that has matches: a settled
/// item carries its settlement, a partly matched one its open-item row (both written only by the
/// reconciliation service), so one indexed read finds them.
async fn refuse_matched_items(tx: &mut Transaction, ids: &[i64], vals: &RawVals) -> Result<()> {
    let fields: Vec<&str> = MATCHED_ITEM_FIELDS
        .iter()
        .copied()
        .filter(|f| vals.contains(f))
        .collect();
    if fields.is_empty() || ids.is_empty() {
        return Ok(());
    }
    let rows = tx
        .query(
            "SELECT l.id::int8, COALESCE(m.name, m.id::text) FROM account_move_line l \
             JOIN account_move m ON m.id = l.move_id \
             WHERE l.id = ANY($1::int8[]) AND (l.full_reconcile_id IS NOT NULL \
               OR EXISTS (SELECT 1 FROM account_open_item o WHERE o.line_id = l.id)) \
             ORDER BY l.id LIMIT 1",
            &[&ids],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(());
    };
    let read = |e: &dyn std::fmt::Display| {
        Error::db(format!("A matched journal item could not be read: {e}"))
    };
    let id: i64 = row.try_get(0).map_err(|e| read(&e))?;
    let entry: String = row.try_get(1).map_err(|e| read(&e))?;
    Err(Error::user(format!(
        "Journal item {id} of entry {entry} is matched with other items, so its {} cannot \
         change; unmatch it first (Unreconcile).",
        fields.join(", ")
    )))
}

/// What a line reads from its entry: currency, company currency, company and date.
#[derive(Clone, Copy)]
struct EntryInfo {
    currency: Option<i64>,
    company_currency: Option<i64>,
    company: Option<i64>,
    date: Option<Date>,
}

fn date_cell(cell: &Cell, field: &str) -> Result<Option<Date>> {
    match cell {
        Cell::Null | Cell::Bool(false) => Ok(None),
        Cell::Date(date) => Ok(Some(*date)),
        other => Err(Error::validation(format!(
            "{field} takes a date, not {other:?}."
        ))),
    }
}

/// The [`EntryInfo`] of the entries of `move_ids`, by entry.
async fn move_entries(
    tx: &mut Transaction,
    env: &Env,
    move_ids: &[i64],
) -> Result<BTreeMap<i64, EntryInfo>> {
    let moves = env
        .records("account.move")?
        .browse(move_ids.iter().copied())?;
    let currencies = moves.cells(tx, "currency_id").await?;
    let company_currencies = moves.cells(tx, "company_currency_id").await?;
    let companies = moves.cells(tx, "company_id").await?;
    let dates = moves.cells(tx, "date").await?;
    let mut by_move = BTreeMap::new();
    let rows = moves
        .ids()
        .into_iter()
        .zip(currencies)
        .zip(company_currencies)
        .zip(companies)
        .zip(dates);
    for ((((id, currency), company_currency), company), date) in rows {
        by_move.insert(
            id,
            EntryInfo {
                currency: id_cell(Some(&currency), "currency_id")?,
                company_currency: id_cell(Some(&company_currency), "company_currency_id")?,
                company: id_cell(Some(&company), "company_id")?,
                date: date_cell(&date, "date")?,
            },
        );
    }
    Ok(by_move)
}

/// The conversion that fills the amount a foreign-currency row lacks: the balance from the amount
/// in currency, or the amount in currency from the balance. None when both or neither are given.
fn missing_amount(
    vals: &RawVals,
    entry: EntryInfo,
    currency: i64,
) -> Result<Option<(&'static str, Conversion)>> {
    let balance = amount_of(vals.get("balance"), "balance")?;
    let amount = amount_of(vals.get("amount_currency"), "amount_currency")?;
    let (target, given, foreign_to_company) = match (balance, amount) {
        (None, Some(amount)) => ("balance", amount, true),
        (Some(balance), None) => ("amount_currency", balance, false),
        _ => return Ok(None),
    };
    let (Some(company), Some(date), Some(company_currency)) =
        (entry.company, entry.date, entry.company_currency)
    else {
        return Err(Error::invariant(
            "a journal entry lacks the company, date or company currency its line converts at",
        ));
    };
    let (from, to) = if foreign_to_company {
        (currency, company_currency)
    } else {
        (company_currency, currency)
    };
    Ok(Some((
        target,
        Conversion {
            company,
            date,
            company_currency,
            from,
            to,
            amount: given,
        },
    )))
}

const CURRENCY_ONLY_REFUSED: &str = "Changing only the currency of a journal item that has an amount would keep its \
     amounts under the new currency: give the amount in currency (the balance is converted at the entry's rate) or \
     both amounts.";

#[methods(AccountMoveLine, addon = "account")]
impl AccountMoveLine {
    /// Turns written debits and credits into balances before the generated-column guard.
    /// On a write, a side written as zero keeps the stored amount of the other side.
    #[method]
    async fn _adapt_generated_input(
        this: &RecordSet<AccountMoveLine>,
        tx: &mut Transaction,
        input: GeneratedInput,
    ) -> Result<Vec<RawVals>> {
        let create = input.is_create();
        let mut rows = input.into_rows();
        if create {
            for row in &mut rows {
                adapt_row(row, None)?;
            }
            return Ok(rows);
        }
        let balances = this.mapped(tx, AccountMoveLine::BALANCE).await?;
        let stornos = this.mapped(tx, AccountMoveLine::IS_STORNO).await?;
        for ((row, balance), storno) in rows.iter_mut().zip(balances).zip(stornos) {
            let (positive, negative) = (balance.max(Decimal::ZERO), balance.min(Decimal::ZERO));
            // the generated expressions of debit and credit, on the stored row
            let stored = if storno {
                (negative, -positive)
            } else {
                (positive, -negative)
            };
            adapt_row(row, Some(stored))?;
        }
        Ok(rows)
    }

    /// Fills the line currency from the entry, keeps company-currency amounts equal and converts
    /// the missing amount of a foreign-currency line at the entry date (one rate read per company
    /// and date); the entries are checked for balance after the lines exist (unless skipped).
    #[method(override)]
    async fn create(
        this: &RecordSet<AccountMoveLine>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let env = this.env().clone();
        let skip = skip_balance_check(&env)?;
        super::refuse_context_defaults(
            &env,
            &[
                "move_id",
                "parent_state",
                "is_storno",
                "full_reconcile_id",
                "matched_debit_ids",
                "matched_credit_ids",
            ],
        )?;
        let mut move_ids = Vec::with_capacity(vals_list.len());
        let mut storno_moves = Vec::new();
        for vals in &vals_list {
            refuse_line_inputs(vals)?;
            if let Some(id) = id_cell(vals.get("move_id"), "move_id")? {
                move_ids.push(id);
                if matches!(vals.get("is_storno"), Some(Cell::Bool(true))) {
                    storno_moves.push(id);
                }
            }
        }
        move_ids.sort_unstable();
        move_ids.dedup();
        storno_moves.sort_unstable();
        storno_moves.dedup();
        check_storno_enabled(tx, &storno_moves).await?;
        // adding an item changes its entry: the caller needs write access on it
        super::post::hold_draft_entries(
            tx,
            &env,
            &move_ids,
            Operation::Write,
            "add items to",
            LockScope::Tax,
        )
        .await?;
        let entries = move_entries(tx, &env, &move_ids).await?;
        let mut conversions = Vec::new();
        let mut targets = Vec::new();
        for (index, vals) in vals_list.iter_mut().enumerate() {
            let entry =
                id_cell(vals.get("move_id"), "move_id")?.and_then(|id| entries.get(&id).copied());
            let Some(entry) = entry else {
                continue; // the required check names the missing entry
            };
            let currency = match id_cell(vals.get("currency_id"), "currency_id")? {
                Some(currency) => Some(currency),
                None => {
                    if let Some(currency) = entry.currency {
                        vals.insert("currency_id".into(), Cell::Id(currency));
                    }
                    entry.currency
                }
            };
            if is_text_line(vals) {
                continue; // the CHECK refuses an amount or an account on a section or note
            }
            if currency == entry.company_currency {
                align_company_amounts(vals)?;
            } else if let Some(currency) = currency
                && let Some((field, conversion)) = missing_amount(vals, entry, currency)?
            {
                conversions.push(conversion);
                targets.push((index, field));
            }
            // a line without any currency is refused by the required check of currency_id
        }
        let converted = convert_amounts(&env, tx, &conversions).await?;
        for ((index, field), amount) in targets.into_iter().zip(converted) {
            let vals = vals_list
                .get_mut(index)
                .ok_or_else(|| Error::invariant("a converted line is out of range"))?;
            vals.insert(field.into(), Cell::Decimal(amount));
        }
        let created = sup.call(tx, this.as_records(), (vals_list,)).await?;
        if !skip {
            super::refresh_entry_totals(tx, &move_ids).await?;
            check_balanced(tx, &move_ids).await?;
        }
        Ok(created)
    }

    /// A line stays in its entry (/096); on a posted or cancelled entry only
    /// [`super::post::POSTED_LINE_WRITABLE`] may be written; amounts are aligned per currency, an
    /// amount in currency written alone on a foreign line re-derives its balance at the entry date,
    /// and the entries are checked for balance after the write.
    #[method(override)]
    async fn write(
        this: &RecordSet<AccountMoveLine>,
        tx: &mut Transaction,
        mut vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        refuse_line_inputs(&vals)?;
        let env = this.env().clone();
        // a new product on an invoice line takes that product's taxes unless the write gives some
        if matches!(vals.get("product_id"), Some(Cell::Id(_)))
            && !vals.contains("tax_ids")
            && vals.commands("tax_ids").is_none()
            && let Some(context) = crate::invoice::line_tax_context(tx, this.as_records()).await?
        {
            crate::invoice::fill_line_taxes(tx, &env, &mut vals, &context).await?;
        }
        let skip = skip_balance_check(&env)?;
        let entries = this.mapped(tx, AccountMoveLine::MOVE_ID).await?;
        let mut move_ids: Vec<i64> = entries
            .iter()
            .filter_map(|id| id.and_then(|id| id.as_real()).map(|id| id.get()))
            .collect();
        if let Some(target) = id_cell(vals.get("move_id"), "move_id")? {
            if move_ids.iter().any(|id| *id != target) || move_ids.len() != this.len() {
                return Err(Error::validation(
                    "A journal item stays in the entry it was created in: delete it and add a new \
                     line to the other entry.",
                ));
            }
            vals.remove("move_id");
        }
        move_ids.sort_unstable();
        move_ids.dedup();
        if matches!(vals.get("is_storno"), Some(Cell::Bool(true))) {
            check_storno_enabled(tx, &move_ids).await?;
        }
        let closed = super::post::closed_fields(&vals, &super::post::POSTED_LINE_WRITABLE);
        if !closed.is_empty() {
            let what = format!("change {} on the items of", closed.join(", "));
            let scope = super::post::line_lock_scope(&closed);
            super::post::hold_draft_entries(tx, &env, &move_ids, Operation::Write, &what, scope)
                .await?;
        }
        refuse_matched_items(tx, &this.ids(), &vals).await?;
        let amounts = ["balance", "amount_currency", "currency_id"]
            .iter()
            .any(|f| vals.contains(f));
        if !amounts {
            return sup.call(tx, this.as_records(), (vals,)).await;
        }
        // one write per (company currency or not, stored balance when only the currency changes,
        // derived balance of a foreign line)
        let currencies = match id_cell(vals.get("currency_id"), "currency_id")? {
            Some(currency) => vec![Some(currency); this.len()],
            None => this
                .mapped(tx, AccountMoveLine::CURRENCY_ID)
                .await?
                .into_iter()
                .map(|id| id.and_then(|id| id.as_real()).map(|id| id.get()))
                .collect(),
        };
        let company_currencies = this
            .mapped(tx, AccountMoveLine::COMPANY_CURRENCY_ID)
            .await?;
        let text_lines = this.mapped(tx, AccountMoveLine::DISPLAY_TYPE).await?;
        let currency_only = !vals.contains("balance") && !vals.contains("amount_currency");
        let balances = if currency_only {
            this.mapped(tx, AccountMoveLine::BALANCE).await?
        } else {
            vec![Decimal::ZERO; this.len()]
        };
        // an amount in currency written without a balance re-derives the balance of each foreign
        // line at its entry's company and date
        let derive = vals.contains("amount_currency") && !vals.contains("balance");
        let (companies, dates) = if derive {
            (
                this.mapped(tx, AccountMoveLine::COMPANY_ID).await?,
                this.mapped(tx, AccountMoveLine::DATE).await?,
            )
        } else {
            (vec![None; this.len()], vec![None; this.len()])
        };
        let mut lines = Vec::with_capacity(this.len());
        let mut conversions = Vec::new();
        let rows = this
            .ids()
            .into_iter()
            .zip(text_lines)
            .zip(company_currencies)
            .zip(currencies)
            .zip(balances)
            .zip(companies.into_iter().zip(dates));
        for (((((id, kind), company_currency), currency), balance), (company, date)) in rows {
            let text = kind
                .as_deref()
                .is_some_and(|kind| TEXT_LINE_TYPES.contains(&kind));
            let company_currency = company_currency
                .and_then(|id| id.as_real())
                .map(|id| id.get());
            let same = !text && currency == company_currency;
            // a currency-only write would keep the stored amounts under the new currency
            // (a silent rate); the caller gives the amount in currency (converted) or both amounts.
            if currency_only && !text && !same && !balance.is_zero() {
                return Err(Error::validation(CURRENCY_ONLY_REFUSED));
            }
            let mut converted = false;
            if derive && !text && !same {
                let entry = EntryInfo {
                    currency,
                    company_currency,
                    company: company.and_then(|id| id.as_real()).map(|id| id.get()),
                    date,
                };
                let currency = currency.ok_or_else(|| {
                    Error::invariant(format!("journal item {id} has no currency"))
                })?;
                if let Some((_, conversion)) = missing_amount(&vals, entry, currency)? {
                    conversions.push(conversion);
                    converted = true;
                }
            }
            lines.push((id, same, balance, converted));
        }
        let mut derived = convert_amounts(&env, tx, &conversions).await?.into_iter();
        let mut groups: BTreeMap<(bool, Decimal, Option<Decimal>), Vec<i64>> = BTreeMap::new();
        for (id, same, balance, converted) in lines {
            let derived = if converted {
                Some(derived.next().ok_or_else(|| {
                    Error::invariant("a derived balance is missing for a journal item")
                })?)
            } else {
                None
            };
            groups.entry((same, balance, derived)).or_default().push(id);
        }
        // the chain continues once; further groups re-enter the public write with aligned values
        // (or both amounts), which form a single group there (the balance check runs once below)
        let mut sup = Some(sup);
        for ((same, stored_balance, derived), ids) in groups {
            let mut group_vals = vals.clone();
            if same {
                if currency_only {
                    group_vals.insert("amount_currency".into(), Cell::Decimal(stored_balance));
                } else {
                    align_company_amounts(&mut group_vals)?;
                }
            } else if let Some(balance) = derived {
                group_vals.insert("balance".into(), Cell::Decimal(balance));
            }
            let records = this.as_records().browse(ids)?;
            match sup.take() {
                Some(next) => next.call(tx, &records, (group_vals,)).await?,
                None => unchecked(&records).write(tx, group_vals).await?,
            }
        }
        if !skip {
            super::refresh_entry_totals(tx, &move_ids).await?;
        }
        if !skip && (vals.contains("balance") || vals.contains("amount_currency")) {
            check_balanced(tx, &move_ids).await?;
        }
        Ok(())
    }

    /// Deleting lines of a draft rechecks the balance of what remains of their entries.
    #[method(override)]
    async fn unlink(
        this: &RecordSet<AccountMoveLine>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        let skip = skip_balance_check(this.env())?;
        let mut move_ids: Vec<i64> = this
            .mapped(tx, AccountMoveLine::MOVE_ID)
            .await?
            .into_iter()
            .filter_map(|id| id.and_then(|id| id.as_real()).map(|id| id.get()))
            .collect();
        move_ids.sort_unstable();
        move_ids.dedup();
        super::post::hold_draft_entries(
            tx,
            this.env(),
            &move_ids,
            Operation::Write,
            "delete the items of",
            LockScope::Tax,
        )
        .await?;
        sup.call(tx, this.as_records(), ()).await?;
        if !skip {
            super::refresh_entry_totals(tx, &move_ids).await?;
            check_balanced(tx, &move_ids).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(value: i64) -> Decimal {
        Decimal::from(value)
    }

    #[test]
    fn one_side_written_keeps_or_clears_the_other() {
        assert_eq!(balance_of(Some(d(150)), None, None).unwrap(), d(150));
        assert_eq!(balance_of(None, Some(d(150)), None).unwrap(), d(-150));
        assert_eq!(
            balance_of(Some(d(10)), None, Some((d(0), d(20)))).unwrap(),
            d(10)
        );
        assert_eq!(
            balance_of(Some(d(0)), None, Some((d(0), d(20)))).unwrap(),
            d(-20)
        );
        assert_eq!(
            balance_of(None, Some(d(0)), Some((d(7), d(0)))).unwrap(),
            d(7)
        );
        assert_eq!(balance_of(Some(d(0)), Some(d(30)), None).unwrap(), d(-30));
        assert!(balance_of(Some(d(1)), Some(d(1)), None).is_err());
    }
}
