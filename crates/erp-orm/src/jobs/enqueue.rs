//! Enqueue: add jobs inside the business transaction (4, D9.1, design §5.1).
//!
//! The job commits with the data that caused it, or never exists. One set-based `INSERT`
//! (`unnest` arrays) writes any number of jobs; a job with a `dedup_key` that a live job of the
//! kind already holds is coalesced into it instead.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use erp_fields::{Error, Result, TimestampTz, Uuid};

use super::error;
use super::kind::{JobKindDecl, PrincipalPolicy};
use super::{JobKey, JobRef, Priority, StoredPrincipal};
use crate::principal::{PrincipalKind, SUPERUSER_ID, UserId};
use crate::{Env, Transaction};

/// Most jobs one [`Transaction::enqueue_many`] call takes.
pub const MAX_ENQUEUE_BATCH: usize = 1_024;

/// When a job becomes due.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum When {
    /// As soon as the transaction commits (the database clock decides).
    Now,
    /// At a given instant.
    At(TimestampTz),
    /// This long after the insert, on the database clock.
    After(Duration),
}

/// What a second enqueue with a live `dedup_key` does to the job already there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OnDuplicate {
    /// Leave it as it is.
    Keep,
    /// Make it due at the earlier of the two times.
    Earliest,
}

/// Which principal runs the job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RunAs {
    /// The principal of the enqueuing environment (the default).
    Caller,
    /// Another user. Allowed for the `System` principal; anyone else needs the audited
    /// `jobs.enqueue_as` elevation, which does not exist yet, so it is refused.
    User(UserId),
}

/// The result of one enqueue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EnqueueOutcome {
    /// A new job.
    Inserted(JobRef),
    /// A live job of the kind already held the `dedup_key`; this is that job.
    Coalesced(JobRef),
}

impl EnqueueOutcome {
    /// The job, new or existing.
    pub fn job(&self) -> JobRef {
        match self {
            Self::Inserted(job) | Self::Coalesced(job) => *job,
        }
    }

    /// Whether this call created the job.
    pub fn is_new(&self) -> bool {
        matches!(self, Self::Inserted(_))
    }
}

/// A job to add. Built with [`EnqueueRequest::new`]; the payload is serialized once and its size
/// checked before anything is sent.
///
/// ```
/// use erp_orm::jobs::{EnqueueRequest, JobKey, OnDuplicate, Priority, When};
/// use std::time::Duration;
/// let request = EnqueueRequest::new("stock.recount")
///     .payload(&serde_json::json!({"warehouse": 3}))?
///     .at(When::After(Duration::from_secs(60)))
///     .priority(Priority::new(2)?)
///     .dedup(JobKey::new("recount:3")?, OnDuplicate::Earliest);
/// assert_eq!(request.kind(), "stock.recount");
/// # Ok::<(), erp_fields::Error>(())
/// ```
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct EnqueueRequest {
    kind: &'static str,
    payload: serde_json::Value,
    when: When,
    priority: Option<Priority>,
    singleton: Option<JobKey>,
    dedup: Option<(JobKey, OnDuplicate)>,
    run_as: RunAs,
    max_attempts: Option<u32>,
}

impl EnqueueRequest {
    /// A job of `kind` with an empty payload, due now, with the kind's priority and attempts.
    pub fn new(kind: &'static str) -> Self {
        Self {
            kind,
            payload: serde_json::Value::Object(serde_json::Map::new()),
            when: When::Now,
            priority: None,
            singleton: None,
            dedup: None,
            run_as: RunAs::Caller,
            max_attempts: None,
        }
    }

    /// The kind.
    pub fn kind(&self) -> &'static str {
        self.kind
    }

    /// Replace the payload with an already built value.
    pub(crate) fn with_payload_value(mut self, value: serde_json::Value) -> Self {
        self.payload = value;
        self
    }

    /// The payload: data the job reads, never evaluated, never a credential.
    ///
    /// # Errors
    ///
    /// `Validation` when `value` does not serialize to JSON.
    pub fn payload<T: serde::Serialize>(mut self, value: &T) -> Result<Self> {
        self.payload =
            serde_json::to_value(value).map_err(|_| error::payload_unserializable(self.kind))?;
        Ok(self)
    }

    /// When the job is due.
    #[must_use]
    pub fn at(mut self, when: When) -> Self {
        self.when = when;
        self
    }

    /// The priority, instead of the kind's default.
    #[must_use]
    pub fn priority(mut self, priority: Priority) -> Self {
        self.priority = Some(priority);
        self
    }

    /// At most one job holding this key runs at a time, across all nodes.
    #[must_use]
    pub fn singleton(mut self, key: JobKey) -> Self {
        self.singleton = Some(key);
        self
    }

    /// Coalesce into a live job of this kind that holds the same key.
    #[must_use]
    pub fn dedup(mut self, key: JobKey, on_duplicate: OnDuplicate) -> Self {
        self.dedup = Some((key, on_duplicate));
        self
    }

    /// Run as another principal than the caller (see [`RunAs`]).
    #[must_use]
    pub fn run_as(mut self, who: RunAs) -> Self {
        self.run_as = who;
        self
    }

    /// Attempts before a transient failure dead-letters the job, instead of the kind's.
    ///
    /// # Errors
    ///
    /// `Validation` outside 1..=100.
    pub fn max_attempts(mut self, attempts: u32) -> Result<Self> {
        if !(1..=100).contains(&attempts) {
            return Err(error::invalid_attempts(self.kind, attempts, 100));
        }
        self.max_attempts = Some(attempts);
        Ok(self)
    }
}

/// The registered kinds by name, built once per process (the slice is immutable).
pub(crate) fn kind_map() -> &'static HashMap<&'static str, &'static JobKindDecl> {
    static MAP: OnceLock<HashMap<&'static str, &'static JobKindDecl>> = OnceLock::new();
    MAP.get_or_init(|| super::JOB_KINDS.iter().map(|k| (k.name, k)).collect())
}

/// The size `value` has once stored as `jsonb` and read back as text, which is what the
/// table's `CHECK` measures: compact JSON plus the space `jsonb` prints after each `:` and `,`.
pub(crate) fn jsonb_text_len(value: &serde_json::Value) -> usize {
    use serde_json::Value;
    match value {
        Value::Null => 4,
        Value::Bool(true) => 4,
        Value::Bool(false) => 5,
        Value::Number(n) => n.to_string().len(),
        Value::String(s) => serde_json::to_string(s).map_or(s.len() + 2, |t| t.len()),
        Value::Array(items) => {
            2 + items.iter().map(jsonb_text_len).sum::<usize>() + items.len().saturating_sub(1) * 2
        }
        Value::Object(map) => {
            2 + map
                .iter()
                .map(|(k, v)| {
                    serde_json::to_string(k).map_or(k.len() + 2, |t| t.len())
                        + 2
                        + jsonb_text_len(v)
                })
                .sum::<usize>()
                + map.len().saturating_sub(1) * 2
        }
    }
}

/// One validated request, ready to bind.
struct Row {
    uuid: Uuid,
    decl: &'static JobKindDecl,
    priority: i32,
    at: Option<TimestampTz>,
    after_secs: Option<f64>,
    max_attempts: i32,
    singleton: Option<String>,
    dedup: Option<(String, OnDuplicate)>,
    payload: serde_json::Value,
    run_as_kind: &'static str,
    run_as_user: i64,
    caller_companies: bool,
}

const ENQUEUE_SQL: &str = "\
INSERT INTO erp_job (job_uuid, kind, kind_version, priority, run_after, max_attempts, singleton_key, \
    dedup_key, payload, run_as_kind, run_as_user, run_as_companies, run_as_company, \
    enqueued_by_kind, enqueued_by_user, correlation_id, causation_operation) \
SELECT r.uuid, r.kind, r.version::int2, r.priority::int2, \
       COALESCE(r.at, clock_timestamp() + make_interval(secs => COALESCE(r.after_secs, 0))), \
       r.max_attempts, r.singleton, r.dedup, r.payload, r.run_as_kind, r.run_as_user, \
       CASE WHEN r.caller_companies THEN $14::int8[] ELSE '{}'::int8[] END, \
       CASE WHEN r.caller_companies THEN $15::int8 END, \
       $16, $17, $18::uuid, $19::uuid \
  FROM unnest($1::uuid[], $2::text[], $3::int4[], $4::int4[], $5::timestamptz[], $6::float8[], \
              $7::int4[], $8::text[], $9::text[], $10::jsonb[], $11::text[], $12::int8[], $13::bool[]) \
       AS r(uuid, kind, version, priority, at, after_secs, max_attempts, singleton, dedup, payload, \
            run_as_kind, run_as_user, caller_companies) \
ON CONFLICT (kind, dedup_key) WHERE dedup_key IS NOT NULL AND state <> 'dead' DO NOTHING \
RETURNING id, job_uuid, (run_after <= clock_timestamp() + make_interval(secs => $20::float8))";

/// The live job holding each (kind, dedup key) pair, for the requests the insert skipped.
const COALESCED_SQL: &str = "\
SELECT r.ord, j.id, j.job_uuid \
  FROM unnest($1::text[], $2::text[], $3::int4[]) AS r(kind, dedup, ord) \
  JOIN erp_job j ON j.kind = r.kind AND j.dedup_key = r.dedup AND j.state <> 'dead'";

/// Lower the due time of ready jobs a second enqueue coalesced into, without waiting on a job a
/// worker holds (`SKIP LOCKED`: a running job's time no longer matters).
const EARLIEST_SQL: &str = "\
UPDATE erp_job j SET run_after = LEAST(j.run_after, \
       COALESCE(x.at, clock_timestamp() + make_interval(secs => COALESCE(x.after_secs, 0)))) \
  FROM unnest($1::int8[], $2::timestamptz[], $3::float8[]) AS x(id, at, after_secs) \
 WHERE j.id IN (SELECT id FROM erp_job WHERE id = ANY($1) AND state = 'ready' FOR UPDATE SKIP LOCKED) \
   AND j.id = x.id";

impl Transaction {
    /// Add one job in this transaction: it exists after the commit, never after a rollback. See
    /// [`Transaction::enqueue_many`] for the checks.
    ///
    /// # Errors
    ///
    /// As [`Transaction::enqueue_many`].
    pub async fn enqueue(&mut self, env: &Env, request: EnqueueRequest) -> Result<EnqueueOutcome> {
        let mut outcomes = self.enqueue_many(env, vec![request]).await?;
        outcomes
            .pop()
            .ok_or_else(|| Error::internal("enqueue returned no outcome"))
    }

    /// Add jobs in this transaction with one set-based `INSERT` (at most
    /// [`MAX_ENQUEUE_BATCH`]). The outcomes come in the order of the requests.
    ///
    /// Checked before anything is sent: the kind is registered in this binary, the payload fits
    /// `jobs.payload_max_bytes`, the principal can be stored (a public or link principal cannot;
    /// agents and services wait for), and `RunAs::User` is used only by the `System`
    /// principal. A kind that runs as `System` stores the system principal whoever enqueues.
    /// A request with a `dedup_key` that a live job of the kind holds is coalesced into it
    /// (`Keep` leaves it, `Earliest` lowers its due time); under `REPEATABLE READ` a concurrent
    /// enqueue of the same key can end in a serialization failure, which the unit of work's
    /// retry handles.
    ///
    /// # Errors
    ///
    /// `Programming` for an unregistered kind, or `run_as` on a kind that runs as the system;
    /// `Validation` for an oversized payload or a batch over the limit; `Access` for `run_as` by a
    /// principal that may not (naming) and for a public or link principal; the
    /// database's errors.
    pub async fn enqueue_many(
        &mut self,
        env: &Env,
        requests: Vec<EnqueueRequest>,
    ) -> Result<Vec<EnqueueOutcome>> {
        if requests.is_empty() {
            return Ok(Vec::new());
        }
        if requests.len() > MAX_ENQUEUE_BATCH {
            return Err(error::batch_too_large(requests.len(), MAX_ENQUEUE_BATCH));
        }
        let settings = std::sync::Arc::clone(&self.shared.job_settings);
        let caller = StoredPrincipal::capture(env.principal())?;
        let mut rows = Vec::with_capacity(requests.len());
        for request in requests {
            rows.push(prepare(&request, env, &caller, &settings)?);
        }
        let identity = env.operation_identity();
        let correlation = match identity {
            Some(identity) => Some(Uuid::parse(identity.correlation().as_str())?),
            None => None,
        };
        let causation = match identity {
            Some(identity) => Some(Uuid::parse(identity.operation().as_str())?),
            None => None,
        };
        let companies: Vec<i64> = caller.companies.iter().map(|c| c.get()).collect();
        let current: Option<i64> = caller.current_company.map(|c| c.get());
        let enqueued_by_kind = caller.kind_text();
        let enqueued_by_user = caller.subject.get();

        let uuids: Vec<Uuid> = rows.iter().map(|r| r.uuid).collect();
        let kinds: Vec<&str> = rows.iter().map(|r| r.decl.name).collect();
        let versions: Vec<i32> = rows.iter().map(|r| i32::from(r.decl.version)).collect();
        let priorities: Vec<i32> = rows.iter().map(|r| r.priority).collect();
        let ats: Vec<Option<TimestampTz>> = rows.iter().map(|r| r.at).collect();
        let afters: Vec<Option<f64>> = rows.iter().map(|r| r.after_secs).collect();
        let attempts: Vec<i32> = rows.iter().map(|r| r.max_attempts).collect();
        let singletons: Vec<Option<&str>> = rows.iter().map(|r| r.singleton.as_deref()).collect();
        let dedups: Vec<Option<&str>> = rows
            .iter()
            .map(|r| r.dedup.as_ref().map(|(k, _)| k.as_str()))
            .collect();
        let payloads: Vec<&serde_json::Value> = rows.iter().map(|r| &r.payload).collect();
        let run_as_kinds: Vec<&str> = rows.iter().map(|r| r.run_as_kind).collect();
        let run_as_users: Vec<i64> = rows.iter().map(|r| r.run_as_user).collect();
        let caller_flags: Vec<bool> = rows.iter().map(|r| r.caller_companies).collect();
        let inserted = self
            .query(
                ENQUEUE_SQL,
                &[
                    &uuids,
                    &kinds,
                    &versions,
                    &priorities,
                    &ats,
                    &afters,
                    &attempts,
                    &singletons,
                    &dedups,
                    &payloads,
                    &run_as_kinds,
                    &run_as_users,
                    &caller_flags,
                    &companies,
                    &current,
                    &enqueued_by_kind,
                    &enqueued_by_user,
                    &correlation,
                    &causation,
                    &settings.wake_slack().as_secs_f64(),
                ],
            )
            .await?;
        let mut by_uuid: HashMap<Uuid, i64> = HashMap::with_capacity(inserted.len());
        let mut due_soon = false;
        for row in &inserted {
            let id: i64 = row.try_get(0).map_err(undecodable)?;
            let uuid: Uuid = row.try_get(1).map_err(undecodable)?;
            due_soon |= row.try_get::<_, bool>(2).map_err(undecodable)?;
            by_uuid.insert(uuid, id);
        }
        if due_soon {
            self.wake_for_due_jobs(&settings).await?;
        }
        let mut outcomes: Vec<Option<EnqueueOutcome>> = rows
            .iter()
            .map(|r| {
                by_uuid
                    .get(&r.uuid)
                    .map(|id| EnqueueOutcome::Inserted(JobRef::new(*id, r.uuid)))
            })
            .collect();
        if outcomes.iter().all(Option::is_some) {
            return outcomes
                .into_iter()
                .map(|o| o.ok_or_else(|| Error::internal("enqueue lost an outcome")))
                .collect();
        }
        self.resolve_coalesced(&rows, &mut outcomes).await?;
        outcomes
            .into_iter()
            .map(|o| o.ok_or_else(|| Error::internal("enqueue lost an outcome")))
            .collect()
    }

    /// Tell the job runners that a job is due now (a trigger that made a schedule due, say): poke
    /// this process's runners after the commit and send the coalesced `NOTIFY` (2).
    ///
    /// # Errors
    ///
    /// The database's errors.
    pub async fn job_wake(&mut self) -> Result<()> {
        let settings = std::sync::Arc::clone(&self.shared.job_settings);
        self.wake_for_due_jobs(&settings).await
    }

    /// A job due within `jobs.wake_slack` was inserted: poke this process's runners after the
    /// commit and, within the NOTIFY budget, tell the other nodes (2).
    pub(crate) async fn wake_for_due_jobs(&mut self, settings: &super::JobSettings) -> Result<()> {
        let orm_id = self.orm_id();
        self.after_commit(move || super::wake::wake_local(orm_id));
        if super::wake::should_notify(orm_id, settings.notify_interval()) {
            let channel = crate::signal::NotifyChannel::new(super::wake::JOBS_CHANNEL)?;
            if self.notify_once(&channel).await? {
                self.after_commit(move || super::wake::mark_notified(orm_id));
            }
        }
        Ok(())
    }

    /// The requests the insert skipped: find the live job each coalesced into and apply
    /// `Earliest`.
    async fn resolve_coalesced(
        &mut self,
        rows: &[Row],
        outcomes: &mut [Option<EnqueueOutcome>],
    ) -> Result<()> {
        let mut kinds: Vec<&str> = Vec::new();
        let mut keys: Vec<&str> = Vec::new();
        let mut ords: Vec<i32> = Vec::new();
        for (i, (row, outcome)) in rows.iter().zip(outcomes.iter()).enumerate() {
            if outcome.is_none() {
                let Some((key, _)) = &row.dedup else {
                    return Err(Error::internal(
                        "enqueue skipped a job that has no dedup key",
                    ));
                };
                kinds.push(row.decl.name);
                keys.push(key);
                ords.push(i32::try_from(i).map_err(|_| Error::internal("batch index overflow"))?);
            }
        }
        let found = self.query(COALESCED_SQL, &[&kinds, &keys, &ords]).await?;
        let mut lower_ids: Vec<i64> = Vec::new();
        let mut lower_at: Vec<Option<TimestampTz>> = Vec::new();
        let mut lower_after: Vec<Option<f64>> = Vec::new();
        for row in &found {
            let ord: i32 = row.try_get(0).map_err(undecodable)?;
            let id: i64 = row.try_get(1).map_err(undecodable)?;
            let uuid: Uuid = row.try_get(2).map_err(undecodable)?;
            let index = usize::try_from(ord).map_err(|_| Error::internal("batch index"))?;
            if let (Some(slot), Some(source)) = (outcomes.get_mut(index), rows.get(index)) {
                *slot = Some(EnqueueOutcome::Coalesced(JobRef::new(id, uuid)));
                if matches!(source.dedup, Some((_, OnDuplicate::Earliest))) {
                    lower_ids.push(id);
                    lower_at.push(source.at);
                    lower_after.push(source.after_secs);
                }
            }
        }
        if outcomes.iter().any(Option::is_none) {
            // The conflicting job finished or died between the insert and this read: its key is
            // free again. Failing loudly beats guessing; the caller's retry enqueues again.
            return Err(Error::new(
                erp_fields::ErrorKind::Concurrency,
                "a job with the same dedup key finished while it was being coalesced into; \
                 enqueue again",
            )
            .with_sqlstate(Some("40001".to_owned())));
        }
        if !lower_ids.is_empty() {
            self.execute(EARLIEST_SQL, &[&lower_ids, &lower_at, &lower_after])
                .await?;
        }
        Ok(())
    }
}

fn undecodable(source: tokio_postgres::Error) -> Error {
    Error::internal("an erp_job row did not decode").with_source(source)
}

fn prepare(
    request: &EnqueueRequest,
    env: &Env,
    caller: &StoredPrincipal,
    settings: &super::JobSettings,
) -> Result<Row> {
    let decl = *kind_map()
        .get(request.kind)
        .ok_or_else(|| error::unknown_kind(request.kind))?;
    let size = jsonb_text_len(&request.payload);
    let limit = settings.payload_max_bytes();
    if size > limit {
        return Err(error::payload_too_large(request.kind, size, limit));
    }
    let max_attempts = request
        .max_attempts
        .or(decl.max_attempts)
        .unwrap_or_else(|| settings.max_attempts());
    if !(1..=100).contains(&max_attempts) {
        return Err(error::invalid_attempts(request.kind, max_attempts, 100));
    }
    let (run_as_kind, run_as_user, caller_companies) = match (decl.principal, request.run_as) {
        (PrincipalPolicy::System, RunAs::Caller) => {
            (PrincipalKind::System.as_str(), SUPERUSER_ID.get(), false)
        }
        (PrincipalPolicy::System, RunAs::User(_)) => {
            return Err(Error::programming(format!(
                "job kind '{}' runs as the system principal; run_as does not apply to it",
                decl.name
            )));
        }
        (PrincipalPolicy::Caller, RunAs::Caller) => {
            (caller.kind_text(), caller.subject.get(), true)
        }
        (PrincipalPolicy::Caller, RunAs::User(user)) => {
            // Only the core may name another principal until's elevation hook exists.
            if env.principal().kind() != PrincipalKind::System {
                return Err(error::run_as_needs_audit());
            }
            (PrincipalKind::User.as_str(), user.get(), false)
        }
    };
    let at = match request.when {
        When::At(at) => Some(at),
        When::Now | When::After(_) => None,
    };
    let after_secs = match request.when {
        When::After(d) => Some(d.as_secs_f64()),
        When::Now | When::At(_) => None,
    };
    Ok(Row {
        uuid: new_uuid()?,
        decl,
        priority: i32::from(request.priority.unwrap_or(decl.priority).get()),
        at,
        after_secs,
        max_attempts: i32::try_from(max_attempts)
            .map_err(|_| error::invalid_attempts(request.kind, max_attempts, 100))?,
        singleton: request.singleton.as_ref().map(|k| k.as_str().to_owned()),
        dedup: request
            .dedup
            .as_ref()
            .map(|(k, on)| (k.as_str().to_owned(), *on)),
        payload: request.payload.clone(),
        run_as_kind,
        run_as_user,
        caller_companies,
    })
}

/// A fresh UUIDv7 for a job, from the system CSPRNG.
fn new_uuid() -> Result<Uuid> {
    let id = crate::CorrelationId::generate()?;
    Uuid::parse(id.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_rendered_size_counts_the_spaces_jsonb_prints() {
        assert_eq!(jsonb_text_len(&json!({})), 2);
        assert_eq!(jsonb_text_len(&json!([])), 2);
        assert_eq!(jsonb_text_len(&json!({"a": 1})), r#"{"a": 1}"#.len());
        assert_eq!(jsonb_text_len(&json!([1, 2, 3])), "[1, 2, 3]".len());
        assert_eq!(
            jsonb_text_len(&json!({"a": [1, {"b": null}], "c": "x"})),
            r#"{"a": [1, {"b": null}], "c": "x"}"#.len()
        );
        assert_eq!(jsonb_text_len(&json!("é\n")), r#""é\n""#.len());
        assert_eq!(jsonb_text_len(&json!(true)), 4);
        assert_eq!(jsonb_text_len(&json!(false)), 5);
    }

    #[test]
    fn a_request_defaults_to_an_empty_object_due_now() {
        let request = EnqueueRequest::new("a.b");
        assert_eq!(request.payload, json!({}));
        assert_eq!(request.when, When::Now);
        assert_eq!(request.run_as, RunAs::Caller);
    }

    #[test]
    fn attempts_are_bounded() {
        assert_eq!(
            EnqueueRequest::new("a.b")
                .max_attempts(0)
                .unwrap_err()
                .message(),
            "max_attempts 0 of a 'a.b' job is outside 1..=100"
        );
        assert!(EnqueueRequest::new("a.b").max_attempts(100).is_ok());
    }

    #[test]
    fn job_uuids_are_version_seven() {
        let uuid = new_uuid().unwrap();
        assert_eq!(uuid.as_bytes()[6] >> 4, 7);
    }
}
