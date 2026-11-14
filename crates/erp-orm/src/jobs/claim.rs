//! Claim and reap (D7; design §5.2).
//!
//! A claim is one short `READ COMMITTED` transaction, so it never raises a serialization
//! failure; a job without a singleton key is claimed by one statement in one round trip. A
//! singleton key needs two statements (see [`SINGLETON_CANDIDATES_SQL`]). The reaper returns jobs
//! whose lease ran out, skipping any row a running chunk still holds.

use std::sync::Arc;
use std::time::Duration;

use erp_fields::{Error, Result, Uuid};
use tokio_postgres::types::{ToSql, Type};

use super::{JobKindDecl, JobRef, StoredPrincipal};
use crate::principal::SUPERUSER_ID;
use crate::transaction::CommitError;
use crate::{BeginOptions, FinalStatement, Isolation, Orm, Timeouts};

/// What a claim returns for one job: everything a run needs, nothing it must read again.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ClaimedJob {
    pub(crate) job: JobRef,
    pub(crate) decl: &'static JobKindDecl,
    pub(crate) kind_version: u16,
    pub(crate) attempt: u32,
    pub(crate) max_attempts: u32,
    pub(crate) epoch: i64,
    pub(crate) lease: Duration,
    pub(crate) settings: Arc<super::JobSettingsSnapshot>,
    pub(crate) singleton: Option<String>,
    pub(crate) payload: serde_json::Value,
    pub(crate) progress: Option<serde_json::Value>,
    pub(crate) failure_rank: i16,
    pub(crate) principal: StoredPrincipal,
    pub(crate) correlation: Option<Uuid>,
    pub(crate) last_error_code: Option<String>,
}

impl ClaimedJob {
    /// The job.
    pub fn job(&self) -> JobRef {
        self.job
    }

    /// The kind.
    pub fn kind(&self) -> &'static JobKindDecl {
        self.decl
    }

    /// The version the job was enqueued at.
    pub fn kind_version(&self) -> u16 {
        self.kind_version
    }

    /// The attempt number of this claim, from 1.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// The lease epoch this claim holds (the fence).
    pub fn epoch(&self) -> i64 {
        self.epoch
    }

    /// The singleton key, when the job has one.
    pub fn singleton_key(&self) -> Option<&str> {
        self.singleton.as_deref()
    }
}

/// The columns a claim returns, in the order [`decode`] reads them.
macro_rules! claimed_columns {
    () => {
        "j.id, j.job_uuid, j.kind, j.kind_version, j.attempts, j.max_attempts, j.lease_epoch, \
         j.singleton_key, j.payload, j.progress, j.failure_rank, j.run_as_kind, j.run_as_user, \
         j.run_as_companies, j.run_as_company, j.correlation_id, j.last_error_code"
    };
}

/// Claim up to `$2` jobs without a singleton key: `$1` kinds this binary can run, `$3` the lease
/// owner, `$4` the lease in seconds of each kind of `$1` (same order). The lease is read from
/// the database clock; no node compares its own clock with it.
pub(crate) const CLAIM_SQL: &str = concat!(
    "\
WITH c AS (
  SELECT id FROM erp_job
   WHERE state = 'ready' AND run_after <= clock_timestamp() AND kind = ANY($1)
     AND singleton_key IS NULL
   ORDER BY failure_rank, priority, run_after, id
   FOR NO KEY UPDATE SKIP LOCKED
   LIMIT $2)
UPDATE erp_job j
   SET state = 'running', lease_owner = $3, lease_epoch = j.lease_epoch + 1,
       lease_until = clock_timestamp() + make_interval(secs => (
           SELECT k.secs FROM unnest($1::text[], $4::float8[]) AS k(name, secs) WHERE k.name = j.kind)),
       attempts = j.attempts + 1, started_at = clock_timestamp()
  FROM c WHERE j.id = c.id
RETURNING ",
    claimed_columns!()
);

/// Statement 1 of a singleton claim: ready candidates with a key, each tried for the key's
/// transaction-level advisory lock. `$1` kinds, `$2` limit.
pub(crate) const SINGLETON_CANDIDATES_SQL: &str = "\
SELECT id, singleton_key, pg_try_advisory_xact_lock(1703, hashtext(singleton_key)) AS got
  FROM erp_job
 WHERE state = 'ready' AND run_after <= clock_timestamp() AND kind = ANY($1)
   AND singleton_key IS NOT NULL
 ORDER BY failure_rank, priority, run_after, id
 FOR NO KEY UPDATE SKIP LOCKED
 LIMIT $2";

/// Statement 2, in a fresh snapshot: claim the candidates whose key lock was obtained and for
/// which no other job with the key is running on a live lease. `$1` ids (one per key), `$2`
/// owner, `$3` kinds, `$4` lease seconds per kind. A holder that committed `running` released
/// the key lock only at its commit, so this statement sees it; one statement would race.
pub(crate) const SINGLETON_CLAIM_SQL: &str = concat!(
    "\
UPDATE erp_job j
   SET state = 'running', lease_owner = $2, lease_epoch = j.lease_epoch + 1,
       lease_until = clock_timestamp() + make_interval(secs => (
           SELECT k.secs FROM unnest($3::text[], $4::float8[]) AS k(name, secs) WHERE k.name = j.kind)),
       attempts = j.attempts + 1, started_at = clock_timestamp()
 WHERE j.id = ANY($1) AND j.state = 'ready'
   AND NOT EXISTS (SELECT 1 FROM erp_job r
                    WHERE r.singleton_key = j.singleton_key AND r.state = 'running'
                      AND r.lease_until > clock_timestamp())
RETURNING ",
    claimed_columns!()
);

/// The earliest `run_after` of a ready job of `$1` kinds: when the runner next has something to do.
pub(crate) const NEXT_DUE_SQL: &str = "\
SELECT (extract(epoch FROM min(run_after) - clock_timestamp()) * 1000)::float8
  FROM erp_job WHERE state = 'ready' AND kind = ANY($1)";

/// The text of the plain claim statement (feature `test-faults`).
#[cfg(feature = "test-faults")]
#[doc(hidden)]
pub fn claim_statement_for_tests() -> &'static str {
    CLAIM_SQL
}

pub(crate) fn claim_timeouts() -> Result<Timeouts> {
    Timeouts::new(
        Duration::from_secs(1),
        Duration::from_secs(5),
        Duration::from_secs(10),
    )
}

pub(crate) fn claim_options() -> Result<BeginOptions> {
    Ok(BeginOptions::new(SUPERUSER_ID)
        .isolation(Isolation::ReadCommitted)
        .timeouts(claim_timeouts()?)
        .on_primary())
}

fn undecodable(source: tokio_postgres::Error) -> Error {
    Error::internal("an erp_job row did not decode").with_source(source)
}

/// Decode one claimed row (columns of [`claimed_columns!`]).
pub(crate) fn decode(
    row: &tokio_postgres::Row,
    lease: Duration,
    settings: Arc<super::JobSettingsSnapshot>,
) -> Result<ClaimedJob> {
    let kind: String = row.try_get(2).map_err(undecodable)?;
    let decl = *super::enqueue::kind_map()
        .get(kind.as_str())
        .ok_or_else(|| {
            Error::internal(format!(
                "a job of kind '{kind}' was claimed but this binary does not register it"
            ))
        })?;
    let id: i64 = row.try_get(0).map_err(undecodable)?;
    let uuid: Uuid = row.try_get(1).map_err(undecodable)?;
    let kind_version: i16 = row.try_get(3).map_err(undecodable)?;
    let attempts: i32 = row.try_get(4).map_err(undecodable)?;
    let max_attempts: i32 = row.try_get(5).map_err(undecodable)?;
    let run_as_kind: String = row.try_get(11).map_err(undecodable)?;
    let run_as_user: i64 = row.try_get(12).map_err(undecodable)?;
    let companies: Vec<i64> = row.try_get(13).map_err(undecodable)?;
    let company: Option<i64> = row.try_get(14).map_err(undecodable)?;
    Ok(ClaimedJob {
        job: JobRef::new(id, uuid),
        decl,
        kind_version: u16::try_from(kind_version)
            .map_err(|_| Error::internal("erp_job holds a negative kind_version"))?,
        attempt: u32::try_from(attempts)
            .map_err(|_| Error::internal("erp_job holds negative attempts"))?,
        max_attempts: u32::try_from(max_attempts)
            .map_err(|_| Error::internal("erp_job holds negative max_attempts"))?,
        epoch: row.try_get(6).map_err(undecodable)?,
        lease,
        settings,
        singleton: row.try_get(7).map_err(undecodable)?,
        payload: row.try_get(8).map_err(undecodable)?,
        progress: row.try_get(9).map_err(undecodable)?,
        failure_rank: row.try_get(10).map_err(undecodable)?,
        principal: StoredPrincipal::from_columns(&run_as_kind, run_as_user, &companies, company)?,
        correlation: row.try_get(15).map_err(undecodable)?,
        last_error_code: row.try_get(16).map_err(undecodable)?,
    })
}

/// Claim up to `limit` jobs of `kinds` that carry no singleton key, in one round trip.
pub(crate) async fn claim_plain(
    orm: &Orm,
    kinds: &[&'static JobKindDecl],
    leases: &[f64],
    limit: i64,
    owner: Uuid,
    settings: Arc<super::JobSettingsSnapshot>,
) -> Result<Vec<ClaimedJob>> {
    let names: Vec<&str> = kinds.iter().map(|k| k.name).collect();
    let leases_vec: Vec<f64> = leases.to_vec();
    let params: [(&(dyn ToSql + Sync), Type); 4] = [
        (&names, Type::TEXT_ARRAY),
        (&limit, Type::INT8),
        (&owner, Type::UUID),
        (&leases_vec, Type::FLOAT8_ARRAY),
    ];
    let rows = orm
        .one_flight(claim_options()?, FinalStatement::new(CLAIM_SQL, &params))
        .await
        .map_err(CommitError::into_error)?;
    rows.iter()
        .map(|row| {
            let lease = lease_of(row, kinds, leases)?;
            decode(row, lease, Arc::clone(&settings))
        })
        .collect()
}

/// The lease the claim gave the job of `row` (looked up by kind in the parallel arrays).
fn lease_of(
    row: &tokio_postgres::Row,
    kinds: &[&'static JobKindDecl],
    leases: &[f64],
) -> Result<Duration> {
    let kind: String = row.try_get(2).map_err(undecodable)?;
    let secs = kinds
        .iter()
        .zip(leases)
        .find(|(k, _)| k.name == kind)
        .map(|(_, secs)| *secs)
        .ok_or_else(|| Error::internal("a claimed job's kind is not in the claim's kind list"))?;
    Ok(Duration::from_secs_f64(secs))
}

/// The candidates of statement 1 kept for statement 2: those whose key lock was obtained, the
/// first of each key (two ready jobs of one key must not both start).
pub(crate) fn pick_per_key(candidates: &[(i64, String, bool)]) -> Vec<i64> {
    let mut seen: Vec<&str> = Vec::new();
    let mut ids = Vec::new();
    for (id, key, got) in candidates {
        if *got && !seen.contains(&key.as_str()) {
            seen.push(key);
            ids.push(*id);
        }
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_first_candidate_of_a_key_is_kept_and_only_when_its_lock_was_obtained() {
        let candidates = vec![
            (1, "a".to_owned(), true),
            (2, "a".to_owned(), true),
            (3, "b".to_owned(), false),
            (4, "c".to_owned(), true),
            (5, "b".to_owned(), true),
        ];
        assert_eq!(pick_per_key(&candidates), [1, 4, 5]);
        assert!(pick_per_key(&[]).is_empty());
    }

    #[test]
    fn the_claim_statements_return_the_columns_the_decoder_reads() {
        for sql in [CLAIM_SQL, SINGLETON_CLAIM_SQL] {
            assert!(sql.contains("RETURNING j.id, j.job_uuid, j.kind, j.kind_version"));
            assert!(sql.ends_with("j.last_error_code"));
        }
    }
}
