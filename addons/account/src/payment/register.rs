// SPDX-License-Identifier: MIT OR Apache-2.0
//! The Register Payment dialog (`account.payment.register`, observed in the reference
//! `wizard/account_payment_register.py`, our own implementation).
//!
//! Opened from posted invoices, bills and credit notes of one company. It proposes the journal (the
//! first bank, then cash journal of the company), its method line for the direction, today's date,
//! the amount due and a memo of the document numbers. Confirming creates the payments through the
//! payment model with the caller's rights, posts them and matches each with its documents in the
//! reconciliation unit, so the documents show Paid or Partially Paid at once. Grouped (the default),
//! one payment per commercial partner carries the netted amount; not grouped, every document gets
//! its own payment for its full amount due. Only a registration of one payment takes an entered
//! amount (below the due it leaves the rest open). A dialog is its creator's, registers once (its
//! row is deleted with the payments) and pays only what it showed as due.
//!
//! First cut: documents in the company currency on one receivable or payable account per payment.
//! Refused by name: other currencies, write-offs of a difference, installments, early-payment
//! discounts and blocked documents.

use std::collections::{BTreeMap, BTreeSet};

use erp_sdk::prelude::*;
use erp_sdk::serde_json::{Value as Json, json};
use erp_sdk::{Command, Operation, ValueMap};

use crate::moves::AccountMove;
use crate::reconcile::reconcile_lines;

const INVOICE_TYPES: [&str; 4] = ["out_invoice", "out_refund", "in_invoice", "in_refund"];

/// The temporary input of one registration; rows expire after an hour. A row is its creator's
/// only (the access row's creator predicate) and of its documents' company.
///
/// No default records: the reference ships none (a wizard).
#[model(
    name = "account.payment.register",
    description = "Register Payment",
    kind = "transient",
    transient_max_age_seconds = 3600,
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny")
)]
pub struct AccountPaymentRegister {
    /// The documents to pay: posted invoices, bills and credit notes with an amount due.
    #[field(
        many2many,
        comodel = "account.move",
        relation = "account_payment_register_move_rel",
        column1 = "wizard_id",
        column2 = "move_id",
        domain = [
            ("state", "=", "posted"),
            ("payment_state", "in", ["not_paid", "partial"]),
            ("move_type", "in", ["out_invoice", "out_refund", "in_invoice", "in_refund"]),
        ],
        required = true,
        string = "Documents"
    )]
    pub invoice_ids: Vec<RecId>,
    /// The documents' company.
    #[field(
        many2one,
        comodel = "res.company",
        compute = "_compute_company",
        store,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// The company currency, in which this release pays.
    #[field(
        many2one,
        comodel = "res.currency",
        compute = "_compute_company",
        store,
        string = "Currency"
    )]
    pub currency_id: Option<RecId>,
    /// The commercial partner of the documents.
    #[field(
        many2one,
        comodel = "res.partner",
        compute = "_compute_from_documents",
        store,
        string = "Customer/Vendor"
    )]
    pub partner_id: Option<RecId>,
    /// Customer documents or vendor documents.
    #[field(
        selection,
        selection = [("customer", "Customer"), ("supplier", "Vendor")],
        compute = "_compute_from_documents",
        store,
        string = "Partner Type"
    )]
    pub partner_type: Option<Arc<str>>,
    /// Received or sent, from the sign of what the documents leave due.
    #[field(
        selection,
        selection = [("outbound", "Send"), ("inbound", "Receive")],
        compute = "_compute_from_documents",
        store,
        string = "Payment Type"
    )]
    pub payment_type: Option<Arc<str>>,
    /// What the documents leave due, netted (credit notes lower it).
    #[field(
        monetary,
        currency_field = "currency_id",
        compute = "_compute_from_documents",
        store,
        string = "Amount Due"
    )]
    pub amount_due: Decimal,
    /// The amount to pay; the amount due unless changed.
    #[field(
        monetary,
        currency_field = "currency_id",
        compute = "_compute_from_documents",
        store,
        readonly = false,
        string = "Amount"
    )]
    pub amount: Decimal,
    /// The memo of the payment: the document numbers unless changed.
    #[field(
        char,
        compute = "_compute_from_documents",
        store,
        readonly = false,
        string = "Memo"
    )]
    pub communication: Option<Arc<str>>,
    /// The bank or cash journal that receives or sends the money.
    #[field(
        many2one,
        comodel = "account.journal",
        domain = [("type", "in", ["bank", "cash", "credit"])],
        compute = "_compute_journal",
        store,
        readonly = false,
        string = "Journal"
    )]
    pub journal_id: Option<RecId>,
    /// The journal's method line for the direction.
    #[field(
        many2one,
        comodel = "account.payment.method.line",
        compute = "_compute_method_line",
        store,
        readonly = false,
        string = "Payment Method"
    )]
    pub payment_method_line_id: Option<RecId>,
    /// The payment date.
    #[field(date, required = true, string = "Payment Date")]
    pub payment_date: Option<Date>,
    /// One payment for all documents of a partner (on), or one per document for its full amount
    /// due (off).
    #[field(boolean, default = true, string = "Group Payments")]
    pub group_payment: bool,
    /// The registration makes one payment: only then can its amount and memo be changed.
    #[field(boolean, compute = "_compute_single_payment", string = "One Payment")]
    pub single_payment: bool,
    /// Difference between amount due and entered payment amount (`amount_due - amount`).
    #[field(
        monetary,
        currency_field = "currency_id",
        compute = "_compute_payment_difference",
        string = "Payment Difference"
    )]
    pub payment_difference: Decimal,
    /// Whether a payment difference stays open on the invoice or is written off to an account.
    #[field(
        selection,
        selection = [
            ("open", "Keep open"),
            ("reconcile", "Mark as fully paid"),
        ],
        required = true,
        default = "open",
        string = "Payment Difference Handling"
    )]
    pub payment_difference_handling: Option<Arc<str>>,
    /// Expense or income account receiving the write-off when marking fully paid.
    #[field(
        many2one,
        comodel = "account.account",
        check_company = true,
        ondelete = "restrict",
        string = "Post Difference In"
    )]
    pub writeoff_account_id: Option<RecId>,
    /// Label of the write-off journal item.
    #[field(char, default = "Write-Off", string = "Label")]
    pub writeoff_label: Option<Arc<str>>,
}

/// What a document brings to a registration, read with the caller's rights.
struct Document {
    id: i64,
    name: String,
    kind: String,
    state: String,
    payment_state: String,
    company: i64,
    currency: i64,
    partner: Option<i64>,
    residual: Decimal,
    account: Option<i64>,
}

impl Document {
    fn customer(&self) -> bool {
        self.kind.starts_with("out_")
    }

    /// The amount due with its sign: positive when the partner owes it (invoice) or we owe it
    /// (bill), negative for credit notes.
    fn signed(&self) -> Decimal {
        if self.kind.ends_with("_refund") {
            -self.residual
        } else {
            self.residual
        }
    }
}

/// An action dictionary as the RPC answers it.
fn action(json: Json) -> Value {
    Value::Json(std::sync::Arc::new(json))
}

fn db(error: impl std::fmt::Display) -> Error {
    Error::db(format!(
        "A document could not be read for the payment: {error}"
    ))
}

/// Reads the documents of `moves` after the caller's read check, in id order: their facts and the
/// receivable or payable account of their open items (`None` when they have several). In an
/// onchange the dialog's documents are in-memory records: callers pass their origin.
async fn documents(tx: &mut Transaction, moves: &Records) -> Result<Vec<Document>> {
    moves.check_access(tx, Operation::Read).await?;
    let ids = moves.ids();
    let rows = tx
        .query(
            "SELECT m.id::int8, COALESCE(m.name, '')::text, m.move_type::text, m.state::text, \
             COALESCE(m.payment_state, '')::text, m.company_id::int8, m.currency_id::int8, \
             co.currency_id::int8, COALESCE(p.commercial_partner_id, p.id)::int8, \
             COALESCE(m.amount_residual, 0), \
             (SELECT CASE WHEN count(DISTINCT l.account_id) = 1 THEN min(l.account_id) END::int8 \
              FROM account_move_line l WHERE l.move_id = m.id AND l.display_type = 'payment_term' \
              AND l.full_reconcile_id IS NULL) \
             FROM account_move m JOIN res_company co ON co.id = m.company_id \
             LEFT JOIN res_partner p ON p.id = m.partner_id \
             WHERE m.id = ANY($1::int8[]) ORDER BY m.id",
            &[&ids],
        )
        .await?;
    if rows.len() != ids.len() {
        let found: Vec<i64> = rows.iter().filter_map(|row| row.try_get(0).ok()).collect();
        let missing: Vec<String> = ids
            .iter()
            .filter(|id| !found.contains(id))
            .map(i64::to_string)
            .collect();
        return Err(Error::missing(format!(
            "The documents {} no longer exist; open Register Payment again.",
            missing.join(", ")
        )));
    }
    rows.iter()
        .map(|row| {
            Ok(Document {
                id: row.try_get(0).map_err(db)?,
                name: row.try_get(1).map_err(db)?,
                kind: row.try_get(2).map_err(db)?,
                state: row.try_get(3).map_err(db)?,
                payment_state: row.try_get(4).map_err(db)?,
                company: row.try_get(5).map_err(db)?,
                currency: row.try_get(6).map_err(db)?,
                partner: row.try_get(8).map_err(db)?,
                residual: row.try_get(9).map_err(db)?,
                account: row.try_get(10).map_err(db)?,
            })
        })
        .collect()
}

/// One payment a registration creates.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Planned {
    pub partner: Option<i64>,
    pub account: i64,
    pub customer: bool,
    pub inbound: bool,
    pub amount: Decimal,
    pub memo: String,
    pub documents: Vec<i64>,
}

/// The checks a registration asks of its documents, each refused by the document's name.
fn admit(docs: &[Document]) -> Result<()> {
    let Some(first) = docs.first() else {
        return Err(Error::user("Choose at least one document to pay."));
    };
    for doc in docs {
        if !INVOICE_TYPES.contains(&doc.kind.as_str()) {
            return Err(Error::user(format!(
                "{} is not an invoice, bill or credit note; journal entries are matched with \
                 payments by reconciliation instead.",
                doc.name_or_draft()
            )));
        }
        if doc.state != "posted" {
            return Err(Error::user(format!(
                "Only confirmed documents can be paid; {} is not confirmed.",
                doc.name_or_draft()
            )));
        }
        if doc.payment_state == "blocked" {
            return Err(Error::user(format!("{} is blocked for payment.", doc.name)));
        }
        if doc.residual.is_zero() || !matches!(doc.payment_state.as_str(), "not_paid" | "partial") {
            return Err(Error::user(format!(
                "{} has nothing left to pay.",
                doc.name
            )));
        }
        if doc.account.is_none() {
            return Err(Error::user(format!(
                "{} has its amount due on several receivable or payable accounts; such documents \
                 cannot be paid here yet.",
                doc.name
            )));
        }
    }
    if docs.iter().any(|d| d.currency != first.currency) {
        return Err(Error::user(
            "The documents use different currencies; register payments one currency at a time.",
        ));
    }
    if docs.iter().any(|d| d.company != first.company) {
        return Err(Error::user(
            "The documents belong to several companies; register their payments one company at \
             a time.",
        ));
    }
    if docs.iter().any(|d| d.customer() != first.customer()) {
        return Err(Error::user(
            "Customer documents and vendor documents cannot be paid in one registration.",
        ));
    }
    Ok(())
}

impl Document {
    /// The number, or for an unnumbered draft its label and id ("Draft Entry 12").
    fn name_or_draft(&self) -> String {
        if self.name.is_empty() || self.name == "/" {
            let label = crate::moves::draft_label(Some(&self.kind)).unwrap_or("Draft");
            format!("{label} {}", self.id)
        } else {
            self.name.clone()
        }
    }
}

fn memo(docs: &[&Document]) -> String {
    let mut names: Vec<&str> = docs.iter().map(|d| d.name.as_str()).collect();
    names.sort_unstable();
    names.join(", ")
}

/// The documents of each payment: grouped, one group per commercial partner and account; not
/// grouped, one per document.
fn groups(docs: &[Document], group: bool) -> BTreeMap<(Option<i64>, i64), Vec<&Document>> {
    let mut groups: BTreeMap<(Option<i64>, i64), Vec<&Document>> = BTreeMap::new();
    for doc in docs {
        let account = doc.account.unwrap_or_default();
        let key = if group {
            (doc.partner, account)
        } else {
            (Some(doc.id), account)
        };
        groups.entry(key).or_default().push(doc);
    }
    groups
}

/// The payments of a registration: grouped, one per commercial
/// partner and account carrying the netted amount; not grouped, one per document for its own
/// amount due. When there is one payment it carries `amount`, and `memo_given` replaces the
/// document numbers. `due` is what the dialog showed as due: when the documents now leave
/// another amount due, or several payments would be made from a changed amount, it is refused.
fn plan(
    docs: &[Document],
    group: bool,
    amount: Decimal,
    due: Decimal,
    memo_given: Option<&str>,
) -> Result<Vec<Planned>> {
    admit(docs)?;
    let all: Vec<&Document> = docs.iter().collect();
    let now: Decimal = docs.iter().map(Document::signed).sum::<Decimal>().abs();
    if now != due {
        return Err(Error::user(format!(
            "What is due on {} changed since the dialog was opened ({due} then, {now} now); open \
             Register Payment again.",
            memo(&all)
        )));
    }
    let groups = groups(docs, group);
    let single = groups.len() == 1;
    if !single && amount != due {
        return Err(Error::user(format!(
            "The amount was changed, but {} payments would be made ({}); keep the amount due, or \
             register the payments one at a time.",
            groups.len(),
            if group {
                "one per partner and account"
            } else {
                "one per document"
            }
        )));
    }
    let mut out = Vec::with_capacity(groups.len());
    for ((_, account), members) in groups {
        let customer = members[0].customer();
        let net: Decimal = members.iter().map(|d| d.signed()).sum();
        if net.is_zero() {
            return Err(Error::user(format!(
                "The amounts due of {} offset each other, so no payment is made.",
                memo(&members)
            )));
        }
        let pay = if single { amount } else { net.abs() };
        if pay <= Decimal::ZERO {
            return Err(Error::user("The amount to pay must be more than zero."));
        }
        let memo = match memo_given.map(str::trim) {
            Some(given) if single && !given.is_empty() => given.to_owned(),
            _ => memo(&members),
        };
        out.push(Planned {
            partner: members[0].partner,
            account,
            customer,
            // a customer pays what is due to us; we pay a vendor what we owe; credit notes turn it
            inbound: customer == (net > Decimal::ZERO),
            amount: pay,
            memo,
            documents: members.iter().map(|d| d.id).collect(),
        });
    }
    Ok(out)
}

/// Resolve the company before the wizard row and its relations exist. Only the final
/// link set determines the date; reads use the same principal as the dialog.
async fn default_payment_date(
    tx: &mut Transaction,
    records: &Records,
    values: &ValueMap,
) -> Result<Date> {
    let env = records.env();
    let moves = env.records("account.move")?;
    let mut ids = BTreeSet::new();
    if let Some(value) = values.get("invoice_ids") {
        for command in records.commands_from_value(&moves, "invoice_ids", value)? {
            match command {
                Command::Set(next) => {
                    ids = next.into_iter().map(|id| id.get()).collect();
                }
                Command::Link(id) => {
                    ids.insert(id.get());
                }
                Command::Clear => ids.clear(),
                Command::Unlink(id) => {
                    ids.remove(&id.get());
                }
                Command::Create(_) | Command::Update(..) | Command::Delete(_) => {
                    return Err(Error::validation(
                        "Choose existing invoice links before defaulting the payment date.",
                    ));
                }
            }
        }
    }
    let company = if ids.is_empty() {
        // An empty new dialog uses its active company; document-linked dialogs never do.
        env.company_id()?
            .ok_or_else(|| Error::validation("Select a company before opening Register Payment."))?
    } else {
        let docs = moves.browse(ids)?;
        let companies = docs.cells(tx, "company_id").await?;
        let mut unique = BTreeSet::new();
        for cell in companies {
            unique.insert(
                crate::moves::id_cell(Some(&cell), "company_id")?
                    .ok_or_else(|| Error::validation("A payment document needs a company."))?,
            );
        }
        if unique.len() != 1 {
            return Err(Error::user(
                "The documents belong to several companies; register their payments one company at a time.",
            ));
        }
        unique
            .into_iter()
            .next()
            .ok_or_else(|| Error::missing("The payment documents are unavailable."))?
    };
    let days = erp_addon_base::company_calendar::business_days(tx, env, &[company]).await?;
    days.get(&company)
        .map(|day| day.date)
        .ok_or_else(|| Error::missing(format!("Company {company} is unavailable.")))
}

#[methods(AccountPaymentRegister, addon = "account")]
impl AccountPaymentRegister {
    /// The company of the first document and its currency. Read-only (a candidate compute): a
    /// create that carries the amount is admitted instead of refused for an async currency. The
    /// documents are linked after the INSERT, so this sees none then and the company and currency
    /// arrive with the recompute after the link.
    #[compute(candidate, depends = ["invoice_ids"])]
    async fn _compute_company(
        this: &RecordSet<AccountPaymentRegister>,
        tx: &mut ReadTx<'_>,
        out: &mut Assign<'_, AccountPaymentRegister>,
    ) -> Result<()> {
        let lists = tx.mapped(this, AccountPaymentRegister::INVOICE_IDS).await?;
        for (i, list) in lists.into_iter().enumerate() {
            // in an onchange the documents are in-memory records of their stored rows; the list is
            // the x2many read, which leaves out the documents the caller's record rules hide
            let ids: Vec<i64> = list
                .iter()
                .filter_map(|id| id.as_real().or(id.origin()))
                .map(|id| id.get())
                .collect();
            let rows = tx
                .query(
                    "SELECT m.company_id::int8, m.currency_id::int8 FROM account_move m \
                     WHERE m.id = ANY($1::int8[]) ORDER BY m.id LIMIT 1",
                    &[&ids],
                )
                .await?;
            let (company, currency) = match rows.first() {
                Some(row) => (
                    RecId::from_real(row.try_get(0).map_err(db)?),
                    RecId::from_real(row.try_get(1).map_err(db)?),
                ),
                None => (None, None),
            };
            out.set(i, AccountPaymentRegister::COMPANY_ID, company)?;
            out.set(i, AccountPaymentRegister::CURRENCY_ID, currency)?;
        }
        Ok(())
    }

    /// Partner, direction, amount due, amount and memo from the documents. Lenient: what
    /// cannot be paid together is refused by name at confirmation, not while the dialog is filled.
    ///
    /// The first computation keeps an amount and a memo the row already holds: the client creates
    /// the dialog with what the user entered, and the documents are linked (and this computed)
    /// after the INSERT, outside the protection of the given values (`relcrud.rs`, x2many writes
    /// of a create). An amount due already computed means the documents changed: both reset.
    #[compute(depends = ["invoice_ids"])]
    async fn _compute_from_documents(
        this: &RecordSet<AccountPaymentRegister>,
        tx: &mut Transaction,
        out: &mut Assign<'_, AccountPaymentRegister>,
    ) -> Result<()> {
        let lists = this.mapped(tx, AccountPaymentRegister::INVOICE_IDS).await?;
        let held_due = this.mapped(tx, AccountPaymentRegister::AMOUNT_DUE).await?;
        let held_amount = this.mapped(tx, AccountPaymentRegister::AMOUNT).await?;
        let held_memo = this
            .mapped(tx, AccountPaymentRegister::COMMUNICATION)
            .await?;
        let moves = this.env().records("account.move")?;
        for (i, list) in lists.into_iter().enumerate() {
            let first = held_due[i].is_zero();
            let docs = documents(tx, &moves.browse_ids(list).origin()).await?;
            let net: Decimal = docs.iter().map(Document::signed).sum();
            let customer = docs.first().is_none_or(Document::customer);
            let partner = docs
                .first()
                .and_then(|d| d.partner)
                .filter(|p| docs.iter().all(|d| d.partner == Some(*p)));
            out.set(
                i,
                AccountPaymentRegister::PARTNER_ID,
                partner.and_then(RecId::from_real),
            )?;
            out.set(
                i,
                AccountPaymentRegister::PARTNER_TYPE,
                Some(Arc::<str>::from(if customer {
                    "customer"
                } else {
                    "supplier"
                })),
            )?;
            let inbound = customer == (net >= Decimal::ZERO);
            out.set(
                i,
                AccountPaymentRegister::PAYMENT_TYPE,
                Some(Arc::<str>::from(if inbound {
                    "inbound"
                } else {
                    "outbound"
                })),
            )?;
            out.set(i, AccountPaymentRegister::AMOUNT_DUE, net.abs())?;
            let amount = match held_amount[i] {
                held if first && !held.is_zero() => held,
                _ => net.abs(),
            };
            out.set(i, AccountPaymentRegister::AMOUNT, amount)?;
            let all: Vec<&Document> = docs.iter().collect();
            let communication = match &held_memo[i] {
                Some(held) if first && !held.trim().is_empty() => Arc::clone(held),
                _ => Arc::<str>::from(memo(&all)),
            };
            out.set(
                i,
                AccountPaymentRegister::COMMUNICATION,
                Some(communication),
            )?;
        }
        Ok(())
    }

    /// Difference between what is due and the entered amount (`amount_due - amount`).
    #[compute(depends = ["amount_due", "amount"])]
    async fn _compute_payment_difference(
        this: &RecordSet<AccountPaymentRegister>,
        tx: &mut Transaction,
        out: &mut Assign<'_, AccountPaymentRegister>,
    ) -> Result<()> {
        let dues = this.mapped(tx, AccountPaymentRegister::AMOUNT_DUE).await?;
        let amounts = this.mapped(tx, AccountPaymentRegister::AMOUNT).await?;
        for (i, (due, amt)) in dues.into_iter().zip(amounts).enumerate() {
            out.set(i, AccountPaymentRegister::PAYMENT_DIFFERENCE, due - amt)?;
        }
        Ok(())
    }

    /// Whether the documents make one payment (grouping as chosen).
    #[compute(depends = ["invoice_ids", "group_payment"])]
    async fn _compute_single_payment(
        this: &RecordSet<AccountPaymentRegister>,
        tx: &mut Transaction,
        out: &mut Assign<'_, AccountPaymentRegister>,
    ) -> Result<()> {
        let lists = this.mapped(tx, AccountPaymentRegister::INVOICE_IDS).await?;
        let grouped = this
            .mapped(tx, AccountPaymentRegister::GROUP_PAYMENT)
            .await?;
        let moves = this.env().records("account.move")?;
        for (i, (list, group)) in lists.into_iter().zip(grouped).enumerate() {
            let docs = documents(tx, &moves.browse_ids(list).origin()).await?;
            out.set(
                i,
                AccountPaymentRegister::SINGLE_PAYMENT,
                groups(&docs, group).len() <= 1,
            )?;
        }
        Ok(())
    }

    /// The company's first bank journal, else its first cash journal; a journal the row already
    /// holds (the user's choice, given with the create) is kept: Pay refuses one of another company.
    #[compute(depends = ["company_id"])]
    async fn _compute_journal(
        this: &RecordSet<AccountPaymentRegister>,
        tx: &mut Transaction,
        out: &mut Assign<'_, AccountPaymentRegister>,
    ) -> Result<()> {
        let companies = this.mapped(tx, AccountPaymentRegister::COMPANY_ID).await?;
        let held = this.mapped(tx, AccountPaymentRegister::JOURNAL_ID).await?;
        let journals = this.env().records("account.journal")?;
        for (i, company) in companies.into_iter().enumerate() {
            let mut found = held[i];
            if found.is_none()
                && let Some(company) = company
            {
                for kind in ["bank", "cash"] {
                    let hits = journals
                        .search(
                            tx,
                            &Domain::and([
                                Domain::condition("company_id", Operator::Eq, company.get())?,
                                Domain::condition("type", Operator::Eq, kind)?,
                            ]),
                        )
                        .await?;
                    if let Some(first) = hits.ids().first() {
                        found = RecId::from_real(*first);
                        break;
                    }
                }
            }
            out.set(i, AccountPaymentRegister::JOURNAL_ID, found)?;
        }
        Ok(())
    }

    /// The journal's first method line of the direction; a line the row already holds is kept
    /// while it is one of the journal's lines of the direction.
    #[compute(depends = ["journal_id", "payment_type"])]
    async fn _compute_method_line(
        this: &RecordSet<AccountPaymentRegister>,
        tx: &mut Transaction,
        out: &mut Assign<'_, AccountPaymentRegister>,
    ) -> Result<()> {
        let journals = this.mapped(tx, AccountPaymentRegister::JOURNAL_ID).await?;
        let kinds = this
            .mapped(tx, AccountPaymentRegister::PAYMENT_TYPE)
            .await?;
        let held = this
            .mapped(tx, AccountPaymentRegister::PAYMENT_METHOD_LINE_ID)
            .await?;
        let lines = this.env().records("account.payment.method.line")?;
        for (i, (journal, kind)) in journals.into_iter().zip(kinds).enumerate() {
            let mut found = None;
            if let Some(journal) = journal {
                let direction = kind.as_deref().unwrap_or("inbound");
                let hits = lines
                    .search(
                        tx,
                        &Domain::and([
                            Domain::condition("journal_id", Operator::Eq, journal.get())?,
                            Domain::condition("payment_type", Operator::Eq, direction)?,
                        ]),
                    )
                    .await?;
                let ids = hits.ids();
                found = held[i]
                    .filter(|line| ids.contains(&line.get()))
                    .or_else(|| ids.first().and_then(|id| RecId::from_real(*id)));
            }
            out.set(i, AccountPaymentRegister::PAYMENT_METHOD_LINE_ID, found)?;
        }
        Ok(())
    }

    /// The payment date follows the documents' company when the dialog opens.
    #[method(override)]
    async fn default_get(
        this: &RecordSet<AccountPaymentRegister>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<ValueMap> {
        let wants_date = names.iter().any(|n| n == "payment_date");
        let wants_documents = names.iter().any(|n| n == "invoice_ids");
        let mut requested = names;
        if wants_date && !wants_documents {
            requested.push("invoice_ids".to_owned());
        }
        let mut values: ValueMap = sup.call(tx, this.as_records(), (requested,)).await?;
        if wants_date && !values.contains_key("payment_date") {
            let date = default_payment_date(tx, this.as_records(), &values).await?;
            values.insert("payment_date".to_owned(), Value::Date(date));
        }
        if !wants_documents {
            values.shift_remove("invoice_ids");
        }
        Ok(values)
    }

    /// Resolve a missing required date before the document relations are inserted.
    #[method(override)]
    async fn create(
        this: &RecordSet<AccountPaymentRegister>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        this.as_records()
            .check_access(tx, Operation::Create)
            .await?;
        for vals in &mut vals_list {
            if vals.contains("payment_date") {
                continue;
            }
            let env = if let Some(commands) = vals.commands("invoice_ids") {
                this.env().with_context([(
                    "default_invoice_ids",
                    Value::list(commands.iter().map(Command::to_value)),
                )])
            } else if let Some(value) = vals.get("invoice_ids") {
                this.env()
                    .with_context([("default_invoice_ids", Value::from_cell(value))])
            } else {
                this.env().clone()
            };
            let model = env.records("account.payment.register")?;
            let defaults: ValueMap = model
                .call(tx, "default_get", (vec!["payment_date".to_owned()],))
                .await?;
            let defaults = model.vals_from_values(&defaults)?;
            let date = defaults
                .get("payment_date")
                .ok_or_else(|| Error::validation("Choose a payment date."))?;
            vals.insert("payment_date".into(), date.clone());
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }

    /// Creates and posts the payments, matches each with its documents, and closes the dialog.
    ///
    /// # Errors
    ///
    /// `UserError` naming what cannot be paid (see the module documentation); the payment
    /// model's and the reconciliation's own refusals (rights, missing outstanding account).
    #[method]
    #[rpc(effect = write)]
    pub async fn action_create_payments(
        this: &RecordSet<AccountPaymentRegister>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let wizard = this.ensure_one()?;
        wizard
            .as_records()
            .check_access(tx, Operation::Write)
            .await?;
        let id = wizard.as_records().ids()[0];
        // a dialog registers once: a second call (a double click, a resend) finds it gone
        let held = tx
            .query(
                "SELECT id FROM account_payment_register WHERE id = $1 FOR UPDATE",
                &[&id],
            )
            .await?;
        if held.is_empty() {
            return Err(Error::missing(
                "This payment dialog was already used or has expired; open Register Payment again.",
            ));
        }
        // the relation rows as stored: the field's domain would drop a document paid or blocked
        // since the dialog opened, and the refusal must name it
        let linked = tx
            .query(
                "SELECT move_id::int8 FROM account_payment_register_move_rel \
                 WHERE wizard_id = $1 ORDER BY move_id",
                &[&id],
            )
            .await?;
        let linked: Vec<i64> = linked
            .iter()
            .map(|row| row.try_get::<_, i64>(0).map_err(db))
            .collect::<Result<_>>()?;
        let moves = wizard.env().records("account.move")?.browse(linked)?;
        let docs = documents(tx, &moves).await?;
        let group = wizard
            .get(tx, AccountPaymentRegister::GROUP_PAYMENT)
            .await?;
        let amount = wizard.get(tx, AccountPaymentRegister::AMOUNT).await?;
        let due = wizard.get(tx, AccountPaymentRegister::AMOUNT_DUE).await?;
        let memo_given = wizard
            .get(tx, AccountPaymentRegister::COMMUNICATION)
            .await?;
        let date = wizard
            .get(tx, AccountPaymentRegister::PAYMENT_DATE)
            .await?
            .ok_or_else(|| Error::validation("Set the payment date."))?;
        let journal = wizard
            .get(tx, AccountPaymentRegister::JOURNAL_ID)
            .await?
            .ok_or_else(|| Error::validation("Choose the bank or cash journal of the payment."))?;
        let line = wizard
            .get(tx, AccountPaymentRegister::PAYMENT_METHOD_LINE_ID)
            .await?;
        let diff_handling = wizard
            .get(tx, AccountPaymentRegister::PAYMENT_DIFFERENCE_HANDLING)
            .await?
            .unwrap_or_else(|| Arc::from("open"));
        let writeoff_account = wizard
            .get(tx, AccountPaymentRegister::WRITEOFF_ACCOUNT_ID)
            .await?
            .map(|id| id.get());
        let writeoff_label = wizard
            .get(tx, AccountPaymentRegister::WRITEOFF_LABEL)
            .await?
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| Arc::from("Write-Off"));
        let planned = plan(&docs, group, amount, due, memo_given.as_deref())?;
        let env = wizard.env().clone();
        let company = docs.first().map(|d| d.company).unwrap_or_default();
        let doc_currency = docs.first().map(|d| d.currency).unwrap_or_default();
        fits_company(tx, &env, journal.get(), company).await?;
        let line = match line {
            Some(line) => Some(chosen_line(tx, &env, line.get()).await?),
            None => None,
        };
        let payment_diff = due - amount;
        let apply_writeoff =
            planned.len() == 1 && diff_handling.as_ref() == "reconcile" && !payment_diff.is_zero();
        if apply_writeoff && writeoff_account.is_none() {
            return Err(Error::user(
                "Choose a 'Post Difference In' account to write off the payment difference.",
            ));
        }
        let payments_model = env.with_company(company)?.records("account.payment")?;
        let mut vals_list = Vec::with_capacity(planned.len());
        for p in &planned {
            let mut vals = RawVals::new()
                .set_cell("date", Cell::Date(date))
                .set_cell("company_id", Cell::Id(company))
                .set_cell("currency_id", Cell::Id(doc_currency))
                .set_cell("journal_id", Cell::Id(journal.get()))
                .set_cell("amount", Cell::Decimal(p.amount))
                .set_cell(
                    "payment_type",
                    Cell::Str(if p.inbound { "inbound" } else { "outbound" }.into()),
                )
                .set_cell(
                    "partner_type",
                    Cell::Str(if p.customer { "customer" } else { "supplier" }.into()),
                )
                .set_cell("destination_account_id", Cell::Id(p.account))
                .set_cell("memo", Cell::Str(p.memo.as_str().into()));
            if let Some(partner) = p.partner {
                vals = vals.set_cell("partner_id", Cell::Id(partner));
            }
            // every payment takes the chosen method; one of the other direction cannot
            if let Some((line, inbound, name)) = &line {
                if *inbound != p.inbound {
                    let (method, payment) = if *inbound {
                        ("receives", "sends")
                    } else {
                        ("sends", "receives")
                    };
                    return Err(Error::user(format!(
                        "The payment method {name} {method} money, but the payment of {} {payment} \
                         it; register these documents separately.",
                        p.memo
                    )));
                }
                vals = vals.set_cell("payment_method_line_id", Cell::Id(*line));
            }
            vals_list.push(vals);
        }
        let payments = payments_model.create(tx, vals_list).await?;
        super::post_payments(tx, &payments).await?;
        // When marking fully paid with a difference, post a balanced write-off entry and
        // include its receivable/payable counterpart line in reconciliation.
        let mut writeoff_counterpart_line: Option<i64> = None;
        if apply_writeoff && let (Some(wo_account), Some(p)) = (writeoff_account, planned.first()) {
            let wo_env = env.with_company(company)?;
            let (counterpart_cur, writeoff_cur) = compute_writeoff_balance(payment_diff, p.inbound);
            let gen_journal = find_misc_journal(tx, company).await?;
            let comp_currency = company_currency_id(tx, company).await?;
            let (counterpart_bal, writeoff_bal) = if doc_currency == comp_currency {
                (counterpart_cur, writeoff_cur)
            } else {
                let converted = convert_to_company_currency(
                    tx,
                    &wo_env,
                    doc_currency,
                    comp_currency,
                    company,
                    date,
                    counterpart_cur,
                )
                .await?;
                (converted, -converted)
            };
            let mut mvals = RawVals::new();
            mvals.insert("move_type".into(), Cell::Str("entry".into()));
            mvals.insert("journal_id".into(), Cell::Id(gen_journal));
            mvals.insert("company_id".into(), Cell::Id(company));
            mvals.insert("currency_id".into(), Cell::Id(doc_currency));
            mvals.insert("date".into(), Cell::Date(date));
            mvals.insert("ref".into(), Cell::Str(Arc::clone(&writeoff_label)));
            if let Some(partner) = p.partner {
                mvals.insert("partner_id".into(), Cell::Id(partner));
            }
            let unchecked_moves = crate::moves::unchecked(&wo_env.records("account.move")?);
            let wo_move = unchecked_moves.create(tx, vec![mvals]).await?;
            let wo_move_id = wo_move.ids()[0];
            let mut l1 = RawVals::new();
            l1.insert("move_id".into(), Cell::Id(wo_move_id));
            l1.insert("account_id".into(), Cell::Id(p.account));
            l1.insert("name".into(), Cell::Str(Arc::clone(&writeoff_label)));
            l1.insert("currency_id".into(), Cell::Id(doc_currency));
            l1.insert("balance".into(), Cell::Decimal(counterpart_bal));
            l1.insert("amount_currency".into(), Cell::Decimal(counterpart_cur));
            if let Some(partner) = p.partner {
                l1.insert("partner_id".into(), Cell::Id(partner));
            }
            let mut l2 = RawVals::new();
            l2.insert("move_id".into(), Cell::Id(wo_move_id));
            l2.insert("account_id".into(), Cell::Id(wo_account));
            l2.insert("name".into(), Cell::Str(Arc::clone(&writeoff_label)));
            l2.insert("currency_id".into(), Cell::Id(doc_currency));
            l2.insert("balance".into(), Cell::Decimal(writeoff_bal));
            l2.insert("amount_currency".into(), Cell::Decimal(writeoff_cur));
            if let Some(partner) = p.partner {
                l2.insert("partner_id".into(), Cell::Id(partner));
            }
            let unchecked_lines = crate::moves::unchecked(&wo_env.records("account.move.line")?);
            let created_lines = unchecked_lines.create(tx, vec![l1, l2]).await?;
            writeoff_counterpart_line = created_lines.ids().first().copied();
            crate::moves::post_moves(tx, &wo_env.records("account.move")?.browse([wo_move_id])?)
                .await?;
        }
        let lines = env.records("account.move.line")?;
        for (payment, p) in payments.ids().into_iter().zip(&planned) {
            let rows = tx
                .query(
                    "SELECT l.id::int8 FROM account_move_line l \
                     JOIN account_payment pay ON pay.move_id = l.move_id \
                     WHERE pay.id = $1 AND l.account_id = $2 \
                     UNION ALL \
                     SELECT l.id::int8 FROM account_move_line l \
                     LEFT JOIN account_open_item o ON o.line_id = l.id \
                     WHERE l.move_id = ANY($3::int8[]) AND l.account_id = $2 \
                     AND l.display_type = 'payment_term' AND l.full_reconcile_id IS NULL \
                     AND COALESCE(o.amount_residual, l.balance) <> 0 \
                     ORDER BY 1",
                    &[&payment, &p.account, &p.documents],
                )
                .await?;
            let mut ids: Vec<i64> = rows
                .iter()
                .map(|row| row.try_get::<_, i64>(0).map_err(db))
                .collect::<Result<_>>()?;
            if let Some(wo_lid) = writeoff_counterpart_line {
                ids.push(wo_lid);
                ids.sort_unstable();
                ids.dedup();
            }
            reconcile_lines(tx, &lines.browse(ids)?).await?;
        }
        let names = payments.cells(tx, "name").await?;
        let names: Vec<String> = names
            .iter()
            .map(|cell| match cell {
                Cell::Str(name) => name.to_string(),
                _ => String::new(),
            })
            .collect();
        wizard.as_records().unlink(tx).await?;
        let message = if names.len() == 1 {
            format!("Payment {} is registered.", names[0])
        } else {
            format!(
                "{} payments are registered: {}.",
                names.len(),
                names.join(", ")
            )
        };
        Ok(action(json!({
            "type": "ir.actions.client",
            "tag": "display_notification",
            "params": {
                "message": message,
                "type": "success",
                "next": {"type": "ir.actions.act_window_close"},
            },
        })))
    }
}

/// The display name of `id` of `model` as the caller reads it.
async fn shown(tx: &mut Transaction, env: &Env, model: &str, id: i64) -> Result<String> {
    Ok(
        match env
            .records(model)?
            .browse([id])?
            .cell(tx, "display_name")
            .await?
        {
            Cell::Str(name) => name.to_string(),
            _ => id.to_string(),
        },
    )
}

/// Refuses a journal that is neither of `company` nor of one of its parent companies.
async fn fits_company(tx: &mut Transaction, env: &Env, journal: i64, company: i64) -> Result<()> {
    let rows = tx
        .query(
            "SELECT COALESCE(j.company_id = c.id \
                 OR c.parent_path LIKE jc.parent_path || '%', false) \
             FROM account_journal j JOIN res_company jc ON jc.id = j.company_id \
             CROSS JOIN res_company c WHERE j.id = $1 AND c.id = $2",
            &[&journal, &company],
        )
        .await?;
    let fits = match rows.first() {
        Some(row) => row.try_get::<_, bool>(0).map_err(db)?,
        None => {
            return Err(Error::missing(
                "The journal of the payment no longer exists.",
            ));
        }
    };
    if !fits {
        return Err(Error::user(format!(
            "The journal {} belongs to another company than the documents; choose a journal of {}.",
            shown(tx, env, "account.journal", journal).await?,
            shown(tx, env, "res.company", company).await?
        )));
    }
    Ok(())
}

/// The chosen method line: its id, whether it receives money, and its name.
async fn chosen_line(tx: &mut Transaction, env: &Env, line: i64) -> Result<(i64, bool, String)> {
    let records = env.records("account.payment.method.line")?.browse([line])?;
    let inbound = matches!(
        records.cell(tx, "payment_type").await?,
        Cell::Str(kind) if kind.as_ref() == "inbound"
    );
    Ok((
        line,
        inbound,
        // the message is about one journal's dialog: the plain name, as the dropdown shows it
        shown(
            tx,
            &env.with_context([("hide_payment_journal_id", Value::Int(1))]),
            "account.payment.method.line",
            line,
        )
        .await?,
    ))
}

#[methods(AccountMove, addon = "account")]
impl AccountMove {
    /// The Register Payment button of a confirmed document: opens the dialog on it.
    ///
    /// # Errors
    ///
    /// `UserError` for a document that is not confirmed or not an invoice, bill or credit note.
    #[method]
    #[rpc(effect = read)]
    pub async fn action_register_payment(
        this: &RecordSet<AccountMove>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let docs = documents(tx, this.as_records()).await?;
        if let Some(doc) = docs.iter().find(|d| d.state != "posted") {
            return Err(Error::user(format!(
                "Only confirmed documents can be paid; {} is not confirmed.",
                doc.name_or_draft()
            )));
        }
        if let Some(doc) = docs
            .iter()
            .find(|d| !INVOICE_TYPES.contains(&d.kind.as_str()))
        {
            return Err(Error::user(format!(
                "{} is not an invoice, bill or credit note; journal entries are matched with \
                 payments by reconciliation instead.",
                doc.name
            )));
        }
        let ids: Vec<i64> = docs.iter().map(|d| d.id).collect();
        Ok(action(json!({
            "type": "ir.actions.act_window",
            "name": "Register Payment",
            "res_model": "account.payment.register",
            "target": "new",
            "views": [[false, "form"]],
            "context": {"default_invoice_ids": [[6, 0, ids]]},
        })))
    }
}

/// Pure exact-decimal write-off split: returns `(counterpart_balance, writeoff_balance)`
/// such that `counterpart_balance + writeoff_balance == 0`.
/// For an inbound customer underpayment (`difference = due - paid > 0`), receivable is credited
/// (`-difference`) and the write-off expense account is debited (`+difference`).
/// For an outbound vendor underpayment (`difference > 0`), payable is debited (`+difference`)
/// and the write-off income account is credited (`-difference`).
pub(crate) fn compute_writeoff_balance(difference: Decimal, inbound: bool) -> (Decimal, Decimal) {
    let counterpart = if inbound { -difference } else { difference };
    (counterpart, -counterpart)
}

async fn find_misc_journal(tx: &mut Transaction, company: i64) -> Result<i64> {
    let rows = tx
        .query(
            "SELECT id::int8 FROM account_journal \
             WHERE company_id = $1 AND type = 'general' AND active = true \
             ORDER BY sequence, id LIMIT 1",
            &[&company],
        )
        .await?;
    rows.first()
        .ok_or_else(|| {
            Error::user(
                "No active Miscellaneous (general) journal is configured for this company to post the payment write-off entry.",
            )
        })?
        .try_get(0)
        .map_err(db)
}

async fn company_currency_id(tx: &mut Transaction, company: i64) -> Result<i64> {
    let rows = tx
        .query(
            "SELECT currency_id::int8 FROM res_company WHERE id = $1",
            &[&company],
        )
        .await?;
    rows.first()
        .ok_or_else(|| Error::missing("Company currency not found."))?
        .try_get(0)
        .map_err(db)
}

async fn convert_to_company_currency(
    tx: &mut Transaction,
    env: &Env,
    from_currency: i64,
    to_currency: i64,
    company: i64,
    date: Date,
    amount: Decimal,
) -> Result<Decimal> {
    let from_rec = RecId::from_real(from_currency)
        .ok_or_else(|| Error::invariant("Invalid source currency id."))?;
    let to_rec = RecId::from_real(to_currency)
        .ok_or_else(|| Error::invariant("Invalid target currency id."))?;
    let co_rec =
        RecId::from_real(company).ok_or_else(|| Error::invariant("Invalid company id."))?;
    let currencies = [from_rec, to_rec];
    let rounding = erp_addon_base::money::RoundingTable::load(env, tx, &currencies).await?;
    let book = erp_addon_base::money::RateBook::load(
        env,
        tx,
        co_rec,
        date,
        erp_sdk::money::RateDateRule::OnOrBefore,
        &currencies,
    )
    .await?;
    let exact = book.snapshot(from_rec, to_rec)?.convert_exact(amount)?;
    Ok(exact.quantize(
        rounding.get(to_rec)?.increment(),
        erp_addon_base::money::CURRENCY_ROUNDING_MODE,
    )?)
}

#[cfg(test)]
#[path = "tests/register.rs"]
mod tests;

#[cfg(test)]
mod writeoff_tests {
    use super::compute_writeoff_balance;
    use erp_sdk::Decimal;
    use std::str::FromStr;

    #[test]
    fn writeoff_conserves_exact_zero_sum_inbound_and_outbound() {
        let diff = Decimal::from_str("12.35").expect("valid decimal");
        let (ar, wo_exp) = compute_writeoff_balance(diff, true);
        assert_eq!(ar, -diff);
        assert_eq!(wo_exp, diff);
        assert_eq!(ar + wo_exp, Decimal::ZERO);

        let (ap, wo_inc) = compute_writeoff_balance(diff, false);
        assert_eq!(ap, diff);
        assert_eq!(wo_inc, -diff);
        assert_eq!(ap + wo_inc, Decimal::ZERO);
    }
}
