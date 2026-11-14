// SPDX-License-Identifier: MIT OR Apache-2.0
use super::{
    LockField,
    policy::{FIELDS, valid_date},
};
use crate::fiscal::ensure::{NOT_ALLOWED, require_company_access};
use crate::fiscal::status::{MANAGER_GROUP, sync_lock_dates};
use erp_addon_base::ResCompany;
use erp_sdk::prelude::*;
use erp_sdk::{
    Operation,
    audit::{AccountLockDateChanged, AuthEvent},
};

const ACTIVATION: &str = "Hard lock activation is unavailable: a later release must verify the maintenance drain, entry checks and activation audit first.";
/// A company-date patch. Unmentioned fields stay unchanged; `None` explicitly clears a soft date.
#[derive(Debug, Clone, Default)]
pub struct LockPatch {
    dates: [Option<Option<Date>>; 5],
}
impl LockPatch {
    /// Empty patch.
    pub fn new() -> Self {
        Self::default()
    }
    /// Set or clear one field. Repeating a field replaces the earlier proposal.
    #[must_use]
    pub fn set(mut self, field: LockField, date: Option<Date>) -> Self {
        self.dates[index(field)] = Some(date);
        self
    }
    fn empty(&self) -> bool {
        self.dates.iter().all(Option::is_none)
    }
}
fn index(field: LockField) -> usize {
    match field {
        LockField::FiscalYear => 0,
        LockField::Sale => 1,
        LockField::Purchase => 2,
        LockField::Tax => 3,
        LockField::Hard => 4,
    }
}
fn db(error: impl std::fmt::Display) -> Error {
    Error::db(error.to_string())
}
async fn authorize(tx: &mut Transaction, company: i64) -> Result<()> {
    let env = tx.env();
    if !env.su() && !tx.has_group(&env, MANAGER_GROUP).await? {
        return Err(Error::access(
            "Only accounting administrators can change lock dates.",
        ));
    }
    require_company_access(tx, &[company]).await
}
struct Change {
    field: LockField,
    old: Option<Date>,
    new: Option<Date>,
}
async fn prepare(tx: &mut Transaction, company: i64, patch: &LockPatch) -> Result<Vec<Change>> {
    authorize(tx, company).await?;
    tx.flush().await?;
    let rows=tx.query("SELECT fiscalyear_lock_date,sale_lock_date,purchase_lock_date,tax_lock_date,hard_lock_date \
        FROM res_company WHERE id=$1 FOR UPDATE",&[&company]).await?;
    let row = rows.first().ok_or_else(|| Error::access(NOT_ALLOWED))?;
    let today = tx.env().context_today()?;
    let mut changes = Vec::new();
    for (i, field) in FIELDS.iter().copied().enumerate() {
        let Some(new) = patch.dates[i] else {
            continue;
        };
        let old: Option<Date> = row.try_get(i).map_err(db)?;
        if new == old {
            continue;
        }
        if field == LockField::Hard {
            if old.is_some() && (new.is_none() || new < old) {
                return Err(Error::validation(
                    "hard_lock_date cannot be removed or decreased.",
                ));
            }
            return Err(Error::user(ACTIVATION));
        }
        if let Some(date) = new {
            if !valid_date(date) {
                return Err(Error::validation(format!(
                    "{} is outside PostgreSQL's date range.",
                    field.as_str()
                )));
            }
            if date > today {
                return Err(Error::validation(format!(
                    "{} cannot be set in the future.",
                    field.as_str()
                )));
            }
        }
        changes.push(Change { field, old, new });
    }
    // Preflight the real audit store before the first change; the enclosing savepoint drops
    // buffered evidence if any later validation/write/synchronization fails.
    for change in &changes {
        tx.audit_auth(AuthEvent::AccountLockDateChanged(
            AccountLockDateChanged::new(company, change.field, change.old, change.new)?,
        ))?;
    }
    Ok(changes)
}
async fn finish(tx: &mut Transaction, company: i64, changes: &[Change]) -> Result<()> {
    tx.invalidate_model("res.company", None).await?;
    if changes.iter().any(|c| c.field == LockField::FiscalYear) {
        sync_lock_dates(tx, company).await?;
    }
    Ok(())
}
/// Change a company's soft lock dates with authorization, period synchronization and atomic H4 evidence.
/// Hard activation and exceptions remain unavailable; owning fiscal status operations are separate.
/// A savepoint makes even a caught validation error leave policy/evidence unchanged.
/// # Errors
/// `Access` outside company/manager scope; `Validation` for invalid/future dates or hard lowering;
/// `UserError` for unavailable hard activation; audit/database/synchronization errors.
pub async fn change_lock_dates(
    tx: &mut Transaction,
    company: i64,
    patch: &LockPatch,
) -> Result<()> {
    tx.savepoint(async |tx| {
        let changes = prepare(tx, company, patch).await?;
        let uid = tx.env().uid();
        for change in &changes {
            // The identifier comes only from the closed enum, never from request input; the row
            // is stamped like an ORM write.
            let sql = format!(
                "UPDATE res_company SET {}=$2, write_uid=$3, write_date=(now() AT TIME ZONE 'UTC') \
                 WHERE id=$1",
                change.field.as_str()
            );
            let changed = tx.execute(&sql, &[&company, &change.new, &uid]).await?;
            if changed != 1 {
                return Err(Error::internal(
                    "Lock-date change did not update its company.",
                ));
            }
        }
        finish(tx, company, &changes).await
    })
    .await
}
/// The columns only the lock policy, the stock lock and fiscal period operations write; a new
/// company starts with all of them empty.
const LOCK_COLUMNS: [&str; 7] = [
    "fiscalyear_lock_date",
    "sale_lock_date",
    "purchase_lock_date",
    "tax_lock_date",
    "hard_lock_date",
    "stock_lock_date",
    "fiscal_period_serial",
];
fn patch_from(vals: &RawVals) -> Result<LockPatch> {
    crate::tax::reject_effective_company_defaults(vals)?;
    for field in ["stock_lock_date", "fiscal_period_serial"] {
        if vals.contains(field) {
            return Err(Error::validation(if field == "stock_lock_date" {
                "stock_lock_date changes only through its stock lock operation."
            } else {
                "fiscal_period_serial changes only through fiscal period operations."
            }));
        }
    }
    let mut patch = LockPatch::new();
    for field in FIELDS {
        if let Some(cell) = vals.get(field.as_str()) {
            let date = match cell {
                Cell::Null | Cell::Bool(false) => None,
                Cell::Date(date) => Some(*date),
                _ => {
                    return Err(Error::validation(format!(
                        "{} must be a date or false.",
                        field.as_str()
                    )));
                }
            };
            patch = patch.set(field, date);
        }
    }
    Ok(patch)
}
#[extend(ResCompany, addon = "account")]
struct AccountLockPolicy {}
#[methods(ResCompany, addon = "account")]
impl AccountLockPolicy {
    #[method(override)]
    async fn write(
        this: &RecordSet<ResCompany>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        this.as_records().check_access(tx, Operation::Write).await?;
        erp_sdk::__private::base_check_write_fields(tx, this.as_records(), &vals)?;
        crate::fpos::reject_fence_value(&vals)?;
        let configuration =
            crate::company::validation::prepare_write(this.as_records(), tx, &vals).await?;
        let patch = patch_from(&vals)?;
        if patch.empty() {
            if configuration {
                // Roll back even when a native caller catches a later company/field constraint.
                return tx
                    .savepoint(async move |tx| sup.call(tx, this.as_records(), (vals,)).await)
                    .await;
            }
            return sup.call(tx, this.as_records(), (vals,)).await;
        }
        let ids = this.ids();
        if ids.is_empty() {
            return Ok(());
        }
        if ids.len() != 1 {
            return Err(Error::validation(
                "Lock dates change on one company at a time.",
            ));
        }
        tx.savepoint(async |tx| {
            let changes = prepare(tx, ids[0], &patch).await?;
            sup.call(tx, this.as_records(), (vals,)).await?;
            tx.flush().await?;
            finish(tx, ids[0], &changes).await
        })
        .await
    }
    #[method(override)]
    async fn create(
        this: &RecordSet<ResCompany>,
        tx: &mut Transaction,
        mut vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        this.as_records()
            .check_access(tx, Operation::Create)
            .await?;
        // the base create fills a missing column from a `default_<field>` key after this check
        let context = this.env().context();
        if let Some(field) = LOCK_COLUMNS
            .iter()
            .find(|f| context.contains(&format!("default_{f}")))
        {
            return Err(Error::validation(format!(
                "A new company cannot take {field} from a default in the context; create it \
                 without lock dates, then change them through the lock policy."
            )));
        }
        if context.contains("default_fpos_revision") {
            return Err(Error::validation(crate::fpos::FENCE_REFUSED));
        }
        for vals in &mut vals_list {
            erp_sdk::__private::base_check_write_fields(tx, this.as_records(), vals)?;
            crate::fpos::reject_fence_value(vals)?;
            crate::company::validation::prepare_create(this.as_records(), tx, vals).await?;
            let patch = patch_from(vals)?;
            if patch.dates.iter().any(|d| matches!(d, Some(Some(_)))) {
                return Err(Error::validation(
                    "Create the company without lock dates, then change them through the lock policy.",
                ));
            }
            // a new company starts unlocked: no stored `ir.default` can supply these columns
            for field in LOCK_COLUMNS {
                vals.insert(field.into(), Cell::Null);
            }
        }
        tx.savepoint(async move |tx| sup.call(tx, this.as_records(), (vals_list,)).await)
            .await
    }
}
