// SPDX-License-Identifier: MIT OR Apache-2.0
//! A journal is the book an entry is posted in: its type, its company, its short code (the fixed
//! text of its entry numbers), its default and special accounts and its numbering declaration
//! (2). The values an administrator may change (code length, proposed codes, the
//! account types allowed as default account) are the declared settings of [`settings`].
//! What needs journal entries (company change, archive with drafts, hash mode) and the creation
//! of default accounts arrive with the move and company-field slices.

pub mod settings;

use std::collections::{BTreeMap, BTreeSet};

use erp_sdk::Operation;
use erp_sdk::prelude::*;
use erp_sdk::sequence::{MAX_PATTERN_PADDING, SequencePattern, SequenceReset};

use crate::fiscal::ensure::require_company_or_ancestor_access;
use settings::{CODE_MAX_LENGTH, JournalSettings};

/// Journal types whose default account is a liquidity account (bank, cash, credit card).
pub fn is_liquidity(journal_type: &str) -> bool {
    matches!(journal_type, "bank" | "cash" | "credit")
}

/// Journal types numbered per year by default; the others per month.
fn numbered_per_year(journal_type: &str) -> bool {
    matches!(journal_type, "sale" | "bank" | "cash" | "credit")
}

/// The refusal of the unique key on (company, code).
const CODE_UNIQUE: &str = "Each journal code can be used once per company.";

/// The task that creates a liquidity journal's default account from the company's code prefix.
pub const DEFAULT_ACCOUNT_TASK: &str = "a later release";

/// `account.journal.group`: a ledger, a named group of journals.
/// The reference keeps one list for the database (no company); so does this model.
#[model(
    name = "account.journal.group",
    description = "Account Journal Group",
    order = "sequence, id",
    keys = [key(
        name = "_uniq_name",
        fields = ["name"],
        message = "Another ledger already has this name."
    )],
)]
pub struct AccountJournalGroup {
    /// Name of the ledger.
    #[field(char, required = true, string = "Ledger group")]
    pub name: Option<Arc<str>>,
    /// Journals of the ledger, archived ones included.
    #[field(
        one2many,
        comodel = "account.journal",
        inverse_name = "journal_group_id",
        context = [("active_test", false)],
        string = "Included Journals"
    )]
    pub included_journal_ids: Vec<RecId>,
    /// Display order.
    #[field(integer, default = 10)]
    pub sequence: i32,
}

/// `account.journal`: a book of original entry of one company. Journals of a parent company are
/// usable by its branches (`check_company_domain_parent_of`).
#[model(
    name = "account.journal",
    description = "Journal",
    order = "sequence, type, code",
    rec_names_search = ["name", "code"],
    company_access = direct(field = "company_id", branch = "ancestor", empty = "deny"),
    company_match = "parent_of",
    constraints = [
        (
            "_code_company_uniq",
            "UNIQUE (company_id, code)",
            "Each journal code can be used once per company."
        ),
        (
            "_sequence_padding_range",
            "CHECK (sequence_padding BETWEEN 1 AND 64)",
            "The number digits of a journal must be between 1 and 64."
        ),
    ],
)]
pub struct AccountJournal {
    /// Journal name.
    #[field(char, required = true, translate = true, string = "Journal Name")]
    pub name: Option<Arc<str>>,
    /// Short code, unique per company; the fixed text of the journal's entry numbers.
    #[field(char, required = true, copy = false, string = "Short Code")]
    pub code: Option<Arc<str>>,
    /// Archived journals are hidden without being removed.
    #[field(boolean, default = true)]
    pub active: bool,
    /// What the journal records.
    #[field(
        selection,
        selection = [
            ("sale", "Sales"),
            ("purchase", "Purchase"),
            ("cash", "Cash"),
            ("bank", "Bank"),
            ("credit", "Credit Card"),
            ("general", "Miscellaneous"),
        ],
        required = true,
        string = "Type"
    )]
    pub r#type: Option<Arc<str>>,
    /// The journal's company; the active company when none is given. No index of its own (a
    /// many2one gets one by default): the unique key on (company, code) leads with it.
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        readonly = true,
        index = false,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// Foreign currency of the journal; empty means the company currency.
    #[field(
        many2one,
        comodel = "res.currency",
        index = "btree_not_null",
        string = "Currency"
    )]
    pub currency_id: Option<RecId>,
    /// Account of the journal's own lines; its type must be allowed for the journal type.
    #[field(
        many2one,
        comodel = "account.account",
        ondelete = "restrict",
        copy = false,
        index = "btree_not_null",
        string = "Default Account"
    )]
    pub default_account_id: Option<RecId>,
    /// Account bank statement lines wait on until they are reconciled (bank, cash, credit).
    #[field(
        many2one,
        comodel = "account.account",
        ondelete = "restrict",
        index = "btree_not_null",
        string = "Suspense Account"
    )]
    pub suspense_account_id: Option<RecId>,
    /// Account of a cash register's surplus (bank and cash). Restricted like the default and
    /// suspense accounts: the reference empties it when a shared account is deleted, also on
    /// journals of companies the user cannot see.
    #[field(
        many2one,
        comodel = "account.account",
        ondelete = "restrict",
        index = "btree_not_null",
        string = "Profit Account"
    )]
    pub profit_account_id: Option<RecId>,
    /// Account of a cash register's shortfall (bank and cash).
    #[field(
        many2one,
        comodel = "account.account",
        ondelete = "restrict",
        index = "btree_not_null",
        string = "Loss Account"
    )]
    pub loss_account_id: Option<RecId>,
    /// Order on the dashboard and in lists.
    #[field(integer, default = 10)]
    pub sequence: i32,
    /// Credit notes get their own number chain (`R` before the code).
    #[field(
        boolean,
        compute = "_compute_sequence_defaults",
        store,
        readonly = false,
        string = "Dedicated Credit Note Sequence"
    )]
    pub refund_sequence: bool,
    /// Payments get their own number chain (`P` before the code).
    #[field(
        boolean,
        compute = "_compute_sequence_defaults",
        store,
        readonly = false,
        string = "Dedicated Payment Sequence"
    )]
    pub payment_sequence: bool,
    /// When the entry numbers restart at 1; set from the type at create when not given, and kept
    /// when the type changes later.
    #[field(
        selection,
        selection = [
            ("never", "Never"),
            ("year", "Every year"),
            ("month", "Every month"),
            ("year_range", "Every fiscal year"),
            ("year_range_month", "Every month of the fiscal year"),
        ],
        string = "Numbers Restart"
    )]
    pub sequence_reset: Option<Arc<str>>,
    /// Digits of the counter of the entry numbers (1 to 64); set from the type at create when
    /// not given, and kept when the type changes later.
    #[field(integer, string = "Number Digits")]
    pub sequence_padding: i32,
    /// The ledger the journal posts to; empty is the statutory ledger. A ledger in use cannot be
    /// deleted: ledgers have no company, so a deletion would move journals of companies the user
    /// cannot see to the statutory ledger (bug fix BUG-129).
    #[field(
        many2one,
        comodel = "account.journal.group",
        ondelete = "restrict",
        index = "btree_not_null",
        string = "Ledger"
    )]
    pub journal_group_id: Option<RecId>,
    /// Dashboard colour index.
    #[field(integer, default = 0, string = "Color Index")]
    pub color: i32,
    /// Shown on the accounting dashboard.
    #[field(boolean, default = true, string = "Show journal on dashboard")]
    pub show_on_dashboard: bool,
}

fn text_of(value: &Option<Arc<str>>) -> &str {
    value.as_deref().unwrap_or_default()
}

fn id_of(value: Option<RecId>) -> Option<i64> {
    value.and_then(|id| id.as_real()).map(|real| real.get())
}

#[methods(AccountJournal, addon = "account")]
impl AccountJournal {
    /// Separate chains by type: credit notes apart for sales and purchases,
    /// payments apart for bank, cash and credit card. Values given
    /// at create are kept. The restart rule and the digits are create-time defaults instead
    /// ([`numbering_defaults`]), so a later type change keeps the administrator's numbering.
    #[compute(depends = ["type"])]
    fn _compute_sequence_defaults(batch: &mut Batch<'_, AccountJournal>) -> Result<()> {
        for i in batch.indices() {
            let journal_type = batch.get(i, AccountJournal::TYPE)?;
            let journal_type = text_of(&journal_type);
            batch.set(
                i,
                AccountJournal::REFUND_SEQUENCE,
                matches!(journal_type, "sale" | "purchase"),
            )?;
            batch.set(
                i,
                AccountJournal::PAYMENT_SEQUENCE,
                is_liquidity(journal_type),
            )?;
        }
        Ok(())
    }

    /// Fills the company (the active one when none is given) and the numbering defaults of the
    /// type. A journal without code gets the first free code of the type's prefix setting.
    #[method(override)]
    async fn create(
        this: &RecordSet<AccountJournal>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let mut companies = Vec::with_capacity(vals_list.len());
        let mut taken: BTreeMap<i64, Vec<String>> = BTreeMap::new();
        let mut typed = false;
        let mut unproposed = Vec::new();
        for vals in &mut vals_list {
            let company = match vals.get("company_id") {
                Some(Cell::Id(id)) => *id,
                Some(Cell::Int(id)) => {
                    let id = i64::from(*id);
                    vals.insert("company_id".into(), Cell::Id(id));
                    id
                }
                None | Some(Cell::Null | Cell::Bool(false)) => {
                    let id = this.env().company_id()?.ok_or_else(|| {
                        Error::validation("No company is active: give the new journal a company.")
                    })?;
                    vals.insert("company_id".into(), Cell::Id(id));
                    id
                }
                Some(other) => {
                    return Err(Error::validation(format!(
                        "The company of a journal is a company id, not {other:?}."
                    )));
                }
            };
            match vals.get("code") {
                Some(Cell::Str(code)) if !code.is_empty() => {
                    typed = true;
                    taken.entry(company).or_default().push(code.to_string());
                }
                _ => unproposed.push(company),
            }
            companies.push(company);
        }
        if !unproposed.is_empty() && !this.env().su() {
            // the code proposal reads every journal of the company elevated: only for a caller who
            // may create journals there (the model check, then the journal company rule)
            this.as_records()
                .check_access(tx, Operation::Create)
                .await?;
            require_company_or_ancestor_access(tx, this.env(), &unproposed).await?;
        }
        let mut settings = None;
        let mut allocated = false;
        for (vals, company) in vals_list.iter_mut().zip(companies) {
            let Some(Cell::Str(journal_type)) = vals.get("type").cloned() else {
                continue; // the required check names the missing type
            };
            let (reset, padding) = numbering_defaults(&journal_type);
            if !vals.contains("sequence_reset") {
                vals.insert("sequence_reset".into(), Cell::Str(reset.into()));
            }
            if !vals.contains("sequence_padding") {
                vals.insert("sequence_padding".into(), Cell::Int(padding));
            }
            if matches!(vals.get("code"), Some(Cell::Str(code)) if !code.is_empty()) {
                continue;
            }
            if settings.is_none() {
                settings = Some(JournalSettings::read(tx).await?);
            }
            let settings = settings
                .as_ref()
                .ok_or_else(|| Error::internal("journal settings were not read"))?;
            let used = taken.entry(company).or_default();
            let code = next_code(tx, this.env(), company, &journal_type, settings, used).await?;
            vals.insert("code".into(), Cell::Str(code.into()));
            allocated = true;
        }
        // the insert reaches the unique key at the flush; a concurrent create that proposed the same
        // code first wins, and this one is told why a code nobody typed is "not unique" (a typed
        // code in the batch may be the duplicate itself: saving again would not help)
        let created = async {
            let created = sup.call(tx, this.as_records(), (vals_list,)).await?;
            tx.flush().await?;
            Ok::<_, Error>(created)
        }
        .await
        .map_err(|error: Error| {
            if allocated && !typed && error.message().contains(CODE_UNIQUE) {
                Error::conflict(
                    "Another journal of the company took the proposed code at the same time. \
                     Save again to get the next free code.",
                )
                .with_source(error)
            } else {
                error
            }
        })?;
        check_currency_consistency(tx, this.env(), &created.ids(), &[]).await?;
        Ok(created)
    }

    /// A new journal currency is copied onto the default account of bank, cash and credit card
    /// journals; then the journal and its default account must agree on the
    /// foreign currency.
    #[method(override)]
    async fn write(
        this: &RecordSet<AccountJournal>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        let currency = vals.contains("currency_id");
        let account = vals.contains("default_account_id");
        sup.call(tx, this.as_records(), (vals,)).await?;
        if currency {
            for journal in this.iter() {
                let journal_type = journal.get(tx, AccountJournal::TYPE).await?;
                let default = id_of(journal.get(tx, AccountJournal::DEFAULT_ACCOUNT_ID).await?);
                let (true, Some(default)) = (is_liquidity(text_of(&journal_type)), default) else {
                    continue;
                };
                let currency = id_of(journal.get(tx, AccountJournal::CURRENCY_ID).await?);
                let cell = currency.map_or(Cell::Null, Cell::Id);
                this.env()
                    .records("account.account")?
                    .browse(vec![default])?
                    .write(tx, RawVals::new().set_cell("currency_id", cell))
                    .await?;
            }
        }
        if currency || account {
            check_currency_consistency(tx, this.env(), &this.ids(), &[]).await?;
        }
        Ok(())
    }

    /// A configured journal remains compatible with each company that selected it.
    #[constrains("company_id")]
    async fn _check_company_configuration_links(
        this: &RecordSet<AccountJournal>,
        tx: &mut Transaction,
    ) -> Result<()> {
        crate::company::validation::check_journals(tx, &this.ids()).await
    }

    /// The code is one to [`CODE_MAX_LENGTH`] characters, without a space at its start or end.
    #[constrains("code")]
    async fn _check_code(this: &RecordSet<AccountJournal>, tx: &mut Transaction) -> Result<()> {
        let settings = JournalSettings::read(tx).await?;
        for code in this.mapped(tx, AccountJournal::CODE).await? {
            let code = text_of(&code);
            if code != code.trim() {
                return Err(Error::validation(format!(
                    "The journal code {code:?} starts or ends with a space; remove it."
                )));
            }
            if code.is_empty() || code.chars().count() > settings.code_max_length {
                return Err(Error::validation(format!(
                    "The journal code {code:?} must have 1 to {} characters (setting {CODE_MAX_LENGTH}).",
                    settings.code_max_length
                )));
            }
        }
        Ok(())
    }

    /// Every number chain of the journal has a valid pattern, so a posting
    /// never meets a pattern error.
    #[constrains(
        "code",
        "sequence_reset",
        "sequence_padding",
        "refund_sequence",
        "payment_sequence"
    )]
    async fn _check_sequence_settings(
        this: &RecordSet<AccountJournal>,
        tx: &mut Transaction,
    ) -> Result<()> {
        for journal in this.iter() {
            let sequence = JournalSequence::of(tx, &journal).await?;
            for kind in ["document", "refund", "payment"] {
                sequence.pattern(kind)?;
            }
        }
        Ok(())
    }

    /// The default account's type must suit the journal. A
    /// bank, cash or credit card journal has a default account, on create and on every later
    /// change of its type or account, until the account is created for it
    /// ([`DEFAULT_ACCOUNT_TASK`]).
    #[constrains("type", "default_account_id")]
    async fn _check_default_account_type(
        this: &RecordSet<AccountJournal>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let mut pairs = Vec::new();
        for journal in this.iter() {
            let journal_type = journal.get(tx, AccountJournal::TYPE).await?;
            let journal_type = text_of(&journal_type);
            match id_of(journal.get(tx, AccountJournal::DEFAULT_ACCOUNT_ID).await?) {
                Some(account) => pairs.push((journal_type.to_owned(), account)),
                None if is_liquidity(journal_type) => {
                    return Err(Error::validation(format!(
                        "A {journal_type} journal needs a default account: select a liquidity \
                         account. Creating it automatically comes with task {DEFAULT_ACCOUNT_TASK}."
                    )));
                }
                None => {}
            }
        }
        check_default_account_types(tx, this.env(), &pairs, false).await
    }
}

/// The numbering declaration of a journal (2), read by the numbering service. Made
/// only by [`JournalSequence::of`], which checks the stored values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalSequence {
    code: String,
    reset: SequenceReset,
    padding: usize,
    refund_sequence: bool,
    payment_sequence: bool,
}

impl JournalSequence {
    /// The declaration of `journal`.
    ///
    /// # Errors
    ///
    /// Read errors; `Validation` for a stored reset outside the selection or a padding outside 1
    /// to [`MAX_PATTERN_PADDING`].
    pub async fn of(tx: &mut Transaction, journal: &RecordSet<AccountJournal>) -> Result<Self> {
        let code = journal.get(tx, AccountJournal::CODE).await?;
        let reset = journal.get(tx, AccountJournal::SEQUENCE_RESET).await?;
        let padding = journal.get(tx, AccountJournal::SEQUENCE_PADDING).await?;
        let padding = usize::try_from(padding)
            .ok()
            .filter(|p| (1..=MAX_PATTERN_PADDING).contains(p))
            .ok_or_else(|| {
                Error::validation(format!(
                    "The number digits of a journal must be between 1 and {MAX_PATTERN_PADDING}, got {padding}."
                ))
            })?;
        Ok(Self {
            code: text_of(&code).to_owned(),
            reset: SequenceReset::parse(text_of(&reset))?,
            padding,
            refund_sequence: journal.get(tx, AccountJournal::REFUND_SEQUENCE).await?,
            payment_sequence: journal.get(tx, AccountJournal::PAYMENT_SEQUENCE).await?,
        })
    }

    /// The chain kind and pattern of a document of `kind` (`document`, `refund`, `payment`):
    /// a refund or payment without its own chain is numbered in the document chain.
    ///
    /// # Errors
    ///
    /// `Validation` for another kind or a pattern the code cannot make (a line break in it).
    pub fn pattern(&self, kind: &str) -> Result<(&'static str, SequencePattern)> {
        let (chain, letter) = match kind {
            "document" => ("document", ""),
            "refund" if self.refund_sequence => ("refund", "R"),
            "payment" if self.payment_sequence => ("payment", "P"),
            "refund" | "payment" => ("document", ""),
            other => {
                return Err(Error::validation(format!(
                    "{other:?} is not a sequence kind of a journal (document, refund, payment)"
                )));
            }
        };
        let pattern = SequencePattern::builder(self.reset)
            .prefix(format!("{letter}{}/", self.code))
            .padding(self.padding)
            .build()
            .map_err(|error| {
                Error::validation(format!(
                    "Journal {}: its entry numbers cannot be made from this code and numbering: {}",
                    self.code,
                    error.message()
                ))
            })?;
        Ok((chain, pattern))
    }
}

/// The restart rule and digits a new journal of `journal_type` gets when none are given: yearly
/// numbers of five digits for sales and liquidity journals, monthly numbers of four digits for the
/// others.
fn numbering_defaults(journal_type: &str) -> (&'static str, i32) {
    if numbered_per_year(journal_type) {
        ("year", 5)
    } else {
        ("month", 4)
    }
}

/// The first free code `<prefix><n>` for a journal of `journal_type` in `company`: codes of the
/// company and its ancestors and codes of the same create batch (`used`) are taken.
async fn next_code(
    tx: &mut Transaction,
    env: &Env,
    company: i64,
    journal_type: &str,
    settings: &JournalSettings,
    used: &mut Vec<String>,
) -> Result<String> {
    let prefix = settings.prefix(journal_type)?.to_owned();
    let journals = env
        .records("account.journal")?
        .sudo(true)
        .with_context([("active_test", Value::Bool(false))]);
    let found = journals
        .search(
            tx,
            &Domain::condition("company_id", Operator::ParentOf, company)?,
        )
        .await?;
    let mut taken: BTreeSet<String> = found
        .cells(tx, "code")
        .await?
        .into_iter()
        .filter_map(|cell| match cell {
            Cell::Str(code) => Some(code.to_string()),
            _ => None,
        })
        .collect();
    taken.extend(used.iter().cloned());
    // among len + 1 candidates one is free
    for n in 1..=taken.len() + 1 {
        let code = format!("{prefix}{n}");
        if code.chars().count() > settings.code_max_length {
            break;
        }
        if !taken.contains(&code) {
            used.push(code.clone());
            return Ok(code);
        }
    }
    Err(Error::validation(format!(
        "No free journal code starting with {prefix} fits in {} characters: give the journal a \
         code, or raise the setting {CODE_MAX_LENGTH}.",
        settings.code_max_length
    )))
}

const JOURNAL_SIDE: &str = "A sale or purchase journal cannot take a receivable or payable account as its default account.";
const ACCOUNT_SIDE: &str = "This account is the default account of a sale or purchase journal, so its \
     type cannot be receivable or payable.";

/// Each `(journal type, default account)` pair: a sale or purchase journal never defaults to a
/// receivable or payable account,
/// and the account type is one the journal type's setting allows.
async fn check_default_account_types(
    tx: &mut Transaction,
    env: &Env,
    pairs: &[(String, i64)],
    account_side: bool,
) -> Result<()> {
    if pairs.is_empty() {
        return Ok(());
    }
    let ids: BTreeSet<i64> = pairs.iter().map(|(_, account)| *account).collect();
    // An account the caller may not read is refused before its type is read, with one sentence
    // whatever that type is: a type-dependent outcome would tell the type (the type is read
    // below with the framework's rights).
    let mut checked = BTreeSet::new();
    for (journal_type, account) in pairs {
        if checked.insert(*account) && !readable(tx, env, "account.account", *account).await? {
            return Err(Error::validation(format!(
                "The account #{account} cannot be the default account of a {journal_type} journal."
            )));
        }
    }
    let accounts = env
        .records("account.account")?
        .sudo(true)
        .with_context([("active_test", Value::Bool(false))])
        .browse(ids.into_iter().collect::<Vec<_>>())?;
    let mut types: BTreeMap<i64, String> = BTreeMap::new();
    for (id, cell) in accounts
        .ids()
        .into_iter()
        .zip(accounts.cells(tx, "account_type").await?)
    {
        match cell {
            Cell::Str(t) => types.insert(id, t.to_string()),
            other => {
                return Err(Error::internal(format!(
                    "account {id} has the account type {other:?}"
                )));
            }
        };
    }
    let settings = JournalSettings::read(tx).await?;
    for (journal_type, account) in pairs {
        let account_type = types
            .get(account)
            .ok_or_else(|| Error::internal(format!("account {account} was not read")))?;
        let side = matches!(journal_type.as_str(), "sale" | "purchase")
            && matches!(
                account_type.as_str(),
                "asset_receivable" | "liability_payable"
            );
        if side {
            return Err(Error::validation(if account_side {
                ACCOUNT_SIDE
            } else {
                JOURNAL_SIDE
            }));
        }
        settings.check_default_account_type(journal_type, account_type)?;
    }
    Ok(())
}

/// The account side of the default-account type rules: accounts in `accounts` that a journal uses
/// as default account keep a type the journal allows.
///
/// # Errors
///
/// `Validation` with the reference's message or naming the setting.
pub async fn check_accounts_as_default(
    tx: &mut Transaction,
    env: &Env,
    accounts: &[i64],
) -> Result<()> {
    if accounts.is_empty() {
        return Ok(());
    }
    let journals = env
        .records("account.journal")?
        .sudo(true)
        .with_context([("active_test", Value::Bool(false))]);
    let found = journals
        .search(
            tx,
            &Domain::condition("default_account_id", Operator::In, accounts.to_vec())?,
        )
        .await?;
    if found.is_empty() {
        return Ok(());
    }
    let types = found.cells(tx, "type").await?;
    let defaults = found.cells(tx, "default_account_id").await?;
    let pairs = types
        .into_iter()
        .zip(defaults)
        .map(|pair| match pair {
            (Cell::Str(t), Cell::Id(account)) => Ok((t.to_string(), account)),
            other => Err(Error::internal(format!(
                "a journal found by its default account reads as {other:?}"
            ))),
        })
        .collect::<Result<Vec<(String, i64)>>>()?;
    check_default_account_types(tx, env, &pairs, true).await
}

/// A journal with a foreign currency and its default account in another currency is refused,
/// among `journals` and the journals using one of `accounts` as default account
/// (the journal side is). An account without a
/// currency fits any journal, as in the reference.
///
/// # Errors
///
/// `Validation` "Journal ... and account ... must use the same foreign currency."
pub async fn check_currency_consistency(
    tx: &mut Transaction,
    env: &Env,
    journals: &[i64],
    accounts: &[i64],
) -> Result<()> {
    if journals.is_empty() && accounts.is_empty() {
        return Ok(());
    }
    tx.flush().await?;
    let rows = tx
        .query(
            "SELECT j.id::int8, a.id::int8 FROM account_journal j \
             JOIN res_company c ON c.id = j.company_id \
             JOIN account_account a ON a.id = j.default_account_id \
             WHERE j.currency_id IS NOT NULL AND j.currency_id <> c.currency_id \
               AND a.currency_id <> j.currency_id \
               AND (j.id = ANY($1::int8[]) OR a.id = ANY($2::int8[])) \
             ORDER BY j.id LIMIT 1",
            &[&journals, &accounts],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(());
    };
    let read = |i: usize| -> Result<i64> {
        row.try_get(i)
            .map_err(|e| Error::db(format!("journal currency check: {e}")))
    };
    let (journal, account) = (read(0)?, read(1)?);
    let journal = visible_name(tx, env, "account.journal", journal).await?;
    let account = visible_name(tx, env, "account.account", account).await?;
    Err(Error::validation(format!(
        "Journal '{journal}' and account '{account}' must use the same foreign currency."
    )))
}

/// The account fields of a journal, with their labels.
const ACCOUNT_FIELDS: [(&str, &str); 4] = [
    ("default_account_id", "Default Account"),
    ("suspense_account_id", "Suspense Account"),
    ("profit_account_id", "Profit Account"),
    ("loss_account_id", "Loss Account"),
];

/// Accounts in `accounts` keep, among their companies, the company of every journal using them as
/// default, suspense, profit or loss account, or one of its parent companies: the `parent_of`
/// match the journal's account fields are checked with on the journal side (the same
/// `parent_path` prefix test as `fiscal/ensure.rs`). The reference checks only journal items when
/// an account loses a company (bug fix BUG-128).
///
/// # Errors
///
/// `Validation` naming the account, the account field and the journal; a record the caller cannot
/// read is named by its id.
pub async fn check_journal_companies(
    tx: &mut Transaction,
    env: &Env,
    accounts: &[i64],
) -> Result<()> {
    if accounts.is_empty() {
        return Ok(());
    }
    tx.flush().await?;
    let rows = tx
        .query(
            "SELECT j.id::int8, a.id::int8, k.field FROM account_journal j \
             JOIN res_company jc ON jc.id = j.company_id \
             CROSS JOIN LATERAL (VALUES (j.default_account_id, 'default_account_id'), \
                 (j.suspense_account_id, 'suspense_account_id'), \
                 (j.profit_account_id, 'profit_account_id'), \
                 (j.loss_account_id, 'loss_account_id')) AS k(account_id, field) \
             JOIN account_account a ON a.id = k.account_id \
             WHERE (j.default_account_id = ANY($1::int8[]) OR j.suspense_account_id = ANY($1::int8[]) \
                    OR j.profit_account_id = ANY($1::int8[]) OR j.loss_account_id = ANY($1::int8[])) \
               AND a.id = ANY($1::int8[]) \
               AND NOT EXISTS (SELECT 1 FROM account_account_res_company_rel r \
                   JOIN res_company ac ON ac.id = r.res_company_id \
                   WHERE r.account_account_id = a.id \
                     AND jc.parent_path LIKE ac.parent_path || '%') \
             ORDER BY j.id, k.field LIMIT 1",
            &[&accounts],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(());
    };
    let journal: i64 = row
        .try_get(0)
        .map_err(|e| Error::db(format!("journal company check: {e}")))?;
    let account: i64 = row
        .try_get(1)
        .map_err(|e| Error::db(format!("journal company check: {e}")))?;
    let field: String = row
        .try_get(2)
        .map_err(|e| Error::db(format!("journal company check: {e}")))?;
    let label = ACCOUNT_FIELDS
        .iter()
        .find(|(name, _)| *name == field)
        .map(|(_, label)| *label)
        .ok_or_else(|| Error::internal(format!("journal company check: field {field}")))?;
    let name = visible_name(tx, env, "account.account", account).await?;
    let journal = visible_name(tx, env, "account.journal", journal).await?;
    Err(Error::validation(format!(
        "The account '{name}' is the {label} of the journal '{journal}' of a company that is not \
         among the account's companies: keep that company, or one of its parent companies, on the \
         account."
    )))
}

/// The name of record `id` of `model` when the caller may read it, else `#<id>`: a refusal never
/// shows the name of a record the caller cannot see (archived records count as visible), and a
/// caller who may not read `model` at all still gets the refusal, not an access error.
///
/// # Errors
///
/// Read errors that are not refusals.
async fn visible_name(tx: &mut Transaction, env: &Env, model: &str, id: i64) -> Result<String> {
    if !readable(tx, env, model, id).await? {
        return Ok(format!("#{id}"));
    }
    match env
        .sudo(false)
        .records(model)?
        .browse(vec![id])?
        .cell(tx, "name")
        .await?
    {
        Cell::Str(name) => Ok(name.to_string()),
        other => Err(Error::internal(format!("{model} name is {other:?}"))),
    }
}

/// Whether the caller of `env` may read record `id` of `model` (the model check and the record
/// rules; a missing record is not readable). Constraints run elevated: the caller is `env`
/// without the elevation (the superuser stays one).
///
/// # Errors
///
/// Failures that are not refusals.
async fn readable(tx: &mut Transaction, env: &Env, model: &str, id: i64) -> Result<bool> {
    let record = env.sudo(false).records(model)?.browse(vec![id])?;
    Ok(!record
        .filtered_access(tx, Operation::Read)
        .await?
        .is_empty())
}
