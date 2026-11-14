// SPDX-License-Identifier: MIT OR Apache-2.0
//! Validate configuration before mutation; preserve base company ACLs and lock-policy overrides.
use crate::ledger::code::code_is_valid;
use erp_sdk::{ErrorKind, Operation, prelude::*};

const PREFIXES: [&str; 3] = [
    "bank_account_code_prefix",
    "cash_account_code_prefix",
    "transfer_account_code_prefix",
];
const ACCOUNT_FIELDS: [&str; 11] = [
    "transfer_account_id",
    "default_cash_difference_income_account_id",
    "default_cash_difference_expense_account_id",
    "account_journal_suspense_account_id",
    "account_journal_early_pay_discount_gain_account_id",
    "account_journal_early_pay_discount_loss_account_id",
    "income_currency_exchange_account_id",
    "expense_currency_exchange_account_id",
    "account_default_pos_receivable_account_id",
    "expense_accrual_account_id",
    "revenue_accrual_account_id",
];
const JOURNAL_FIELDS: [&str; 2] = [
    "currency_exchange_journal_id",
    "automatic_entry_default_journal_id",
];
const OPTIONS: [&str; 4] = [
    "tax_calculation_rounding_method",
    "anglo_saxon_accounting",
    "expects_chart_of_accounts",
    "chart_template",
];
const CHART_UNAVAILABLE: &str = "Chart activation is unavailable until a later release.";

fn touches_configuration(vals: &RawVals) -> bool {
    PREFIXES
        .into_iter()
        .chain(ACCOUNT_FIELDS)
        .chain(JOURNAL_FIELDS)
        .chain(OPTIONS)
        .any(|field| vals.contains(field))
}
fn empty(cell: &Cell) -> bool {
    matches!(cell, Cell::Null | Cell::Bool(false))
        || matches!(cell, Cell::Str(value) if value.is_empty())
}
fn validate_scalars(vals: &RawVals) -> Result<()> {
    for field in PREFIXES {
        if let Some(cell) = vals.get(field) {
            // Empty is explicitly unconfigured. The hard bound is the existing account-code
            // storage width, not an allocation/generation budget or a hidden business choice.
            let valid = empty(cell)
                || matches!(cell, Cell::Str(value) if value.len() <= 64 && code_is_valid(value));
            if !valid {
                return Err(Error::validation(format!(
                    "{field} must be empty or contain only ASCII letters, digits, dots and dashes, up to 64 characters."
                )));
            }
        }
    }
    if let Some(cell) = vals.get("chart_template")
        && !empty(cell)
    {
        return Err(Error::user(CHART_UNAVAILABLE));
    }
    Ok(())
}
async fn authorize_company(records: &Records, tx: &mut Transaction) -> Result<()> {
    if records.env().su() {
        return Ok(());
    }
    let allowed = tx.companies(records.env()).await?;
    if records
        .ids()
        .iter()
        .any(|id| !allowed.iter().any(|company| company.get() == *id))
    {
        return Err(Error::access("Choose a company available to your account."));
    }
    Ok(())
}
async fn targets_readable(records: &Records, tx: &mut Transaction, vals: &RawVals) -> Result<()> {
    for (fields, model) in [
        (ACCOUNT_FIELDS.as_slice(), "account.account"),
        (JOURNAL_FIELDS.as_slice(), "account.journal"),
    ] {
        for field in fields {
            let id = match vals.get(field) {
                Some(Cell::Id(id)) => *id,
                Some(Cell::Int(id)) => i64::from(*id),
                // Absent/empty links need no target. Other malformed values are rejected by
                // the ORM field converter, without a lookup or a substituted value.
                _ => continue,
            };
            let target = records.env().records(model)?.browse([id])?;
            let readable = match target.check_access(tx, Operation::Read).await {
                Ok(()) => target.exists(tx).await?.len() == 1,
                Err(error) if error.kind() == ErrorKind::Access => false,
                Err(error) => return Err(error),
            };
            if !readable {
                return Err(Error::access(format!(
                    "The account or journal selected for {field} is unavailable."
                )));
            }
        }
    }
    Ok(())
}
/// Called by account's existing company write override after its ACL/field checks.
pub(crate) async fn prepare_write(
    records: &Records,
    tx: &mut Transaction,
    vals: &RawVals,
) -> Result<bool> {
    if !touches_configuration(vals) {
        return Ok(false);
    }
    authorize_company(records, tx).await?;
    validate_scalars(vals)?;
    if vals.contains("chart_template") {
        for company in records.iter() {
            if !empty(&company.cell(tx, "chart_template").await?) {
                return Err(Error::user(CHART_UNAVAILABLE));
            }
        }
    }
    targets_readable(records, tx, vals).await?;
    Ok(true)
}
/// Called by account's existing company create override before the first mutation.
pub(crate) async fn prepare_create(
    records: &Records,
    tx: &mut Transaction,
    vals: &RawVals,
) -> Result<()> {
    validate_scalars(vals)?;
    if touches_configuration(vals) {
        targets_readable(records, tx, vals).await?;
    }
    Ok(())
}

/// Reverse side of the company fields: membership edits cannot strand configured references.
pub(crate) async fn check_accounts(tx: &mut Transaction, ids: &[i64]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    tx.flush().await?;
    // Identifiers come only from this module's closed field list; record ids remain parameters.
    let columns = ACCOUNT_FIELDS
        .iter()
        .map(|field| format!("(c.{field})"))
        .collect::<Vec<_>>()
        .join(",");
    let rows=tx.query(&format!("SELECT 1 FROM res_company c CROSS JOIN LATERAL (VALUES {columns}) AS configured(target) \
        WHERE configured.target = ANY($1::int8[]) AND NOT EXISTS (SELECT 1 FROM account_account_res_company_rel r \
        JOIN res_company owner ON owner.id=r.res_company_id WHERE r.account_account_id=configured.target \
        AND c.parent_path LIKE owner.parent_path || '%') LIMIT 1"),&[&ids]).await?;
    if rows.is_empty() {
        Ok(())
    } else {
        Err(Error::validation(
            "An account used by company accounting configuration must remain available to that company.",
        ))
    }
}
/// Reverse side of the journal fields, with a generic refusal that discloses no hidden company.
pub(crate) async fn check_journals(tx: &mut Transaction, ids: &[i64]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    tx.flush().await?;
    let columns = JOURNAL_FIELDS
        .iter()
        .map(|field| format!("(c.{field})"))
        .collect::<Vec<_>>()
        .join(",");
    let rows=tx.query(&format!("SELECT 1 FROM res_company c CROSS JOIN LATERAL (VALUES {columns}) AS configured(target) \
        WHERE configured.target = ANY($1::int8[]) AND NOT EXISTS (SELECT 1 FROM account_journal j \
        JOIN res_company owner ON owner.id=j.company_id WHERE j.id=configured.target \
        AND c.parent_path LIKE owner.parent_path || '%') LIMIT 1"),&[&ids]).await?;
    if rows.is_empty() {
        Ok(())
    } else {
        Err(Error::validation(
            "A journal used by company accounting configuration must remain available to that company.",
        ))
    }
}
