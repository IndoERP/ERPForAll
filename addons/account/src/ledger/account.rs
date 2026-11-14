// SPDX-License-Identifier: MIT OR Apache-2.0
//! This slice holds the account record with its type rules, tags, hierarchy and per-company
//! code; the journal checks (currency, default-account type) run in `crate::journal`. What needs
//! journal items or taxes (deletion guard, taxes on off-balance accounts, opening balances, merge)
//! arrives with the slice that owns those tables;
//! lists each case and its slice.

use std::collections::BTreeMap;

use erp_sdk::prelude::*;
use erp_sdk::{Command, Operation};

use super::code::{active_root_company, codes_by_account};
use super::root::root_name;

/// The group of an account type: the part before the first underscore (`asset_cash` is `asset`,
/// `off_balance` is `off`), as.
pub fn internal_group_of(account_type: &str) -> &str {
    account_type.split('_').next().unwrap_or(account_type)
}

/// Whether balances of the type carry forward into the next fiscal year: every type except the
/// profit-and-loss groups and the current-year earnings.
pub fn includes_initial_balance(account_type: &str) -> bool {
    !matches!(internal_group_of(account_type), "income" | "expense")
        && account_type != "equity_unaffected"
}

/// `account.account`: an account of the chart, shared by the companies in `company_ids`.
#[model(
    name = "account.account",
    description = "Account",
    order = "id",
    parent_store = true,
    parent_name = "parent_id",
    company_access = direct(field = "company_ids", branch = "ancestor", empty = "deny"),
    company_match = "parent_of_any",
)]
pub struct AccountAccount {
    /// Account name.
    #[field(
        char,
        required = true,
        translate = true,
        index = "trigram",
        string = "Account Name"
    )]
    pub name: Option<Arc<str>>,
    /// Description.
    #[field(text, translate = true)]
    pub description: Option<Arc<str>>,
    /// Forces every journal item of the account into this currency (bank journals).
    #[field(many2one, comodel = "res.currency", string = "Account Currency")]
    pub currency_id: Option<RecId>,
    /// The code in the active company's root company; written through the code mapping.
    #[field(
        char,
        size = 64,
        compute = "_compute_code",
        inverse = "_inverse_code",
        string = "Code"
    )]
    pub code: Option<Arc<str>>,
    /// Archived accounts stay on posted entries but cannot be picked.
    #[field(boolean, default = true)]
    pub active: bool,
    /// Type: drives the reports, the fiscal-year closing and the defaults below.
    #[field(
        selection,
        selection = [
            ("asset_receivable", "Receivable"),
            ("asset_cash", "Bank and Cash"),
            ("asset_current", "Current Assets"),
            ("asset_non_current", "Non-current Assets"),
            ("asset_prepayments", "Prepayments"),
            ("asset_fixed", "Fixed Assets"),
            ("liability_payable", "Payable"),
            ("liability_credit_card", "Credit Card"),
            ("liability_current", "Current Liabilities"),
            ("liability_non_current", "Non-current Liabilities"),
            ("equity", "Equity"),
            ("equity_unaffected", "Current Year Earnings"),
            ("income", "Income"),
            ("income_other", "Other Income"),
            ("expense", "Expenses"),
            ("expense_other", "Other Expenses"),
            ("expense_depreciation", "Depreciation"),
            ("expense_direct_cost", "Cost of Revenue"),
            ("off_balance", "Off-Balance Sheet"),
        ],
        required = true,
        index = true,
        string = "Type"
    )]
    pub account_type: Option<Arc<str>>,
    /// Whether the balance carries into the next fiscal year.
    #[field(
        boolean,
        compute = "_compute_type_flags",
        store,
        string = "Bring Accounts Balance Forward"
    )]
    pub include_initial_balance: bool,
    /// Top-level category of the type.
    #[field(
        selection,
        selection = [
            ("equity", "Equity"),
            ("asset", "Asset"),
            ("liability", "Liability"),
            ("income", "Income"),
            ("expense", "Expense"),
            ("off", "Off Balance"),
        ],
        compute = "_compute_type_flags",
        store,
        string = "Internal Group"
    )]
    pub internal_group: Option<Arc<str>>,
    /// Used in reconciliation; receivable and payable accounts always have it.
    #[field(
        boolean,
        compute = "_compute_reconcile",
        store,
        readonly = false,
        string = "Payment Reconciliation"
    )]
    pub reconcile: bool,
    /// Internal notes.
    #[field(text, string = "Internal Notes")]
    pub note: Option<Arc<str>>,
    /// Belongs to Non Trade Receivable/Payable in reports.
    #[field(boolean, default = false)]
    pub non_trade: bool,
    /// Companies that use the account.
    #[field(
        many2many,
        comodel = "res.company",
        relation = "account_account_res_company_rel",
        column1 = "account_account_id",
        column2 = "res_company_id",
        string = "Companies"
    )]
    pub company_ids: Vec<RecId>,
    /// Default taxes when a document has no product tax choice.
    #[field(
        many2many,
        comodel = "account.tax",
        relation = "account_account_tax_default_rel",
        column1 = "account_id",
        column2 = "tax_id",
        check_company = true,
        string = "Default taxes"
    )]
    pub tax_ids: Vec<RecId>,
    /// The codes of the account, one per root company. Declared after `company_ids` on purpose:
    /// x2many values of a create are written in field order, and the company check of a code row
    /// reads the account's companies, which must be written first.
    #[field(
        one2many,
        comodel = "account.code.mapping",
        inverse_name = "account_id"
    )]
    pub code_mapping_ids: Vec<RecId>,
    /// Optional tags for custom reporting.
    #[field(
        many2many,
        comodel = "account.account.tag",
        relation = "account_account_account_tag",
        column1 = "account_account_id",
        column2 = "account_account_tag_id",
        ondelete = "restrict",
        string = "Tags"
    )]
    pub tag_ids: Vec<RecId>,
    /// First characters of the code, for report grouping.
    #[field(many2one, comodel = "account.root", compute = "_compute_root_id")]
    pub root_id: Option<RecId>,
    /// Parent account of the chart hierarchy.
    #[field(
        many2one,
        comodel = "account.account",
        ondelete = "restrict",
        index = true,
        string = "Parent Account"
    )]
    pub parent_id: Option<RecId>,
    /// Materialized ancestry path managed by the ORM parent store.
    #[field(char, index = true)]
    pub parent_path: Option<Arc<str>>,
}

fn text_of(value: &Option<Arc<str>>) -> &str {
    value.as_deref().unwrap_or_default()
}

#[methods(AccountAccount, addon = "account")]
impl AccountAccount {
    /// `internal_group` and `include_initial_balance` follow the type.
    #[compute(depends = ["account_type"])]
    fn _compute_type_flags(batch: &mut Batch<'_, AccountAccount>) -> Result<()> {
        for i in batch.indices() {
            let account_type = batch.get(i, AccountAccount::ACCOUNT_TYPE)?;
            let account_type = text_of(&account_type);
            let group: Option<Arc<str>> =
                (!account_type.is_empty()).then(|| internal_group_of(account_type).into());
            batch.set(i, AccountAccount::INTERNAL_GROUP, group)?;
            batch.set(
                i,
                AccountAccount::INCLUDE_INITIAL_BALANCE,
                includes_initial_balance(account_type),
            )?;
        }
        Ok(())
    }

    /// Reconcile is forced by the type for receivable, payable, bank and cash, credit card,
    /// off-balance and the income, expense and equity groups; other types keep the user's choice.
    #[compute(depends = ["account_type"])]
    fn _compute_reconcile(batch: &mut Batch<'_, AccountAccount>) -> Result<()> {
        for i in batch.indices() {
            let account_type = batch.get(i, AccountAccount::ACCOUNT_TYPE)?;
            let account_type = text_of(&account_type);
            let forced = if matches!(
                internal_group_of(account_type),
                "income" | "expense" | "equity"
            ) {
                Some(false)
            } else if matches!(account_type, "asset_receivable" | "liability_payable") {
                Some(true)
            } else if matches!(
                account_type,
                "asset_cash" | "liability_credit_card" | "off_balance"
            ) {
                Some(false)
            } else {
                None
            };
            if let Some(value) = forced {
                batch.set(i, AccountAccount::RECONCILE, value)?;
            }
        }
        Ok(())
    }

    /// The code in the root company of the active company.
    #[compute(depends = ["code_mapping_ids.code"], depends_context = ["company"])]
    async fn _compute_code(
        this: &RecordSet<AccountAccount>,
        tx: &mut Transaction,
        out: &mut Assign<'_, AccountAccount>,
    ) -> Result<()> {
        let root = active_root_company(tx, this.env()).await?;
        let ids: Vec<i64> = this
            .as_records()
            .rec_ids()
            .iter()
            .filter_map(|id| id.as_real().map(|real| real.get()))
            .collect();
        let codes = codes_by_account(tx, this.env(), &ids, root).await?;
        for (index, id) in this.as_records().rec_ids().iter().enumerate() {
            let code = id
                .as_real()
                .and_then(|real| codes.get(&real.get()))
                .map(|code| Arc::<str>::from(code.as_str()));
            out.set(index, AccountAccount::CODE, code)?;
        }
        Ok(())
    }

    /// Writing the code stores it for the root company of the active company, whichever branch
    /// the user works; an empty code removes the entry. At create the given
    /// value is not readable here, so `create` stores it itself ([`store_code`]).
    #[method]
    async fn _inverse_code(this: &RecordSet<AccountAccount>, tx: &mut Transaction) -> Result<()> {
        let root = active_root_company(tx, this.env()).await?;
        for account in this.iter() {
            let id = account.as_records().ids()[0];
            let code = account.get(tx, AccountAccount::CODE).await?;
            store_code(tx, this.env(), root, id, text_of(&code), false).await?;
        }
        // every company of the tree reads the new value (the reference invalidates `code` for all
        // companies here too)
        tx.invalidate_records(this.as_records(), Some(&["code", "root_id"]))
            .await?;
        Ok(())
    }

    /// The root node of the code in the active company.
    #[compute(depends = ["code_mapping_ids.code"], depends_context = ["company"])]
    async fn _compute_root_id(
        this: &RecordSet<AccountAccount>,
        tx: &mut Transaction,
        out: &mut Assign<'_, AccountAccount>,
    ) -> Result<()> {
        let root = active_root_company(tx, this.env()).await?;
        let ids: Vec<i64> = this
            .as_records()
            .rec_ids()
            .iter()
            .filter_map(|id| id.as_real().map(|real| real.get()))
            .collect();
        let codes = codes_by_account(tx, this.env(), &ids, root).await?;
        let names: Vec<String> = codes.values().filter_map(|c| root_name(c)).collect();
        let roots = this.env().sudo(true).records("account.root")?;
        let found = if names.is_empty() {
            roots.browse([])?
        } else {
            let domain = Domain::condition("name", Operator::In, names.clone())?;
            roots.search(tx, &domain).await?
        };
        let root_ids = found.ids();
        let root_names = found.cells(tx, "name").await?;
        let by_name: std::collections::BTreeMap<String, i64> = root_names
            .into_iter()
            .zip(root_ids)
            .filter_map(|(cell, id)| match cell {
                Cell::Str(name) => Some((name.to_string(), id)),
                _ => None,
            })
            .collect();
        for (index, id) in this.as_records().rec_ids().iter().enumerate() {
            let node = id
                .as_real()
                .and_then(|real| codes.get(&real.get()))
                .and_then(|code| root_name(code))
                .and_then(|name| by_name.get(&name).copied())
                .and_then(RecId::from_real);
            out.set(index, AccountAccount::ROOT_ID, node)?;
        }
        Ok(())
    }

    /// A code row the same values give for that root company (`code_mapping_ids`) wins over
    /// `code`; a row for another root company is kept next to it.
    ///
    /// # Errors
    /// `Validation` for a `code` that is not text (absent and null mean no code), and the errors
    /// of the create and of the code storage.
    #[method(override)]
    async fn create(
        this: &RecordSet<AccountAccount>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let company = this.env().company_id()?.and_then(RecId::from_real);
        let mut codes = Vec::with_capacity(vals_list.len());
        for vals in &mut vals_list {
            if let Some(id) = company
                && !vals.contains("company_ids")
                && vals.commands("company_ids").is_none()
            {
                vals.insert_commands("company_ids".into(), vec![Command::link(id)]);
            }
            codes.push(match vals.remove("code") {
                None | Some(Cell::Null) => String::new(),
                Some(Cell::Str(code)) => code.to_string(),
                Some(other) => {
                    return Err(Error::validation(format!(
                        "The account code must be text, not {other:?}."
                    )));
                }
            });
        }
        let created = sup.call(tx, this.as_records(), (vals_list,)).await?;
        if codes.iter().any(|code| !code.is_empty()) {
            let ids = created.ids();
            // the relation table is the truth: pending company commands must reach it first
            tx.flush().await?;
            let active = match company {
                Some(_) => Some(active_root_company(tx, this.env()).await?),
                None => None,
            };
            let rows = tx
                .query(
                    "SELECT r.account_account_id, \
                            COALESCE(NULLIF(split_part(c.parent_path, '/', 1), ''), c.id::text)::int8 \
                     FROM account_account_res_company_rel r \
                     JOIN res_company c ON c.id = r.res_company_id \
                     WHERE r.account_account_id = ANY($1::int8[]) \
                     ORDER BY r.account_account_id, c.id",
                    &[&ids],
                )
                .await?;
            let mut trees: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
            for row in &rows {
                trees.entry(row.get(0)).or_default().push(row.get(1));
            }
            // a caller outside superuser mode stores only into the trees of its own companies
            let mine = if this.env().su() {
                None
            } else {
                let companies: Vec<i64> = tx
                    .companies(this.env())
                    .await?
                    .iter()
                    .map(|c| c.get())
                    .collect();
                let rows = tx
                    .query(
                        "SELECT DISTINCT COALESCE(NULLIF(split_part(parent_path, '/', 1), ''), id::text)::int8 \
                         FROM res_company WHERE id = ANY($1::int8[])",
                        &[&companies],
                    )
                    .await?;
                Some(rows.iter().map(|r| r.get::<_, i64>(0)).collect::<Vec<_>>())
            };
            for (id, code) in ids.into_iter().zip(&codes) {
                if code.is_empty() {
                    continue;
                }
                let own = trees.get(&id).map(Vec::as_slice).unwrap_or_default();
                let root = match (active, &mine) {
                    (Some(root), _) if own.contains(&root) => root,
                    (_, None) => *own.first().or(active.as_ref()).ok_or_else(|| {
                        Error::validation(
                            "Account codes belong to a company: select an active company first.",
                        )
                    })?,
                    (_, Some(mine)) => own
                        .iter()
                        .copied()
                        .find(|root| mine.contains(root))
                        .ok_or_else(|| {
                            Error::validation(
                                "The account belongs to no company you are working in: its code cannot be stored.",
                            )
                        })?,
                };
                // a code row of the same call for this root company is the code: it wins over `code`
                store_code(tx, this.env(), root, id, code, true).await?;
            }
        }
        Ok(created)
    }

    /// Receivable and payable accounts are reconcilable.
    #[constrains("account_type", "reconcile")]
    async fn _check_reconcile(
        this: &RecordSet<AccountAccount>,
        tx: &mut Transaction,
    ) -> Result<()> {
        for account in this.iter() {
            let account_type = account.get(tx, AccountAccount::ACCOUNT_TYPE).await?;
            if matches!(
                text_of(&account_type),
                "asset_receivable" | "liability_payable"
            ) && !account.get(tx, AccountAccount::RECONCILE).await?
            {
                let label = code_label(tx, &account).await?;
                return Err(Error::validation(format!(
                    "A receivable or payable account must allow reconciliation (account code: {label})."
                )));
            }
        }
        Ok(())
    }

    /// Off-balance accounts carry neither reconciliation nor default taxes.
    #[constrains("account_type", "reconcile")]
    async fn _check_off_balance(
        this: &RecordSet<AccountAccount>,
        tx: &mut Transaction,
    ) -> Result<()> {
        for account in this.iter() {
            let account_type = account.get(tx, AccountAccount::ACCOUNT_TYPE).await?;
            if text_of(&account_type) == "off_balance"
                && account.get(tx, AccountAccount::RECONCILE).await?
            {
                return Err(Error::validation(
                    "Off-balance accounts take no part in reconciliation: turn reconciliation off for this account.",
                ));
            }
        }
        Ok(())
    }

    /// Separate from reconciliation so its stored recompute cannot validate taxes before
    /// pending relation commands (for example clear plus type change) are applied.
    #[constrains("account_type", "tax_ids")]
    async fn _check_off_balance_taxes(
        this: &RecordSet<AccountAccount>,
        tx: &mut Transaction,
    ) -> Result<()> {
        for account in this.iter() {
            if account
                .get(tx, AccountAccount::ACCOUNT_TYPE)
                .await?
                .as_deref()
                == Some("off_balance")
                && !account
                    .as_records()
                    .related(tx, "tax_ids")
                    .await?
                    .is_empty()
            {
                return Err(Error::validation(
                    "Off-balance accounts cannot carry default taxes: clear the taxes before changing the account type.",
                ));
            }
        }
        Ok(())
    }

    /// A distribution account stays until its assignments are removed. The restrictive FK also
    /// protects against concurrent inserts.
    #[method(override)]
    async fn unlink(
        this: &RecordSet<AccountAccount>,
        tx: &mut Transaction,
        sup: Super,
    ) -> Result<()> {
        let records = this.as_records();
        records.check_access(tx, Operation::Unlink).await?;
        if !this.is_empty() {
            tx.flush().await?;
            let ids = this.ids();
            // Integrity check, not a tax-data read: only existence for admitted account ids is
            // inspected. Hidden and archived distribution owners must still protect their FK.
            if !tx.query("SELECT 1 FROM account_tax_repartition_line WHERE account_id = ANY($1::int8[]) LIMIT 1", &[&ids]).await?.is_empty() {
                if records.has_access(tx, Operation::Read).await?
                    && tx.has_field_access(this.env(), "account.account", "name", FieldOp::Read).await?
                    && tx.has_field_access(this.env(), "account.account", "code", FieldOp::Read).await?
                {
                    let mut labels=Vec::new();
                    for account in this.iter() {
                        let name=account.get(tx,AccountAccount::NAME).await?.ok_or_else(||Error::programming("An account label is missing."))?;
                        let code=if this.env().company_id()?.is_some() {Some(code_label(tx,&account).await?)} else {None};
                        labels.push(if let Some(code)=code && !code.is_empty() {format!("{code}: {name}")} else {name.to_string()});
                    }
                    return Err(Error::user(format!("Tax distributions still use these accounts: {}. Remove their assignments before deleting the accounts.",labels.join(", "))));
                }
                return Err(Error::user(format!("{} account(s) cannot be deleted because tax distributions still refer to them.", ids.len())));
            }
        }
        sup.call(tx, records, ()).await
    }

    /// An account has a company, and a bank or cash account only one; the journals
    /// using it keep their company among its companies. The check that no journal
    /// item sits in a company being removed comes with the journal item model.
    #[constrains("company_ids", "account_type")]
    async fn _check_company_consistency(
        this: &RecordSet<AccountAccount>,
        tx: &mut Transaction,
    ) -> Result<()> {
        // the relation table is the truth: pending commands must reach it first
        tx.flush().await?;
        let ids = this.ids();
        let cash = tx
            .query(
                "SELECT a.id FROM account_account a WHERE a.id = ANY($1::int8[]) \
                 AND a.account_type = 'asset_cash' \
                 AND (SELECT count(*) FROM account_account_res_company_rel r \
                      WHERE r.account_account_id = a.id) > 1 LIMIT 1",
                &[&ids],
            )
            .await?;
        if !cash.is_empty() {
            return Err(Error::validation(
                "A bank or cash account belongs to one company only.",
            ));
        }
        let orphans = tx
            .query(
                "SELECT a.id FROM account_account a WHERE a.id = ANY($1::int8[]) \
                 AND NOT EXISTS (SELECT 1 FROM account_account_res_company_rel r \
                                 WHERE r.account_account_id = a.id) ORDER BY a.id",
                &[&ids],
            )
            .await?;
        if !orphans.is_empty() {
            let orphans: Vec<i64> = orphans.iter().map(|row| row.get(0)).collect();
            let names = this
                .browse(orphans)?
                .mapped(tx, AccountAccount::NAME)
                .await?;
            let lines: Vec<String> = names
                .iter()
                .map(|name| format!("- {}", text_of(name)))
                .collect();
            return Err(Error::validation(format!(
                "Each account needs at least one company; these have none:\n{}",
                lines.join("\n")
            )));
        }
        crate::journal::check_journal_companies(tx, this.env(), &ids).await?;
        crate::company::validation::check_accounts(tx, &ids).await
    }

    /// The foreign currency of a journal and of its default account agree.
    #[constrains("currency_id")]
    async fn _check_journal_consistency(
        this: &RecordSet<AccountAccount>,
        tx: &mut Transaction,
    ) -> Result<()> {
        crate::journal::check_currency_consistency(tx, this.env(), &[], &this.ids()).await
    }

    /// An account used as a journal's default account keeps a type the journal allows.
    #[constrains("account_type")]
    async fn _check_journal_default_account_type(
        this: &RecordSet<AccountAccount>,
        tx: &mut Transaction,
    ) -> Result<()> {
        crate::journal::check_accounts_as_default(tx, this.env(), &this.ids()).await
    }
}

/// Store `code` as the code of account `id` in root company `root`: create the row, update it, or
/// remove it when `code` is empty; with `keep_existing` a row already there stays as it is.
async fn store_code(
    tx: &mut Transaction,
    env: &Env,
    root: i64,
    id: i64,
    code: &str,
    keep_existing: bool,
) -> Result<()> {
    let mappings = env.sudo(true).records("account.code.mapping")?;
    let existing = mappings
        .search(
            tx,
            &Domain::and([
                Domain::condition("account_id", Operator::Eq, id)?,
                Domain::condition("company_id", Operator::Eq, root)?,
            ]),
        )
        .await?;
    if keep_existing && !existing.is_empty() {
        return Ok(());
    }
    match (existing.is_empty(), code.is_empty()) {
        (true, true) => {}
        (true, false) => {
            let vals = RawVals::new()
                .set_cell("account_id", Cell::Id(id))
                .set_cell("company_id", Cell::Id(root))
                .set_cell("code", Cell::Str(code.into()));
            mappings.create(tx, vec![vals]).await?;
        }
        (false, false) => {
            let vals = RawVals::new().set_cell("code", Cell::Str(code.into()));
            existing.write(tx, vals).await?;
        }
        (false, true) => existing.unlink(tx).await?,
    }
    Ok(())
}

/// The code of `account` in the active company for an error message; empty when it has none or
/// no company is active, so a refusal never fails on the way to its message.
async fn code_label(tx: &mut Transaction, account: &RecordSet<AccountAccount>) -> Result<String> {
    if account.env().company_id()?.is_none() {
        return Ok(String::new());
    }
    let root = active_root_company(tx, account.env()).await?;
    let codes = codes_by_account(tx, account.env(), &account.as_records().ids(), root).await?;
    Ok(codes.into_values().next().unwrap_or_default())
}
