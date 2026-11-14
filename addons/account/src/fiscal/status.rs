// SPDX-License-Identifier: MIT OR Apache-2.0
//! The status machine of the period rows (D7, D8;
//! D3 R1d, R2).
//!
//! These functions hold only what the period table itself decides: which transitions exist,
//! which states refuse them, the run generations and the period set. What the lock operations
//! add around them belongs to their tasks:
//!
//! - **** (accounting): the lock-date setters and their validations (
//!   drafts, unreconciled statement lines, a hard lock date that may not decrease), the lock
//!   wizard and exceptions, `SET LOCAL lock_timeout`, the drain between T1 and T2 of the hard lock
//!   and the counts T2 checks;
//! - **** (stock lock through `stock_account`): the drain, the readiness counts and the
//!   replay-job check of the stock lock's T2, the "stuck" display setting (point 5);
//! - ****: the `close_period` precondition of the hard lock for a
//!   company on the valuation bridge (amendment G4) and the close-horizon refusal of D7 (G7).
//!
//! Every operation requires the accounting manager group (the reference's lock wizard is for
//! accounting managers only),
//! outside superuser mode, a company the caller holds or a branch of one.
//! It takes the exclusive advisory lock of each period it may change in id order, so it waits for
//! writers holding the shared lock of [`super::period_admission`]; the wait is bounded by the
//! transaction's `lock_timeout` (`OrmConfig::timeouts`, or `BeginOptions::timeouts` for the unit;
//! the lock operations' own `SET LOCAL lock_timeout` is's).
//!
//! Lock order, the same on every path so none waits for another in a cycle: the target company
//! row (a setter's write, or `FOR UPDATE` in a lock run's second step), then the root company row
//! (`fiscal_period_serial`), then the period advisory locks in id order.

use erp_sdk::prelude::*;

use super::PERIOD_LOCK_CLASS;
use super::ensure::{
    ANCESTOR_OF_C, MAX_PERIODS_PER_CALL, NOT_ALLOWED, company_tree, require_company_access,
};

/// The row lock a lock run's second step takes on a company before its advisory locks.
const ROW_LOCK: &str = "SELECT 1 FROM res_company WHERE id = $1 FOR UPDATE";

/// The group that may change period statuses.
pub const MANAGER_GROUP: &str = "account.group_account_manager";

fn db(e: impl std::fmt::Display) -> Error {
    Error::db(e.to_string())
}

async fn require_manager(tx: &mut Transaction) -> Result<()> {
    let env = tx.env().clone();
    if env.su() || tx.has_group(&env, MANAGER_GROUP).await? {
        return Ok(());
    }
    Err(Error::access(
        "Only accounting administrators can lock or reopen fiscal periods.",
    ))
}

/// [`MANAGER_GROUP`], then the company scope of [`require_company_access`].
async fn authorize(tx: &mut Transaction, company_id: i64) -> Result<()> {
    require_manager(tx).await?;
    require_company_access(tx, &[company_id]).await
}

/// Bring the period rows of `company_id` and its branches in line with the company lock dates
/// after a setter wrote them (R2: "their setters update period rows"). Call it in the
/// setter's transaction, after the company write; it flushes the transaction's pending writes
/// first, so a date written through the record API is the one it reads.
///
/// It first takes the exclusive advisory lock of every period it may change, in id order, before
/// its refusal checks and its updates: a writer admitted for a period it closes
/// commits first, and a writer that asks after this transaction commits sees the new status. The
/// wait is bounded by the transaction's `lock_timeout`.
///
/// The effective date of each company is the latest over itself and its ancestors;
/// the effective stock lock date
/// is the later of the stock and the hard lock date.
///
/// - Raising the fiscal-year lock date closes the open periods ending on or before it; lowering
///   it reopens the closed periods ending after it, all in one statement (D7).
/// - Lowering the stock lock date reopens the stock status of the stock-locked periods ending
///   after it (D7: the stock lock date is lowered first).
///
/// # Errors
///
/// `Access` without [`MANAGER_GROUP`], or ([`NOT_ALLOWED`]) for a company the caller does not hold
/// or one that does not exist; `Validation` when it would change more than
/// [`MAX_PERIODS_PER_CALL`] periods per company of the tree; `Validation`, before anything is written, naming the first
/// period that refuses: lowering the fiscal-year lock date across a `hard_locking` or `hard_locked`
/// period, or across a `stock_locked` period (lower the stock lock date first); a hard lock date
/// that covers a period not `hard_locked` or no longer covers a `hard_locked` one, and a stock lock
/// date that covers a period not `stock_locked` (those dates move only with their lock operations).
/// A concurrent [`super::ensure_periods`] for the same tree makes the statement fail with a
/// serialization failure, which `Orm::run` retries.
pub async fn sync_lock_dates(tx: &mut Transaction, company_id: i64) -> Result<SyncedPeriods> {
    tx.flush().await?;
    authorize(tx, company_id).await?;
    let (root, tree) = company_tree(tx, company_id).await?;
    bump_serial(tx, root).await?;
    // the target's path was checked by company_tree; the ancestors are read from each path's ids
    let scope = format!(
        "WITH c AS ( \
           SELECT c.id, \
             (SELECT max(a.fiscalyear_lock_date) FROM res_company a WHERE {ANCESTOR_OF_C}) AS fiscal, \
             (SELECT max(a.hard_lock_date) FROM res_company a WHERE {ANCESTOR_OF_C}) AS hard, \
             (SELECT GREATEST(max(a.stock_lock_date), max(a.hard_lock_date)) FROM res_company a \
              WHERE {ANCESTOR_OF_C}) AS stock \
           FROM res_company c JOIN res_company t ON c.parent_path LIKE t.parent_path || '%' \
           WHERE t.id = $1) "
    );
    let changing = tx
        .query(
            &format!(
                "{scope}SELECT p.id::int8 FROM account_fiscal_period p JOIN c ON c.id = p.company_id \
                 WHERE (p.status = 'open' AND p.date_to <= c.fiscal) \
                 OR (p.status = 'closed' AND (c.fiscal IS NULL OR p.date_to > c.fiscal)) \
                 OR (p.stock_lock_status = 'stock_locked' AND (c.stock IS NULL OR p.date_to > c.stock)) \
                 ORDER BY p.id"
            ),
            &[&company_id],
        )
        .await?
        .iter()
        .map(|r| r.try_get(0).map_err(db))
        .collect::<Result<Vec<i64>>>()?;
    let bound = MAX_PERIODS_PER_CALL.saturating_mul(tree.len());
    if changing.len() > bound {
        return Err(Error::validation(format!(
            "the lock dates would change {} fiscal periods at once; at most {MAX_PERIODS_PER_CALL} \
             per company ({bound} for this tree of {} companies) change in one call: move the lock \
             date in smaller steps",
            changing.len(),
            tree.len()
        )));
    }
    lock_keys(tx, &changing).await?;
    let refusals: [(&str, &str); 5] = [
        (
            "p.status IN ('hard_locking', 'hard_locked') AND (c.fiscal IS NULL OR p.date_to > c.fiscal)",
            "the fiscal-year lock date cannot be lowered across the hard-locked period",
        ),
        (
            "p.status = 'closed' AND p.stock_lock_status = 'stock_locked' AND p.date_to <= c.stock \
             AND (c.fiscal IS NULL OR p.date_to > c.fiscal)",
            "the fiscal-year lock date cannot reopen the stock-locked period; lower the stock lock \
             date first:",
        ),
        (
            "p.status <> 'hard_locked' AND p.date_to <= c.hard",
            "the hard lock date is set by the hard lock operation; it does not cover the period",
        ),
        (
            "p.status = 'hard_locked' AND (c.hard IS NULL OR p.date_to > c.hard)",
            "the hard lock date must stay on or after the end of the hard-locked period",
        ),
        (
            "p.stock_lock_status <> 'stock_locked' AND p.date_to <= c.stock",
            "the stock lock date is set by the stock lock operation; it does not cover the period",
        ),
    ];
    for (condition, message) in refusals {
        let sql = format!(
            "{scope}SELECT p.name, p.company_id::int8 FROM account_fiscal_period p \
             JOIN c ON c.id = p.company_id WHERE {condition} ORDER BY p.date_from, p.id LIMIT 1"
        );
        if let Some(row) = tx.query(&sql, &[&company_id]).await?.first() {
            let name: String = row.try_get(0).map_err(db)?;
            let company: i64 = row.try_get(1).map_err(db)?;
            return Err(Error::validation(format!(
                "{message} {name} of company {company}."
            )));
        }
    }
    let update = |set: &str, condition: &str| {
        format!(
            "{scope}UPDATE account_fiscal_period p SET {set} FROM c \
             WHERE c.id = p.company_id AND p.id = ANY($2::int8[]) AND {condition}"
        )
    };
    let closed = tx
        .execute(
            &update(
                "status = 'closed', closed_at = (now() AT TIME ZONE 'UTC')",
                "p.status = 'open' AND p.date_to <= c.fiscal",
            ),
            &[&company_id, &changing],
        )
        .await?;
    let reopened = tx
        .execute(
            &update(
                "status = 'open', closed_at = NULL",
                "p.status = 'closed' AND (c.fiscal IS NULL OR p.date_to > c.fiscal)",
            ),
            &[&company_id, &changing],
        )
        .await?;
    let stock_reopened = tx
        .execute(
            &update(
                "stock_lock_status = 'open'",
                "p.stock_lock_status = 'stock_locked' AND (c.stock IS NULL OR p.date_to > c.stock)",
            ),
            &[&company_id, &changing],
        )
        .await?;
    tx.invalidate_model(super::PERIOD_MODEL, None).await?;
    Ok(SyncedPeriods {
        closed,
        reopened,
        stock_reopened,
    })
}

/// Bump the root company's `fiscal_period_serial` (it serializes period creation with the
/// synchronisation and the lock dates, see [`super::ensure_periods`]) and drop its cached value.
async fn bump_serial(tx: &mut Transaction, root: i64) -> Result<()> {
    tx.execute(
        "UPDATE res_company SET fiscal_period_serial = COALESCE(fiscal_period_serial, 0) + 1 WHERE id = $1",
        &[&root],
    )
    .await?;
    tx.invalidate_model("res.company", Some(&["fiscal_period_serial"]))
        .await
}

/// What [`sync_lock_dates`] changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncedPeriods {
    /// Periods moved from `open` to `closed`.
    pub closed: u64,
    /// Periods moved from `closed` to `open`.
    pub reopened: u64,
    /// Periods whose stock status moved from `stock_locked` to `open`.
    pub stock_reopened: u64,
}

/// A target period and the matching periods of its company's branches, which every lock statement
/// moves together ("Period set").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeriodSet {
    company_id: i64,
    date_from: Date,
    date_to: Date,
    ids: Vec<i64>,
}

impl PeriodSet {
    /// The set of `period_id`.
    ///
    /// # Errors
    ///
    /// `Access` without [`MANAGER_GROUP`], or ([`NOT_ALLOWED`]) outside superuser mode for a
    /// period of a company the caller does not hold or one that does not exist (the same answer);
    /// `Missing` in superuser mode when the period does not exist, and when a branch has no period
    /// with the same kind and range (run [`super::ensure_periods`] for the root company first),
    /// naming the branch.
    pub async fn of(tx: &mut Transaction, period_id: i64) -> Result<Self> {
        require_manager(tx).await?;
        let owner = tx
            .query(
                "SELECT company_id::int8 FROM account_fiscal_period WHERE id = $1",
                &[&period_id],
            )
            .await?;
        let Some(owner) = owner.first() else {
            return Err(if tx.env().su() {
                Error::missing(format!("fiscal period {period_id} does not exist"))
            } else {
                Error::access(NOT_ALLOWED)
            });
        };
        let company: i64 = owner.try_get(0).map_err(db)?;
        require_company_access(tx, &[company]).await?;
        // its parent_path is checked before the branches are read from it
        company_tree(tx, company).await?;
        let rows = tx
            .query(
                "SELECT t.date_from, t.date_to, b.id::int8, p.id::int8 \
                 FROM account_fiscal_period t JOIN res_company tc ON tc.id = t.company_id \
                 JOIN res_company b ON b.parent_path LIKE tc.parent_path || '%' \
                 LEFT JOIN account_fiscal_period p ON p.company_id = b.id AND p.kind = t.kind \
                   AND p.date_from = t.date_from AND p.date_to = t.date_to \
                 WHERE t.id = $1 ORDER BY b.id",
                &[&period_id],
            )
            .await?;
        let first = rows
            .first()
            .ok_or_else(|| Error::internal(format!("fiscal period {period_id} vanished")))?;
        let mut set = Self {
            company_id: company,
            date_from: first.try_get(0).map_err(db)?,
            date_to: first.try_get(1).map_err(db)?,
            ids: Vec::with_capacity(rows.len()),
        };
        for row in &rows {
            let branch: i64 = row.try_get(2).map_err(db)?;
            let id: Option<i64> = row.try_get(3).map_err(db)?;
            let id = id.ok_or_else(|| {
                Error::missing(format!(
                    "company {branch} has no fiscal period {}..{}; create the periods of its root \
                     company with ensure_periods first",
                    set.date_from, set.date_to
                ))
            })?;
            set.ids.push(id);
        }
        set.ids.sort_unstable();
        Ok(set)
    }

    /// The periods, in id order.
    pub fn ids(&self) -> &[i64] {
        &self.ids
    }

    /// The target period's company.
    pub fn company_id(&self) -> i64 {
        self.company_id
    }

    /// The last day of the periods.
    pub fn date_to(&self) -> Date {
        self.date_to
    }
}

/// One lock run on a period set: the generation each period got from T1. Every later statement
/// of the run applies only to rows still at that generation ("Run generation").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockRun {
    set: PeriodSet,
    generations: Vec<i32>,
}

impl LockRun {
    /// A run seen earlier, for a cancel or a re-run by another user who read the generations from
    /// the period rows (point 5). `generations` are in the order of `set.ids()`.
    ///
    /// # Errors
    ///
    /// `Validation` when the counts differ.
    pub fn resume(set: PeriodSet, generations: Vec<i32>) -> Result<Self> {
        if generations.len() != set.ids.len() {
            return Err(Error::validation(format!(
                "{} generations given for {} periods",
                generations.len(),
                set.ids.len()
            )));
        }
        Ok(Self { set, generations })
    }

    /// The period set.
    pub fn set(&self) -> &PeriodSet {
        &self.set
    }

    /// The generations, in the order of the set's ids.
    pub fn generations(&self) -> &[i32] {
        &self.generations
    }
}

/// The state of one period, as reported when a run's statement matched no row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeriodState {
    /// The period.
    pub id: i64,
    /// Its name.
    pub name: String,
    /// `status`.
    pub status: String,
    /// `stock_lock_status`.
    pub stock_lock_status: String,
    /// `stock_lock_generation`.
    pub stock_lock_generation: i32,
    /// `hard_lock_generation`.
    pub hard_lock_generation: i32,
}

/// The result of a T2 write or a cancel.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LockOutcome {
    /// Every period of the set moved.
    Applied,
    /// No period moved: another operation moved the set or a newer run superseded this one. The
    /// states let the caller report which (step 3); nothing was written.
    NotApplied(Vec<PeriodState>),
}

#[derive(Clone, Copy)]
enum Lock {
    Hard,
    Stock,
}

impl Lock {
    fn generation(self) -> &'static str {
        match self {
            Self::Hard => "hard_lock_generation",
            Self::Stock => "stock_lock_generation",
        }
    }
}

/// The exclusive advisory locks of `ids` (in id order), bounded by the transaction's
/// `lock_timeout`.
async fn lock_keys(tx: &mut Transaction, ids: &[i64]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let keys: Vec<i32> = ids
        .iter()
        .map(|id| i32::try_from(*id).map_err(|_| Error::internal("period id beyond int4")))
        .collect::<Result<_>>()?;
    tx.query(
        "SELECT pg_advisory_xact_lock($1::int4, k) FROM unnest($2::int4[]) \
         WITH ORDINALITY AS l(k, ord) ORDER BY ord",
        &[&PERIOD_LOCK_CLASS, &keys],
    )
    .await?;
    Ok(())
}

async fn states(tx: &mut Transaction, set: &PeriodSet) -> Result<Vec<PeriodState>> {
    let rows = tx
        .query(
            "SELECT id::int8, name, status, stock_lock_status, stock_lock_generation, \
             hard_lock_generation FROM account_fiscal_period WHERE id = ANY($1::int8[]) ORDER BY id",
            &[&set.ids],
        )
        .await?;
    rows.iter()
        .map(|r| {
            Ok(PeriodState {
                id: r.try_get(0).map_err(db)?,
                name: r.try_get(1).map_err(db)?,
                status: r.try_get(2).map_err(db)?,
                stock_lock_status: r.try_get(3).map_err(db)?,
                stock_lock_generation: r.try_get(4).map_err(db)?,
                hard_lock_generation: r.try_get(5).map_err(db)?,
            })
        })
        .collect()
}

/// Run T1 of a lock. The exclusive advisory locks of the set are held, and every status change of
/// a lock goes through them, so counting the rows that may move and then moving them is
/// all-or-nothing: a set where one period cannot move is refused before anything is written,
/// naming that period and its state.
async fn begin(
    tx: &mut Transaction,
    set: &PeriodSet,
    lock: Lock,
    update: &str,
    refusal: fn(&PeriodState) -> String,
) -> Result<LockRun> {
    authorize(tx, set.company_id).await?;
    // a lock date never passes a period its lock has not reached (lead note 2026-11-13)
    let (unlocked, out_of_order) = match lock {
        Lock::Stock => (
            "stock_lock_status <> 'stock_locked'",
            "is not stock-locked; stock-lock periods in order",
        ),
        Lock::Hard => (
            "status <> 'hard_locked'",
            "is not hard-locked; hard-lock periods in order",
        ),
    };
    let earlier = tx
        .query(
            &format!(
                "SELECT name FROM account_fiscal_period WHERE company_id = $1 AND kind = 'regular' \
                 AND date_to < $2 AND {unlocked} ORDER BY date_from LIMIT 1"
            ),
            &[&set.company_id, &set.date_from],
        )
        .await?;
    if let Some(row) = earlier.first() {
        let name: String = row.try_get(0).map_err(db)?;
        return Err(Error::validation(format!(
            "The earlier fiscal period {name} {out_of_order}."
        )));
    }
    lock_keys(tx, &set.ids).await?;
    let all = states(tx, set).await?;
    if let Some(blocked) = all.iter().find(|s| !can_begin(lock, s)) {
        return Err(Error::validation(refusal(blocked)));
    }
    if all.len() != set.ids.len() {
        return Err(Error::missing("a period of the lock set no longer exists"));
    }
    let user = tx.env().uid();
    let rows = tx.query(update, &[&set.ids, &user]).await?;
    let mut by_id: Vec<(i64, i32)> = rows
        .iter()
        .map(|r| Ok((r.try_get(0).map_err(db)?, r.try_get(1).map_err(db)?)))
        .collect::<Result<_>>()?;
    by_id.sort_unstable();
    let ids: Vec<i64> = by_id.iter().map(|(id, _)| *id).collect();
    if ids != set.ids {
        return Err(Error::internal(format!(
            "lock T1 moved periods {ids:?} of the set {:?}",
            set.ids
        )));
    }
    tx.invalidate_model(super::PERIOD_MODEL, None).await?;
    Ok(LockRun {
        set: set.clone(),
        generations: by_id.into_iter().map(|(_, g)| g).collect(),
    })
}

fn can_begin(lock: Lock, s: &PeriodState) -> bool {
    match lock {
        Lock::Hard => matches!(s.status.as_str(), "closed" | "hard_locking"),
        Lock::Stock => {
            matches!(s.stock_lock_status.as_str(), "open" | "stock_locking")
                && !matches!(s.status.as_str(), "hard_locking" | "hard_locked")
        }
    }
}

/// Run a T2 write or a cancel: applied to every row of the run, or to none (the states are then
/// reported and nothing is written); a set where only part matches is refused before any write.
/// It runs at `READ COMMITTED` (D8), so a conditional update re-checks its row after a
/// lock wait, and takes the company and root company rows `FOR UPDATE` before the advisory locks
/// (the module's lock order).
async fn finish(
    tx: &mut Transaction,
    run: &LockRun,
    lock: Lock,
    assign: &str,
    guard: &str,
) -> Result<LockOutcome> {
    let company = run.set.company_id;
    authorize(tx, company).await?;
    let isolation = tx
        .query("SELECT current_setting('transaction_isolation')", &[])
        .await?;
    let isolation: String = isolation
        .first()
        .ok_or_else(|| Error::internal("current_setting returned no row"))?
        .try_get(0)
        .map_err(db)?;
    if isolation != "read committed" {
        return Err(Error::programming(format!(
            "the second step of a lock run needs a READ COMMITTED transaction; this \
             one is {isolation}: open it with BeginOptions::isolation(ReadCommitted)"
        )));
    }
    let (root, _) = company_tree(tx, company).await?;
    // the company row, then the root's: the order a setter takes them in
    tx.query(ROW_LOCK, &[&company]).await?;
    if root != company {
        tx.query(ROW_LOCK, &[&root]).await?;
    }
    lock_keys(tx, &run.set.ids).await?;
    let filter = format!("p.id = v.id AND p.{} = v.g AND {guard}", lock.generation());
    let source = "unnest($1::int8[], $2::int4[]) AS v(id, g)";
    let size = run.set.ids.len() as u64;
    let rows = tx
        .query(
            &format!("SELECT count(*)::int8 FROM account_fiscal_period p, {source} WHERE {filter}"),
            &[&run.set.ids, &run.generations],
        )
        .await?;
    let count: i64 = rows
        .first()
        .ok_or_else(|| Error::internal("count returned no row"))?
        .try_get(0)
        .map_err(db)?;
    if count == 0 {
        return Ok(LockOutcome::NotApplied(states(tx, &run.set).await?));
    }
    if u64::try_from(count).ok() != Some(size) {
        return Err(Error::validation(format!(
            "the lock statement matches {count} of the {size} periods of its set; nothing was \
             changed"
        )));
    }
    let updated = tx
        .execute(
            &format!("UPDATE account_fiscal_period p SET {assign} FROM {source} WHERE {filter}"),
            &[&run.set.ids, &run.generations],
        )
        .await?;
    if updated != size {
        return Err(Error::internal(format!(
            "the lock statement moved {updated} of {size} periods"
        )));
    }
    tx.invalidate_model(super::PERIOD_MODEL, None).await?;
    Ok(LockOutcome::Applied)
}

/// Hard lock T1 (amending R1d): moves the set from `closed` to
/// `hard_locking` (idempotent on `hard_locking`: the starter and the generation of the live run are
/// kept). Does not write the company hard lock date. The caller then commits, drains and runs
/// [`hard_lock_complete`] or [`hard_lock_reset`] in a new transaction.
///
/// # Errors
///
/// `Access` without [`MANAGER_GROUP`] or for a company the caller does not hold
/// ([`NOT_ALLOWED`]); `Validation` naming the earlier regular period of the company that is not
/// `hard_locked` (the hard lock is in order), or the first period of the set that is still `open`
/// (the fiscal-year lock must close it first) or already `hard_locked`.
pub async fn hard_lock_begin(tx: &mut Transaction, set: &PeriodSet) -> Result<LockRun> {
    begin(
        tx,
        set,
        Lock::Hard,
        "UPDATE account_fiscal_period SET status = 'hard_locking', \
           hard_lock_started_by = CASE WHEN status = 'hard_locking' THEN hard_lock_started_by ELSE $2 END, \
           hard_lock_started_at = CASE WHEN status = 'hard_locking' THEN hard_lock_started_at \
             ELSE (now() AT TIME ZONE 'UTC') END, \
           hard_lock_generation = CASE WHEN status = 'hard_locking' THEN hard_lock_generation \
             ELSE hard_lock_generation + 1 END \
         WHERE id = ANY($1::int8[]) AND status IN ('closed', 'hard_locking') \
         RETURNING id::int8, hard_lock_generation",
        |s| match s.status.as_str() {
            "open" => format!(
                "The fiscal period {} is open: close it with the fiscal-year lock date before the \
                 hard lock.",
                s.name
            ),
            other => format!("The fiscal period {} is already {other}.", s.name),
        },
    )
    .await
}

/// Hard lock T2 when the caller's counts are zero: `hard_locked` with `stock_locked` in the same
/// statement (a hard lock implies the stock lock), both starter pairs cleared; then, only when
/// every period moved, the company hard lock date becomes the later of itself and the set's end.
///
/// # Errors
///
/// `Access` as [`hard_lock_begin`]; `Validation` when only part of the set matched; database
/// errors.
pub async fn hard_lock_complete(tx: &mut Transaction, run: &LockRun) -> Result<LockOutcome> {
    let outcome = finish(
        tx,
        run,
        Lock::Hard,
        "status = 'hard_locked', hard_locked_at = (now() AT TIME ZONE 'UTC'), stock_lock_status = 'stock_locked', \
         hard_lock_started_by = NULL, hard_lock_started_at = NULL, \
         stock_lock_started_by = NULL, stock_lock_started_at = NULL",
        "p.status = 'hard_locking'",
    )
    .await?;
    if outcome == LockOutcome::Applied {
        write_company_date(tx, run, "hard_lock_date").await?;
    }
    Ok(outcome)
}

/// Hard lock T2 when a count is not zero, and the audited cancel of a stuck hard lock: back to
/// `closed`, starter cleared; the company date is untouched.
///
/// # Errors
///
/// As [`hard_lock_complete`].
pub async fn hard_lock_reset(tx: &mut Transaction, run: &LockRun) -> Result<LockOutcome> {
    finish(
        tx,
        run,
        Lock::Hard,
        "status = 'closed', hard_lock_started_by = NULL, hard_lock_started_at = NULL",
        "p.status = 'hard_locking'",
    )
    .await
}

/// Stock lock T1 (step 1): moves the set to `stock_locking` (idempotent on
/// `stock_locking`). Refused first when an earlier regular period of the company is not
/// `stock_locked`, so the stock lock date never passes an open period.
///
/// # Errors
///
/// `Access` as [`hard_lock_begin`]; `Validation` naming the earlier period that is not stock-locked, or the first period
/// of the set that is `stock_locked` or in a hard-lock state.
pub async fn stock_lock_begin(tx: &mut Transaction, set: &PeriodSet) -> Result<LockRun> {
    begin(
        tx,
        set,
        Lock::Stock,
        "UPDATE account_fiscal_period SET stock_lock_status = 'stock_locking', \
           stock_lock_started_by = CASE WHEN stock_lock_status = 'stock_locking' \
             THEN stock_lock_started_by ELSE $2 END, \
           stock_lock_started_at = CASE WHEN stock_lock_status = 'stock_locking' \
             THEN stock_lock_started_at ELSE (now() AT TIME ZONE 'UTC') END, \
           stock_lock_generation = CASE WHEN stock_lock_status = 'stock_locking' \
             THEN stock_lock_generation ELSE stock_lock_generation + 1 END \
         WHERE id = ANY($1::int8[]) AND stock_lock_status IN ('open', 'stock_locking') \
           AND status NOT IN ('hard_locking', 'hard_locked') \
         RETURNING id::int8, stock_lock_generation",
        |s| {
            if matches!(s.status.as_str(), "hard_locking" | "hard_locked") {
                format!("The fiscal period {} is {}.", s.name, s.status)
            } else {
                format!(
                    "The fiscal period {} is already {}.",
                    s.name, s.stock_lock_status
                )
            }
        },
    )
    .await
}

/// Stock lock T2 when the caller's counts are zero: `stock_locked`, starter cleared; then, only
/// when every period moved, the company stock lock date becomes the later of itself and the set's
/// end (never decreasing, so out-of-order commits cannot move it back).
///
/// # Errors
///
/// As [`hard_lock_complete`].
pub async fn stock_lock_complete(tx: &mut Transaction, run: &LockRun) -> Result<LockOutcome> {
    let outcome = finish(
        tx,
        run,
        Lock::Stock,
        "stock_lock_status = 'stock_locked', stock_lock_started_by = NULL, \
         stock_lock_started_at = NULL",
        "p.stock_lock_status = 'stock_locking'",
    )
    .await?;
    if outcome == LockOutcome::Applied {
        write_company_date(tx, run, "stock_lock_date").await?;
    }
    Ok(outcome)
}

/// Stock lock T2 when a count is not zero, and the audited cancel: back to `open`, starter
/// cleared.
///
/// # Errors
///
/// As [`hard_lock_complete`].
pub async fn stock_lock_reset(tx: &mut Transaction, run: &LockRun) -> Result<LockOutcome> {
    finish(
        tx,
        run,
        Lock::Stock,
        "stock_lock_status = 'open', stock_lock_started_by = NULL, stock_lock_started_at = NULL",
        "p.stock_lock_status = 'stock_locking'",
    )
    .await
}

/// The company date of a completed run, and the root's serial, so a concurrent
/// [`super::ensure_periods`] serializes with it and derives its statuses from the new date.
async fn write_company_date(tx: &mut Transaction, run: &LockRun, column: &str) -> Result<()> {
    let sql = format!("UPDATE res_company SET {column} = GREATEST({column}, $2) WHERE id = $1");
    tx.execute(&sql, &[&run.set.company_id, &run.set.date_to])
        .await?;
    tx.invalidate_model("res.company", Some(&[column])).await?;
    let (root, _) = company_tree(tx, run.set.company_id).await?;
    bump_serial(tx, root).await
}
