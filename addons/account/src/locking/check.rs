// SPDX-License-Identifier: MIT OR Apache-2.0
use super::policy::EffectiveDates;
use super::{LockCheck, LockRefusal, LockVerdict};
use crate::fiscal::ensure::require_company_access;
use crate::fiscal::{PeriodVerdict, WriteClass, period_admission};
use erp_sdk::prelude::*;
use std::collections::BTreeMap;

/// Check a batch against company/ancestor dates and the fiscal periods, in input order.
/// Company scope is checked before policy details are read. Empty input reads nothing.
/// Call once per batch, in the transaction performing the write; entry hooks must separately
/// authorize their business records. No exception or context bypass is supported.
///
/// # Errors
/// `Access` for unauthorized/invalid companies; `Internal` for broken hierarchy; database errors.
/// Per-date locks and missing periods are typed verdicts, not silently skipped.
pub async fn check_lock_dates(
    tx: &mut Transaction,
    checks: &[LockCheck],
) -> Result<Vec<LockVerdict>> {
    if checks.is_empty() {
        return Ok(Vec::new());
    }
    let mut companies: Vec<i64> = checks.iter().map(|c| c.company).collect();
    companies.sort_unstable();
    companies.dedup();
    require_company_access(tx, &companies).await?;
    tx.flush().await?;
    // Validate every parent edge as well as the path shape. A syntactically valid truncated
    // branch path must not silently discard an ancestor's restrictions.
    let rows = tx
        .query(
            "WITH c AS (SELECT id,parent_path,CASE WHEN parent_path ~ '^([0-9]+/)+$' \
        THEN string_to_array(rtrim(parent_path,'/'),'/')::int8[] END AS path FROM res_company \
        WHERE id=ANY($1::int8[])) \
        SELECT c.id::int8,c.parent_path,count(a.id)::int8, \
        max(a.fiscalyear_lock_date),max(a.sale_lock_date),max(a.purchase_lock_date), \
        max(a.tax_lock_date),max(a.hard_lock_date), \
        bool_and(a.parent_id IS NOT DISTINCT FROM c.path[(p.ord-1)::int4]) \
        FROM c LEFT JOIN LATERAL unnest(c.path) WITH ORDINALITY AS p(id,ord) ON true \
        LEFT JOIN res_company a ON a.id=p.id GROUP BY c.id,c.parent_path,c.path ORDER BY c.id",
            &[&companies],
        )
        .await?;
    let mut states = BTreeMap::new();
    for row in rows {
        let id: i64 = row.try_get(0).map_err(db)?;
        let path: Option<String> = row.try_get(1).map_err(db)?;
        let ancestors: i64 = row.try_get(2).map_err(db)?;
        let valid = path
            .as_deref()
            .is_some_and(|p| p.ends_with(&format!("/{id}/")) || p == format!("{id}/"))
            && path.as_deref().is_some_and(|p| {
                i64::try_from(p.split('/').filter(|part| !part.is_empty()).count()).ok()
                    == Some(ancestors)
            });
        let edges: Option<bool> = row.try_get(8).map_err(db)?;
        if !valid || edges != Some(true) {
            return Err(Error::internal(format!(
                "Company {id} has an invalid lock-policy hierarchy."
            )));
        }
        let mut dates = EffectiveDates::default();
        for (i, date) in dates.0.iter_mut().enumerate() {
            *date = row.try_get(i + 3).map_err(db)?;
        }
        states.insert(id, dates);
    }
    let period_inputs: Vec<_> = checks.iter().map(|c| (c.company, c.date)).collect();
    let periods = period_admission(tx, WriteClass::Accounting, &period_inputs).await?;
    checks
        .iter()
        .zip(periods)
        .map(|(input, period)| {
            let state = states
                .get(&input.company)
                .ok_or_else(|| Error::access(crate::fiscal::ensure::NOT_ALLOWED))?;
            // An in-flight hard lock has not published its company date; it still binds every check.
            if matches!(
                period,
                PeriodVerdict::HardLock { .. } | PeriodVerdict::NoPeriod { .. }
            ) {
                return Ok(LockVerdict::Refused(LockRefusal::Period(period)));
            }
            let scalar = state.check(input);
            if matches!(scalar, LockVerdict::Allowed) && !period.is_open() {
                Ok(LockVerdict::Refused(LockRefusal::Period(period)))
            } else {
                Ok(scalar)
            }
        })
        .collect()
}
fn db(error: impl std::fmt::Display) -> Error {
    Error::db(error.to_string())
}
