// SPDX-License-Identifier: MIT OR Apache-2.0
//! A company tree shares one chart: the code is stored once per root company, so a branch reads
//! and writes the root's code and two companies of one tree cannot give
//! the same code to two accounts (`UNIQUE (company_id, code)`). Sibling trees
//! have different roots and may reuse a code.

use std::collections::BTreeMap;

use erp_sdk::prelude::*;

use super::root::ensure_roots;

/// Whether `code` uses only letters, digits, dots and dashes (`ACCOUNT_CODE_REGEX`,
/// ).
pub fn code_is_valid(code: &str) -> bool {
    !code.is_empty()
        && code
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

/// The root company of the environment's active company.
///
/// # Errors
/// `Validation` without an active company or when the company row is gone.
pub async fn active_root_company(tx: &mut Transaction, env: &Env) -> Result<i64> {
    let company = env.company_id()?.ok_or_else(|| {
        Error::validation("Account codes belong to a company: select an active company first.")
    })?;
    let rows = tx
        .query(
            "SELECT COALESCE(NULLIF(split_part(parent_path, '/', 1), ''), id::text)::int8 \
             FROM res_company WHERE id = $1",
            &[&company],
        )
        .await?;
    let row = rows
        .first()
        .ok_or_else(|| Error::validation(format!("Company {company} does not exist.")))?;
    row.try_get::<_, i64>(0)
        .map_err(|e| Error::db(format!("Root company could not be read: {e}")))
}

/// The code of each account in `root`, by account id.
///
/// # Errors
/// Any error of the search or the reads.
pub async fn codes_by_account(
    tx: &mut Transaction,
    env: &Env,
    accounts: &[i64],
    root: i64,
) -> Result<BTreeMap<i64, String>> {
    if accounts.is_empty() {
        return Ok(BTreeMap::new());
    }
    let domain = Domain::and([
        Domain::condition("account_id", Operator::In, accounts.to_vec())?,
        Domain::condition("company_id", Operator::Eq, root)?,
    ]);
    let rows = env
        .sudo(true)
        .records("account.code.mapping")?
        .search(tx, &domain)
        .await?;
    let ids = rows.cells(tx, "account_id").await?;
    let codes = rows.cells(tx, "code").await?;
    Ok(ids
        .into_iter()
        .zip(codes)
        .filter_map(|pair| match pair {
            (Cell::Id(id), Cell::Str(code)) => Some((id, code.to_string())),
            _ => None,
        })
        .collect())
}

/// `account.code.mapping`: one row per account and root company that has a code.
#[model(
    name = "account.code.mapping",
    description = "Mapping of account codes per company",
    table = "account_account_code",
    order = "company_id, code, id",
    company_access = direct(field = "company_id", branch = "ancestor", empty = "deny"),
    keys = [
        key(
            name = "_company_code_uniq",
            fields = ["company_id", "code"],
            message = "Account codes must be unique. You can't create accounts with these duplicate codes."
        ),
        key(
            name = "_account_company_uniq",
            fields = ["account_id", "company_id"],
            message = "An account has one code per root company."
        ),
    ],
)]
pub struct AccountCodeMapping {
    /// The account.
    #[field(
        many2one,
        comodel = "account.account",
        required = true,
        ondelete = "cascade",
        index = true,
        readonly = true,
        string = "Account"
    )]
    pub account_id: Option<RecId>,
    /// The root company the code belongs to.
    #[field(
        many2one,
        comodel = "res.company",
        required = true,
        ondelete = "restrict",
        index = true,
        string = "Company"
    )]
    pub company_id: Option<RecId>,
    /// The code.
    #[field(char, size = 64, required = true, string = "Code")]
    pub code: Option<Arc<str>>,
}

#[methods(AccountCodeMapping, addon = "account")]
impl AccountCodeMapping {
    #[constrains("code")]
    async fn _check_code(this: &RecordSet<AccountCodeMapping>, tx: &mut Transaction) -> Result<()> {
        for code in this
            .mapped(tx, AccountCodeMapping::CODE)
            .await?
            .into_iter()
            .flatten()
        {
            if !code_is_valid(&code) {
                return Err(Error::validation(format!(
                    "The account code can only contain alphanumeric characters, dots, and dashes. (account code: {code})"
                )));
            }
        }
        Ok(())
    }

    /// A code belongs to a root company: a branch shares its root's chart.
    #[constrains("company_id")]
    async fn _check_root_company(
        this: &RecordSet<AccountCodeMapping>,
        tx: &mut Transaction,
    ) -> Result<()> {
        let companies: Vec<i64> = this
            .mapped(tx, AccountCodeMapping::COMPANY_ID)
            .await?
            .into_iter()
            .flatten()
            .map(|c| c.get())
            .collect();
        let rows = tx
            .query(
                "SELECT name FROM res_company WHERE id = ANY($1::int8[]) \
                 AND parent_id IS NOT NULL LIMIT 1",
                &[&companies],
            )
            .await?;
        if !rows.is_empty() {
            return Err(Error::validation(
                "Account codes are kept for the root company of a company tree; write the code from the root company or one of its branches through the account.",
            ));
        }
        Ok(())
    }

    /// New codes create the root nodes they need.
    #[method(override)]
    async fn create(
        this: &RecordSet<AccountCodeMapping>,
        tx: &mut Transaction,
        vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let codes: Vec<String> = vals_list
            .iter()
            .filter_map(|v| match v.get("code") {
                Some(Cell::Str(code)) => Some(code.to_string()),
                _ => None,
            })
            .collect();
        let created = sup.call(tx, this.as_records(), (vals_list,)).await?;
        ensure_roots(tx, this.env(), &codes).await?;
        Ok(created)
    }

    /// A changed code creates the root nodes it needs. The account and the company of a row are
    /// set at create only: re-pointing a code to another account or tree is refused (the manager
    /// group holds write access for the code).
    ///
    /// # Errors
    /// `Validation` when `vals` gives `account_id` or `company_id` a value other than the stored one.
    #[method(override)]
    async fn write(
        this: &RecordSet<AccountCodeMapping>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        for field in ["account_id", "company_id"] {
            let Some(new) = vals.get(field) else { continue };
            for row in this.as_records().iter() {
                if row.cell(tx, field).await? != *new {
                    return Err(Error::validation(format!(
                        "The {field} of an account code row cannot be changed: write the code, or remove the row and add another."
                    )));
                }
            }
        }
        let code = match vals.get("code") {
            Some(Cell::Str(code)) => Some(code.to_string()),
            _ => None,
        };
        sup.call(tx, this.as_records(), (vals,)).await?;
        if let Some(code) = code {
            ensure_roots(tx, this.env(), &[code]).await?;
        }
        Ok(())
    }
}
