// SPDX-License-Identifier: MIT OR Apache-2.0
//! An entry ([`AccountMove`]) is the header: journal, company, currency, date and state. Its items
//! ([`AccountMoveLine`]) carry one stored amount, `balance` in the company currency; `debit` and
//! `credit` are virtual generated columns derived from it, and a written debit or
//! credit is turned into a balance by the line's generated-input adapter before the
//! ordinary write runs. On a line in another currency than the company's, the amount that is not
//! given is converted through the rate at the entry date (see the guide's
//! "Lines in another currency").
//!
//! An entry is created and edited as a draft, checked for balance and deleted while it is one.
//! Its state changes only through the posting service in [`post`]: `action_post`, `button_draft`
//! and `button_cancel`; a caller-written state is refused. A posted or cancelled entry
//! and its items keep their accounting fields. Rules the database will enforce with the posted-row
//! guards (company of accounts and journals, the line's entry never changing, amount in currency
//! equal to the balance in the company currency) are checked by the ORM for now.

mod display;
pub(crate) use display::draft_label;
mod line;
pub(crate) mod post;
mod reversal;

pub use line::AccountMoveLine;
pub(crate) use post::refuse_locked_items;
pub use post::{POSTED_LINE_WRITABLE, POSTED_MOVE_WRITABLE};
pub use post::{POSTER_GROUP, cancel_document_drafts, cancel_moves, post_moves, reset_moves};

use erp_sdk::Operation;
use erp_sdk::ValueMap;
use erp_sdk::prelude::*;

/// The context key that skips the balance check of drafts (`check_move_validity`).
pub const CHECK_KEY: &str = "check_move_validity";

/// The selection keys of a line's `display_type` that carry no amount and no account.
pub(crate) const TEXT_LINE_TYPES: [&str; 3] = ["line_section", "line_subsection", "line_note"];

/// `account.move`: a journal entry, draft, posted or cancelled.
#[model(
    name = "account.move",
    description = "Journal Entry",
    order = "date desc, name desc, id desc",
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    owner_only_stored_fields = [],
)]
pub struct AccountMove {
    /// Number of the entry; empty on a draft that was never numbered, given at posting.
    #[field(char, copy = false, string = "Number")]
    pub name: Option<Arc<str>>,
    /// The kind of a draft, then its number, with the reference in brackets (`display.rs`).
    #[field(char, compute = "_compute_display_name")]
    pub display_name: Option<Arc<str>>,
    /// Reference of the entry.
    #[field(char, copy = false, string = "Reference")]
    pub r#ref: Option<Arc<str>>,
    /// Accounting date; today when none is given.
    #[field(date, required = true, string = "Date")]
    pub date: Option<Date>,
    /// Draft, posted or cancelled; only the posting service changes it (`action_post`,
    /// `button_draft`, `button_cancel`).
    #[field(
        selection,
        selection = [("draft", "Draft"), ("posted", "Posted"), ("cancel", "Cancelled")],
        required = true,
        readonly = true,
        copy = false,
        default = "draft",
        string = "Status"
    )]
    pub state: Option<Arc<str>>,
    /// The kind of entry: a plain journal entry, or a customer or vendor invoice or credit note
    /// (receipts come with a later invoicing slice).
    #[field(
        selection,
        selection = [
            ("entry", "Journal Entry"),
            ("out_invoice", "Customer Invoice"),
            ("out_refund", "Customer Credit Note"),
            ("in_invoice", "Vendor Bill"),
            ("in_refund", "Vendor Credit Note"),
        ],
        required = true,
        default = "entry",
        string = "Type"
    )]
    pub move_type: Option<Arc<str>>,
    /// The journal; the first miscellaneous journal of the company when none is given.
    #[field(
        many2one,
        comodel = "account.journal",
        required = true,
        check_company = true,
        ondelete = "restrict",
        string = "Journal"
    )]
    pub journal_id: Option<RecId>,
    /// The company; follows the journal when none is given.
    #[field(many2one, comodel = "res.company", required = true, string = "Company")]
    pub company_id: Option<RecId>,
    /// Currency of the entry: the journal's, else the company's.
    #[field(
        many2one,
        comodel = "res.currency",
        required = true,
        string = "Currency"
    )]
    pub currency_id: Option<RecId>,
    /// The company currency, read through the company.
    #[field(
        many2one,
        comodel = "res.currency",
        related = "company_id.currency_id",
        string = "Company Currency"
    )]
    pub company_currency_id: Option<RecId>,
    /// Partner of the entry.
    #[field(
        many2one,
        comodel = "res.partner",
        check_company = true,
        ondelete = "restrict",
        index = "btree_not_null",
        string = "Partner"
    )]
    pub partner_id: Option<RecId>,
    /// The payment whose entry this is; written by the payment service only.
    #[field(
        many2one,
        comodel = "account.payment",
        readonly = true,
        copy = false,
        ondelete = "set null",
        index = "btree_not_null",
        string = "Payment"
    )]
    pub origin_payment_id: Option<RecId>,
    /// The journal items; deleted with the entry.
    #[field(
        one2many,
        comodel = "account.move.line",
        inverse_name = "move_id",
        copy = true,
        string = "Journal Items"
    )]
    pub line_ids: Vec<RecId>,
    /// Invoice or bill date; a customer document confirmed without one takes today.
    #[field(date, copy = false, string = "Invoice/Bill Date")]
    pub invoice_date: Option<Date>,
    /// Due date: the last instalment of the payment terms; without terms the one set by hand,
    /// else the invoice date.
    #[field(
        date,
        string = "Due Date",
        help = "With payment terms, the date of the last instalment. Without terms, the date set by hand, else the invoice date."
    )]
    pub invoice_date_due: Option<Date>,
    /// Terms that split the amount due into instalments; a new invoice takes the partner's.
    #[field(
        many2one,
        comodel = "account.payment.term",
        copy = true,
        ondelete = "restrict",
        index = "btree_not_null",
        check_company = true,
        string = "Payment Terms"
    )]
    pub invoice_payment_term_id: Option<RecId>,
    /// The lines a user edits on an invoice: products, sections and notes (the tax and
    /// receivable/payable lines of `line_ids` are kept in step by the invoice line sync).
    #[field(
        one2many,
        comodel = "account.move.line",
        inverse_name = "move_id",
        domain = [("display_type", "in", ["product", "line_section", "line_subsection", "line_note"])],
        copy = false,
        string = "Invoice Lines"
    )]
    pub invoice_line_ids: Vec<RecId>,
    /// Total without taxes, in the document currency, signed so an invoice reads positive.
    #[field(
        monetary,
        currency_field = "currency_id",
        readonly = true,
        copy = false,
        string = "Untaxed Amount"
    )]
    pub amount_untaxed: Decimal,
    /// Total of the taxes, in the document currency.
    #[field(
        monetary,
        currency_field = "currency_id",
        readonly = true,
        copy = false,
        string = "Tax"
    )]
    pub amount_tax: Decimal,
    /// Untaxed amount plus taxes, in the document currency.
    #[field(
        monetary,
        currency_field = "currency_id",
        readonly = true,
        copy = false,
        string = "Total"
    )]
    pub amount_total: Decimal,
    /// Amount still due: what remains on the receivable or payable items once matched with
    /// payments, written by the reconciliation service (`crate::reconcile`).
    #[field(
        monetary,
        currency_field = "currency_id",
        readonly = true,
        copy = false,
        string = "Amount Due"
    )]
    pub amount_residual: Decimal,
    /// Payment status, written with the amount due by the reconciliation service.
    #[field(
        selection,
        selection = [
            ("not_paid", "Not Paid"),
            ("in_payment", "In Payment"),
            ("paid", "Paid"),
            ("partial", "Partially Paid"),
            ("reversed", "Reversed"),
            ("blocked", "Blocked"),
        ],
        readonly = true,
        copy = false,
        default = "not_paid",
        string = "Payment Status"
    )]
    pub payment_state: Option<Arc<str>>,
    /// True while Reset to Draft would be refused: the entry is not cancelled (a posted entry is
    /// corrected by a reversal under the posting policy, a draft already is one), an item of it is
    /// matched, or it is a payment's entry. A form hides the button with it.
    #[field(
        boolean,
        compute = "_compute_reset_blocked",
        string = "Reset to Draft Blocked"
    )]
    pub reset_blocked: bool,
    /// Readable payments linked to or reconciled with this move.
    #[field(integer, compute = "_compute_payment_count", string = "Payment Count")]
    pub payment_count: i32,
}

#[methods(AccountMove, addon = "account")]
impl AccountMove {
    /// A new entry shows its currency before it is saved: the journal's currency when the
    /// context names a journal, else the company's, the same order `create` derives it in.
    #[method(override)]
    async fn default_get(
        this: &RecordSet<AccountMove>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<ValueMap> {
        let mut values: ValueMap = sup.call(tx, this.as_records(), (names.clone(),)).await?;
        if names.iter().any(|n| n == "currency_id") && !values.contains_key("currency_id") {
            let env = this.env();
            let mut currency = None;
            if let Some(Value::Int(journal)) = env.context().get("default_journal_id") {
                let journal = env.records("account.journal")?.browse([journal])?;
                currency = id_cell(Some(&journal.cell(tx, "currency_id").await?), "currency_id")?;
            }
            if currency.is_none() {
                let company = match env.context().get("default_company_id") {
                    Some(Value::Int(company)) => Some(company),
                    _ => env.company_id()?,
                };
                if let Some(company) = company {
                    let company = env.records("res.company")?.browse([company])?;
                    currency =
                        id_cell(Some(&company.cell(tx, "currency_id").await?), "currency_id")?;
                }
            }
            if let Some(currency) = currency {
                values.insert("currency_id".to_owned(), Value::Int(currency));
            }
        }
        Ok(values)
    }
    /// Fills the defaults of a draft and checks the balance of the new
    /// entries once, after their lines.
    #[method(override)]
    async fn create(
        this: &RecordSet<AccountMove>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let env = this.env().clone();
        let skip = skip_balance_check(&env)?;
        refuse_context_defaults(&env, &["state", PAYMENT_LINK])?;
        refuse_sync_owned_defaults(&env)?;
        for vals in &mut vals_list {
            refuse_state(vals, "state")?;
            refuse_sync_owned(vals)?;
            refuse_payment_link(vals)?;
            // given with the record, so no default (context or stored) can stand in for Post
            if !vals.contains("state") {
                vals.insert("state".into(), Cell::Str("draft".into()));
            }
            crate::invoice::prepare_create(tx, &env, vals).await?;
            fill_defaults(tx, &env, vals).await?;
            crate::invoice::fill_create_taxes(tx, &env, vals).await?;
        }
        let records = unchecked(this.as_records());
        let created = sup.call(tx, &records, (vals_list,)).await?;
        crate::invoice::sync_lines(tx, &env, &created.ids()).await?;
        refresh_entry_totals(tx, &created.ids()).await?;
        if !skip {
            check_balanced(tx, &created.ids()).await?;
        }
        Ok(created.with_env(env))
    }

    /// A journal change of a never-posted draft clears its number; line commands
    /// (`line_ids` and `invoice_line_ids`) are checked for balance once, after all of them ran. A
    /// draft invoice whose accounting date changes has its foreign-currency balances converted
    /// again at the new date. On a posted or cancelled entry only
    /// [`post::POSTED_MOVE_WRITABLE`] may be written; a written state is refused there too, since
    /// the state changes only through Post, Reset to Draft and Cancel. The totals and the payment
    /// status ([`SYNC_OWNED_FIELDS`]) are refused on a draft as well.
    #[method(override)]
    async fn write(
        this: &RecordSet<AccountMove>,
        tx: &mut Transaction,
        mut vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        refuse_state(&vals, "state")?;
        refuse_payment_link(&vals)?;
        let skip = skip_balance_check(this.env())?;
        if vals.contains("journal_id") && !vals.contains("name") {
            vals.insert("name".into(), Cell::Null);
        }
        // invoice lines are journal items too: on a plain entry no sync balances them
        let lines_changed =
            vals.commands("line_ids").is_some() || vals.commands("invoice_line_ids").is_some();
        let sync = crate::invoice::prepare_write(tx, this.as_records(), &mut vals).await?;
        // a new accounting date (given, or carried by the invoice date above) is a new rate date
        let redated = vals.contains("date");
        // on the final values: the invoice date above may have added the accounting date
        let closed = post::closed_fields(&vals, &post::POSTED_MOVE_WRITABLE);
        if !closed.is_empty() {
            let what = format!("change {} of", closed.join(", "));
            // the date, the number and the items are what a lock date protects on an entry
            let scope = if closed
                .iter()
                .any(|f| ["date", "name", "line_ids", "invoice_line_ids"].contains(f))
            {
                post::LockScope::Tax
            } else {
                post::LockScope::Unprotected
            };
            let new_date = match vals.get("date") {
                Some(Cell::Date(date)) => Some(*date),
                _ => None,
            };
            post::refuse_closed_entries(
                tx,
                this.env(),
                &this.ids(),
                Operation::Write,
                &what,
                scope,
                new_date,
            )
            .await?;
        }
        refuse_sync_owned(&vals)?;
        sup.call(tx, &unchecked(this.as_records()), (vals,)).await?;
        if sync && redated {
            crate::invoice::sync_lines_redated(tx, this.env(), &this.ids()).await?;
        } else if sync {
            crate::invoice::sync_lines(tx, this.env(), &this.ids()).await?;
        }
        if lines_changed {
            refresh_entry_totals(tx, &this.ids()).await?;
        }
        if lines_changed && !skip {
            check_balanced(tx, &this.ids()).await?;
        }
        Ok(())
    }

    /// Deleting a draft deletes its lines; no balance check runs on the way.
    /// A posted or cancelled entry is refused.
    #[method(override)]
    async fn unlink(this: &RecordSet<AccountMove>, tx: &mut Transaction, sup: Super) -> Result<()> {
        tx.flush().await?;
        post::refuse_closed_entries(
            tx,
            this.env(),
            &this.ids(),
            Operation::Unlink,
            "delete",
            post::LockScope::Tax,
            None,
        )
        .await?;
        post::refuse_consequences(tx, &this.ids(), "delete").await?;
        sup.call(tx, &unchecked(this.as_records()), ()).await
    }
}

/// The figures of an entry that only the invoice line sync writes (past this override, with the
/// base write): the totals, the amount due and the payment status. A caller value for one of them
/// is refused on create and on write, drafts included: posting does not recompute them, so a
/// draft written with a paid status or another total would keep it once posted.
pub(crate) const SYNC_OWNED_FIELDS: [&str; 5] = [
    "amount_untaxed",
    "amount_tax",
    "amount_total",
    "amount_residual",
    "payment_state",
];

/// Refuses a caller value for one of [`SYNC_OWNED_FIELDS`], naming the field.
pub(crate) fn refuse_sync_owned(vals: &RawVals) -> Result<()> {
    match SYNC_OWNED_FIELDS.iter().find(|field| vals.contains(field)) {
        None => Ok(()),
        Some(field) => Err(Error::user(format!(
            "The field {field} of a journal entry is computed from its lines and cannot be set; \
             change the lines instead."
        ))),
    }
}

/// Refuses a context default for one of [`SYNC_OWNED_FIELDS`]: the base create would apply it
/// after [`refuse_sync_owned`] has read the values.
fn refuse_sync_owned_defaults(env: &Env) -> Result<()> {
    let named = SYNC_OWNED_FIELDS
        .iter()
        .find(|field| env.context().contains(&format!("default_{field}")));
    match named {
        None => Ok(()),
        Some(field) => Err(Error::user(format!(
            "The context key default_{field} is not accepted: the field {field} of a journal \
             entry is computed from its lines."
        ))),
    }
}

/// The entry's link to its payment, set by the payment service only.
const PAYMENT_LINK: &str = "origin_payment_id";

fn refuse_payment_link(vals: &RawVals) -> Result<()> {
    if vals.contains(PAYMENT_LINK) {
        return Err(Error::user(
            "The payment of a journal entry is set when the payment is confirmed; it cannot be \
             written.",
        ));
    }
    Ok(())
}

/// The refusal of a caller-written state.
pub(crate) fn state_refusal() -> Error {
    Error::user(
        "The status of a journal entry changes only through Post, Reset to Draft and Cancel.",
    )
}

/// Refuses a caller-supplied state other than draft (`field` is `state` on entries,
/// `parent_state` on lines).
pub(crate) fn refuse_state(vals: &RawVals, field: &str) -> Result<()> {
    match vals.get(field) {
        None => Ok(()),
        Some(Cell::Str(state)) if field == "state" && state.as_ref() == "draft" => Ok(()),
        Some(_) => Err(state_refusal()),
    }
}

/// Refuses a context default for a field this addon guards on create. The base create applies
/// `default_<field>` after the checks here have read the values, so such a key would go around them:
/// an entry born posted, an item attached to a posted entry.
pub(crate) fn refuse_context_defaults(env: &Env, fields: &[&str]) -> Result<()> {
    for field in fields {
        if env.context().contains(&format!("default_{field}")) {
            return Err(Error::user(format!(
                "The context key default_{field} is not accepted when a journal entry or a journal \
                 item is created; give the value with the record."
            )));
        }
    }
    Ok(())
}

/// True when the caller's context skips the balance check; any value but a boolean is refused.
pub(crate) fn skip_balance_check(env: &Env) -> Result<bool> {
    match env.context().get(CHECK_KEY) {
        None | Some(Value::Bool(true)) => Ok(false),
        Some(Value::Bool(false)) => Ok(true),
        Some(other) => Err(Error::programming(format!(
            "The context key {CHECK_KEY} takes true or false, not {other:?}."
        ))),
    }
}

/// The same records with the balance check skipped, so nested line commands do not check an entry
/// that is only half written; the caller checks once at the end.
pub(crate) fn unchecked(records: &Records) -> Records {
    records.with_context([(CHECK_KEY, Value::Bool(false))])
}

/// The statement setting the total of plain entries (`move_type = entry`) from their items: the
/// sum of the debit items' amounts in the entry's currency, as the reference shows a
/// miscellaneous entry's total (invoice totals come from the invoice line sync). `filter` narrows
/// the entries (`e` is the entry); only entries whose total changed are written.
fn entry_totals_sql(filter: &str) -> String {
    format!(
        "UPDATE account_move m SET amount_total = t.total \
         FROM (SELECT e.id, COALESCE(SUM(l.amount_currency) FILTER (WHERE l.balance > 0), 0) \
               AS total \
               FROM account_move e LEFT JOIN account_move_line l ON l.move_id = e.id \
               WHERE e.move_type = 'entry'{filter} GROUP BY e.id) t \
         WHERE m.id = t.id AND m.amount_total IS DISTINCT FROM t.total"
    )
}

/// Sets the total of the plain entries among `move_ids` from their items. Called where items change
/// with the balance check on, and at posting: items written directly with the check skipped give
/// their entry its total at the next entry write or at posting.
pub(crate) async fn refresh_entry_totals(tx: &mut Transaction, move_ids: &[i64]) -> Result<()> {
    if move_ids.is_empty() {
        return Ok(());
    }
    tx.flush().await?;
    tx.execute(
        &entry_totals_sql(" AND e.id = ANY($1::int8[])"),
        &[&move_ids],
    )
    .await?;
    let moves = tx
        .env()
        .records("account.move")?
        .browse(move_ids.iter().copied())?;
    tx.invalidate_records(&moves, Some(&["amount_total"])).await
}

/// Entries posted before 1.3 kept a zero total unless they were invoices: give every plain entry
/// the total of its debit items.
#[erp_sdk::migration(addon = "account", version = "1.3", stage = post)]
pub async fn fill_entry_totals(
    tx: &mut Transaction,
    _from: &erp_sdk::migrations::SemverVersion,
) -> Result<()> {
    tx.execute(&entry_totals_sql(""), &[]).await?;
    Ok(())
}

/// Refuses the entries among `move_ids` whose balances do not sum to zero. Amounts are exact on
/// the company currency increment, so the sum is compared with zero without rounding.
/// One aggregate for all the entries.
pub(crate) async fn check_balanced(tx: &mut Transaction, move_ids: &[i64]) -> Result<()> {
    if move_ids.is_empty() {
        return Ok(());
    }
    tx.flush().await?;
    let rows = tx
        .query(
            "SELECT move_id, SUM(balance)::text FROM account_move_line \
             WHERE move_id = ANY($1::int8[]) GROUP BY move_id HAVING SUM(balance) <> 0 \
             ORDER BY move_id",
            &[&move_ids],
        )
        .await?;
    let mut unbalanced = Vec::with_capacity(rows.len());
    for row in &rows {
        let id: i64 = row
            .try_get(0)
            .map_err(|e| Error::db(format!("An entry id could not be read: {e}")))?;
        let difference: String = row
            .try_get(1)
            .map_err(|e| Error::db(format!("An entry balance could not be read: {e}")))?;
        unbalanced.push((id, difference));
    }
    match unbalanced.as_slice() {
        [] => Ok(()),
        [(id, difference)] => Err(Error::user(format!(
            "Debits and credits must be equal before saving; in entry {id} they differ by {difference}."
        ))),
        many => Err(Error::user(format!(
            "Debits and credits must be equal before saving; they differ in these entries:\n{}",
            many.iter()
                .map(|(id, difference)| format!("- entry {id}: off by {difference}"))
                .collect::<Vec<_>>()
                .join("\n")
        ))),
    }
}

/// The record id in a many2one cell, `None` when empty.
pub(crate) fn id_cell(cell: Option<&Cell>, field: &str) -> Result<Option<i64>> {
    match cell {
        None | Some(Cell::Null | Cell::Bool(false)) => Ok(None),
        Some(Cell::Id(id)) => Ok(Some(*id)),
        Some(Cell::Int(id)) => Ok(Some(i64::from(*id))),
        Some(other) => Err(Error::validation(format!(
            "{field} takes a record id, not {other:?}."
        ))),
    }
}

/// Journal, company, currency, date and type of a new draft, in the order the reference derives
/// them: the journal from the company (the given one, else the active one), the company from the
/// journal, the currency from the journal then the company.
async fn fill_defaults(tx: &mut Transaction, env: &Env, vals: &mut RawVals) -> Result<()> {
    let given_company = id_cell(vals.get("company_id"), "company_id")?;
    let journal = match id_cell(vals.get("journal_id"), "journal_id")? {
        Some(journal) => journal,
        None => {
            let company = match given_company {
                Some(company) => company,
                None => env.company_id()?.ok_or_else(|| {
                    Error::user("No company is active: give the journal entry a company.")
                })?,
            };
            let currency = id_cell(vals.get("currency_id"), "currency_id")?;
            let journal = default_journal(tx, env, company, currency).await?;
            vals.insert("journal_id".into(), Cell::Id(journal));
            journal
        }
    };
    let journals = env.records("account.journal")?;
    let record = journals.browse([journal])?;
    if given_company.is_none() {
        let company = match env.company_id()? {
            // the active company when the journal belongs to it or one of its parents
            Some(active) => {
                let shared = journals
                    .search(
                        tx,
                        &Domain::and([
                            Domain::condition("id", Operator::Eq, journal)?,
                            Domain::condition("company_id", Operator::ParentOf, active)?,
                        ]),
                    )
                    .await?;
                if shared.is_empty() {
                    id_cell(Some(&record.cell(tx, "company_id").await?), "company_id")?
                } else {
                    Some(active)
                }
            }
            None => id_cell(Some(&record.cell(tx, "company_id").await?), "company_id")?,
        };
        if let Some(company) = company {
            vals.insert("company_id".into(), Cell::Id(company));
        }
    }
    if id_cell(vals.get("currency_id"), "currency_id")?.is_none() {
        let currency = match id_cell(Some(&record.cell(tx, "currency_id").await?), "currency_id")? {
            Some(currency) => Some(currency),
            None => match id_cell(vals.get("company_id"), "company_id")? {
                Some(company) => id_cell(
                    Some(
                        &env.records("res.company")?
                            .browse([company])?
                            .cell(tx, "currency_id")
                            .await?,
                    ),
                    "currency_id",
                )?,
                None => None,
            },
        };
        if let Some(currency) = currency {
            vals.insert("currency_id".into(), Cell::Id(currency));
        }
    }
    if matches!(
        vals.get("date"),
        None | Some(Cell::Null | Cell::Bool(false))
    ) {
        let company = id_cell(vals.get("company_id"), "company_id")?.ok_or_else(|| {
            Error::validation("A journal entry needs a company before its date can be set.")
        })?;
        let days = erp_addon_base::company_calendar::business_days(tx, env, &[company]).await?;
        let day = days
            .get(&company)
            .ok_or_else(|| Error::missing(format!("Company {company} is unavailable.")))?;
        vals.insert("date".into(), Cell::Date(day.date));
    }
    Ok(())
}

/// The first miscellaneous journal the caller can read in `company` or one of its parents, in the
/// journal order; one in `currency` first when a currency is given.
async fn default_journal(
    tx: &mut Transaction,
    env: &Env,
    company: i64,
    currency: Option<i64>,
) -> Result<i64> {
    let found = env
        .records("account.journal")?
        .search(
            tx,
            &Domain::and([
                Domain::condition("type", Operator::Eq, "general")?,
                Domain::condition("company_id", Operator::ParentOf, company)?,
            ]),
        )
        .await?;
    let ids = found.ids();
    if let Some(currency) = currency {
        let currencies = found.cells(tx, "currency_id").await?;
        if let Some(position) = currencies.iter().position(|c| *c == Cell::Id(currency)) {
            return Ok(ids[position]);
        }
    }
    ids.first().copied().ok_or_else(|| {
        Error::user(format!(
            "No miscellaneous journal is available for company {company}: create one in \
             Accounting, Configuration, Journals, or give the entry a journal."
        ))
    })
}
