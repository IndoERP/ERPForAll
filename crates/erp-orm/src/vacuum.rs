//! Cleanup of transient (wizard) models: [`Orm::vacuum_transient`] (
//! package B, spec).
//!
//! Adopted: the
//! same outcomes, the same `Records::unlink` method chain, age first and then count. Modified for
//! several replicas of one database: the cleanup is a cycle owned through a lease row in
//! `erp_transient_vacuum_state` (one row per model), every batch is a short fenced transaction, and
//! a budget bounds what one cycle may remove.
//!
//! This is a library capability. Nothing calls it: no scheduler, worker or request starts a
//! cleanup, so a deployment removes no transient row until a host invokes
//! [`Orm::vacuum_transient`] (package D of the plan decides how).
//!
//! # The protocol
//!
//! 1. **Claim.** One short transaction locks the model's state row, waiting at most one second
//!    (a longer wait is [`VacuumOutcome::Busy`]). A live lease of another attempt is `Busy`. Otherwise
//!    the claim increments the epoch (fails closed at `i64::MAX`), stores a fresh random attempt
//!    UUID and a lease of 60 seconds, and commits before any row is touched. An open cycle (a
//!    previous attempt stopped, crashed or was cut off) is inherited with its cutoff, phase,
//!    count trigger and budget counters; otherwise a new cycle starts only when the database says
//!    it is due (300 seconds after the previous cycle started, [`VacuumOutcome::NotDue`] before).
//!    An open cycle stored under another policy is aborted instead of inherited.
//! 2. **Batches.** Each batch is a fresh `REPEATABLE READ` transaction with the lock limit 1 s,
//!    the statement limit 20 s and the transaction backstop 30 s. It first locks the state row and
//!    checks owner, epoch, the lease against the database clock and the policy; selects at most
//!    1,000 ids `ORDER BY id ... FOR UPDATE SKIP LOCKED`; deletes them with the normal
//!    `Records::unlink` as the system principal; and records the progress and renews the lease
//!    in the same transaction. A serialization failure or deadlock is retried from a fresh
//!    selection, never with an old id list.
//! 4. **Budget.** A cycle removes at most 100,000 rows in at most 100 batches; the counters are
//!    persisted with the deletions, a takeover inherits them and a new cycle resets them. A cycle
//!    that reaches either cap ends as [`VacuumOutcome::BudgetExhausted`] and the next cycle is due
//!    at `GREATEST(next_due, now + 300 s)`. The caller's [`VacuumOptions`] only lower the caps of one call.
//!
//! Rows locked by other transactions are skipped, which never proves that nothing is left: the
//! cycle then stays open and the call reports [`VacuumOutcome::Pending`]. The caps bound the rows
//! selected directly; an `unlink` override or a cascade may touch more.
//!
//! A commit whose outcome is not known is an error that names it; it is never retried, and the
//! next claim reads the durable state.

use std::time::Duration;

use erp_fields::{Error, ErrorKind, Result};
use erp_registry::TransientPolicy;

use crate::transaction::{CommitError, is_retryable, jitter};
use crate::{BeginOptions, Isolation, Orm, SUPERUSER_ID, Timeouts, Transaction};

/// Most rows one batch selects directly.
const HARD_BATCH_ROWS: u32 = 1_000;
/// Most batches (that removed rows) of one cycle.
const HARD_CYCLE_BATCHES: u32 = 100;
/// Most rows one cycle removes.
const HARD_CYCLE_ROWS: u64 = 100_000;
/// Seconds a lease lasts, renewed by every batch.
const LEASE_SECONDS: i64 = 60;
/// Seconds between the starts of two cycles.
const CADENCE_SECONDS: i64 = 300;
/// Rows written within this many seconds are never removed.
const RECENT_WRITE_SECONDS: u64 = 300;
/// Attempts of one batch on serialization failures and deadlocks.
const MAX_ATTEMPTS: u32 = 5;

/// The state table (`erp_transient_vacuum_state`), installed by `Orm::sync_schema` like the other
/// core tables of `erp-orm` (listed in `core_ddl::ORM_DDL`).
pub(crate) const VACUUM_TABLES: crate::CoreDdl = crate::CoreDdl {
    owner: "erp-orm",
    name: "erp_transient_vacuum_state",
    purpose: "the transient vacuum state",
    tables: &["erp_transient_vacuum_state"],
    sql: VACUUM_DDL,
    after_models: None,
};

/// One row per transient model: the lease (owner attempt UUID, epoch, expiry), when the next cycle
/// is due, and the open cycle (phase, cutoff, count trigger, the policy it started under and its
/// persisted budget). Times are the database's. No row holds record data.
pub(crate) const VACUUM_DDL: &str = "\
CREATE TABLE IF NOT EXISTS erp_transient_vacuum_state (
    model text PRIMARY KEY,
    epoch int8 NOT NULL DEFAULT 0 CHECK (epoch >= 0),
    owner uuid,
    lease_expires_at timestamptz,
    next_due_at timestamptz NOT NULL,
    phase text NOT NULL DEFAULT 'idle' CHECK (phase IN ('idle', 'age', 'count')),
    cycle_started_at timestamptz,
    cycle_cutoff timestamptz,
    count_latched boolean NOT NULL DEFAULT false,
    policy_fingerprint text,
    rows_total int8 NOT NULL DEFAULT 0 CHECK (rows_total >= 0),
    batches_total int8 NOT NULL DEFAULT 0 CHECK (batches_total >= 0),
    CONSTRAINT erp_transient_vacuum_state_lease
        CHECK ((owner IS NULL) = (lease_expires_at IS NULL)),
    CONSTRAINT erp_transient_vacuum_state_open_cycle
        CHECK (phase = 'idle' OR (cycle_cutoff IS NOT NULL AND policy_fingerprint IS NOT NULL))
);
REVOKE ALL ON erp_transient_vacuum_state FROM PUBLIC";

/// Per-call limits of [`Orm::vacuum_transient`]. They can lower the hard caps (1,000 rows per
/// batch, 100 batches and 100,000 rows per cycle); a value of zero, or above the cap, is refused.
/// A call that stops at one of its own caps leaves the cycle open for the next call.
///
/// ```
/// use erp_orm::VacuumOptions;
/// let options = VacuumOptions::default().max_batch_rows(200)?.max_batches(5)?;
/// assert_eq!(options.batch_rows(), 200);
/// assert!(VacuumOptions::default().max_batches(0).is_err());
/// # Ok::<(), erp_fields::Error>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VacuumOptions {
    batch_rows: u32,
    batches: u32,
    rows: u64,
    #[cfg(feature = "test-faults")]
    faults: VacuumFaults,
}

/// Faults a call simulates for the process tests (package C). Compiled only with the
/// `test-faults` feature, which no production build enables; absent otherwise, not disabled.
#[cfg(feature = "test-faults")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
struct VacuumFaults {
    /// Injected into the commit of every batch.
    commit: Option<crate::CommitFault>,
    /// Wait here while another session holds this advisory lock.
    gate: Option<(VacuumPause, i64)>,
    /// Advisory lock a gated batch holds while it waits, so a test can see it is waiting.
    ready: Option<i64>,
}

/// Where a call waits for its gate.
#[cfg(feature = "test-faults")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum VacuumPause {
    /// Before every batch once the call has committed a batch that removed rows (the lease is
    /// live, no transaction is open): a stalled owner.
    BetweenBatches,
    /// Inside a batch, right after its snapshot and its lock on the state row.
    AfterSnapshot,
}

/// Batch retries (serialization failures and deadlocks) this process has made.
#[cfg(feature = "test-faults")]
static RETRIES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Batch retries this process has made (tests only, feature `test-faults`).
#[cfg(feature = "test-faults")]
#[doc(hidden)]
pub fn vacuum_batch_retries() -> u64 {
    RETRIES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Polls of a gate (20 ms apart) before the call fails instead of waiting for ever.
#[cfg(feature = "test-faults")]
const GATE_POLLS: u32 = 500;

impl Default for VacuumOptions {
    /// The hard caps.
    fn default() -> Self {
        Self {
            batch_rows: HARD_BATCH_ROWS,
            batches: HARD_CYCLE_BATCHES,
            rows: HARD_CYCLE_ROWS,
            #[cfg(feature = "test-faults")]
            faults: VacuumFaults::default(),
        }
    }
}

fn bounded(what: &str, value: u64, hard: u64) -> Result<()> {
    if value == 0 || value > hard {
        return Err(Error::programming(format!(
            "VacuumOptions {what} must be between 1 and {hard} (the cap cannot be raised or \
             disabled), got {value}"
        )));
    }
    Ok(())
}

impl VacuumOptions {
    /// Select at most `rows` ids per batch.
    ///
    /// # Errors
    ///
    /// `Programming` when `rows` is 0 or above 1,000.
    pub fn max_batch_rows(mut self, rows: u32) -> Result<Self> {
        bounded("max_batch_rows", rows.into(), HARD_BATCH_ROWS.into())?;
        self.batch_rows = rows;
        Ok(self)
    }

    /// Run at most `batches` batches in this call.
    ///
    /// # Errors
    ///
    /// `Programming` when `batches` is 0 or above 100.
    pub fn max_batches(mut self, batches: u32) -> Result<Self> {
        bounded("max_batches", batches.into(), HARD_CYCLE_BATCHES.into())?;
        self.batches = batches;
        Ok(self)
    }

    /// Remove at most `rows` rows in this call.
    ///
    /// # Errors
    ///
    /// `Programming` when `rows` is 0 or above 100,000.
    pub fn max_rows(mut self, rows: u64) -> Result<Self> {
        bounded("max_rows", rows, HARD_CYCLE_ROWS)?;
        self.rows = rows;
        Ok(self)
    }

    /// Rows per batch.
    pub fn batch_rows(&self) -> u32 {
        self.batch_rows
    }

    /// Batches per call.
    pub fn batches(&self) -> u32 {
        self.batches
    }

    /// Rows per call.
    pub fn rows(&self) -> u64 {
        self.rows
    }
}

/// Test seams (feature `test-faults` only; never compiled into a production build).
#[cfg(feature = "test-faults")]
impl VacuumOptions {
    /// Simulate `fault` in the commit of every batch of the call.
    #[doc(hidden)]
    #[must_use]
    pub fn with_commit_fault(mut self, fault: crate::CommitFault) -> Self {
        self.faults.commit = Some(fault);
        self
    }

    /// Stall the call between batches until no other session holds the advisory lock `key`.
    #[doc(hidden)]
    #[must_use]
    pub fn pause_between_batches(mut self, key: i64) -> Self {
        self.faults.gate = Some((VacuumPause::BetweenBatches, key));
        self
    }

    /// Stall every batch, after its snapshot and state-row lock, until no other session holds the
    /// advisory lock `key`. While it waits the batch holds the advisory lock `ready`, which a test
    /// reads from `pg_locks` to know the batch is waiting.
    #[doc(hidden)]
    #[must_use]
    pub fn pause_after_snapshot(mut self, key: i64, ready: i64) -> Self {
        self.faults.gate = Some((VacuumPause::AfterSnapshot, key));
        self.faults.ready = Some(ready);
        self
    }
}

/// Wait until `pg_try_advisory_xact_lock(key)` succeeds on `tx` (another session holds it
/// meanwhile), at most [`GATE_POLLS`] polls.
#[cfg(feature = "test-faults")]
async fn wait_gate(tx: &mut Transaction, key: i64) -> Result<()> {
    for _ in 0..GATE_POLLS {
        let rows = tx
            .query("SELECT pg_try_advisory_xact_lock($1)", &[&key])
            .await?;
        if rows.first().is_some_and(|r| r.get::<_, bool>(0)) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Err(Error::new(
        ErrorKind::Internal,
        format!("vacuum test gate {key} never opened"),
    ))
}

/// How a call to [`Orm::vacuum_transient`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum VacuumOutcome {
    /// Both thresholds of the model's policy are disabled: nothing was read or written.
    Disabled,
    /// No cycle is open and the next one is not due yet.
    NotDue,
    /// Another attempt holds a live lease, or the state row stayed locked past the claim's wait.
    Busy,
    /// The lease was lost (the epoch or owner changed, the lease expired by the database clock, or
    /// the stored cycle is idle): the batch in flight rolled back and nothing more was done. Rows removed by earlier batches stay removed.
    LostLease,
    /// The cycle stored for the model belonged to another policy and was aborted before any of its
    /// metadata was used; a new cycle starts when it is due.
    PolicyChanged,
    /// The cycle finished: no eligible row is left.
    Completed,
    /// The cycle is still open: this call reached its own caps, or the rows left are locked by
    /// other transactions. Call again; the next attempt continues the same cycle.
    Pending,
    /// The cycle reached its 100,000 rows or 100 batches and ended; the next cycle is due at the
    /// later of its normal due time and 300 seconds from now (`GREATEST(next_due, now + 300 s)`).
    BudgetExhausted,
}

impl std::fmt::Display for VacuumOutcome {
    /// The stable lowercase name of the outcome (`completed`, `not_due`, `budget_exhausted`, ...),
    /// the form operators and scripts read.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Disabled => "disabled",
            Self::NotDue => "not_due",
            Self::Busy => "busy",
            Self::LostLease => "lost_lease",
            Self::PolicyChanged => "policy_changed",
            Self::Completed => "completed",
            Self::Pending => "pending",
            Self::BudgetExhausted => "budget_exhausted",
        })
    }
}

/// What [`Orm::vacuum_transient`] did. Counts are of committed work only.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct VacuumReport {
    /// How the call ended.
    pub outcome: VacuumOutcome,
    /// Rows removed by the batches this call committed.
    pub deleted_rows: u64,
    /// Batches this call committed that removed rows.
    pub batches: u64,
    /// The epoch this call claimed; `None` when it claimed nothing.
    pub epoch: Option<i64>,
    /// Rows the whole (possibly inherited) cycle has removed, including earlier attempts.
    pub cycle_rows: u64,
    /// Batches the whole cycle has committed, including earlier attempts.
    pub cycle_batches: u64,
    /// True when the claim found a cycle of another policy and aborted it.
    pub aborted_previous_cycle: bool,
}

impl VacuumReport {
    fn bare(outcome: VacuumOutcome) -> Self {
        Self {
            outcome,
            deleted_rows: 0,
            batches: 0,
            epoch: None,
            cycle_rows: 0,
            cycle_batches: 0,
            aborted_previous_cycle: false,
        }
    }
}

/// The identity of a model's policy as stored in its open cycle: a cycle started under another
/// policy is aborted, never continued under the new thresholds. Text, stable across processes.
///
/// ```
/// use erp_orm::transient_policy_fingerprint;
/// use erp_registry::TransientPolicy;
/// let text = transient_policy_fingerprint("my.wizard", &TransientPolicy::default());
/// assert_eq!(text, "v1;my.wizard;age=3600;count=none");
/// ```
#[doc(hidden)]
#[must_use]
pub fn transient_policy_fingerprint(model: &str, policy: &TransientPolicy) -> String {
    let show = |v: Option<u64>| v.map_or_else(|| "none".to_owned(), |v| v.to_string());
    format!(
        "v1;{model};age={};count={}",
        show(policy.max_age_seconds()),
        show(policy.max_count())
    )
}

// ----- internals ------------------------------------------------------------------------------

/// A phase of an open cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Age,
    Count,
}

impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Age => "age",
            Self::Count => "count",
        }
    }

    fn parse(text: &str) -> Result<Option<Self>> {
        match text {
            "idle" => Ok(None),
            "age" => Ok(Some(Self::Age)),
            "count" => Ok(Some(Self::Count)),
            other => Err(Error::new(
                ErrorKind::Internal,
                format!("erp_transient_vacuum_state holds the unknown phase '{other}'"),
            )),
        }
    }
}

/// The model being cleaned, resolved from the registry once per call.
struct Target<'a> {
    model: &'a str,
    table: String,
    age: Option<u64>,
    count: Option<u64>,
    fingerprint: String,
}

/// The attempt that holds the lease.
struct Lease {
    owner: String,
    epoch: i64,
}

enum Claim {
    Held { lease: Lease, aborted: bool },
    Busy,
    NotDue { aborted: bool },
}

/// How a batch leaves the cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    /// More work: keep the lease and go on.
    Continue,
    /// No eligible row left.
    Completed,
    /// A cap of the cycle was reached.
    Exhausted,
    /// Stop with the cycle open (call caps, or locked rows).
    Pending,
}

enum Step {
    Done {
        end: End,
        rows: u64,
        cycle_rows: u64,
        cycle_batches: u64,
    },
    Lost,
    PolicyChanged,
}

fn timeouts() -> Result<Timeouts> {
    Timeouts::new(
        Duration::from_secs(1),
        Duration::from_secs(20),
        Duration::from_secs(30),
    )
}

fn count_err(what: &str) -> Error {
    Error::new(
        ErrorKind::Internal,
        format!("transient vacuum: {what} is out of range"),
    )
}

fn to_u64(value: i64, what: &str) -> Result<u64> {
    u64::try_from(value).map_err(|_| count_err(what))
}

fn to_i64(value: u64, what: &str) -> Result<i64> {
    i64::try_from(value).map_err(|_| count_err(what))
}

/// The age predicate shared by selection and the remaining-work check: `$1` model, `$2` seconds.
/// The cutoff comes from the state row, which the transaction holds locked.
const OLDER_THAN: &str = "COALESCE(write_date, create_date) < \
     (((SELECT cycle_cutoff FROM erp_transient_vacuum_state WHERE model = $1) AT TIME ZONE 'UTC') \
     - $2::int8 * interval '1 second')";

const CLAIM_LOCK_SQL: &str = "\
SELECT epoch, COALESCE(owner IS NOT NULL AND lease_expires_at > clock_timestamp(), false), phase, \
       policy_fingerprint, (next_due_at <= clock_timestamp()) \
  FROM erp_transient_vacuum_state WHERE model = $1 FOR UPDATE";

const CLAIM_ABORT_SQL: &str = "\
UPDATE erp_transient_vacuum_state SET epoch = epoch + 1, owner = NULL, lease_expires_at = NULL, \
       phase = 'idle', count_latched = false, rows_total = 0, batches_total = 0 \
 WHERE model = $1 AND epoch = $2 RETURNING epoch";

const CLAIM_FRESH_SQL: &str = "\
WITH t AS (SELECT clock_timestamp() AS now) \
UPDATE erp_transient_vacuum_state s SET epoch = s.epoch + 1, owner = gen_random_uuid(), \
       lease_expires_at = t.now + $5::int8 * interval '1 second', \
       next_due_at = t.now + $6::int8 * interval '1 second', phase = $3, \
       cycle_started_at = t.now, cycle_cutoff = t.now, count_latched = false, \
       policy_fingerprint = $4, rows_total = 0, batches_total = 0 \
  FROM t WHERE s.model = $1 AND s.epoch = $2 RETURNING s.owner::text, s.epoch";

const CLAIM_CONTINUE_SQL: &str = "\
UPDATE erp_transient_vacuum_state SET epoch = epoch + 1, owner = gen_random_uuid(), \
       lease_expires_at = clock_timestamp() + $3::int8 * interval '1 second' \
 WHERE model = $1 AND epoch = $2 RETURNING owner::text, epoch";

const BATCH_LOCK_SQL: &str = "\
SELECT owner::text, epoch, COALESCE(lease_expires_at > clock_timestamp(), false), phase, \
       count_latched, policy_fingerprint, rows_total, batches_total \
  FROM erp_transient_vacuum_state WHERE model = $1 FOR UPDATE";

/// Conditional progress write: `$2` owner, `$3` epoch, `$4` phase, `$5` latch, `$6`/`$7` added
/// rows/batches, `$8` hand the lease back, `$9` push the next cycle out to a full cadence from now,
/// `$10` lease seconds.
const BATCH_UPDATE_SQL: &str = "\
UPDATE erp_transient_vacuum_state SET phase = $4, count_latched = $5, \
       rows_total = rows_total + $6, batches_total = batches_total + $7, \
       owner = CASE WHEN $8 THEN NULL ELSE owner END, \
       lease_expires_at = CASE WHEN $8 THEN NULL \
                               ELSE clock_timestamp() + $10::int8 * interval '1 second' END, \
       next_due_at = CASE WHEN $9 \
                          THEN GREATEST(next_due_at, clock_timestamp() + $11::int8 * interval '1 second') \
                          ELSE next_due_at END \
 WHERE model = $1 AND owner = $2::text::uuid AND epoch = $3";

const RELEASE_SQL: &str = "\
UPDATE erp_transient_vacuum_state SET owner = NULL, lease_expires_at = NULL \
 WHERE model = $1 AND owner = $2::text::uuid AND epoch = $3";

fn indeterminate(model: &str, error: Error) -> Error {
    Error::new(
        error.kind(),
        format!(
            "transient vacuum of '{model}': the outcome of a commit is not known (the batch may \
             or may not have been applied); it was not retried and the next claim reads the \
             stored state: {}",
            error.message()
        ),
    )
    .with_sqlstate(error.sqlstate().map(str::to_owned))
    .with_source(error)
}

impl Orm {
    /// Remove the old rows of the transient model `model` through the normal `unlink` chain, as
    /// described in the module documentation: claim the model's lease, then run fenced batches
    /// until the cycle ends, the call's own caps are reached, or the lease is lost.
    ///
    /// Trust model: this is a host-only call. It opens its own system-principal transactions and
    /// takes no principal or context; it is not an RPC method, not a model method and not a
    /// WebAssembly import, and a context key grants nothing. Code that holds an [`Orm`] can start
    /// a superuser transaction anyway, so no capability marker is added. The `unlink` overrides of
    /// the model run as they would for any deletion, with the system principal.
    ///
    /// Safe to call from several processes at once: the database decides who owns the cycle.
    /// Nothing calls it automatically.
    ///
    /// # Errors
    ///
    /// `Programming` when `model` is not a model of the registry, is not transient, or keeps no
    /// `write_date` (`_log_access` off). The `unlink` chain's own error when a deletion fails: the
    /// failing batch and its progress roll back, earlier batches stay committed, the lease is handed
    /// back and the error keeps its kind and message. An error naming an unknown commit outcome
    /// when `COMMIT` got no answer; that batch is not retried. Database errors otherwise,
    /// including a lock wait on the state row inside a batch (the lease is then treated as
    /// unsafe to continue).
    pub async fn vacuum_transient(
        &self,
        model: &str,
        options: VacuumOptions,
    ) -> Result<VacuumReport> {
        crate::route::unit_of_work(self.vacuum_inner(model, options)).await
    }

    async fn vacuum_inner(&self, model: &str, options: VacuumOptions) -> Result<VacuumReport> {
        let registry = self.registry();
        let spec = registry.model(model).ok_or_else(|| {
            Error::programming(format!(
                "vacuum_transient: '{model}' is not a model of this registry"
            ))
        })?;
        crate::event_namespace::refuse_event_namespace(spec, "the transient vacuum")?;
        let Some(policy) = spec.transient_policy else {
            return Err(Error::programming(format!(
                "vacuum_transient: '{model}' is not a transient model: only transient models have \
                 a cleanup policy"
            )));
        };
        if !spec.log_access || spec.field("write_date").is_none() {
            return Err(Error::programming(format!(
                "vacuum_transient: '{model}' keeps no write_date (_log_access is off): the age of \
                 its rows is unknown"
            )));
        }
        if policy.is_cleanup_disabled() {
            return Ok(VacuumReport::bare(VacuumOutcome::Disabled));
        }
        let target = Target {
            model,
            table: spec.quoted_table().to_owned(),
            age: policy.max_age_seconds(),
            count: policy.max_count(),
            fingerprint: transient_policy_fingerprint(model, &policy),
        };
        let (lease, aborted) = match self.claim(&target).await? {
            Claim::Busy => return Ok(VacuumReport::bare(VacuumOutcome::Busy)),
            Claim::NotDue { aborted } => {
                let mut report = VacuumReport::bare(VacuumOutcome::NotDue);
                report.aborted_previous_cycle = aborted;
                return Ok(report);
            }
            Claim::Held { lease, aborted } => (lease, aborted),
        };
        let mut report = VacuumReport::bare(VacuumOutcome::Pending);
        report.epoch = Some(lease.epoch);
        report.aborted_previous_cycle = aborted;
        // A cycle needs at most two phase changes besides its row batches: the guard only turns a
        // logic error into a loud failure.
        let guard = u64::from(options.batches) + 4;
        let mut turns = 0u64;
        loop {
            turns += 1;
            if turns > guard {
                self.release_quietly(&target, &lease).await;
                return Err(Error::new(
                    ErrorKind::Internal,
                    format!("transient vacuum of '{model}' did not settle within {guard} batches"),
                ));
            }
            let mut unknown_commit = false;
            let step = match self
                .batch(&target, &lease, &options, &report, &mut unknown_commit)
                .await
            {
                Ok(step) => step,
                Err(error) => {
                    // an unknown commit leaves the state to the next claim; anything else hands
                    // the lease back so the next call does not wait for it to expire
                    if !unknown_commit {
                        self.release_quietly(&target, &lease).await;
                    }
                    return Err(error);
                }
            };
            match step {
                Step::Lost => {
                    report.outcome = VacuumOutcome::LostLease;
                    return Ok(report);
                }
                Step::PolicyChanged => {
                    report.outcome = VacuumOutcome::PolicyChanged;
                    return Ok(report);
                }
                Step::Done {
                    end,
                    rows,
                    cycle_rows,
                    cycle_batches,
                } => {
                    report.deleted_rows += rows;
                    report.batches += u64::from(rows > 0);
                    report.cycle_rows = cycle_rows;
                    report.cycle_batches = cycle_batches;
                    report.outcome = match end {
                        End::Continue => continue,
                        End::Completed => VacuumOutcome::Completed,
                        End::Exhausted => VacuumOutcome::BudgetExhausted,
                        End::Pending => VacuumOutcome::Pending,
                    };
                    return Ok(report);
                }
            }
        }
    }

    /// The claim transaction (READ COMMITTED, raw statements only: the state row is the one thing
    /// it touches).
    async fn claim(&self, target: &Target<'_>) -> Result<Claim> {
        let mut tx = self
            .begin_opts(
                BeginOptions::new(SUPERUSER_ID)
                    .isolation(Isolation::ReadCommitted)
                    .timeouts(timeouts()?),
            )
            .await?;
        match claim_work(&mut tx, target).await {
            Ok((claim, write)) if write => match tx.commit_pipelined(None).await {
                Ok(()) => Ok(claim),
                Err(CommitError::RolledBack(error)) => Err(error),
                Err(CommitError::Unknown(error)) => Err(indeterminate(target.model, error)),
            },
            Ok((claim, _)) => {
                tx.rollback().await?;
                Ok(claim)
            }
            // a lock wait past one second: someone else is claiming or running
            Err(error) if error.kind() == ErrorKind::Lock => {
                let _ = tx.discard_after(error).await;
                Ok(Claim::Busy)
            }
            Err(error) => Err(tx.discard_after(error).await),
        }
    }

    /// One batch, retried from a fresh selection on a serialization failure or deadlock.
    async fn batch(
        &self,
        target: &Target<'_>,
        lease: &Lease,
        options: &VacuumOptions,
        so_far: &VacuumReport,
        unknown_commit: &mut bool,
    ) -> Result<Step> {
        let mut attempt = 0u32;
        #[cfg(feature = "test-faults")]
        if let (Some((VacuumPause::BetweenBatches, key)), true) =
            (options.faults.gate, so_far.batches > 0)
        {
            let mut tx = self
                .begin_opts(
                    BeginOptions::new(SUPERUSER_ID)
                        .isolation(Isolation::ReadCommitted)
                        .timeouts(timeouts()?),
                )
                .await?;
            match wait_gate(&mut tx, key).await {
                Ok(()) => tx.rollback().await?,
                Err(error) => return Err(tx.discard_after(error).await),
            }
        }
        loop {
            attempt += 1;
            let mut tx = self
                .begin_opts(BeginOptions::new(SUPERUSER_ID).timeouts(timeouts()?))
                .await?;
            let outcome = match batch_work(&mut tx, target, lease, options, so_far).await {
                Ok((step, true)) => {
                    #[cfg(feature = "test-faults")]
                    if let Some(fault) = options.faults.commit {
                        tx.inject_commit_fault(fault);
                    }
                    match tx.commit_pipelined(None).await {
                        Ok(()) => Ok(step),
                        Err(CommitError::RolledBack(error)) => Err(error),
                        Err(CommitError::Unknown(error)) => {
                            *unknown_commit = true;
                            return Err(indeterminate(target.model, error));
                        }
                    }
                }
                Ok((step, false)) => match tx.rollback().await {
                    Ok(()) => Ok(step),
                    Err(error) => Err(error),
                },
                Err(error) => Err(tx.discard_after(error).await),
            };
            match outcome {
                Err(error) if is_retryable(&error) && attempt < MAX_ATTEMPTS => {
                    #[cfg(feature = "test-faults")]
                    RETRIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let bound = 20u64 << (attempt - 1).min(5);
                    tokio::time::sleep(Duration::from_millis(jitter(bound))).await;
                }
                other => return other,
            }
        }
    }

    /// Hand the lease back after a failure, when the state still shows it ours. A failure of this
    /// is logged: the lease then simply expires.
    async fn release_quietly(&self, target: &Target<'_>, lease: &Lease) {
        let result = async {
            let mut tx = self
                .begin_opts(
                    BeginOptions::new(SUPERUSER_ID)
                        .isolation(Isolation::ReadCommitted)
                        .timeouts(timeouts()?),
                )
                .await?;
            let work = tx
                .execute(RELEASE_SQL, &[&target.model, &lease.owner, &lease.epoch])
                .await;
            match work {
                Ok(_) => tx.commit().await,
                Err(error) => Err(tx.discard_after(error).await),
            }
        }
        .await;
        if let Err(error) = result {
            tracing::warn!(
                target: "erp::vacuum",
                model = target.model,
                epoch = lease.epoch,
                error = %error,
                "transient vacuum could not hand its lease back; it expires on its own"
            );
        }
    }
}

/// The claim's statements. Returns the claim and whether the transaction wrote (and so commits).
async fn claim_work(tx: &mut Transaction, target: &Target<'_>) -> Result<(Claim, bool)> {
    tx.execute(
        "INSERT INTO erp_transient_vacuum_state (model, next_due_at) \
         VALUES ($1, clock_timestamp()) ON CONFLICT (model) DO NOTHING",
        &[&target.model],
    )
    .await?;
    let rows = tx.query(CLAIM_LOCK_SQL, &[&target.model]).await?;
    let row = rows.first().ok_or_else(|| {
        Error::new(
            ErrorKind::Internal,
            format!(
                "transient vacuum: the state row of '{}' vanished",
                target.model
            ),
        )
    })?;
    let mut epoch: i64 = row.get(0);
    let held: bool = row.get(1);
    let phase: String = row.get(2);
    let stored_fingerprint: Option<String> = row.get(3);
    let due: bool = row.get(4);
    if held {
        return Ok((Claim::Busy, false));
    }
    if epoch == i64::MAX {
        return Err(Error::new(
            ErrorKind::Internal,
            format!(
                "transient vacuum of '{}': the epoch is at its maximum and cannot be incremented",
                target.model
            ),
        ));
    }
    let mut open = Phase::parse(&phase)?;
    let mut aborted = false;
    if open.is_some() && stored_fingerprint.as_deref() != Some(target.fingerprint.as_str()) {
        // the open cycle ran under another policy: end it before any of its metadata is used
        let rows = tx.query(CLAIM_ABORT_SQL, &[&target.model, &epoch]).await?;
        epoch = rows
            .first()
            .map(|r| r.get::<_, i64>(0))
            .ok_or_else(|| lost_row(target.model))?;
        open = None;
        aborted = true;
    }
    let (owner, epoch) = match open {
        Some(_) => {
            let rows = tx
                .query(CLAIM_CONTINUE_SQL, &[&target.model, &epoch, &LEASE_SECONDS])
                .await?;
            let row = rows.first().ok_or_else(|| lost_row(target.model))?;
            (row.get::<_, String>(0), row.get::<_, i64>(1))
        }
        None if !due => return Ok((Claim::NotDue { aborted }, aborted)),
        None => {
            let start = if target.age.is_some() {
                Phase::Age
            } else {
                Phase::Count
            };
            let rows = tx
                .query(
                    CLAIM_FRESH_SQL,
                    &[
                        &target.model,
                        &epoch,
                        &start.as_str(),
                        &target.fingerprint,
                        &LEASE_SECONDS,
                        &CADENCE_SECONDS,
                    ],
                )
                .await?;
            let row = rows.first().ok_or_else(|| lost_row(target.model))?;
            (row.get::<_, String>(0), row.get::<_, i64>(1))
        }
    };
    Ok((
        Claim::Held {
            lease: Lease { owner, epoch },
            aborted,
        },
        true,
    ))
}

fn lost_row(model: &str) -> Error {
    Error::new(
        ErrorKind::Internal,
        format!("transient vacuum: the locked state row of '{model}' was not updated"),
    )
}

/// The write of a batch's outcome. False when the owner or epoch no longer matches.
#[allow(clippy::too_many_arguments, reason = "one statement's parameters")]
async fn write_progress(
    tx: &mut Transaction,
    target: &Target<'_>,
    lease: &Lease,
    phase: &str,
    latched: bool,
    rows: i64,
    batches: i64,
    release: bool,
    bump: bool,
) -> Result<bool> {
    let changed = tx
        .execute(
            BATCH_UPDATE_SQL,
            &[
                &target.model,
                &lease.owner,
                &lease.epoch,
                &phase,
                &latched,
                &rows,
                &batches,
                &release,
                &bump,
                &LEASE_SECONDS,
                &CADENCE_SECONDS,
            ],
        )
        .await?;
    Ok(changed == 1)
}

/// The statements of one batch. The bool says whether the transaction is to be committed (true) or
/// rolled back (false).
async fn batch_work(
    tx: &mut Transaction,
    target: &Target<'_>,
    lease: &Lease,
    options: &VacuumOptions,
    so_far: &VacuumReport,
) -> Result<(Step, bool)> {
    // `begin_opts` sends BEGIN, the limits and the signal read in its opening flight, so the
    // REPEATABLE READ snapshot is taken before this statement. The lock query is the first
    // statement of this function, not of the transaction: if another transaction committed a
    // change to the state row in between, FOR UPDATE fails with a serialization error and the
    // batch is retried from a fresh snapshot, so the checks below always see the current row.
    let rows = tx.query(BATCH_LOCK_SQL, &[&target.model]).await?;
    #[cfg(feature = "test-faults")]
    if let Some((VacuumPause::AfterSnapshot, key)) = options.faults.gate {
        let ready = options.faults.ready.ok_or_else(|| {
            Error::new(ErrorKind::Internal, "vacuum test gate without a ready lock")
        })?;
        // blocking: a lock that is not taken fails the batch loudly (lock timeout)
        tx.query("SELECT pg_advisory_xact_lock($1)", &[&ready])
            .await?;
        wait_gate(tx, key).await?;
    }
    let Some(row) = rows.first() else {
        return Ok((Step::Lost, false));
    };
    let owner: Option<String> = row.get(0);
    let epoch: i64 = row.get(1);
    let live: bool = row.get(2);
    let phase: String = row.get(3);
    let mut latched: bool = row.get(4);
    let stored_fingerprint: Option<String> = row.get(5);
    let cycle_rows = to_u64(row.get(6), "rows_total")?;
    let cycle_batches = to_u64(row.get(7), "batches_total")?;
    if owner.as_deref() != Some(lease.owner.as_str()) || epoch != lease.epoch || !live {
        return Ok((Step::Lost, false));
    }
    let Some(phase) = Phase::parse(&phase)? else {
        return Ok((Step::Lost, false));
    };
    if stored_fingerprint.as_deref() != Some(target.fingerprint.as_str()) {
        // abort the cycle: end it and hand the lease back, so nothing runs on stale metadata
        let ok = write_progress(tx, target, lease, "idle", false, 0, 0, true, false).await?;
        return Ok((if ok { Step::PolicyChanged } else { Step::Lost }, ok));
    }
    let finish = |end: End, phase: Phase, latched: bool, rows: u64, batches: u64| {
        (end, phase, latched, rows, batches)
    };
    let table = &target.table;
    let mut phase = phase;
    // the cycle's own budget may be spent already (an inherited cycle)
    let cycle_rows_left = HARD_CYCLE_ROWS.saturating_sub(cycle_rows);
    let cycle_batches_left = u64::from(HARD_CYCLE_BATCHES).saturating_sub(cycle_batches);
    let call_rows_left = options.rows.saturating_sub(so_far.deleted_rows);
    let call_batches_left = u64::from(options.batches).saturating_sub(so_far.batches);
    if cycle_rows_left == 0 || cycle_batches_left == 0 {
        let ok = write_progress(tx, target, lease, "idle", false, 0, 0, true, true).await?;
        return Ok((
            if ok {
                Step::Done {
                    end: End::Exhausted,
                    rows: 0,
                    cycle_rows,
                    cycle_batches,
                }
            } else {
                Step::Lost
            },
            ok,
        ));
    }
    if call_rows_left == 0 || call_batches_left == 0 {
        let ok = write_progress(
            tx,
            target,
            lease,
            phase.as_str(),
            latched,
            0,
            0,
            true,
            false,
        )
        .await?;
        return Ok((
            if ok {
                Step::Done {
                    end: End::Pending,
                    rows: 0,
                    cycle_rows,
                    cycle_batches,
                }
            } else {
                Step::Lost
            },
            ok,
        ));
    }

    // the count phase decides once whether the limit is exceeded, then stays latched
    if phase == Phase::Count && !latched {
        let max = target.count.ok_or_else(|| {
            Error::new(
                ErrorKind::Internal,
                format!(
                    "transient vacuum of '{}': count phase without a count limit",
                    target.model
                ),
            )
        })?;
        let rows = tx
            .query(&format!("SELECT count(*) FROM {table}"), &[])
            .await?;
        let total = rows
            .first()
            .map(|r| r.get::<_, i64>(0))
            .ok_or_else(|| count_err("the row count"))?;
        let total = to_u64(total, "row count")?;
        if total > max {
            latched = true;
        } else {
            let ok = write_progress(tx, target, lease, "idle", false, 0, 0, true, false).await?;
            return Ok((
                if ok {
                    Step::Done {
                        end: End::Completed,
                        rows: 0,
                        cycle_rows,
                        cycle_batches,
                    }
                } else {
                    Step::Lost
                },
                ok,
            ));
        }
    }

    let seconds = match phase {
        Phase::Age => target
            .age
            .map_or(RECENT_WRITE_SECONDS, |a| a.max(RECENT_WRITE_SECONDS)),
        Phase::Count => RECENT_WRITE_SECONDS,
    };
    let seconds = to_i64(seconds, "age")?;
    let limit = u64::from(options.batch_rows)
        .min(u64::from(HARD_BATCH_ROWS))
        .min(cycle_rows_left)
        .min(call_rows_left);
    let limit = to_i64(limit, "batch size")?;
    let select = format!(
        "SELECT id FROM {table} WHERE {OLDER_THAN} ORDER BY id LIMIT $3 FOR UPDATE SKIP LOCKED"
    );
    let ids: Vec<i64> = tx
        .query(&select, &[&target.model, &seconds, &limit])
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect();

    let (end, next_phase, next_latched, add_rows, add_batches) = if ids.is_empty() {
        // Rows other transactions hold locked are skipped: an empty selection proves nothing until
        // a plain read says no eligible row is left.
        let left = tx
            .query(
                &format!("SELECT EXISTS (SELECT 1 FROM {table} WHERE {OLDER_THAN})"),
                &[&target.model, &seconds],
            )
            .await?;
        let remaining = left
            .first()
            .map(|r| r.get::<_, bool>(0))
            .ok_or_else(|| count_err("the remaining-work check"))?;
        if remaining {
            finish(End::Pending, phase, latched, 0, 0)
        } else if phase == Phase::Age && target.count.is_some() {
            phase = Phase::Count;
            finish(End::Continue, phase, false, 0, 0)
        } else {
            finish(End::Completed, phase, latched, 0, 0)
        }
    } else {
        let selected = ids.len() as u64;
        tx.env()
            .records(target.model)?
            .browse(ids)?
            .unlink(tx)
            .await?;
        let new_rows = cycle_rows + selected;
        let new_batches = cycle_batches + 1;
        let end = if new_rows >= HARD_CYCLE_ROWS || new_batches >= u64::from(HARD_CYCLE_BATCHES) {
            End::Exhausted
        } else if so_far.batches + 1 >= u64::from(options.batches)
            || so_far.deleted_rows + selected >= options.rows
        {
            End::Pending
        } else {
            End::Continue
        };
        finish(end, phase, latched, selected, 1)
    };
    let (store_phase, store_latched, release, bump) = match end {
        End::Continue => (next_phase.as_str(), next_latched, false, false),
        End::Completed => ("idle", false, true, false),
        End::Exhausted => ("idle", false, true, true),
        End::Pending => (next_phase.as_str(), next_latched, true, false),
    };
    let ok = write_progress(
        tx,
        target,
        lease,
        store_phase,
        store_latched,
        to_i64(add_rows, "rows")?,
        to_i64(add_batches, "batches")?,
        release,
        bump,
    )
    .await?;
    if !ok {
        return Ok((Step::Lost, false));
    }
    Ok((
        Step::Done {
            end,
            rows: add_rows,
            cycle_rows: cycle_rows + add_rows,
            cycle_batches: cycle_batches + add_batches,
        },
        true,
    ))
}
