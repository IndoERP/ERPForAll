//! Every batch is its own short READ COMMITTED transaction on the primary (a row touched after
//! the batch's statement started is re-checked, not a serialization failure), deleting at most a
//! fixed number of rows with `FOR UPDATE SKIP LOCKED`, and opens with
//! `pg_try_advisory_xact_lock` on [`GC_LOCK`] (the project's lock class, key 4). The lock is held
//! for one batch, not for the run: when a batch finds it taken, its run stops there and says so
//! ([`GcReport::stopped_by_lock`]), but runs of two nodes can still interleave batch by batch
//! when each finds the lock free between the other's batches. That is safe: `SKIP LOCKED` keeps
//! two batches off the same rows and every delete re-checks its predicate; the lock only saves
//! most duplicated scans. A deadlock (`40P01`) retries the batch ([`super::retry`]). A run stops
//! at its caps and reports where the idle sweep should resume ([`GcReport::next`]).
//! Scheduled server runs additionally fence every batch with the durable run token of
//! [`super::gc_schedule`]; the manual [`Auth::gc_from`] API retains the per-batch contract.

use std::time::Duration;

use erp_fields::{Error, Result};
use erp_orm::audit::MAX_COLLECTED_KEYS;
use erp_orm::{BeginOptions, Isolation, SUPERUSER_ID, Transaction};

use super::hooks::{EndReason, EndedSession};
use super::login::Auth;
use super::retry;
use super::store::{self, EndStatement, GC_BATCH};
use super::throttle;

/// Session rows one run deletes at most (expired and idle together).
pub const RUN_SESSION_CAP: u64 = 100_000;
/// Heap pages one idle-sweep batch reads (32 MiB at 8 KiB pages).
pub const SWEEP_PAGES: u32 = 4_096;
/// Heap pages one run sweeps at most (1 GiB at 8 KiB pages).
pub const RUN_SWEEP_PAGES: u64 = 131_072;
/// Throttle and login-log rows one batch deletes at most.
pub const ROW_BATCH: u32 = 5_000;
/// Rows one run deletes at most in each row phase (throttle, trusted pairs, login log).
pub const RUN_ROW_CAP: u64 = 200_000;
/// The advisory lock every collection transaction takes first: the project's lock class
/// (`erp_orm::node::LOCK_CLASS`) and key 4 (keys 1 – 3 are listed at
/// `erp_modules::system::LOCK_CLASS`).
pub const GC_LOCK: (i32, i32) = (erp_orm::node::LOCK_CLASS, 4);

/// Takes [`GC_LOCK`] (`$1`, `$2`) for the batch's transaction, without waiting.
pub const LOCK_SQL: &str = "SELECT pg_try_advisory_xact_lock($1::int4, $2::int4)";

/// Expired rows, oldest first, through `erp_session_expires` (pending rows included: their
/// `expires_at` is at most the pending lifetime after their creation).
pub const EXPIRED_SQL: &str = "\
DELETE FROM erp_session WHERE id IN (
    SELECT id FROM erp_session
     WHERE expires_at <= now()
     ORDER BY expires_at
     LIMIT $1
       FOR UPDATE SKIP LOCKED)
RETURNING id::text, user_id::int8";

/// Idle rows after the tuple `($1, $2)` and before page `$3` (a TID range scan, PostgreSQL 14+),
/// locked, with the page and offset of each. The scan reads pages in heap order and stops at the
/// `$5`-th row, so the next batch resumes after the largest tuple returned and a dense range is
/// read once (database review round 2, S3). Heap order is how a TID range scan returns rows (a
/// statement that locks rows runs no parallel plan); were it ever different, rows skipped before
/// the largest tuple would wait for the next sweep, and no row that is not idle is deleted.
pub const IDLE_SQL: &str = "\
SELECT id::text, (ctid::text::point)[0]::int8, (ctid::text::point)[1]::int4
  FROM erp_session
 WHERE ctid > ('(' || $1::int8 || ',' || $2::int4 || ')')::tid
   AND ctid < ('(' || $3::int8 || ',0)')::tid
   AND last_seen_at < now() - make_interval(secs => $4)
 LIMIT $5
   FOR UPDATE SKIP LOCKED";

/// The idle rows [`IDLE_SQL`] locked, by id (`$1`, `text[]`).
const IDLE_DELETE: EndStatement = EndStatement::new(
    "DELETE FROM erp_session WHERE id = ANY($1::text[]::uuid[]) \
     RETURNING id::text, user_id::int8",
);

/// The heap pages of `erp_session`.
const PAGES_SQL: &str = "\
SELECT (pg_relation_size('erp_session') / current_setting('block_size')::int8)::int8";

/// Throttle rows of the two counted kinds older than `$1` seconds, oldest first, through the
/// partial index `erp_login_throttle_counted_age` (it holds no trusted pair). Trusted pairs keep
/// their own retention ([`TRUSTED_SQL`]).
pub const THROTTLE_SQL: &str = "\
DELETE FROM erp_login_throttle WHERE (kind, key_hash) IN (
    SELECT kind, key_hash FROM erp_login_throttle
     WHERE last_failure_at < now() - make_interval(secs => $1)
       AND kind IN ('address', 'login', 'principal')
     ORDER BY last_failure_at
     LIMIT $2
       FOR UPDATE SKIP LOCKED)";

/// How long a trusted pair's `erp_login_throttle` row (kind `trusted`, `last_failure_at` = the
/// time of its last successful login) is kept: exactly the window in which the login trusts it
/// ([`throttle::TRUSTED_FOR`], round 1 decision 3; one constant, silent-failure delta S-2).
pub(crate) const TRUSTED_RETENTION: Duration = throttle::TRUSTED_FOR;

// A row collected before its window ends would lock a trusted pair out again (security M1).
const _: () = assert!(TRUSTED_RETENTION.as_secs() >= throttle::TRUSTED_FOR.as_secs());

/// Trusted-pair rows whose last success is older than `$1` seconds, oldest first, through the
/// partial index `erp_login_throttle_trusted_age`.
pub const TRUSTED_SQL: &str = "\
DELETE FROM erp_login_throttle WHERE (kind, key_hash) IN (
    SELECT kind, key_hash FROM erp_login_throttle
     WHERE last_failure_at < now() - make_interval(secs => $1)
       AND kind IN ('trusted', 'principal_trusted')
     ORDER BY last_failure_at
     LIMIT $2
       FOR UPDATE SKIP LOCKED)";

/// Each row is compared with
/// the newest row of its user, found by one top-1 read of the index `(create_uid, create_date
/// DESC NULLS LAST, id DESC)` (database review round 2, S2).
///
/// The outer scan has no resume point: every batch starts again from the start of the table and
/// probes each kept row on its way (one top-1 index read each) before it reaches rows to delete
/// (database review round 3 DD-3). The work is bounded: a run makes at most
/// [`RUN_ROW_CAP`] ÷ [`ROW_BATCH`] = 40 batches, each re-reading at most the rows kept (one per
/// user) plus its batch, and only while a backlog exists; in the steady state one batch finds
/// what the logins of one period added. Accepted; a resume key (`id > $last`) would remove the
/// re-reads if a backlog of many runs ever matters.
pub const USER_LOG_SQL: &str = "\
DELETE FROM res_users_log WHERE id IN (
    SELECT l.id FROM res_users_log l
     WHERE (l.create_date, l.id) < (SELECT n.create_date, n.id FROM res_users_log n
                                     WHERE n.create_uid = l.create_uid
                                       AND n.create_date IS NOT NULL
                                     ORDER BY n.create_date DESC NULLS LAST, n.id DESC
                                     LIMIT 1)
     LIMIT $1
       FOR UPDATE SKIP LOCKED)";

const EXPIRED: EndStatement = EndStatement::new(EXPIRED_SQL);

/// Where the idle sweep of `erp_session` resumes: a heap page (PostgreSQL's `BlockNumber`, so
/// every value is valid; a page past the end of the table starts the sweep over). The caller
/// keeps the one a run returns ([`GcReport::next`]) and passes it to the next run
/// ([`Auth::gc_from`]). must persist it across runs and restarts (any node may run the
/// next one): a cursor kept only in memory restarts at page 0, and on a table larger than one
/// run's sweep cap the tail is then never swept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GcCursor {
    page: u32,
}

impl GcCursor {
    /// The start of the table.
    pub const START: Self = Self { page: 0 };

    /// The cursor at heap page `page` (a persisted [`GcCursor::page`]).
    pub const fn at(page: u32) -> Self {
        Self { page }
    }

    /// The heap page the sweep resumes at.
    pub const fn page(&self) -> u32 {
        self.page
    }
}

/// A phase of a collection run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum GcPhase {
    /// Sessions past `expires_at`.
    Expired,
    /// Sessions idle past the idle limit (the heap sweep).
    Idle,
    /// Old throttle rows.
    Throttle,
    /// Trusted (address, login) pairs older than 30 days.
    Trusted,
    /// Login-log rows older than their user's newest.
    UserLogs,
    /// Expired native API keys (one bounded batch per run).
    ApiKeys,
}

impl GcPhase {
    /// The phase's name in logs.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Expired => "expired sessions",
            Self::Idle => "idle sessions",
            Self::Throttle => "throttle rows",
            Self::Trusted => "trusted pairs",
            Self::UserLogs => "login log",
            Self::ApiKeys => "expired API keys",
        }
    }
}

/// The phases [`Auth::gc_rows`] runs: rows deleted by key, without hooks.
#[derive(Debug, Clone, Copy)]
enum RowPhase {
    Throttle,
    Trusted,
    UserLogs,
}

impl RowPhase {
    /// The report's count of this phase.
    fn count(self, report: &mut GcReport) -> &mut u64 {
        match self {
            Self::Throttle => &mut report.throttle_rows,
            Self::Trusted => &mut report.trusted_rows,
            Self::UserLogs => &mut report.user_logs,
        }
    }
}

/// What one collection run did (counts of committed batches).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct GcReport {
    /// Sessions deleted past `expires_at` (pending rows included).
    pub expired_sessions: u64,
    /// Sessions deleted idle longer than the idle limit.
    pub idle_sessions: u64,
    /// Throttle rows (address and login counters) older than twice the cooldown.
    pub throttle_rows: u64,
    /// Trusted (address, login) pairs whose last success is older than 30 days.
    pub trusted_rows: u64,
    /// Login-log rows older than their user's newest.
    pub user_logs: u64,
    /// Expired API keys deleted in the single bounded key batch.
    pub api_keys: u64,
    /// Another node held the collection lock: the run stopped at a batch of this phase (counts
    /// of earlier batches stay).
    pub stopped_by_lock: Option<GcPhase>,
    /// A cap of the run was reached: more may be left for the next run.
    pub capped: bool,
    /// Where the next run's idle sweep resumes.
    pub next: GcCursor,
    /// Heap pages the idle sweep read, counted toward [`RUN_SWEEP_PAGES`]: a batch that stopped
    /// at its last row counts the pages up to that row's (the next batch reads that page again),
    /// one that finished its range counts the rest of the range.
    pub swept_pages: u64,
}

impl GcReport {
    /// Session rows deleted (expired and idle).
    pub const fn sessions(&self) -> u64 {
        self.expired_sessions + self.idle_sessions
    }
}

/// A collection run that failed: the error, and what the run committed before it (the counts
/// and where the idle sweep should resume). Boxed: a `Result` of it stays one pointer wide.
#[derive(Debug)]
pub struct GcError(Box<GcFailure>);

#[derive(Debug)]
struct GcFailure {
    report: GcReport,
    error: Error,
}

impl GcError {
    /// What the run committed before the error.
    pub fn report(&self) -> &GcReport {
        &self.0.report
    }

    /// The error.
    pub fn error(&self) -> &Error {
        &self.0.error
    }

    /// The error, by value.
    pub fn into_error(self) -> Error {
        self.0.error
    }
}

impl std::fmt::Display for GcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "authentication collection failed after {} sessions, {} throttle rows, {} trusted \
             pairs, {} login-log rows and {} API keys: {}",
            self.0.report.sessions(),
            self.0.report.throttle_rows,
            self.0.report.trusted_rows,
            self.0.report.user_logs,
            self.0.report.api_keys,
            self.0.error
        )
    }
}

/// The cause is in the `Display` text, so `source()` is the cause's own source: a chain reporter
/// prints it once (types-delta S2, the shape of `LoginError::Failed`).
impl std::error::Error for GcError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        std::error::Error::source(&self.0.error)
    }
}

/// The cause alone: the partial report is dropped (read [`GcError::report`] first when the
/// cursor matters).
impl From<GcError> for Error {
    fn from(e: GcError) -> Self {
        e.into_error()
    }
}

/// A batch's outcome.
enum Batch<T> {
    Done(T),
    /// Another node holds the collection lock.
    Locked,
}

/// A phase's outcome.
enum Phase {
    Finished,
    Locked,
}

impl Auth {
    /// One collection run with the idle sweep from the start of the table: [`Auth::gc_from`]
    /// with [`GcCursor::START`]. For tests and small tables; a periodic job uses
    /// [`Auth::gc_from`] with the cursor it persisted. Hidden: a job calling it sweeps the head
    /// of the table only (rust review round 2, N-7).
    ///
    /// # Errors
    ///
    /// As [`Auth::gc_from`].
    #[doc(hidden)]
    pub async fn gc(&self) -> std::result::Result<GcReport, GcError> {
        self.gc_from(GcCursor::START).await
    }

    /// One collection run, the idle sweep resuming at `cursor` (the [`GcReport::next`] of the
    /// previous run). Expired sessions, idle sessions, throttle rows, login-log rows, in batches
    /// of one READ COMMITTED transaction each, with the session-end hooks for every ended
    /// session (hooks use raw statements only: the record API is refused there, and the batch
    /// fails with that error; [`super::hooks`]).
    ///
    /// # Errors
    ///
    /// A [`GcError`] with the first failing batch's error and the report of what committed
    /// before it: `Programming` without the model `res.users.log` (before any batch); the
    /// settings' errors; a deadlock after [`super::retry::DEADLOCK_TRIES`] attempts; the batches'
    /// errors. A job
    /// matches on the `GcError` and persists [`GcError::report`]'s `next` before it converts the
    /// error (`From<GcError> for Error`, which `?` uses, drops the report: the sweep then
    /// restarts where the last persisted cursor was; silent-delta N-7, types-delta N-g).
    pub async fn gc_from(&self, cursor: GcCursor) -> std::result::Result<GcReport, GcError> {
        let mut report = GcReport {
            next: cursor,
            ..GcReport::default()
        };
        match self.run_gc(&mut report, None).await {
            Ok(()) => Ok(report),
            Err(error) => Err(GcError(Box::new(GcFailure { report, error }))),
        }
    }

    pub(super) async fn run_gc(&self, report: &mut GcReport, lease: Option<i64>) -> Result<()> {
        if self.orm().registry().model("res.users.log").is_none() {
            return Err(Error::programming(
                "authentication collection needs the model res.users.log (addon base) in the \
                 registry",
            ));
        }
        let phases = [
            GcPhase::Expired,
            GcPhase::Idle,
            GcPhase::Throttle,
            GcPhase::Trusted,
            GcPhase::UserLogs,
            GcPhase::ApiKeys,
        ];
        for phase in phases {
            let outcome = match phase {
                GcPhase::ApiKeys => self.gc_api_keys(report, lease).await?,
                GcPhase::Expired => self.gc_expired(report, lease).await?,
                GcPhase::Idle => self.gc_idle(report, lease).await?,
                GcPhase::Throttle => {
                    self.gc_rows(
                        report,
                        lease,
                        RowPhase::Throttle,
                        &async |tx: &mut Transaction| {
                            // Each destructive batch reads policy after its opening signal read.
                            // Do not retain a prior cache generation across committed batches.
                            let cooldown = super::settings::load_gc_policy(tx)
                                .await?
                                .cooldown
                                .as_secs_f64();
                            tx.execute(THROTTLE_SQL, &[&(cooldown * 2.0), &i64::from(ROW_BATCH)])
                                .await
                        },
                    )
                    .await?
                }
                GcPhase::Trusted => {
                    self.gc_rows(
                        report,
                        lease,
                        RowPhase::Trusted,
                        &async |tx: &mut Transaction| {
                            tx.execute(
                                TRUSTED_SQL,
                                &[&TRUSTED_RETENTION.as_secs_f64(), &i64::from(ROW_BATCH)],
                            )
                            .await
                        },
                    )
                    .await?
                }
                GcPhase::UserLogs => {
                    self.gc_rows(
                        report,
                        lease,
                        RowPhase::UserLogs,
                        &async |tx: &mut Transaction| {
                            tx.execute(USER_LOG_SQL, &[&i64::from(ROW_BATCH)]).await
                        },
                    )
                    .await?
                }
            };
            if let Phase::Locked = outcome {
                report.stopped_by_lock = Some(phase);
                tracing::info!(
                    phase = phase.as_str(),
                    "authentication collection stopped: another node holds the collection lock"
                );
                break;
            }
        }
        Ok(())
    }

    /// Expired API keys: batches of at most [`MAX_COLLECTED_KEYS`], each its own transaction with
    /// its own audit event, until a short batch or the run's row cap (as the row phases).
    async fn gc_api_keys(&self, report: &mut GcReport, lease: Option<i64>) -> Result<Phase> {
        let limit = u32::try_from(MAX_COLLECTED_KEYS).map_err(|e| {
            Error::programming("the key collection batch bound fits u32").with_source(e)
        })?;
        let mut deleted = 0;
        loop {
            if deleted >= RUN_ROW_CAP {
                report.capped = true;
                return Ok(Phase::Finished);
            }
            let n = match self
                .batch(lease, &async |tx: &mut Transaction| {
                    super::apikey_manage::collect_expired(tx, limit).await
                })
                .await?
            {
                Batch::Done(n) => n,
                Batch::Locked => return Ok(Phase::Locked),
            };
            deleted += n;
            report.api_keys += n;
            if n < u64::from(limit) {
                return Ok(Phase::Finished);
            }
        }
    }

    /// Expired sessions.
    async fn gc_expired(&self, report: &mut GcReport, lease: Option<i64>) -> Result<Phase> {
        loop {
            if report.sessions() >= RUN_SESSION_CAP {
                report.capped = true;
                return Ok(Phase::Finished);
            }
            let ended = match self
                .batch(lease, &async |tx: &mut Transaction| {
                    store::end_with(tx, EXPIRED, &[&i64::from(GC_BATCH)], EndReason::Expired).await
                })
                .await?
            {
                Batch::Done(ended) => count(&ended),
                Batch::Locked => return Ok(Phase::Locked),
            };
            report.expired_sessions += ended;
            if ended < u64::from(GC_BATCH) {
                return Ok(Phase::Finished);
            }
        }
    }

    /// Idle sessions, by page ranges from `report.next`: each batch scans from just after the
    /// last tuple the previous one handled to the end of its range ([`SWEEP_PAGES`] from the
    /// range's start) and stops at its [`GC_BATCH`]-th row.
    async fn gc_idle(&self, report: &mut GcReport, lease: Option<i64>) -> Result<Phase> {
        let initial = report.next;
        let pages = match self
            .batch(lease, &async |tx: &mut Transaction| {
                let rows = tx.query(PAGES_SQL, &[]).await?;
                let first = rows
                    .first()
                    .ok_or_else(|| Error::db("the page count of erp_session returned no row"))?;
                let pages: i64 = first.try_get(0).map_err(|e| {
                    Error::db(format!("the page count of erp_session: {e}")).with_source(e)
                })?;
                let pages = u64::try_from(pages).map_err(|_| {
                    Error::db(format!(
                        "the page count of erp_session is negative: {pages}"
                    ))
                })?;
                // A truncated or empty heap can end the sweep before a destructive batch.
                // Persist that reset in this fenced transaction, not in finalization later.
                if u64::from(initial.page()) >= pages
                    && initial != GcCursor::START
                    && let Some(token) = lease
                {
                    super::gc_schedule::checkpoint(tx, token, GcCursor::START).await?;
                }
                Ok(pages)
            })
            .await?
        {
            Batch::Done(pages) => pages,
            Batch::Locked => return Ok(Phase::Locked),
        };
        // the scan starts after this tuple; offsets start at 1, so offset 0 takes the whole page
        let mut after = Tuple {
            page: u64::from(report.next.page),
            offset: 0,
        };
        let mut range_end = sweep_range_end(after.page)?;
        loop {
            if after.page >= pages {
                // the sweep reached the end of the table: the next run starts over
                report.next = GcCursor::START;
                return Ok(Phase::Finished);
            }
            if report.swept_pages >= RUN_SWEEP_PAGES || report.sessions() >= RUN_SESSION_CAP {
                report.capped = true;
                if report.swept_pages >= RUN_SWEEP_PAGES {
                    tracing::warn!(
                        next_page = after.page,
                        table_pages = pages,
                        "the idle-session sweep reached its cap of {RUN_SWEEP_PAGES} pages; the \
                         next run must resume at next_page (persist GcReport::next)"
                    );
                }
                return Ok(Phase::Finished);
            }
            let (found, progress) = match self
                .batch(lease, &async |tx: &mut Transaction| {
                    let idle_secs = super::settings::load_gc_policy(tx)
                        .await?
                        .max_inactivity
                        .as_secs_f64();
                    let found = idle_batch(tx, after, range_end, idle_secs).await?;
                    let progress = idle_progress(&found, after, range_end, pages)?;
                    if let Some(token) = lease {
                        // Deletes, session-end hooks and their resume cursor are one commit.
                        super::gc_schedule::checkpoint(tx, token, progress.next).await?;
                    }
                    Ok((found, progress))
                })
                .await?
            {
                Batch::Done(done) => done,
                Batch::Locked => return Ok(Phase::Locked),
            };
            report.idle_sessions += found.ended;
            report.swept_pages += progress.swept_pages;
            after = progress.after;
            range_end = progress.range_end;
            report.next = progress.next;
        }
    }

    /// Batches of `delete` until one deletes fewer than [`ROW_BATCH`] rows or the run's cap is
    /// reached, counted in the field of `phase` (a row phase by its type: types-delta N-a).
    async fn gc_rows(
        &self,
        report: &mut GcReport,
        lease: Option<i64>,
        phase: RowPhase,
        delete: &impl AsyncFn(&mut Transaction) -> Result<u64>,
    ) -> Result<Phase> {
        let mut deleted = 0;
        loop {
            if deleted >= RUN_ROW_CAP {
                report.capped = true;
                return Ok(Phase::Finished);
            }
            let n = match self.batch(lease, delete).await? {
                Batch::Done(n) => n,
                Batch::Locked => return Ok(Phase::Locked),
            };
            deleted += n;
            *phase.count(report) += n;
            if n < u64::from(ROW_BATCH) {
                return Ok(Phase::Finished);
            }
        }
    }

    /// Run `work` in a collection transaction: READ COMMITTED on the primary, the collection
    /// lock first, committed; retried on a deadlock ([`super::retry`]).
    async fn batch<T>(
        &self,
        lease: Option<i64>,
        work: &impl AsyncFn(&mut Transaction) -> Result<T>,
    ) -> Result<Batch<T>> {
        let absorbed = &self.retry_counters().gc;
        retry::retry(
            retry::DEADLOCK,
            absorbed,
            "a collection batch",
            move || async move {
                let mut tx = self
                    .orm()
                    .begin_opts(
                        // Each attempt is its own execution: the audited key collection
                        // needs an operation identity.
                        BeginOptions::new(SUPERUSER_ID)
                            .operation_identity(erp_orm::OperationIdentity::generate()?)
                            .on_primary()
                            .isolation(Isolation::ReadCommitted),
                    )
                    .await?;
                let checked = match lease {
                    Some(token) => super::gc_schedule::fence(&mut tx, token).await,
                    None => Ok(()), // explicit manual collector has no scheduled lease
                };
                let locked = match checked {
                    Ok(()) => locked(&mut tx).await,
                    Err(error) => Err(error),
                };
                let outcome = match locked {
                    Ok(false) => Ok(Batch::Locked),
                    Ok(true) => work(&mut tx).await.map(Batch::Done),
                    Err(e) => Err(e),
                };
                match outcome {
                    Ok(batch) => tx.commit().await.map(|()| batch),
                    Err(e) => Err(match tx.rollback().await {
                        Ok(()) => e,
                        Err(r) => e.and_also(&r),
                    }),
                }
            },
        )
        .await
    }
}

/// A heap tuple's position: page and line pointer offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Tuple {
    page: u64,
    offset: i32,
}

/// The pages a full idle batch swept when its largest locked tuple is `last`, it scanned after
/// `after`, and its range ends before page `range_end`; `None` when `last` is not after `after`,
/// or not before `range_end` (database review round 3 DD-2, round 4 C4-1: under READ COMMITTED a
/// row updated meanwhile is returned in its newest version, whose tuple may lie anywhere, before
/// the start or past the range). The sweep then treats the range as done instead of moving
/// backwards, wrapping or skipping past the range: it always makes progress, and rows it skipped
/// wait for the next sweep.
fn swept_to(after: Tuple, last: Tuple, range_end: u64) -> Option<u64> {
    (last > after && last.page < range_end).then(|| last.page.saturating_sub(after.page) + 1)
}

/// The pages from `after` to the end of its range (`range_end`, or the table's `pages` when it
/// ends first), counted when the range is done. The sweep keeps `after` inside its range and
/// before the table's end ([`swept_to`], and the loop's end check), so this is never negative;
/// were it ever, it counts 0 pages rather than wrapping into a false cap (round 4 C4-1).
fn rest_of_range(after: Tuple, range_end: u64, pages: u64) -> u64 {
    range_end.min(pages).saturating_sub(after.page)
}

struct IdleProgress {
    after: Tuple,
    range_end: u64,
    swept_pages: u64,
    next: GcCursor,
}
fn sweep_range_end(page: u64) -> Result<u64> {
    page.checked_add(u64::from(SWEEP_PAGES))
        .ok_or_else(|| Error::programming("authentication collection sweep range overflowed"))
}
fn idle_progress(
    found: &IdleBatch,
    after: Tuple,
    range_end: u64,
    pages: u64,
) -> Result<IdleProgress> {
    let resume = found
        .last
        .filter(|_| found.picked >= u64::from(GC_BATCH))
        .and_then(|last| swept_to(after, last, range_end).map(|pages| (last, pages)));
    let mut progress = match resume {
        // A full batch resumes after its last tuple, in the same range.
        Some((last, swept_pages)) => IdleProgress {
            after: last,
            range_end,
            swept_pages,
            next: cursor_at(last.page)?,
        },
        // The range is done. Rows another transaction locked wait for the next sweep.
        None => {
            let next_after = Tuple {
                page: range_end,
                offset: 0,
            };
            IdleProgress {
                after: next_after,
                range_end: sweep_range_end(range_end)?,
                swept_pages: rest_of_range(after, range_end, pages),
                // Past PostgreSQL's last possible block, the table ends and the sweep resets.
                next: u32::try_from(next_after.page).map_or(GcCursor::START, GcCursor::at),
            }
        }
    };
    if progress.after.page >= pages {
        progress.next = GcCursor::START;
    }
    Ok(progress)
}

/// What one idle batch did.
struct IdleBatch {
    /// Rows [`IDLE_SQL`] locked.
    picked: u64,
    /// Rows deleted (with their hooks).
    ended: u64,
    /// The largest tuple locked.
    last: Option<Tuple>,
}

/// Lock the idle rows after `after` and before page `range_end`, then end them with their hooks.
async fn idle_batch(
    tx: &mut Transaction,
    after: Tuple,
    range_end: u64,
    idle_secs: f64,
) -> Result<IdleBatch> {
    // block numbers are u32, so both pages fit int8
    let from = i64::try_from(after.page)
        .map_err(|_| Error::programming(format!("heap page {} past int8", after.page)))?;
    let to = i64::try_from(range_end)
        .map_err(|_| Error::programming(format!("heap page {range_end} past int8")))?;
    let rows = tx
        .query(
            IDLE_SQL,
            &[&from, &after.offset, &to, &idle_secs, &i64::from(GC_BATCH)],
        )
        .await?;
    let mut ids = Vec::with_capacity(rows.len());
    let mut last: Option<Tuple> = None;
    for row in &rows {
        let id: String = row
            .try_get(0)
            .map_err(|e| Error::db(format!("erp_session.id: {e}")).with_source(e))?;
        let page: i64 = row
            .try_get(1)
            .map_err(|e| Error::db(format!("erp_session.ctid page: {e}")).with_source(e))?;
        let offset: i32 = row
            .try_get(2)
            .map_err(|e| Error::db(format!("erp_session.ctid offset: {e}")).with_source(e))?;
        let page = u64::try_from(page)
            .map_err(|_| Error::db(format!("erp_session.ctid has a negative page: {page}")))?;
        let tuple = Tuple { page, offset };
        last = Some(last.map_or(tuple, |l| l.max(tuple)));
        ids.push(id);
    }
    let ended = if ids.is_empty() {
        0
    } else {
        count(&store::end_with(tx, IDLE_DELETE, &[&ids], EndReason::Expired).await?)
    };
    Ok(IdleBatch {
        picked: rows.len() as u64,
        ended,
        last,
    })
}

/// The cursor at heap page `page`, a page a row was read from (so a block number).
fn cursor_at(page: u64) -> Result<GcCursor> {
    u32::try_from(page).map(GcCursor::at).map_err(|_| {
        Error::db(format!(
            "erp_session.ctid page {page} is not a block number"
        ))
    })
}

/// Take the collection lock in `tx`; `false` when another transaction holds it.
async fn locked(tx: &mut Transaction) -> Result<bool> {
    let rows = tx.query(LOCK_SQL, &[&GC_LOCK.0, &GC_LOCK.1]).await?;
    let first = rows
        .first()
        .ok_or_else(|| Error::db("the collection lock returned no row"))?;
    first
        .try_get(0)
        .map_err(|e| Error::db(format!("the collection lock: {e}")).with_source(e))
}

fn count(ended: &[EndedSession]) -> u64 {
    ended.len() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(page: u64, offset: i32) -> Tuple {
        Tuple { page, offset }
    }

    /// Database review round 3 DD-2 and round 4 C4-1: a full batch whose largest tuple lies
    /// after the start and before the range's end counts the pages from the start's page to its
    /// own; one at or before the start, or at or past the range's end (a row returned in a newer
    /// version on another page), counts the range as done: the sweep never moves backwards and
    /// cannot underflow.
    #[test]
    fn a_full_batch_never_moves_the_sweep_backwards() {
        assert_eq!(swept_to(at(5, 3), at(7, 1), 10), Some(3));
        assert_eq!(swept_to(at(5, 3), at(5, 4), 10), Some(1));
        assert_eq!(swept_to(at(5, 3), at(9, 300), 10), Some(5));
        assert_eq!(swept_to(at(5, 3), at(5, 3), 10), None);
        assert_eq!(swept_to(at(5, 3), at(5, 2), 10), None);
        assert_eq!(swept_to(at(5, 3), at(2, 9), 10), None);
        assert_eq!(swept_to(at(0, 0), at(0, 1), 8), Some(1));
        // the upper side: at or past the range's end
        assert_eq!(swept_to(at(5, 3), at(10, 0), 10), None);
        assert_eq!(swept_to(at(5, 3), at(10, 1), 10), None);
        assert_eq!(swept_to(at(5, 3), at(u64::from(u32::MAX), 1), 10), None);
    }

    /// Round 4 C4-1: the rest of a range is counted from the start to the range's end or the
    /// table's, and is zero, not a wrap, if the start were ever past both.
    #[test]
    fn the_rest_of_a_range_never_wraps() {
        assert_eq!(rest_of_range(at(5, 3), 10, 100), 5);
        assert_eq!(rest_of_range(at(5, 3), 10, 7), 2);
        assert_eq!(rest_of_range(at(10, 0), 10, 100), 0);
        assert_eq!(rest_of_range(at(12, 4), 10, 100), 0);
        assert_eq!(rest_of_range(at(12, 4), 10, 11), 0);
    }
}
