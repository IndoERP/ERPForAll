//! Operator actions on jobs and the recurring rows schedules own (D10.2, D14).
//!
//! `hold`, `resume`, `cancel`, `requeue` and `discard` need the job administration capability
//! (`jobs.admin`, declared by `erp-security`, mapped by base to the system administrators group,
//! given to the ORM with `OrmBuilder::job_admin_capability`) or the system principal. They run in the caller's
//! transaction, so an action commits or rolls back with the caller's own work. Until
//! ships its trail, each is logged at `info` with the actor and the job's uuid, and `requeue`
//! and `discard`, which change a dead letter, say so.

use erp_fields::{Error, Result, TimestampTz, Uuid};

use super::enqueue::kind_map;
use super::error;
use super::{JobKey, JobRef, JobState, PrincipalPolicy, Priority, When};
use crate::principal::{PrincipalKind, SUPERUSER_ID, UserId};
use crate::{Env, Transaction};

/// Most keys one [`Transaction::job_cancel_by_keys`] call takes.
pub const MAX_CANCEL_KEYS: usize = 10_000;

/// How a cancel took effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CancelOutcome {
    /// The job was not running: it left the queue now (with a `cancelled` history row).
    Deleted,
    /// The job is running: it will stop at its next fence and then leave the queue.
    Requested,
}

/// The one recurring job row a schedule owns (2): a job of `kind` whose
/// `dedup_key` and `singleton_key` are `key`, so there is at most one, and at most one runs.
///
/// ```
/// use erp_orm::jobs::{JobKey, RecurringSpec, When};
/// use std::time::Duration;
/// let spec = RecurringSpec::new("core.jobs_history_retention", JobKey::new("core.jobs_history_retention")?)
///     .first_run(When::After(Duration::from_secs(60)));
/// assert_eq!(spec.kind(), "core.jobs_history_retention");
/// # Ok::<(), erp_fields::Error>(())
/// ```
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RecurringSpec {
    kind: &'static str,
    key: JobKey,
    run_after: When,
    priority: Option<Priority>,
    held: bool,
    run_as: Option<UserId>,
    payload: serde_json::Value,
}

impl RecurringSpec {
    /// A recurring row of `kind` keyed `key`, due now, ready.
    pub fn new(kind: &'static str, key: JobKey) -> Self {
        Self {
            kind,
            key,
            run_after: When::Now,
            priority: None,
            held: false,
            run_as: None,
            payload: serde_json::Value::Object(serde_json::Map::new()),
        }
    }

    /// The kind.
    pub fn kind(&self) -> &'static str {
        self.kind
    }

    /// When the row is due.
    #[must_use]
    pub fn first_run(mut self, when: When) -> Self {
        self.run_after = when;
        self
    }

    /// The priority, instead of the kind's.
    #[must_use]
    pub fn priority(mut self, priority: Priority) -> Self {
        self.priority = Some(priority);
        self
    }

    /// Keep the row parked (an inactive schedule): it exists but is never claimed.
    #[must_use]
    pub fn held(mut self, held: bool) -> Self {
        self.held = held;
        self
    }

    /// The user the job runs as (kinds that run as the system ignore it and refuse it).
    #[must_use]
    pub fn run_as(mut self, user: UserId) -> Self {
        self.run_as = Some(user);
        self
    }

    /// The payload.
    ///
    /// # Errors
    ///
    /// `Validation` when `value` does not serialize.
    pub fn payload<T: serde::Serialize>(mut self, value: &T) -> Result<Self> {
        self.payload =
            serde_json::to_value(value).map_err(|_| error::payload_unserializable(self.kind))?;
        Ok(self)
    }
}

const FIND_SQL: &str = "\
SELECT id, job_uuid, state, run_as_kind, run_as_user FROM erp_job
 WHERE kind = $1 AND dedup_key = $2 AND state <> 'dead' FOR UPDATE SKIP LOCKED";

const FIND_LOCKED_SQL: &str = "\
SELECT id FROM erp_job WHERE kind = $1 AND dedup_key = $2 AND state <> 'dead'";

const RESCHEDULE_SQL: &str = "\
UPDATE erp_job SET run_after = COALESCE($2::timestamptz, clock_timestamp() + make_interval(secs => $3::float8)),
       priority = $4::int2, state = $5
 WHERE id = $1 RETURNING job_uuid";

const LOCK_NOWAIT_SQL: &str = "SELECT 1 FROM erp_job WHERE job_uuid = $1 FOR UPDATE NOWAIT";

const STATE_SQL: &str = "SELECT state FROM erp_job WHERE job_uuid = $1";

impl Transaction {
    /// Whether `env` may administer jobs: the system principal, or a user holding `jobs.admin`.
    async fn require_job_admin(&mut self, env: &Env, call: &str) -> Result<()> {
        if env.principal().kind() == PrincipalKind::System {
            return Ok(());
        }
        let capability = self.shared.job_admin.ok_or_else(|| {
            Error::programming(format!(
                "{call} needs the capability that authorizes job administration, and this ORM was built without one (OrmBuilder::job_admin_capability)"
            ))
        })?;
        if self.has_capability(env, capability).await? {
            Ok(())
        } else {
            Err(error::needs_job_admin(call))
        }
    }

    /// The state of the job with `uuid`, for an error message (`None`: gone).
    async fn job_state_of(&mut self, uuid: Uuid) -> Result<Option<JobState>> {
        let rows = self.query(STATE_SQL, &[&uuid]).await?;
        match rows.first() {
            Some(row) => {
                let text: String = row
                    .try_get(0)
                    .map_err(|e| Error::internal("erp_job state did not decode").with_source(e))?;
                Ok(Some(JobState::parse(&text)?))
            }
            None => Ok(None),
        }
    }

    /// The error for an action that matched no row: the job is gone, or in another state.
    async fn explain_no_match(&mut self, call: &str, job: JobRef, wanted: &str) -> Error {
        match self.job_state_of(job.uuid()).await {
            Ok(Some(state)) => {
                error::job_state(call, &job.uuid().to_string(), state.as_str(), wanted)
            }
            Ok(None) => error::job_missing(call, &job.uuid().to_string()),
            Err(error) => error,
        }
    }

    /// Park a ready job: it is never claimed until [`job_resume`](Self::job_resume).
    ///
    /// # Errors
    ///
    /// `Access` without `jobs.admin`; `UserError` when the job is not `ready` (a running job
    /// cannot be held: cancel it or let it finish); `Missing` when it is gone.
    pub async fn job_hold(&mut self, env: &Env, job: JobRef) -> Result<()> {
        self.require_job_admin(env, "job_hold").await?;
        let rows = self
            .query(
                "UPDATE erp_job SET state = 'held' WHERE job_uuid = $1 AND state = 'ready' RETURNING 1",
                &[&job.uuid()],
            )
            .await?;
        if rows.is_empty() {
            return Err(self.explain_no_match("job_hold", job, "ready").await);
        }
        log_action(env, "hold", job);
        Ok(())
    }

    /// Return a held job to `ready`.
    ///
    /// # Errors
    ///
    /// As [`job_hold`](Self::job_hold), for a job that is not `held`.
    pub async fn job_resume(&mut self, env: &Env, job: JobRef) -> Result<()> {
        self.require_job_admin(env, "job_resume").await?;
        let rows = self
            .query(
                "UPDATE erp_job SET state = 'ready' WHERE job_uuid = $1 AND state = 'held' RETURNING 1",
                &[&job.uuid()],
            )
            .await?;
        if rows.is_empty() {
            return Err(self.explain_no_match("job_resume", job, "held").await);
        }
        log_action(env, "resume", job);
        Ok(())
    }

    /// Cancel a job. A job that is not running leaves the queue now, with a `cancelled` history
    /// row; a running job is asked to stop and leaves at its next fence.
    ///
    /// # Errors
    ///
    /// `Access` without `jobs.admin`; `Missing` when the job is gone.
    pub async fn job_cancel(&mut self, env: &Env, job: JobRef) -> Result<CancelOutcome> {
        self.require_job_admin(env, "job_cancel").await?;
        // never wait behind a chunk's row lock: say so and let the operator try again
        match self.query(LOCK_NOWAIT_SQL, &[&job.uuid()]).await {
            Ok(_) => {}
            Err(error) if error.sqlstate() == Some("55P03") => {
                return Err(Error::user(format!(
                    "job {} is being written by a running chunk; cancel it again when the \
                     chunk has committed",
                    job.uuid()
                )));
            }
            Err(error) => return Err(error),
        }
        let removed = self.query(CANCEL_WAITING_SQL, &[&job.uuid()]).await?;
        if !removed.is_empty() {
            log_action(env, "cancel", job);
            return Ok(CancelOutcome::Deleted);
        }
        let flagged = self
            .query(
                "UPDATE erp_job SET cancel_requested = true WHERE job_uuid = $1 AND state = 'running' RETURNING 1",
                &[&job.uuid()],
            )
            .await?;
        if flagged.is_empty() {
            return Err(error::job_missing("job_cancel", &job.uuid().to_string()));
        }
        log_action(env, "cancel", job);
        Ok(CancelOutcome::Requested)
    }

    /// Cancel the live jobs of one `kind` and `dedup_key` that the caller enqueued, and return how
    /// many were cancelled. No administrator capability: it is the owner's own
    /// tool, for an addon that keeps one job per business record and must drop it when the record
    /// is cancelled or rescheduled. A job counts only when its stored principal is the caller's
    /// user (`run_as_user`); a job of another user is left alone, as if the key were missing. In
    /// superuser mode ([`Env::su`], reachable by server code only, never from an RPC or a
    /// context key) every user's job of the key is cancelled. Waiting jobs (`ready`, `held`) leave
    /// the queue now with a `cancelled` history row; a running job is asked to stop and leaves at
    /// its next fence. Dead letters are not live and are left for the operator. A row another
    /// transaction has locked (a claim in flight) is skipped, so it is not counted.
    ///
    /// It returns the count and never fails because nothing matched: **the caller states the count
    /// it expects and fails loudly when it differs**, as the scheduled-message methods do.
    ///
    /// # Errors
    ///
    /// The database's errors.
    pub async fn job_cancel_by_key(&mut self, env: &Env, kind: &str, key: &JobKey) -> Result<u64> {
        self.job_cancel_by_keys(env, kind, std::slice::from_ref(key))
            .await
    }

    /// [`job_cancel_by_key`](Self::job_cancel_by_key) for many keys of one kind in two statements
    /// (5): the set-based form for an addon that drops the jobs of every row of a
    /// deleted record. The same rules apply to each key; the count is the total, at most
    /// `keys.len()` because a key has at most one live job. At most [`MAX_CANCEL_KEYS`] keys per
    /// call.
    ///
    /// # Errors
    ///
    /// `Validation` for more than [`MAX_CANCEL_KEYS`] keys; the database's errors.
    pub async fn job_cancel_by_keys(
        &mut self,
        env: &Env,
        kind: &str,
        keys: &[JobKey],
    ) -> Result<u64> {
        if keys.is_empty() {
            return Ok(0);
        }
        if keys.len() > MAX_CANCEL_KEYS {
            return Err(Error::validation(format!(
                "job_cancel_by_keys takes at most {MAX_CANCEL_KEYS} keys per call, not {}",
                keys.len()
            )));
        }
        let owner: Option<i64> = if env.su() { None } else { Some(env.uid()) };
        let names: Vec<&str> = keys.iter().map(JobKey::as_str).collect();
        let waiting = self
            .query(CANCEL_BY_KEY_SQL, &[&kind, &names, &owner])
            .await?;
        let running = self
            .query(
                "UPDATE erp_job SET cancel_requested = true WHERE kind = $1 AND dedup_key = ANY($2::text[]) \
                 AND state = 'running' AND ($3::int8 IS NULL OR run_as_user = $3::int8) RETURNING 1",
                &[&kind, &names, &owner],
            )
            .await?;
        let count = u64::try_from(waiting.len() + running.len()).unwrap_or(u64::MAX);
        tracing::info!(
            target: "erp::jobs::admin",
            kind,
            keys = keys.len(),
            actor = env.uid(),
            count,
            "jobs cancelled by key (a later release will record it in the audit trail)"
        );
        Ok(count)
    }

    /// Send a dead letter back to `ready` with its attempts reset, due now.
    ///
    /// # Errors
    ///
    /// As [`job_hold`](Self::job_hold), for a job that is not `dead`.
    pub async fn job_requeue(&mut self, env: &Env, job: JobRef) -> Result<()> {
        self.require_job_admin(env, "job_requeue").await?;
        let rows = self
            .query(
                "UPDATE erp_job SET state = 'ready', attempts = 0, failure_rank = 0, dead_at = NULL, \
                 run_after = clock_timestamp() WHERE job_uuid = $1 AND state = 'dead' RETURNING 1",
                &[&job.uuid()],
            )
            .await?;
        if rows.is_empty() {
            return Err(self.explain_no_match("job_requeue", job, "dead").await);
        }
        log_action(env, "requeue", job);
        Ok(())
    }

    /// Remove a dead letter, leaving a `dead_discarded` history row.
    ///
    /// # Errors
    ///
    /// As [`job_hold`](Self::job_hold), for a job that is not `dead`.
    pub async fn job_discard(&mut self, env: &Env, job: JobRef) -> Result<()> {
        self.require_job_admin(env, "job_discard").await?;
        let rows = self.query(DISCARD_SQL, &[&job.uuid()]).await?;
        if rows.is_empty() {
            return Err(self.explain_no_match("job_discard", job, "dead").await);
        }
        log_action(env, "discard", job);
        Ok(())
    }

    /// Make sure the recurring row of `spec` exists with its due time, priority and state: the
    /// one statement path `ir.cron` and the core schedules use to keep exactly one job row per
    /// schedule (2). A row of another principal is replaced (the stored principal
    /// is never updated in place). Only the system principal may call it: the schedule's owner
    /// has already passed its own access check.
    ///
    /// # Errors
    ///
    /// `Access` unless the caller is the system principal; `Programming` for an unregistered
    /// kind or a `run_as` on a kind that runs as the system, or a user-run kind without one;
    /// `UserError` when the existing row is running (a running schedule is not rewritten).
    pub async fn job_upsert_recurring(&mut self, env: &Env, spec: RecurringSpec) -> Result<JobRef> {
        if env.principal().kind() != PrincipalKind::System {
            return Err(Error::access(
                "job_upsert_recurring is for the system principal: schedules are kept by the \
                 modules that own them",
            ));
        }
        let decl = *kind_map()
            .get(spec.kind)
            .ok_or_else(|| error::unknown_kind(spec.kind))?;
        let (kind_text, user) = match (decl.principal, spec.run_as) {
            (PrincipalPolicy::System, None) => (PrincipalKind::System.as_str(), SUPERUSER_ID.get()),
            (PrincipalPolicy::System, Some(_)) => {
                return Err(Error::programming(format!(
                    "job kind '{}' runs as the system principal; run_as does not apply to it",
                    spec.kind
                )));
            }
            (PrincipalPolicy::Caller, Some(user)) => (PrincipalKind::User.as_str(), user.get()),
            (PrincipalPolicy::Caller, None) => {
                return Err(Error::programming(format!(
                    "the recurring job of kind '{}' needs the user it runs as (run_as)",
                    spec.kind
                )));
            }
        };
        let key = spec.key.as_str().to_owned();
        let at: Option<TimestampTz> = match spec.run_after {
            When::At(at) => Some(at),
            _ => None,
        };
        let after: f64 = match spec.run_after {
            When::After(d) => d.as_secs_f64(),
            _ => 0.0,
        };
        let priority = i16::from(spec.priority.unwrap_or(decl.priority).get());
        let state = if spec.held { "held" } else { "ready" };
        let found = self.query(FIND_SQL, &[&spec.kind, &key]).await?;
        if let Some(row) = found.first() {
            let id: i64 = row.try_get(0).map_err(undecodable)?;
            let uuid: Uuid = row.try_get(1).map_err(undecodable)?;
            let current: String = row.try_get(2).map_err(undecodable)?;
            let current_kind: String = row.try_get(3).map_err(undecodable)?;
            let current_user: i64 = row.try_get(4).map_err(undecodable)?;
            if current == "running" {
                return Err(Error::user(format!(
                    "the recurring job {key} of kind '{}' is running and is not rewritten",
                    spec.kind
                )));
            }
            if current_kind == kind_text && current_user == user {
                self.query(RESCHEDULE_SQL, &[&id, &at, &after, &priority, &state])
                    .await?;
                return Ok(JobRef::new(id, uuid));
            }
            // another principal: the stored principal is immutable, so the row is replaced
            self.execute("DELETE FROM erp_job WHERE id = $1", &[&id])
                .await?;
        } else if !self
            .query(FIND_LOCKED_SQL, &[&spec.kind, &key])
            .await?
            .is_empty()
        {
            // a live row exists but a worker holds it (running): not rewritten
            return Err(Error::user(format!(
                "the recurring job {key} of kind '{}' is running and is not rewritten",
                spec.kind
            )));
        }
        let mut request = super::EnqueueRequest::new(spec.kind)
            .at(spec.run_after)
            .dedup(spec.key.clone(), super::OnDuplicate::Keep)
            .singleton(spec.key);
        if let Some(priority) = spec.priority {
            request = request.priority(priority);
        }
        request = request.with_payload_value(spec.payload);
        let run_as = spec.run_as.map(super::RunAs::User);
        if let Some(run_as) = run_as {
            request = request.run_as(run_as);
        }
        let outcome = self.enqueue(env, request).await?;
        if spec.held {
            self.execute(
                "UPDATE erp_job SET state = 'held' WHERE id = $1 AND state = 'ready'",
                &[&outcome.job().id()],
            )
            .await?;
        }
        Ok(outcome.job())
    }

    /// Move the due time of the recurring row `key` of `kind` to `at` (a trigger that is due
    /// earlier than the schedule). Returns whether a ready or held row was changed.
    ///
    /// # Errors
    ///
    /// `Access` unless the caller is the system principal.
    pub async fn job_set_run_after(
        &mut self,
        env: &Env,
        kind: &str,
        key: &JobKey,
        at: TimestampTz,
    ) -> Result<bool> {
        if env.principal().kind() != PrincipalKind::System {
            return Err(Error::access(
                "job_set_run_after is for the system principal: schedules are kept by the \
                 modules that own them",
            ));
        }
        let rows = self
            .query(
                "UPDATE erp_job SET run_after = $3 WHERE kind = $1 AND dedup_key = $2 \
                 AND state IN ('ready', 'held') RETURNING 1",
                &[&kind, &key.as_str(), &at],
            )
            .await?;
        Ok(!rows.is_empty())
    }
}

fn undecodable(source: tokio_postgres::Error) -> Error {
    Error::internal("an erp_job row did not decode").with_source(source)
}

fn log_action(env: &Env, action: &str, job: JobRef) {
    tracing::info!(
        target: "erp::jobs::admin",
        action,
        job = %job.uuid(),
        actor = env.principal().subject().get(),
        "job administration action (a later release will record it in the audit trail)"
    );
}

/// A job that is not running leaves the queue with a `cancelled` history row.
const CANCEL_WAITING_SQL: &str = "\
WITH gone AS (
  DELETE FROM erp_job WHERE job_uuid = $1 AND state IN ('ready', 'held', 'dead')
  RETURNING job_uuid, kind, kind_version, attempts, enqueued_at, started_at, items_done,
            items_failed, run_as_kind, run_as_user, enqueued_by_user, correlation_id,
            last_error_code),
failures AS (
  DELETE FROM erp_job_item_failure f USING gone WHERE f.job_uuid = gone.job_uuid RETURNING 1)
INSERT INTO erp_job_run (job_uuid, kind, kind_version, outcome, attempts, enqueued_at, started_at,
                         items_done, items_failed, run_as_kind, run_as_user, enqueued_by_user,
                         correlation_id, last_error_code)
SELECT job_uuid, kind, kind_version, 'cancelled', attempts, enqueued_at, started_at, items_done,
       items_failed, run_as_kind, run_as_user, enqueued_by_user, correlation_id, last_error_code
  FROM gone
RETURNING 1";

/// The waiting jobs of one kind and key that `$3` (a user id, or null for any) enqueued leave the
/// queue with a `cancelled` history row; a row another transaction holds is skipped.
const CANCEL_BY_KEY_SQL: &str = "\
WITH picked AS (
  SELECT id FROM erp_job
   WHERE kind = $1 AND dedup_key = ANY($2::text[]) AND state IN ('ready', 'held')
     AND ($3::int8 IS NULL OR run_as_user = $3::int8)
   FOR UPDATE SKIP LOCKED),
gone AS (
  DELETE FROM erp_job j USING picked WHERE j.id = picked.id
  RETURNING j.job_uuid, j.kind, j.kind_version, j.attempts, j.enqueued_at, j.started_at,
            j.items_done, j.items_failed, j.run_as_kind, j.run_as_user, j.enqueued_by_user,
            j.correlation_id, j.last_error_code),
failures AS (
  DELETE FROM erp_job_item_failure f USING gone WHERE f.job_uuid = gone.job_uuid RETURNING 1)
INSERT INTO erp_job_run (job_uuid, kind, kind_version, outcome, attempts, enqueued_at, started_at,
                         items_done, items_failed, run_as_kind, run_as_user, enqueued_by_user,
                         correlation_id, last_error_code)
SELECT job_uuid, kind, kind_version, 'cancelled', attempts, enqueued_at, started_at, items_done,
       items_failed, run_as_kind, run_as_user, enqueued_by_user, correlation_id, last_error_code
  FROM gone
RETURNING 1";

const DISCARD_SQL: &str = "\
WITH gone AS (
  DELETE FROM erp_job WHERE job_uuid = $1 AND state = 'dead'
  RETURNING job_uuid, kind, kind_version, attempts, enqueued_at, started_at, items_done,
            items_failed, run_as_kind, run_as_user, enqueued_by_user, correlation_id,
            last_error_code),
failures AS (
  DELETE FROM erp_job_item_failure f USING gone WHERE f.job_uuid = gone.job_uuid RETURNING 1)
INSERT INTO erp_job_run (job_uuid, kind, kind_version, outcome, attempts, enqueued_at, started_at,
                         items_done, items_failed, run_as_kind, run_as_user, enqueued_by_user,
                         correlation_id, last_error_code)
SELECT job_uuid, kind, kind_version, 'dead_discarded', attempts, enqueued_at, started_at,
       items_done, items_failed, run_as_kind, run_as_user, enqueued_by_user, correlation_id,
       last_error_code
  FROM gone
RETURNING 1";
