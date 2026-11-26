// SPDX-License-Identifier: MIT OR Apache-2.0
//! Payments (observed in the reference `account_payment.py`,
//! `account_payment_method.py`, our own implementation).
//!
//! A payment ([`AccountPayment`]) is money received from a customer or paid to a vendor through a
//! bank, cash or credit-card journal. A draft has no journal entry (parity treatment
//! A4 W10); posting builds its entry (a liquidity item on the outstanding account, a counterpart
//! item on the partner's receivable or payable account) and posts it through the posting service,
//! numbered in the journal's payment chain. Its status and the flags `is_reconciled` and
//! `is_matched` are written by one transition function in the unit that matches or unmatches its
//! items ([`service::refresh_payments`]). A method ([`AccountPaymentMethod`]) is
//! a way to pay (manual, and later checks or online providers); a method line
//! ([`AccountPaymentMethodLine`]) is that method on one journal, with the outstanding account it
//! books to.

pub mod batch;
mod register;
mod service;

pub use service::{cancel_payments, draft_payments, post_payments, refresh_payments};

use erp_sdk::prelude::*;

/// The fields only the payment service writes: a caller value, a context default and a copy are
/// refused or dropped.
pub const SERVICE_OWNED: [&str; 7] = [
    "state",
    "name",
    "move_id",
    "is_reconciled",
    "is_matched",
    "outstanding_account_id",
    "is_sent",
];

/// What feeds the journal entry of a payment: refused on a payment that is no longer a draft
/// (paid, reconciled, cancelled or rejected); reset a cancelled or rejected one to draft first.
pub const ENTRY_FIELDS: [&str; 12] = [
    "date",
    "amount",
    "payment_type",
    "partner_type",
    "currency_id",
    "partner_id",
    "destination_account_id",
    "journal_id",
    "company_id",
    "payment_method_line_id",
    "payment_reference",
    "check_number",
];

/// `account.payment.method`: a way to pay or to be paid, in one direction.
#[model(
    name = "account.payment.method",
    description = "Payment Method",
    order = "id",
    keys = [key(
        name = "_code_payment_type_uniq",
        fields = ["code", "payment_type"],
        message = "A payment method with this code and direction already exists."
    )],
)]
pub struct AccountPaymentMethod {
    /// Name shown on the method lines.
    #[field(char, required = true, translate = true, string = "Name")]
    pub name: Option<Arc<str>>,
    /// Technical code, unique per direction.
    #[field(char, required = true, string = "Code")]
    pub code: Option<Arc<str>>,
    /// Incoming or outgoing.
    #[field(
        selection,
        selection = [("inbound", "Inbound"), ("outbound", "Outbound")],
        required = true,
        string = "Type"
    )]
    pub payment_type: Option<Arc<str>>,
}

/// `account.payment.method.line`: a payment method on one journal, with the outstanding account
/// its payments book to.
///
/// Default records: every new bank, cash and credit-card journal gets a manual inbound and a
/// manual outbound line (the journal's create, `journal/mod.rs`); journals that existed before
/// get them at the 1.4 upgrade (`add_default_method_lines` below).
#[model(
    name = "account.payment.method.line",
    description = "Payment Method Line",
    order = "sequence, id",
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "ancestor", empty = "deny"),
)]
pub struct AccountPaymentMethodLine {
    /// Name; the method's when none is given.
    #[field(char, string = "Name")]
    pub name: Option<Arc<str>>,
    /// Order among the journal's lines; the first fitting line is a new payment's default.
    #[field(integer, default = 10)]
    pub sequence: i32,
    /// The method.
    #[field(
        many2one,
        comodel = "account.payment.method",
        required = true,
        ondelete = "cascade",
        string = "Payment Method"
    )]
    pub payment_method_id: Option<RecId>,
    /// The journal.
    #[field(
        many2one,
        comodel = "account.journal",
        required = true,
        ondelete = "cascade",
        check_company = true,
        string = "Journal"
    )]
    pub journal_id: Option<RecId>,
    /// The journal's company.
    #[field(
        many2one,
        comodel = "res.company",
        related = "journal_id.company_id",
        store = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// The outstanding receipts (inbound) or payments (outbound) account of the line's payments;
    /// when empty, the company's transfer account is used.
    #[field(
        many2one,
        comodel = "account.account",
        check_company = true,
        ondelete = "restrict",
        index = "btree_not_null",
        string = "Outstanding Receipts/Payments Account"
    )]
    pub payment_account_id: Option<RecId>,
    /// The method's direction.
    #[field(
        selection,
        selection = [("inbound", "Inbound"), ("outbound", "Outbound")],
        related = "payment_method_id.payment_type",
        store = true,
        string = "Payment Type"
    )]
    pub payment_type: Option<Arc<str>>,
    /// The method's code.
    #[field(char, related = "payment_method_id.code", string = "Code")]
    pub code: Option<Arc<str>>,
    /// "<name> (<journal name>)", the plain name under the context key `hide_payment_journal_id`.
    #[field(char, compute = "_compute_display_name")]
    pub display_name: Option<Arc<str>>,
}

/// The demo outstanding accounts on the manual lines of the demo bank and cash journals where none
/// is set; changes nothing on a database without the demo records. Run by the demo data and again
/// by the 1.4 upgrade step, which creates those lines after the demo data has loaded.
const DEMO_OUTSTANDING: &str = "UPDATE account_payment_method_line l SET payment_account_id = acc.res_id \
     FROM account_journal j, ir_model_data mc, ir_model_data acc \
     WHERE mc.module = 'base' AND mc.model = 'res.company' AND mc.name = 'main_company' \
     AND j.id = l.journal_id AND j.company_id = mc.res_id AND j.type IN ('bank', 'cash') \
     AND acc.module = 'account' AND acc.model = 'account.account' \
     AND acc.name = CASE l.payment_type WHEN 'inbound' \
         THEN 'account_demo_outstanding_receipts' ELSE 'account_demo_outstanding_payments' END \
     AND l.payment_account_id IS NULL";

const DEMO_TRANSFER_ACCOUNT: &str = "UPDATE res_company c SET transfer_account_id = acc.res_id \
     FROM ir_model_data mc, ir_model_data acc \
     WHERE mc.module = 'base' AND mc.model = 'res.company' AND mc.name = 'main_company' \
     AND c.id = mc.res_id \
     AND acc.module = 'account' AND acc.model = 'account.account' \
     AND acc.name = 'account_demo_outstanding_payments' \
     AND c.transfer_account_id IS NULL";

#[methods(AccountPaymentMethodLine, addon = "account")]
impl AccountPaymentMethodLine {
    /// The journal's name is part of the line's name and readable by every internal user, so it is
    /// read with elevated rights; a view that already shows the journal asks for the plain name.
    #[compute(depends = ["name", "journal_id"], depends_context = ["hide_payment_journal_id"])]
    async fn _compute_display_name(
        this: &RecordSet<AccountPaymentMethodLine>,
        tx: &mut Transaction,
        out: &mut Assign<'_, AccountPaymentMethodLine>,
    ) -> Result<()> {
        let hide = matches!(
            this.env().context().get("hide_payment_journal_id"),
            Some(Value::Bool(true)) | Some(Value::Int(1..))
        );
        let names = this.mapped(tx, AccountPaymentMethodLine::NAME).await?;
        let journals = this
            .mapped(tx, AccountPaymentMethodLine::JOURNAL_ID)
            .await?;
        // One elevated read of every distinct journal name, none when the plain name is asked for.
        // A journal not saved yet (negative id) has no stored name: its lines keep the plain name.
        let mut ids: Vec<i64> = journals
            .iter()
            .flatten()
            .map(|j| j.get())
            .filter(|id| *id > 0)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        let mut journal_names = std::collections::HashMap::new();
        if !hide && !ids.is_empty() {
            let records = this
                .env()
                .sudo(true)
                .records("account.journal")?
                .browse(ids)?;
            for (id, cell) in records
                .ids()
                .into_iter()
                .zip(records.cells(tx, "name").await?)
            {
                let Cell::Str(journal_name) = cell else {
                    return Err(Error::invariant(format!(
                        "The name of journal {id} is not text, so the payment method line label \
                         cannot be built."
                    )));
                };
                journal_names.insert(id, journal_name);
            }
        }
        for (i, (name, journal)) in names.into_iter().zip(journals).enumerate() {
            let name = name.as_deref().unwrap_or_default().to_owned();
            let label = match journal.and_then(|j| journal_names.get(&j.get())) {
                Some(journal_name) => format!("{name} ({journal_name})"),
                None => name,
            };
            out.set(
                i,
                AccountPaymentMethodLine::DISPLAY_NAME,
                Some(Arc::<str>::from(label)),
            )?;
        }
        Ok(())
    }

    /// Demo data entry point: the manual lines of the demo bank and cash journals book to the demo
    /// outstanding accounts (inbound to receipts, outbound to payments) where none is set.
    #[method]
    #[data_callable(model)]
    async fn _demo_outstanding_accounts(
        this: &RecordSet<AccountPaymentMethodLine>,
        tx: &mut Transaction,
    ) -> Result<()> {
        if !this.env().su() {
            return Err(Error::access("Demo data is loaded by the installer only."));
        }
        tx.execute(DEMO_OUTSTANDING, &[]).await?;
        tx.execute(DEMO_TRANSFER_ACCOUNT, &[]).await?;
        tx.invalidate_model("account.payment.method.line", Some(&["payment_account_id"]))
            .await?;
        tx.invalidate_model("res.company", Some(&["transfer_account_id"]))
            .await
    }

    /// A line without a name takes its method's.
    #[method(override)]
    async fn create(
        this: &RecordSet<AccountPaymentMethodLine>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        for vals in &mut vals_list {
            if matches!(vals.get("name"), Some(Cell::Str(name)) if !name.is_empty()) {
                continue;
            }
            let Some(Cell::Id(method)) = vals.get("payment_method_id").cloned() else {
                continue; // the required check names the missing method
            };
            let name = this
                .env()
                .records("account.payment.method")?
                .browse([method])?
                .cell(tx, "name")
                .await?;
            vals.insert("name".into(), name);
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }
}

/// `account.payment`: money received or paid.
///
/// No default records: the reference ships none (payments come from documents and people).
#[model(
    name = "account.payment",
    description = "Payment",
    order = "date desc, name desc, id desc",
    check_company_auto = true,
    company_access = direct(field = "company_id", branch = "exact", empty = "deny"),
    constraints = [(
        "_check_amount_not_negative",
        "CHECK (amount >= 0)",
        "A payment amount is never below zero."
    )],
)]
pub struct AccountPayment {
    /// Number: the journal entry's, given at posting.
    #[field(char, readonly = true, copy = false, string = "Number")]
    pub name: Option<Arc<str>>,
    /// The number, or "Draft Payment" before posting.
    #[field(char, compute = "_compute_display_name")]
    pub display_name: Option<Arc<str>>,
    /// Payment date; today when none is given.
    #[field(date, required = true, string = "Date")]
    pub date: Option<Date>,
    /// The journal entry, created at posting.
    #[field(
        many2one,
        comodel = "account.move",
        readonly = true,
        copy = false,
        ondelete = "restrict",
        index = "btree_not_null",
        string = "Journal Entry"
    )]
    pub move_id: Option<RecId>,
    /// A bank, cash or credit-card journal.
    #[field(
        many2one,
        comodel = "account.journal",
        required = true,
        check_company = true,
        ondelete = "restrict",
        domain = [("type", "in", ["bank", "cash", "credit"])],
        string = "Journal"
    )]
    pub journal_id: Option<RecId>,
    /// The company; the journal's when none is given.
    #[field(many2one, comodel = "res.company", required = true, string = "Company")]
    pub company_id: Option<RecId>,
    /// Draft, then Paid at posting, Reconciled once its items are matched; Cancelled and Rejected
    /// end it. Changes only through the payment's actions and its matches.
    #[field(
        selection,
        selection = [
            ("draft", "Draft"),
            ("paid", "Paid"),
            ("reconciled", "Reconciled"),
            ("canceled", "Cancelled"),
            ("rejected", "Rejected"),
        ],
        required = true,
        readonly = true,
        copy = false,
        default = "draft",
        string = "Status"
    )]
    pub state: Option<Arc<str>>,
    /// The counterpart (receivable or payable) items are fully matched.
    #[field(boolean, readonly = true, copy = false, string = "Is Reconciled")]
    pub is_reconciled: bool,
    /// The liquidity item is matched (with a bank statement), or books on the journal's own
    /// account.
    #[field(
        boolean,
        readonly = true,
        copy = false,
        string = "Matched With Statement"
    )]
    pub is_matched: bool,
    /// Sent to the partner.
    #[field(boolean, readonly = true, copy = false, string = "Is Sent")]
    pub is_sent: bool,
    /// The method line; the journal's first line of the direction when none is given.
    #[field(
        many2one,
        comodel = "account.payment.method.line",
        check_company = true,
        ondelete = "restrict",
        copy = true,
        string = "Payment Method"
    )]
    pub payment_method_line_id: Option<RecId>,
    /// The method of the method line.
    #[field(
        many2one,
        comodel = "account.payment.method",
        related = "payment_method_line_id.payment_method_id",
        string = "Method"
    )]
    pub payment_method_id: Option<RecId>,
    /// Amount, never negative.
    #[field(monetary, currency_field = "currency_id", string = "Amount")]
    pub amount: Decimal,
    /// Received (inbound) or sent (outbound).
    #[field(
        selection,
        selection = [("outbound", "Send"), ("inbound", "Receive")],
        required = true,
        default = "inbound",
        string = "Payment Type"
    )]
    pub payment_type: Option<Arc<str>>,
    /// Customer or vendor.
    #[field(
        selection,
        selection = [("customer", "Customer"), ("supplier", "Vendor")],
        required = true,
        default = "customer",
        string = "Partner Type"
    )]
    pub partner_type: Option<Arc<str>>,
    /// Memo; the journal entry's reference.
    #[field(char, string = "Memo")]
    pub memo: Option<Arc<str>>,
    /// The partner's reference of the payment (a transfer or check number).
    #[field(char, copy = false, string = "Payment Reference")]
    pub payment_reference: Option<Arc<str>>,
    /// Currency: the journal's, else the company's.
    #[field(
        many2one,
        comodel = "res.currency",
        required = true,
        string = "Currency"
    )]
    pub currency_id: Option<RecId>,
    /// The company currency.
    #[field(
        many2one,
        comodel = "res.currency",
        related = "company_id.currency_id",
        string = "Company Currency"
    )]
    pub company_currency_id: Option<RecId>,
    /// The customer or vendor.
    #[field(
        many2one,
        comodel = "res.partner",
        check_company = true,
        ondelete = "restrict",
        string = "Customer/Vendor"
    )]
    pub partner_id: Option<RecId>,
    /// The account the liquidity item books to, set at posting.
    #[field(
        many2one,
        comodel = "account.account",
        readonly = true,
        copy = false,
        ondelete = "restrict",
        index = "btree_not_null",
        string = "Outstanding Account"
    )]
    pub outstanding_account_id: Option<RecId>,
    /// The receivable (customer) or payable (vendor) account of the counterpart item; the
    /// company's when none is given.
    #[field(
        many2one,
        comodel = "account.account",
        check_company = true,
        ondelete = "restrict",
        index = "btree_not_null",
        domain = [("account_type", "in", ["asset_receivable", "liability_payable"])],
        string = "Destination Account"
    )]
    pub destination_account_id: Option<RecId>,
    /// The amount, negative when sent.
    #[field(
        monetary,
        currency_field = "currency_id",
        compute = "_compute_amount_signed",
        string = "Amount Signed"
    )]
    pub amount_signed: Decimal,
    /// Readable invoices and vendor bills reconciled with this payment.
    #[field(
        integer,
        compute = "_compute_reconciled_invoice_count",
        string = "Reconciled Invoices"
    )]
    pub reconciled_invoice_count: i32,
    /// 1 when this payment has a readable journal entry, else 0.
    #[field(
        integer,
        compute = "_compute_reconciled_invoice_count",
        string = "Journal Entry"
    )]
    pub journal_entry_count: i32,
    /// Optional batch payment grouping this payment for bank deposit or remittance.
    #[field(
        many2one,
        comodel = "account.batch.payment",
        copy = false,
        ondelete = "set null",
        index = "btree_not_null",
        check_company = true,
        string = "Batch Payment"
    )]
    pub batch_payment_id: Option<RecId>,
    /// Check number when paying or receiving by check.
    #[field(char, copy = false, string = "Check Number")]
    pub check_number: Option<Arc<str>>,
    /// Exact amount spelled out in words for check printing and verification.
    #[field(
        char,
        compute = "_compute_check_amount_in_words",
        string = "Amount in Words"
    )]
    pub check_amount_in_words: Option<Arc<str>>,
}

#[methods(AccountPayment, addon = "account")]
impl AccountPayment {
    /// Pre-fills date, company, currency, bank/cash journal and its default method line on new
    /// payment forms so users do not have to re-pick them manually every time.
    #[method(override)]
    async fn default_get(
        this: &RecordSet<AccountPayment>,
        tx: &mut Transaction,
        names: Vec<String>,
        sup: Super,
    ) -> Result<erp_sdk::ValueMap> {
        let mut values: erp_sdk::ValueMap =
            sup.call(tx, this.as_records(), (names.clone(),)).await?;
        let env = this.env();
        let company = env.company_id()?;
        if let Some(cid) = company {
            if names.iter().any(|n| n == "company_id") && !values.contains_key("company_id") {
                values.insert("company_id".to_owned(), Value::Int(cid));
            }
            if names.iter().any(|n| n == "date") && !values.contains_key("date") {
                let today = erp_addon_base::company_calendar::business_date(tx, cid).await?;
                values.insert("date".to_owned(), Value::Date(today));
            }
            let mut journal_id = match values.get("journal_id") {
                Some(Value::Int(j)) => Some(*j),
                _ => None,
            };
            if journal_id.is_none()
                && names
                    .iter()
                    .any(|n| n == "journal_id" || n == "payment_method_line_id")
            {
                let jrows = tx
                    .query(
                        "SELECT id::int8 FROM account_journal \
                         WHERE company_id = $1 AND type IN ('bank', 'cash') AND active = true \
                         ORDER BY CASE type WHEN 'bank' THEN 0 ELSE 1 END, sequence, id LIMIT 1",
                        &[&cid],
                    )
                    .await?;
                if let Some(jid) = jrows.first().and_then(|r| r.try_get::<_, i64>(0).ok()) {
                    journal_id = Some(jid);
                    if names.iter().any(|n| n == "journal_id") && !values.contains_key("journal_id")
                    {
                        values.insert("journal_id".to_owned(), Value::Int(jid));
                    }
                }
            }
            if names.iter().any(|n| n == "currency_id") && !values.contains_key("currency_id") {
                let crows = tx
                    .query(
                        "SELECT currency_id::int8 FROM res_company WHERE id = $1",
                        &[&cid],
                    )
                    .await?;
                if let Some(cur) = crows.first().and_then(|r| r.try_get::<_, i64>(0).ok()) {
                    values.insert("currency_id".to_owned(), Value::Int(cur));
                }
            }
            if names.iter().any(|n| n == "payment_method_line_id")
                && !values.contains_key("payment_method_line_id")
                && let Some(jid) = journal_id
            {
                let direction: String = match values.get("payment_type") {
                    Some(Value::Str(s)) => s.to_string(),
                    _ => match env.context().get("default_payment_type") {
                        Some(Value::Str(s)) => s.to_string(),
                        _ => "inbound".to_owned(),
                    },
                };
                let mrows = tx
                    .query(
                        "SELECT id::int8 FROM account_payment_method_line \
                         WHERE journal_id = $1 AND payment_type = $2 \
                         ORDER BY sequence, id LIMIT 1",
                        &[&jid, &direction],
                    )
                    .await?;
                if let Some(mid) = mrows.first().and_then(|r| r.try_get::<_, i64>(0).ok()) {
                    values.insert("payment_method_line_id".to_owned(), Value::Int(mid));
                }
            }
        }
        Ok(values)
    }

    #[compute(depends = ["name", "state"])]
    async fn _compute_display_name(
        this: &RecordSet<AccountPayment>,
        tx: &mut Transaction,
        out: &mut Assign<'_, AccountPayment>,
    ) -> Result<()> {
        let names = this.mapped(tx, AccountPayment::NAME).await?;
        for (i, name) in names.into_iter().enumerate() {
            let shown = name
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| Arc::from("Draft Payment"));
            out.set(i, AccountPayment::DISPLAY_NAME, Some(shown))?;
        }
        Ok(())
    }

    #[compute(depends = ["amount", "currency_id"])]
    async fn _compute_check_amount_in_words(
        this: &RecordSet<AccountPayment>,
        tx: &mut Transaction,
        out: &mut Assign<'_, AccountPayment>,
    ) -> Result<()> {
        let amounts = this.mapped(tx, AccountPayment::AMOUNT).await?;
        let currencies = this.mapped(tx, AccountPayment::CURRENCY_ID).await?;
        let mut cur_ids: Vec<i64> = currencies
            .iter()
            .flatten()
            .filter_map(|c| c.as_real())
            .map(|c| c.get())
            .collect();
        cur_ids.sort_unstable();
        cur_ids.dedup();
        let mut info = std::collections::HashMap::<i64, (String, u32)>::new();
        if !cur_ids.is_empty() {
            let rows = tx
                .query(
                    "SELECT id::int8, name::text, COALESCE(decimal_places, 2)::int4 \
                     FROM res_currency WHERE id = ANY($1::int8[])",
                    &[&cur_ids],
                )
                .await?;
            for row in rows {
                let id: i64 = row
                    .try_get(0)
                    .map_err(|e| Error::db(format!("Currency id unreadable: {e}")))?;
                let name: String = row
                    .try_get(1)
                    .map_err(|e| Error::db(format!("Currency code unreadable: {e}")))?;
                let dp: i32 = row
                    .try_get(2)
                    .map_err(|e| Error::db(format!("Currency precision unreadable: {e}")))?;
                let scale = u32::try_from(dp).unwrap_or(2);
                info.insert(id, (name, scale));
            }
        }
        for (i, (amount, cur)) in amounts.into_iter().zip(currencies).enumerate() {
            let (code, scale) = cur
                .and_then(|c| c.as_real())
                .and_then(|c| info.get(&c.get()))
                .map(|(n, s)| (n.as_str(), *s))
                .unwrap_or(("", 2));
            out.set(
                i,
                AccountPayment::CHECK_AMOUNT_IN_WORDS,
                Some(Arc::<str>::from(batch::amount_to_words_scaled(
                    amount, code, scale,
                ))),
            )?;
        }
        Ok(())
    }

    #[compute(depends = ["amount", "payment_type"])]
    async fn _compute_amount_signed(
        this: &RecordSet<AccountPayment>,
        tx: &mut Transaction,
        out: &mut Assign<'_, AccountPayment>,
    ) -> Result<()> {
        let amounts = this.mapped(tx, AccountPayment::AMOUNT).await?;
        let types = this.mapped(tx, AccountPayment::PAYMENT_TYPE).await?;
        for (i, (amount, kind)) in amounts.into_iter().zip(types).enumerate() {
            let signed = if kind.as_deref() == Some("outbound") {
                -amount
            } else {
                amount
            };
            out.set(i, AccountPayment::AMOUNT_SIGNED, signed)?;
        }
        Ok(())
    }

    /// Fills the date, company, currency, method line and destination account of a draft; the
    /// service-owned fields are refused, given or as context defaults.
    #[method(override)]
    async fn create(
        this: &RecordSet<AccountPayment>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let env = this.env().clone();
        refuse_defaults(&env)?;
        for vals in &mut vals_list {
            refuse_owned(vals, true)?;
            service::fill_defaults(tx, &env, vals).await?;
        }
        sup.call(tx, this.as_records(), (vals_list,)).await
    }

    /// The service-owned fields are refused; on a payment that left draft, what feeds its entry
    /// is refused too, and a new memo becomes the entry's reference.
    #[method(override)]
    async fn write(
        this: &RecordSet<AccountPayment>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        refuse_owned(&vals, false)?;
        let entry: Vec<&str> = ENTRY_FIELDS
            .iter()
            .copied()
            .filter(|f| vals.contains(f))
            .collect();
        let memo = vals.contains("memo");
        let batch = vals.contains("batch_payment_id");
        if !entry.is_empty() || memo || batch {
            this.as_records()
                .check_access(tx, erp_sdk::Operation::Write)
                .await?;
        }
        if !entry.is_empty() {
            service::refuse_posted(tx, &this.ids(), &entry.join(", ")).await?;
            service::refuse_ended(tx, &this.ids(), &entry.join(", ")).await?;
        }
        if batch {
            service::refuse_batch_change(tx, &this.ids(), vals.get("batch_payment_id")).await?;
        }
        sup.call(tx, this.as_records(), (vals,)).await?;
        if memo {
            service::memo_to_entries(tx, this.env(), &this.ids()).await?;
        }
        Ok(())
    }

    /// Only a payment without journal entry (draft, cancelled or rejected) can be deleted.
    #[method(override)]
    async fn unlink(
        this: &RecordSet<AccountPayment>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        this.as_records()
            .check_access(tx, erp_sdk::Operation::Unlink)
            .await?;
        service::refuse_posted(tx, &this.ids(), "deleting it").await?;
        sup.call(tx, this.as_records(), ()).await
    }

    /// Posts the draft payments: their journal entries are created and posted; the payments are
    /// Paid and take the entries' numbers.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_post(this: &RecordSet<AccountPayment>, tx: &mut Transaction) -> Result<()> {
        post_payments(tx, this.as_records()).await
    }

    /// Cancels draft payments; a posted one is refused (its entry would need a reversal).
    #[method]
    #[rpc(effect = write)]
    pub async fn action_cancel(
        this: &RecordSet<AccountPayment>,
        tx: &mut Transaction,
    ) -> Result<()> {
        cancel_payments(tx, this.as_records()).await
    }

    /// Returns cancelled or rejected payments to draft; a posted one is refused.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_draft(
        this: &RecordSet<AccountPayment>,
        tx: &mut Transaction,
    ) -> Result<()> {
        draft_payments(tx, this.as_records()).await
    }

    /// Marks paid payments as reconciled by hand.
    #[method]
    #[rpc(effect = write)]
    pub async fn action_validate(
        this: &RecordSet<AccountPayment>,
        tx: &mut Transaction,
    ) -> Result<()> {
        service::validate_payments(tx, this.as_records()).await
    }
}

/// Refuses a service-owned field in `vals`; only a new payment (`creating`) may say it is a draft:
/// on an existing payment a draft state would let Confirm book it a second time.
fn refuse_owned(vals: &RawVals, creating: bool) -> Result<()> {
    let draft = |field: &&str| {
        creating
            && *field == "state"
            && matches!(vals.get("state"), Some(Cell::Str(s)) if s.as_ref() == "draft")
    };
    match SERVICE_OWNED.iter().find(|f| vals.contains(f) && !draft(f)) {
        Some(field) => Err(Error::user(format!(
            "The field {field} of a payment is set by its actions (Confirm, Cancel, Reset to \
             Draft) and its matches; it cannot be written."
        ))),
        None => Ok(()),
    }
}

fn refuse_defaults(env: &Env) -> Result<()> {
    match SERVICE_OWNED
        .iter()
        .find(|f| env.context().contains(&format!("default_{f}")))
    {
        Some(field) => Err(Error::user(format!(
            "The context key default_{field} is not accepted when a payment is created; the field \
             is set by the payment's actions."
        ))),
        None => Ok(()),
    }
}

/// The default method lines of new bank, cash and credit-card journals: Manual Payment inbound and
/// outbound, as the reference gives every such journal. Journals of other types, journals that
/// already have lines, and a database whose manual methods were removed get none.
pub(crate) async fn default_lines(tx: &mut Transaction, env: &Env, journals: &[i64]) -> Result<()> {
    let rows = tx
        .query(
            "SELECT j.id::int8, d.res_id::int8 FROM account_journal j \
             CROSS JOIN ir_model_data d \
             WHERE j.id = ANY($1::int8[]) AND j.type IN ('bank', 'cash', 'credit') \
             AND d.module = 'account' AND d.model = 'account.payment.method' \
             AND d.name IN ('account_payment_method_manual_in', 'account_payment_method_manual_out') \
             AND NOT EXISTS (SELECT 1 FROM account_payment_method_line l WHERE l.journal_id = j.id) \
             ORDER BY j.id, d.name",
            &[&journals],
        )
        .await?;
    let mut vals = Vec::with_capacity(rows.len());
    for row in &rows {
        let journal: i64 = row
            .try_get(0)
            .map_err(|e| Error::db(format!("A journal could not be read: {e}")))?;
        let method: i64 = row
            .try_get(1)
            .map_err(|e| Error::db(format!("A payment method could not be read: {e}")))?;
        vals.push(
            RawVals::new()
                .set_cell("journal_id", Cell::Id(journal))
                .set_cell("payment_method_id", Cell::Id(method)),
        );
    }
    if !vals.is_empty() {
        env.records("account.payment.method.line")?
            .create(tx, vals)
            .await?;
    }
    Ok(())
}

/// Bank, cash and credit-card journals that existed before payments get their manual method lines
/// (the same lines as [`default_lines`], written directly: the upgrade runs before any user), and
/// the demo journals their demo outstanding accounts, as on a fresh install. A line is named after
/// its method in English, else in any language it has, else by its code.
///
/// # Errors
///
/// When the two manual methods were not loaded, naming them.
// 1.4, not 1.3: 1.3 was released without payments (the entry totals), so a database at 1.3 must still run this.
#[erp_sdk::migration(addon = "account", version = "1.4", stage = post)]
pub async fn add_default_method_lines(
    tx: &mut Transaction,
    _from: &erp_sdk::migrations::SemverVersion,
) -> Result<()> {
    let manual = tx
        .query(
            "SELECT count(*)::int8 FROM ir_model_data d \
             JOIN account_payment_method pm ON pm.id = d.res_id \
             WHERE d.module = 'account' AND d.model = 'account.payment.method' \
             AND d.name IN ('account_payment_method_manual_in', 'account_payment_method_manual_out')",
            &[],
        )
        .await?;
    let found: i64 = manual
        .first()
        .map(|row| row.try_get(0))
        .transpose()
        .map_err(|e| {
            Error::db(format!(
                "The manual payment methods could not be counted: {e}"
            ))
        })?
        .unwrap_or_default();
    if found != 2 {
        return Err(Error::invariant(format!(
            "the account upgrade found {found} of the two manual payment methods \
             (account.account_payment_method_manual_in, account.account_payment_method_manual_out); \
             their data file did not load, so the journals would get no payment methods"
        )));
    }
    tx.execute(
        "INSERT INTO account_payment_method_line \
             (name, sequence, payment_method_id, journal_id, company_id, payment_type, \
              create_date, write_date) \
         SELECT COALESCE(pm.name ->> 'en_US', \
                         (SELECT e.value FROM jsonb_each_text(pm.name) e ORDER BY e.key LIMIT 1), \
                         pm.code), \
                10, pm.id, j.id, j.company_id, pm.payment_type, \
                now() AT TIME ZONE 'UTC', now() AT TIME ZONE 'UTC' \
         FROM account_journal j \
         CROSS JOIN ir_model_data d \
         JOIN account_payment_method pm ON pm.id = d.res_id \
         WHERE j.type IN ('bank', 'cash', 'credit') \
           AND d.module = 'account' AND d.model = 'account.payment.method' \
           AND d.name IN ('account_payment_method_manual_in', 'account_payment_method_manual_out') \
           AND NOT EXISTS (SELECT 1 FROM account_payment_method_line l WHERE l.journal_id = j.id) \
         ORDER BY j.id, d.name",
        &[],
    )
    .await?;
    tx.execute(DEMO_OUTSTANDING, &[]).await?;
    Ok(())
}
