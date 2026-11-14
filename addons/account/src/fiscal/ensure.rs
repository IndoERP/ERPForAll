// SPDX-License-Identifier: MIT OR Apache-2.0
//! `ensure_periods` (R2): create the missing regular periods of a company tree.

use erp_sdk::prelude::*;
use erp_sdk::{BeginOptions, Isolation, Orm};

use super::calendar::{PeriodRange, add_months};
use super::settings::{FiscalCalendar, HORIZON_SETTING, LAST_DAY_SETTING, LAST_MONTH_SETTING};

/// The most periods one call creates per company: a safety bound of the call, not a setting
/// (one hundred years of months); an import backfills longer histories in several calls.
pub const MAX_PERIODS_PER_CALL: usize = 1200;

/// The companies `c` of a statement descends from or is (its `parent_path` ids), for joins on an
/// ancestor `a`; a path that is not `<id>/<id>/...` gives none, so a broken path never matches
/// every company as `LIKE a.parent_path || '%'` would for an empty one.
pub(crate) const ANCESTOR_OF_C: &str = "a.id = ANY(CASE WHEN c.parent_path ~ '^([0-9]+/)+$' \
     THEN string_to_array(rtrim(c.parent_path, '/'), '/')::int8[] END)";

fn db(e: impl std::fmt::Display) -> Error {
    Error::db(e.to_string())
}

/// The refusal of a company whose parent store is not filled or not well formed.
fn broken_path(company: i64, path: Option<&str>) -> Error {
    Error::internal(format!(
        "company {company} has no valid parent_path ({path:?}): its company tree cannot be read, \
         so no fiscal period of it is created or changed until the parent store is repaired"
    ))
}

/// The root company of `company_id` and every company of its tree (archived ones included), in
/// id order. The company's and the root's `parent_path` are checked first (`<id>/.../<id>/`, the
/// company's own id last, the root's path its own id).
///
/// # Errors
///
/// `Missing` when the company does not exist; `Internal` naming the company and its path when its,
/// its root's or a tree member's `parent_path` is NULL, empty or malformed (a malformed member
/// would be no company's descendant, so its periods would ignore the lock dates).
pub(crate) async fn company_tree(tx: &mut Transaction, company_id: i64) -> Result<(i64, Vec<i64>)> {
    let rows = tx
        .query(
            "SELECT c.parent_path, r.id::int8, r.parent_path, \
               (SELECT array_agg(b.id::int8 ORDER BY b.id) FROM res_company b \
                WHERE b.parent_path LIKE r.parent_path || '%'), \
               (SELECT b.id::int8 FROM res_company b WHERE b.parent_path LIKE r.parent_path || '%' \
                AND b.parent_path !~ '^([0-9]+/)+$' ORDER BY b.id LIMIT 1), \
               (SELECT b.parent_path FROM res_company b WHERE b.parent_path LIKE r.parent_path || '%' \
                AND b.parent_path !~ '^([0-9]+/)+$' ORDER BY b.id LIMIT 1) \
             FROM res_company c LEFT JOIN res_company r ON r.id = (CASE \
               WHEN c.parent_path ~ '^([0-9]+/)+$' THEN split_part(c.parent_path, '/', 1)::int8 END) \
             WHERE c.id = $1",
            &[&company_id],
        )
        .await?;
    let row = rows
        .first()
        .ok_or_else(|| Error::missing(format!("company {company_id} does not exist")))?;
    let path: Option<String> = row.try_get(0).map_err(db)?;
    let root: Option<i64> = row.try_get(1).map_err(db)?;
    let root_path: Option<String> = row.try_get(2).map_err(db)?;
    let tree: Option<Vec<i64>> = row.try_get(3).map_err(db)?;
    let well_formed = path
        .as_deref()
        .is_some_and(|p| p.ends_with(&format!("/{company_id}/")) || p == format!("{company_id}/"));
    let (Some(root), true) = (root, well_formed) else {
        return Err(broken_path(company_id, path.as_deref()));
    };
    if root_path.as_deref() != Some(format!("{root}/").as_str()) {
        return Err(broken_path(root, root_path.as_deref()));
    }
    let tree = tree.ok_or_else(|| broken_path(root, root_path.as_deref()))?;
    if let Some(member) = row.try_get::<_, Option<i64>>(4).map_err(db)? {
        let member_path: Option<String> = row.try_get(5).map_err(db)?;
        return Err(broken_path(member, member_path.as_deref()));
    }
    Ok((root, tree))
}

/// The refusal of a company the caller may not act for: the same answer whether it exists or not.
pub const NOT_ALLOWED: &str =
    "One of the selected companies does not exist or is not one of yours.";

/// Outside superuser mode, every company of `companies` must be one of the caller's active
/// companies ([`Transaction::companies`]) or a branch below one: a branch follows its parent's lock
/// dates in the reference, so whoever holds a
/// company holds its branches' periods.
///
/// # Errors
///
/// `Access` ([`NOT_ALLOWED`]) for a company outside them or one that does not exist; the errors of
/// [`Transaction::companies`].
pub(crate) async fn require_company_access(tx: &mut Transaction, companies: &[i64]) -> Result<()> {
    let env = tx.env();
    require_held(tx, &env, companies, false).await
}

/// Outside superuser mode of `env`, every company of `companies` must be one of the caller's
/// active companies or an ancestor of one: the companies whose records a `branch = "ancestor"`
/// company rule admits, as the journal's.
///
/// # Errors
///
/// `Access` ([`NOT_ALLOWED`]) for a company outside them or one that does not exist; the errors of
/// [`Transaction::companies`].
pub(crate) async fn require_company_or_ancestor_access(
    tx: &mut Transaction,
    env: &Env,
    companies: &[i64],
) -> Result<()> {
    require_held(tx, env, companies, true).await
}

/// `companies` are, each, an active company of `env`, or a branch below one (`ancestors` false)
/// or an ancestor of one (`ancestors` true).
async fn require_held(
    tx: &mut Transaction,
    env: &Env,
    companies: &[i64],
    ancestors: bool,
) -> Result<()> {
    if env.su() {
        return Ok(());
    }
    let allowed: Vec<i64> = tx.companies(env).await?.iter().map(|c| c.get()).collect();
    let mut wanted = companies.to_vec();
    wanted.sort_unstable();
    wanted.dedup();
    // `c` descends from `a`: the wanted company is `c` below a held one, or `a` above one
    let (wanted_side, held_side) = if ancestors { ("a", "c") } else { ("c", "a") };
    let rows = tx
        .query(
            &format!(
                "SELECT count(DISTINCT {wanted_side}.id)::int8 FROM res_company c JOIN res_company a \
                 ON {ANCESTOR_OF_C} WHERE {wanted_side}.id = ANY($1::int8[]) \
                 AND {held_side}.id = ANY($2::int8[])"
            ),
            &[&wanted, &allowed],
        )
        .await?;
    let held: i64 = rows
        .first()
        .ok_or_else(|| Error::internal("count returned no row"))?
        .try_get(0)
        .map_err(|e| Error::db(e.to_string()))?;
    if usize::try_from(held).ok() == Some(wanted.len()) {
        Ok(())
    } else {
        Err(Error::access(NOT_ALLOWED))
    }
}

/// Create the missing regular fiscal periods that contain a day of `date_from..=date_to`, for the
/// root company of `company_id` and every branch of it, and return how many rows were created.
///
/// Call it **outside** the unit of work that needs the periods, before its first statement, or
/// after that unit failed for a missing period and before it is restarted (R2): it
/// opens its own short transactions on `orm` with `caller` (the principal and context of the unit):
/// one that reads the calendar settings and one at `READ COMMITTED` that creates the rows, so a
/// period it commits is visible to a unit that starts afterwards.
/// Outside superuser mode the company must be one of the caller's or a branch of one;
/// in superuser mode `company_id` must come from a record or a request the
/// caller's access check already admitted, since nothing here checks it. A call that finds every
/// period present writes nothing. It is idempotent: two calls for the same new period both
/// succeed and create it once.
///
/// - The calendar comes from the root company's settings ([`FiscalCalendar`]): fiscal year end,
///   twelve monthly periods per year (R2) and the horizon [`HORIZON_SETTING`].
/// - Statuses are derived from the company lock dates, the latest over the company and its
///   ancestors as in the reference: `hard_locked`
///   on or before the hard lock date, `closed` on or before the fiscal-year lock date, `open`
///   otherwise; `stock_lock_status` is `stock_locked` on or before the later of the stock and the
///   hard lock date (amendment G6).
/// - The insert runs at `READ COMMITTED` after it updated the root company's
///   `fiscal_period_serial`: concurrent calls for the same tree wait for each other, and a
///   lock-date synchronisation ([`super::status::sync_lock_dates`]), which updates the same row,
///   either sees the new rows or is retried by its unit of work. The derived statuses therefore
///   never miss a committed lock date.
///
/// # Errors
///
/// - `Validation`: a reversed range, a range needing more than [`MAX_PERIODS_PER_CALL`] periods, a
///   `date_to` beyond the horizon (naming [`HORIZON_SETTING`]), invalid calendar settings, or a
///   computed period that overlaps an existing one with another range (the fiscal year end changed
///   after periods were created; naming both settings and the existing period);
/// - `Access` ([`NOT_ALLOWED`]): outside superuser mode, a company the caller does not hold or
///   one that does not exist;
/// - `Missing`: in superuser mode, the company does not exist;
/// - any database error.
pub async fn ensure_periods(
    orm: &Orm,
    caller: BeginOptions,
    company_id: i64,
    date_from: Date,
    date_to: Date,
) -> Result<u64> {
    ensure(orm, caller, None, company_id, date_from, date_to).await
}

/// [`ensure_periods`] with a calendar the caller already read with [`FiscalCalendar::read`] for
/// the company's root (a batch import that ensures many ranges reads it once).
///
/// # Errors
///
/// As [`ensure_periods`].
pub async fn ensure_periods_with(
    orm: &Orm,
    caller: BeginOptions,
    calendar: &FiscalCalendar,
    company_id: i64,
    date_from: Date,
    date_to: Date,
) -> Result<u64> {
    ensure(orm, caller, Some(calendar), company_id, date_from, date_to).await
}

async fn ensure(
    orm: &Orm,
    caller: BeginOptions,
    calendar: Option<&FiscalCalendar>,
    company_id: i64,
    date_from: Date,
    date_to: Date,
) -> Result<u64> {
    let calendar = match calendar {
        Some(calendar) => *calendar,
        // the settings are read through the record API, which needs a REPEATABLE READ transaction
        None => {
            let mut tx = orm
                .begin_opts(caller.clone().isolation(Isolation::RepeatableRead))
                .await?;
            let read = async {
                require_company_access(&mut tx, &[company_id]).await?;
                let (root, _) = company_tree(&mut tx, company_id).await?;
                FiscalCalendar::read(&mut tx, root).await
            }
            .await;
            let rollback = tx.rollback().await;
            match read {
                Ok(calendar) => {
                    rollback?;
                    calendar
                }
                Err(error) => return Err(keep_first(error, rollback)),
            }
        }
    };
    let mut tx = orm
        .begin_opts(caller.isolation(Isolation::ReadCommitted))
        .await?;
    let result = async {
        require_company_access(&mut tx, &[company_id]).await?;
        let (root, tree) = company_tree(&mut tx, company_id).await?;
        let periods = periods_in_horizon(&calendar, date_from, date_to)?;
        insert(&mut tx, root, &tree, &periods).await
    }
    .await;
    match result {
        Ok(created) => {
            tx.commit().await?;
            Ok(created)
        }
        Err(error) => Err(keep_first(error, tx.rollback().await)),
    }
}

/// `error`, naming a failed rollback after it rather than replacing it.
fn keep_first(error: Error, rollback: Result<()>) -> Error {
    match rollback {
        Ok(()) => error,
        Err(rollback) => error.and_also(&rollback),
    }
}

fn periods_in_horizon(
    calendar: &FiscalCalendar,
    date_from: Date,
    date_to: Date,
) -> Result<Vec<PeriodRange>> {
    let horizon = add_months(Date::today(), calendar.horizon_months())?;
    if date_to > horizon {
        return Err(Error::validation(format!(
            "fiscal periods can be created up to {horizon}, {} months ahead (setting \
             {HORIZON_SETTING}); {date_to} is later",
            calendar.horizon_months()
        )));
    }
    calendar
        .year_end()
        .periods_between(date_from, date_to, MAX_PERIODS_PER_CALL)
}

/// The `(company, from, to)` of `$1` × `($2, $3)` without a regular period of exactly that range.
const ABSENT: &str = "FROM unnest($1::int8[]) AS c CROSS JOIN unnest($2::date[], $3::date[]) AS p(f, t) \
     WHERE NOT EXISTS (SELECT 1 FROM account_fiscal_period e WHERE e.company_id = c \
       AND e.kind = 'regular' AND e.date_from = p.f AND e.date_to = p.t)";

async fn insert(
    tx: &mut Transaction,
    root: i64,
    tree: &[i64],
    periods: &[PeriodRange],
) -> Result<u64> {
    let froms: Vec<Date> = periods.iter().map(|p| p.date_from).collect();
    let tos: Vec<Date> = periods.iter().map(|p| p.date_to).collect();
    // nothing to create: no write, so a call before every unit of work never queues on the root
    // company row (the counter below moves only with calls that create)
    let any = tx
        .query(
            &format!("SELECT 1 {ABSENT} LIMIT 1"),
            &[&tree, &froms, &tos],
        )
        .await?;
    if any.is_empty() {
        return Ok(0);
    }
    let serial = tx
        .execute(
            "UPDATE res_company SET fiscal_period_serial = COALESCE(fiscal_period_serial, 0) + 1 WHERE id = $1",
            &[&root],
        )
        .await?;
    if serial != 1 {
        return Err(Error::missing(format!("company {root} does not exist")));
    }
    let years: Vec<i32> = periods.iter().map(PeriodRange::year).collect();
    let numbers: Vec<i32> = periods
        .iter()
        .map(|p| p.number as i32) // a period number is 1..=16, exact
        .collect();
    let names: Vec<String> = periods.iter().map(PeriodRange::name).collect();
    let created = tx
        .execute(
            &format!("WITH c AS ( \
               SELECT c.id, \
                 (SELECT max(a.fiscalyear_lock_date) FROM res_company a \
                  WHERE {ANCESTOR_OF_C}) AS fiscal, \
                 (SELECT max(a.hard_lock_date) FROM res_company a \
                  WHERE {ANCESTOR_OF_C}) AS hard, \
                 (SELECT max(a.stock_lock_date) FROM res_company a \
                  WHERE {ANCESTOR_OF_C}) AS stock \
               FROM res_company c WHERE c.id = ANY($1::int8[])), \
             p AS (SELECT * FROM unnest($2::int4[], $3::int4[], $4::date[], $5::date[], $6::text[]) \
                   AS p(fiscal_year, number, date_from, date_to, name)) \
             INSERT INTO account_fiscal_period (company_id, name, fiscal_year, kind, number, \
               date_from, date_to, status, stock_lock_status, closed_at, hard_locked_at, \
               stock_lock_generation, hard_lock_generation) \
             SELECT c.id, p.name, p.fiscal_year, 'regular', p.number, p.date_from, p.date_to, \
               CASE WHEN p.date_to <= c.hard THEN 'hard_locked' \
                    WHEN p.date_to <= c.fiscal THEN 'closed' ELSE 'open' END, \
               CASE WHEN p.date_to <= GREATEST(c.stock, c.hard) THEN 'stock_locked' \
                    ELSE 'open' END, \
               CASE WHEN p.date_to <= GREATEST(c.fiscal, c.hard) THEN (now() AT TIME ZONE 'UTC') END, \
               CASE WHEN p.date_to <= c.hard THEN (now() AT TIME ZONE 'UTC') END, 0, 0 \
             FROM c CROSS JOIN p ORDER BY c.id, p.date_from \
             ON CONFLICT DO NOTHING"),
            &[&tree, &years, &numbers, &froms, &tos, &names],
        )
        .await?;
    // ON CONFLICT DO NOTHING also skips a computed period that overlaps another range: refuse it
    let missing = tx
        .query(
            &format!("SELECT c::int8, p.f, p.t {ABSENT} ORDER BY 1, 2 LIMIT 1"),
            &[&tree, &froms, &tos],
        )
        .await?;
    if let Some(row) = missing.first() {
        let company: i64 = row.try_get(0).map_err(|e| Error::db(e.to_string()))?;
        let from: Date = row.try_get(1).map_err(|e| Error::db(e.to_string()))?;
        let to: Date = row.try_get(2).map_err(|e| Error::db(e.to_string()))?;
        let wanted = periods
            .iter()
            .find(|p| p.date_from == from && p.date_to == to)
            .ok_or_else(|| Error::internal(format!("period {from}..{to} was not computed")))?;
        // the conflict is a range overlap or the period key (fiscal year, kind, number)
        let existing = tx
            .query(
                "SELECT name, date_from, date_to FROM account_fiscal_period \
                 WHERE company_id = $1 AND kind = 'regular' \
                 AND (daterange(date_from, date_to, '[]') && daterange($2, $3, '[]') \
                   OR (fiscal_year = $4 AND number = $5)) \
                 ORDER BY date_from LIMIT 1",
                &[
                    &company,
                    &from,
                    &to,
                    &wanted.year(),
                    &(wanted.number as i32),
                ],
            )
            .await?;
        let Some(r) = existing.first() else {
            return Err(Error::internal(format!(
                "the fiscal period {from}..{to} of company {company} was neither created nor \
                 refused by a conflicting period"
            )));
        };
        let name: String = r.try_get(0).map_err(db)?;
        let f: Date = r.try_get(1).map_err(db)?;
        let t: Date = r.try_get(2).map_err(db)?;
        return Err(Error::validation(format!(
            "the fiscal period {from}..{to} of company {company} cannot be created: it conflicts \
             with the existing period {name} ({f}..{t}). The fiscal year end ({LAST_DAY_SETTING}, \
             {LAST_MONTH_SETTING}) no longer matches the periods already created."
        )));
    }
    Ok(created)
}
