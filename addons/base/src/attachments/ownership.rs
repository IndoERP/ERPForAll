// SPDX-License-Identifier: MIT OR Apache-2.0
//! Durable owner coordination. No schedules are created here.
//!
//! Commit [`prepare`] before [`Prepared::execute`], then commit [`settle`] before interpreting
//! its outcome. Contest publication is a second external step after durable `contest_pending`.
//! JOBS consumers use the same [`SINGLETON`] and fenced chunks for each database transition;
//! external steps perform only conditional store operations. Cleanup locks coordinator, store
//! metadata and owner rows in id order, then GC holds and blob/upload rows. Waiting holds none
//! of these locks.

use super::runtime::{blob_error, col_error, runtime, store_by_key};
use erp_sdk::blob::{
    AttachmentStore, BlobError, Namespace, OwnerIdentity, OwnerMarker, OwnerVersion,
};
use erp_sdk::serde_json::{Map, Value, json};
use erp_sdk::{BeginOptions, Error, Isolation, Orm, Result, SUPERUSER_ID, Transaction};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Database-wide singleton used by heartbeat and every future destructive attachment handler.
pub const SINGLETON: &str = "base.attachment_owner";
/// Heartbeat cadence.
pub const HEARTBEAT_SECS: u64 = 300;
/// A last success younger than this keeps a store fresh: two heartbeats ([`HEARTBEAT_SECS`]).
/// Cleanup admission, health and the continuity of an open observation window read it.
pub const FRESH_SECS: u64 = 2 * HEARTBEAT_SECS;
/// Two heartbeats plus a positive one-minute margin; not a performance target. The CHECK of
/// `ir_blob_owner_state` repeats it as a literal (DDL cannot bind it); a test pins the two.
pub const OBSERVATION_SECS: u64 = 660;
const IDENTITY_CHANGED: &str = "Attachment ownership database identity changed; neutralize or rebase the clone before maintenance.";

/// Why an owner advances. `Observe` also opens a cleanup observation window when none is open
/// (an open one is kept); `Heartbeat` only advances. The scheduled heartbeat uses `Observe`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Heartbeat,
    Observe,
}
impl Purpose {
    fn text(self) -> &'static str {
        match self {
            Self::Heartbeat => "heartbeat",
            Self::Observe => "observe",
        }
    }
    fn parse(s: &str) -> Result<Self> {
        match s {
            "heartbeat" => Ok(Self::Heartbeat),
            "observe" => Ok(Self::Observe),
            _ => Err(Error::db("Invalid attachment owner purpose")),
        }
    }
}
/// Current write-health assessment. This is not a permission to remove any bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Uninitialized,
    Unsupported,
    Healthy,
    Unavailable,
}
/// Resumable observation; Ready still requires per-chunk store/hold/peer checks by CLEANUP.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Observation {
    /// No continuous observation is established.
    Unavailable,
    /// Reschedule without retaining a transaction or a singleton lease.
    Waiting {
        /// Delay until another assessment, on the database clock.
        retry_after: Duration,
    },
    /// Opaque window identity, never a reusable deletion capability.
    Ready {
        /// Identifier of the continuously observed window.
        id: Namespace,
    },
}
/// A committed database settlement. Interpret only after its transaction commits.
#[derive(Debug)]
#[non_exhaustive]
pub enum Settlement {
    /// Exact owner advancement confirmed.
    Confirmed {
        /// The store already contained the intended marker.
        recovered: bool,
    },
    /// Durable local refusal; publish the contest next.
    ContestPending,
    /// Store contest is present; operator intervention is required.
    Contested,
    /// State remains recoverable; the actual typed store error is retained.
    Retry(Arc<BlobError>),
    /// A stale completion cannot affect a later operation.
    Superseded,
}

/// Why the scheduled heartbeat left one store unbeaten in a cycle. A closed set: each variant
/// has a fixed [`code`](Self::code), which the heartbeat logs at error level with the store key
/// and records in its job progress (`refused`: store key to code). Messages are never matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Refusal {
    /// An audited operator recovery of the store is not completed (`recovery_active`).
    RecoveryActive,
    /// The namespace is contested; an audited operator claim is required (`contested`).
    Contested,
    /// The store's namespace or fingerprint no longer matches its owner row (`binding_changed`).
    BindingChanged,
    /// A conflict is recorded but its contest marker is not published yet (`contest_pending`).
    ContestPending,
    /// Another operation replaced this one before it settled (`superseded`).
    Superseded,
    /// The store could not be reached or refused the operation; the next cycle retries
    /// (`store_failed`). When the store write itself failed, the owner row records the failure
    /// (mode `recovering`); a failure while the store is first opened (its fingerprint check)
    /// changes no owner row.
    StoreFailed,
    /// The store's own coordination data cannot be used: it has no fingerprint, its recorded
    /// owner marker does not decode, or its owner generation is exhausted (`store_invalid`).
    /// An operator has to repair the store or its configuration.
    StoreInvalid,
}
impl Refusal {
    /// The fixed code written to the log and the heartbeat progress.
    pub const fn code(self) -> &'static str {
        match self {
            Self::RecoveryActive => "recovery_active",
            Self::Contested => "contested",
            Self::BindingChanged => "binding_changed",
            Self::ContestPending => "contest_pending",
            Self::Superseded => "superseded",
            Self::StoreFailed => "store_failed",
            Self::StoreInvalid => "store_invalid",
        }
    }
}
/// A per-store refusal of [`prepare_store`] with the exact error the public [`prepare`] returns.
struct Refused {
    refusal: Refusal,
    error: Error,
}
impl Refused {
    fn new(refusal: Refusal, error: Error) -> Self {
        Self { refusal, error }
    }
}
/// A store-layer failure (the store is unreachable or refused), as opposed to a database,
/// programming or configuration error, which still fails the caller.
fn is_store_failure(error: &Error) -> bool {
    std::error::Error::source(error)
        .and_then(|s| s.downcast_ref::<BlobError>())
        .is_some_and(|e| !matches!(e, BlobError::Configuration { .. }))
}

/// Committed advancement intent. Private fields prevent caller-selected versions or paths.
#[derive(Clone)]
#[non_exhaustive]
pub struct Prepared {
    store_id: i16,
    namespace: Namespace,
    fingerprint: String,
    store: Arc<dyn AttachmentStore>,
    marker: OwnerMarker,
    expected: Option<OwnerVersion>,
    prior: Option<OwnerMarker>,
    purpose: Purpose,
    probe: bool,
    contest: bool,
}
impl std::fmt::Debug for Prepared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Prepared")
            .field("marker", &self.marker)
            .finish_non_exhaustive()
    }
}
/// External result; only `Prepared::execute` can construct one.
#[derive(Clone)]
#[non_exhaustive]
pub struct Attempt {
    marker: OwnerMarker,
    result: Effect,
}
#[derive(Clone)]
enum Effect {
    Confirmed(OwnerVersion, bool),
    Conflict,
    Contested,
    Failed(Arc<BlobError>),
}

impl Prepared {
    /// Namespace bound to the durable store row.
    pub fn namespace(&self) -> &Namespace {
        &self.namespace
    }
    /// Exact persisted intended marker; retry never changes its time or operation UUID.
    pub fn marker(&self) -> &OwnerMarker {
        &self.marker
    }
    /// Perform conditional store work outside a transaction. Failures are settled durably.
    pub async fn execute(&self) -> Attempt {
        let result = match self.execute_inner().await {
            Ok(v) => v,
            Err(e) => Effect::Failed(Arc::new(e)),
        };
        Attempt {
            marker: self.marker.clone(),
            result,
        }
    }
    async fn execute_inner(&self) -> std::result::Result<Effect, BlobError> {
        if self.contest {
            return Ok(Effect::Conflict);
        }
        if !self.probe {
            self.store.probe_owner().await?;
        }
        if self.store.contested(&self.namespace).await?.is_some() {
            return Ok(Effect::Contested);
        }
        let current = self.store.owner(&self.namespace).await?;
        if let Some(current) = &current
            && current.marker() == &self.marker
        {
            return Ok(Effect::Confirmed(current.version().clone(), true));
        }
        let matches_prior = match (&current, &self.expected, &self.prior) {
            (None, None, None) => true,
            (Some(state), Some(version), Some(marker)) => {
                state.version() == version && state.marker() == marker
            }
            _ => false, // Every other combination is ownership conflict, never initialization.
        };
        if !matches_prior {
            return Ok(Effect::Conflict);
        }
        match self
            .store
            .put_owner(&self.namespace, &self.marker, self.expected.as_ref())
            .await
        {
            Ok(version) => Ok(Effect::Confirmed(version, false)),
            Err(BlobError::OwnerConflict) => {
                // A duplicated external step can have completed exactly this operation.
                let current = self.store.owner(&self.namespace).await?;
                match current {
                    Some(state) if state.marker() == &self.marker => {
                        Ok(Effect::Confirmed(state.version().clone(), true))
                    }
                    _ => Ok(Effect::Conflict),
                }
            }
            Err(e) => Err(e),
        }
    }
    /// Publish only after `settle` committed ContestPending. Creation never clears or replaces
    /// another database's contest. A stale publication is conservatively a namespace refusal.
    /// # Errors
    /// The store's typed failure; preserve it with `settle_contest`.
    pub async fn publish_contest(&self) -> std::result::Result<(), Arc<BlobError>> {
        self.store
            .create_contested(&self.namespace, &self.marker)
            .await
            .map_err(Arc::new)
    }
}

fn decode<T>(result: std::result::Result<T, impl std::fmt::Display>) -> Result<T> {
    result.map_err(|e| col_error("attachment ownership", e))
}
fn token(etag: Option<String>, version: Option<String>) -> Result<Option<OwnerVersion>> {
    match (etag, version) {
        (Some(e), v) => Ok(Some(
            OwnerVersion::new(&e, v.as_deref()).map_err(blob_error)?,
        )),
        (None, None) => Ok(None),
        (None, Some(_)) => Err(Error::db("Attachment owner version lacks its ETag")),
    }
}
pub(super) async fn lock(tx: &mut Transaction) -> Result<()> {
    let rows = tx
        .query("SELECT current_setting('transaction_isolation')", &[])
        .await?;
    let isolation: String = decode(rows[0].try_get(0))?;
    if isolation != "read committed" {
        return Err(Error::programming(
            "Attachment ownership transitions require READ COMMITTED isolation.",
        ));
    }
    tx.query(
        "SELECT pg_advisory_xact_lock(1703, hashtext($1::text))",
        &[&SINGLETON],
    )
    .await?;
    Ok(())
}
pub(super) async fn identity(tx: &mut Transaction) -> Result<OwnerIdentity> {
    // This is an owner-side entry point. Request roles never receive this catalog privilege.
    let physical = tx.query("SELECT system_identifier::text, (SELECT oid::int8 FROM pg_database WHERE datname=current_database()) FROM pg_control_system()", &[]).await?;
    let row = physical
        .first()
        .ok_or_else(|| Error::db("Missing physical database identity"))?;
    let system: String = decode(row.try_get(0))?;
    let oid: i64 = decode(row.try_get(1))?;
    tx.execute("INSERT INTO ir_blob_owner_identity(singleton,system_identifier,database_oid,incarnation) VALUES(true,$1::text::numeric,$2,gen_random_uuid()) ON CONFLICT DO NOTHING", &[&system,&oid]).await?;
    let rows = tx.query("SELECT system_identifier::text,database_oid,incarnation::text FROM ir_blob_owner_identity WHERE singleton", &[]).await?;
    let row = rows
        .first()
        .ok_or_else(|| Error::db("Missing attachment database incarnation"))?;
    let registered: String = decode(row.try_get(0))?;
    let registered_oid: i64 = decode(row.try_get(1))?;
    if registered != system || registered_oid != oid {
        return Err(Error::internal(IDENTITY_CHANGED));
    }
    let incarnation: String = decode(row.try_get(2))?;
    OwnerIdentity::new(
        system
            .parse()
            .map_err(|_| Error::db("Invalid database system identifier"))?,
        u32::try_from(oid).map_err(|_| Error::db("Invalid database OID"))?,
        Namespace::parse(&incarnation).map_err(blob_error)?,
    )
    .map_err(blob_error)
}

/// Prepare under the shared coordinator lock. Caller MUST commit before executing the result.
/// A pending operation is returned unchanged, even when a different purpose was requested.
/// # Errors
/// Database errors, invalid store/identity binding, borrowed namespace, contested state or overflow.
pub async fn prepare(tx: &mut Transaction, key: &str, purpose: Purpose) -> Result<Prepared> {
    prepare_store(tx, key, purpose).await?.map_err(|r| r.error)
}

/// [`prepare`] that tells a refusal of this one store (the inner `Err`, committable: no
/// statement failed) from an error that concerns every store or the database (the outer `Err`:
/// a changed database identity, a database or configuration error, or malformed owner-row
/// columns).
async fn prepare_store(
    tx: &mut Transaction,
    key: &str,
    purpose: Purpose,
) -> Result<std::result::Result<Prepared, Refused>> {
    lock(tx).await?;
    if super::recovery::active_key(tx, key).await? {
        return Ok(Err(Refused::new(
            Refusal::RecoveryActive,
            super::recovery::active_refusal(),
        )));
    }
    let who = identity(tx).await?;
    let rt = runtime(&tx.env())?;
    let store = match store_by_key(tx, &rt, key).await {
        Ok(store) => store,
        Err(e) if is_store_failure(&e) => return Ok(Err(Refused::new(Refusal::StoreFailed, e))),
        Err(e) => return Err(e),
    };
    let Some(fp) = store.store.fingerprint() else {
        return Ok(Err(Refused::new(
            Refusal::StoreInvalid,
            Error::programming("Attachment owner fencing requires a store fingerprint"),
        )));
    };
    tx.execute("INSERT INTO ir_blob_owner_state(store_id,namespace,fingerprint) VALUES($1,$2::text::uuid,$3) ON CONFLICT DO NOTHING", &[&store.id,&store.namespace.as_str(),&fp]).await?;
    let rows = tx.query("SELECT namespace::text,fingerprint,mode,confirmed_marker,confirmed_etag,confirmed_version,pending_marker,pending_etag,pending_version,pending_purpose,probe_protocol FROM ir_blob_owner_state WHERE store_id=$1 FOR UPDATE", &[&store.id]).await?;
    let row = rows
        .first()
        .ok_or_else(|| Error::db("Missing attachment owner state"))?;
    let ns: String = decode(row.try_get(0))?;
    let registered: String = decode(row.try_get(1))?;
    if ns != store.namespace.as_str() || registered != fp {
        return Ok(Err(Refused::new(
            Refusal::BindingChanged,
            Error::internal(
                "Attachment ownership store binding changed; maintenance admission is required.",
            ),
        )));
    }
    let mode: String = decode(row.try_get(2))?;
    if mode == "contested" {
        return Ok(Err(Refused::new(
            Refusal::Contested,
            Error::internal(
                "Attachment ownership is contested; an audited operator claim is required.",
            ),
        )));
    }
    let confirmed: Option<Vec<u8>> = decode(row.try_get(3))?;
    let prior = match confirmed.as_deref().map(OwnerMarker::decode).transpose() {
        Ok(prior) => prior,
        Err(e) => return Ok(Err(Refused::new(Refusal::StoreInvalid, blob_error(e)))),
    };
    if prior.as_ref().is_some_and(|m| m.identity() != &who) {
        return Err(Error::internal(IDENTITY_CHANGED));
    }
    let confirmed_version = token(decode(row.try_get(4))?, decode(row.try_get(5))?)?;
    let pending: Option<Vec<u8>> = decode(row.try_get(6))?;
    let (marker, expected, purpose) = if let Some(bytes) = pending {
        let marker = match OwnerMarker::decode(&bytes) {
            Ok(marker) => marker,
            Err(e) => return Ok(Err(Refused::new(Refusal::StoreInvalid, blob_error(e)))),
        };
        if marker.identity() != &who {
            return Err(Error::internal(IDENTITY_CHANGED));
        }
        let purpose: String = decode(row.try_get(9))?;
        (
            marker,
            token(decode(row.try_get(7))?, decode(row.try_get(8))?)?,
            Purpose::parse(&purpose)?,
        )
    } else {
        let generation = match &prior {
            Some(m) => match m.generation().checked_add(1) {
                Some(next) => next,
                None => {
                    return Ok(Err(Refused::new(
                        Refusal::StoreInvalid,
                        Error::internal(
                            "Attachment owner generation exhausted; maintenance cannot advance it.",
                        ),
                    )));
                }
            },
            None => 1,
        };
        let time = tx
            .query(
                "SELECT floor(extract(epoch FROM clock_timestamp()))::int8",
                &[],
            )
            .await?;
        let seconds: i64 = decode(time[0].try_get(0))?;
        let marker = OwnerMarker::new(
            who,
            generation,
            Namespace::random().map_err(blob_error)?,
            u64::try_from(seconds).map_err(|_| Error::db("Invalid owner preparation time"))?,
        )
        .map_err(blob_error)?;
        let expected = confirmed_version;
        tx.execute("UPDATE ir_blob_owner_state SET mode='recovering',pending_marker=$2,pending_etag=$3,pending_version=$4,pending_purpose=$5 WHERE store_id=$1", &[&store.id,&marker.encode(),&expected.as_ref().map(OwnerVersion::etag),&expected.as_ref().and_then(OwnerVersion::object_version),&purpose.text()]).await?;
        (marker, expected, purpose)
    };
    let probe: Option<i16> = decode(row.try_get(10))?;
    Ok(Ok(Prepared {
        store_id: store.id,
        namespace: store.namespace,
        fingerprint: fp,
        store: store.store,
        marker,
        expected,
        prior,
        purpose,
        probe: probe == Some(1),
        contest: mode == "contest_pending",
    }))
}

async fn current(tx: &mut Transaction, p: &Prepared) -> Result<bool> {
    lock(tx).await?;
    if super::recovery::active_store(tx, p.store_id).await? {
        return Ok(false);
    }
    let rows = tx.query("SELECT pending_marker,namespace::text,fingerprint FROM ir_blob_owner_state WHERE store_id=$1 FOR UPDATE", &[&p.store_id]).await?;
    let Some(row) = rows.first() else {
        return Ok(false);
    };
    let bytes: Option<Vec<u8>> = decode(row.try_get(0))?;
    let ns: String = decode(row.try_get(1))?;
    let fp: String = decode(row.try_get(2))?;
    Ok(bytes.as_deref() == Some(p.marker.encode().as_slice())
        && ns == p.namespace.as_str()
        && fp == p.fingerprint)
}

/// Persist the external outcome. Commit this transaction before returning a stored failure.
/// # Errors
/// Database failures or a mismatched operation result. Store failures are returned as Retry.
pub async fn settle(tx: &mut Transaction, p: &Prepared, attempt: Attempt) -> Result<Settlement> {
    if attempt.marker != p.marker {
        return Err(Error::programming(
            "Attachment owner result belongs to another operation",
        ));
    }
    if !current(tx, p).await? {
        return Ok(Settlement::Superseded);
    }
    let rows = tx
        .query(
            "SELECT mode FROM ir_blob_owner_state WHERE store_id=$1",
            &[&p.store_id],
        )
        .await?;
    let mode: String = decode(rows[0].try_get(0))?;
    // Once refusal is durable, an older successful reply must never clear it.
    if mode == "contested" {
        return Ok(Settlement::Contested);
    }
    if mode == "contest_pending" {
        return Ok(Settlement::ContestPending);
    }
    let was_contested = matches!(&attempt.result, Effect::Contested);
    let outcome = match attempt.result {
        Effect::Confirmed(version, recovered) => {
            // Recovery or a health gap breaks continuity. Ordinary healthy heartbeat successors
            // preserve the observation anchor while moving the currently checked version forward.
            let reset = recovered;
            let fresh = FRESH_SECS as f64;
            tx.execute("UPDATE ir_blob_owner_state SET observation_id = CASE WHEN $2 OR last_success_at IS NULL OR last_success_at <= clock_timestamp()-make_interval(secs=>$3::float8) THEN NULL ELSE observation_id END, observation_started_at = CASE WHEN $2 OR last_success_at IS NULL OR last_success_at <= clock_timestamp()-make_interval(secs=>$3::float8) THEN NULL ELSE observation_started_at END, observation_not_before = CASE WHEN $2 OR last_success_at IS NULL OR last_success_at <= clock_timestamp()-make_interval(secs=>$3::float8) THEN NULL ELSE observation_not_before END WHERE store_id=$1", &[&p.store_id,&reset,&fresh]).await?;
            tx.execute("UPDATE ir_blob_owner_state SET mode='healthy',confirmed_marker=$2,confirmed_etag=$3,confirmed_version=$4,pending_marker=NULL,pending_etag=NULL,pending_version=NULL,pending_purpose=NULL,last_success_at=clock_timestamp(),failure_code=NULL,probe_protocol=1 WHERE store_id=$1", &[&p.store_id,&p.marker.encode(),&version.etag(),&version.object_version()]).await?;
            if p.purpose == Purpose::Observe {
                let observation = OBSERVATION_SECS as f64;
                tx.execute("UPDATE ir_blob_owner_state SET observation_id=$2::text::uuid,observation_started_at=clock_timestamp(),observation_not_before=clock_timestamp()+make_interval(secs=>$3::float8) WHERE store_id=$1 AND observation_id IS NULL", &[&p.store_id,&p.marker.operation().as_str(),&observation]).await?;
            }
            Settlement::Confirmed { recovered }
        }
        Effect::Conflict | Effect::Contested => {
            let contested = was_contested;
            let mode = if contested {
                "contested"
            } else {
                "contest_pending"
            };
            tx.execute("UPDATE ir_blob_owner_state SET mode=$2,failure_code='owner_conflict',observation_id=NULL,observation_started_at=NULL,observation_not_before=NULL WHERE store_id=$1", &[&p.store_id,&mode]).await?;
            tracing::error!(target:"erp::attachments",store=p.store_id,"attachment namespace ownership is contested; removal is refused");
            if contested {
                Settlement::Contested
            } else {
                Settlement::ContestPending
            }
        }
        Effect::Failed(error) => {
            let unsupported = matches!(
                error.as_ref(),
                BlobError::UnsupportedOwnerFence | BlobError::OwnerCapabilityMissing { .. }
            );
            let mode = if p.contest {
                "contest_pending"
            } else if unsupported && p.prior.is_none() {
                "unsupported"
            } else {
                "recovering"
            };
            tx.execute("UPDATE ir_blob_owner_state SET mode=$2,failure_code='store_failure',observation_id=NULL,observation_started_at=NULL,observation_not_before=NULL WHERE store_id=$1", &[&p.store_id,&mode]).await?;
            Settlement::Retry(error)
        }
    };
    Ok(outcome)
}

/// Record contest publication without erasing a failed attempt. Commit before interpreting Retry.
/// # Errors
/// Database failures; the actual publication failure is retained inside Settlement::Retry.
pub async fn settle_contest(
    tx: &mut Transaction,
    p: &Prepared,
    result: std::result::Result<(), Arc<BlobError>>,
) -> Result<Settlement> {
    if !current(tx, p).await? {
        return Ok(Settlement::Superseded);
    }
    let mode = tx
        .query(
            "SELECT mode FROM ir_blob_owner_state WHERE store_id=$1",
            &[&p.store_id],
        )
        .await?;
    let mode: String = decode(mode[0].try_get(0))?;
    if mode == "contested" {
        return Ok(Settlement::Contested);
    }
    if mode != "contest_pending" {
        return Err(Error::programming(
            "Attachment contest publication requires committed contest_pending state",
        ));
    }
    match result {
        Ok(()) => {
            tx.execute("UPDATE ir_blob_owner_state SET mode='contested',failure_code='owner_conflict' WHERE store_id=$1", &[&p.store_id]).await?;
            Ok(Settlement::Contested)
        }
        Err(error) => Ok(Settlement::Retry(error)),
    }
}

struct Assessment {
    health: Health,
    observation: Observation,
    marker: Option<OwnerMarker>,
    version: Option<OwnerVersion>,
    namespace: Option<Namespace>,
    store: Arc<dyn AttachmentStore>,
}
async fn assess(orm: &Orm, key: &str) -> Result<Assessment> {
    // Always a new primary READ COMMITTED transaction, never the caller's old snapshot or a
    // generation cache. End it before network I/O and never return an open transaction.
    let mut tx = orm
        .begin_opts(BeginOptions::new(SUPERUSER_ID).isolation(Isolation::ReadCommitted))
        .await?;
    let rt = runtime(&tx.env())?;
    let store = rt
        .store(key)
        .cloned()
        .ok_or_else(|| Error::programming("Attachment health store is not configured"))?;
    let rows = tx.query("SELECT s.namespace::text,s.fingerprint,s.role,o.mode,o.confirmed_marker,o.confirmed_etag,o.confirmed_version,o.last_success_at > clock_timestamp()-make_interval(secs=>$2::float8),o.observation_id::text,ceil(extract(epoch FROM o.observation_not_before-clock_timestamp()))::int8,o.namespace::text,o.fingerprint,i.system_identifier::text,i.database_oid,i.incarnation::text,(SELECT oid::int8 FROM pg_database WHERE datname=current_database()) FROM ir_blob_store s LEFT JOIN ir_blob_owner_state o ON o.store_id=s.id LEFT JOIN ir_blob_owner_identity i ON i.singleton WHERE s.store_key=$1 AND s.role='own'", &[&key, &(FRESH_SECS as f64)]).await?;
    let mut result = Assessment {
        health: Health::Uninitialized,
        observation: Observation::Unavailable,
        marker: None,
        version: None,
        namespace: None,
        store,
    };
    if let Some(row) = rows.first() {
        let mode: Option<String> = decode(row.try_get(3))?;
        if let Some(mode) = mode {
            result.health = match mode.as_str() {
                "unsupported" => Health::Unsupported,
                "uninitialized" => Health::Uninitialized,
                "healthy" | "recovering" | "contest_pending" | "contested" => Health::Unavailable,
                _ => return Err(Error::db("Invalid attachment ownership mode")),
            };
            let ns: String = decode(row.try_get(0))?;
            let fp: String = decode(row.try_get(1))?;
            let bound_ns: String = decode(row.try_get(10))?;
            let bound_fp: String = decode(row.try_get(11))?;
            if ns != bound_ns
                || fp != bound_fp
                || result.store.fingerprint().as_deref() != Some(fp.as_str())
            {
                return Err(Error::internal(
                    "Attachment ownership store binding changed; maintenance admission is required.",
                ));
            }
            result.namespace = Some(Namespace::parse(&ns).map_err(blob_error)?);
            let bytes: Option<Vec<u8>> = decode(row.try_get(4))?;
            result.marker = bytes
                .as_deref()
                .map(OwnerMarker::decode)
                .transpose()
                .map_err(blob_error)?;
            result.version = token(decode(row.try_get(5))?, decode(row.try_get(6))?)?;
            let fresh: Option<bool> = decode(row.try_get(7))?;
            if mode == "healthy" && fresh == Some(true) {
                let system: String = decode(row.try_get(12))?;
                let oid: i64 = decode(row.try_get(13))?;
                let incarnation: String = decode(row.try_get(14))?;
                let actual_oid: i64 = decode(row.try_get(15))?;
                let identity = OwnerIdentity::new(
                    system
                        .parse()
                        .map_err(|_| Error::db("Invalid database system identifier"))?,
                    u32::try_from(oid).map_err(|_| Error::db("Invalid database OID"))?,
                    Namespace::parse(&incarnation).map_err(blob_error)?,
                )
                .map_err(blob_error)?;
                if actual_oid == oid
                    && result
                        .marker
                        .as_ref()
                        .is_some_and(|m| m.identity() == &identity)
                {
                    result.health = Health::Healthy;
                    let id: Option<String> = decode(row.try_get(8))?;
                    let seconds: Option<i64> = decode(row.try_get(9))?;
                    if let (Some(id), Some(seconds)) = (id, seconds) {
                        result.observation = if seconds > 0 {
                            Observation::Waiting {
                                retry_after: Duration::from_secs(
                                    u64::try_from(seconds)
                                        .map_err(|_| Error::db("Invalid observation delay"))?,
                                ),
                            }
                        } else {
                            Observation::Ready {
                                id: Namespace::parse(&id).map_err(blob_error)?,
                            }
                        };
                    }
                }
            }
        }
    }
    if super::recovery::active_key(&mut tx, key).await? {
        result.health = Health::Unavailable;
        result.observation = Observation::Unavailable;
    }
    tx.rollback().await?;
    drop(tx);
    // Even an unsupported/unhealthy local row must not conceal a visible contest as an exception
    // allowing non-destructive work. A store with no marker implementation has no such read.
    if let Some(ns) = &result.namespace {
        let contested = match result.store.contested(ns).await {
            Ok(marker) => marker.is_some(),
            Err(BlobError::UnsupportedOwnerFence) if result.health == Health::Unsupported => false,
            Err(e) => return Err(blob_error(e)),
        };
        if contested {
            result.health = Health::Unavailable;
            result.observation = Observation::Unavailable;
        }
        if result.health == Health::Healthy {
            let valid = match (
                &result.marker,
                &result.version,
                result.store.owner(ns).await.map_err(blob_error)?,
            ) {
                (Some(marker), Some(version), Some(state)) => {
                    state.marker() == marker && state.version() == version
                }
                _ => false,
            };
            if !valid {
                result.health = Health::Unavailable;
                result.observation = Observation::Unavailable;
            }
        }
    }
    Ok(result)
}

/// Assess health from a fresh primary transaction and current store markers. Opens no owner
/// connection: use a runtime-role ORM in requests. This does not wire upload/link/revive callers.
/// # Errors
/// Database, malformed state, configuration and typed store errors; no silent healthy default.
pub async fn health(orm: &Orm, key: &str) -> Result<Health> {
    Ok(assess(orm, key).await?.health)
}

/// Read durable observation progress without sleeping or retaining a lock. Ready is not deletion
/// authorization: CLEANUP must revalidate every chunk, holds, capabilities and peer admission.
/// # Errors
/// As [`health`].
pub async fn observation(orm: &Orm, key: &str) -> Result<Observation> {
    Ok(assess(orm, key).await?.observation)
}

/// Native heartbeat handler, registered as `base.attachment_owner_heartbeat` and scheduled with
/// the shared singleton key by `gc::ensure_schedules`. Each beat advances with
/// [`Purpose::Observe`]: it opens an observation window when none is open and keeps an open one,
/// so cleanup is admitted [`OBSERVATION_SECS`] after the first healthy beat without operator
/// steps. It writes owner state, so only an owner-role runner can execute it; the runtime role
/// fails loudly.
///
/// A store refused in a cycle (see [`Refusal`]) is logged at error level with its key and code,
/// recorded in the job progress, and the cycle moves on to the next store, so one contested,
/// recovering or failing store never leaves the others unbeaten. Such refusals do not fail the
/// run: it reschedules after [`HEARTBEAT_SECS`], and the refused store's own staleness stays in
/// its owner row. Database errors fail the run for retry. A changed database identity, a
/// configuration error or an unreadable progress value is a permanent error: the row is
/// dead-lettered and the next server start schedules a fresh heartbeat.
#[derive(Default)]
#[non_exhaustive]
pub struct HeartbeatJob;
impl HeartbeatJob {
    /// Fixed native handler; construction never starts a job or grants a database privilege.
    pub const fn new() -> Self {
        Self
    }
}

fn put_slot<T>(slot: &Arc<Mutex<Option<T>>>, value: T) -> Result<()> {
    *slot
        .lock()
        .map_err(|_| Error::internal("Attachment owner handoff lock poisoned"))? = Some(value);
    Ok(())
}
fn take_slot<T>(slot: &Arc<Mutex<Option<T>>>) -> Result<T> {
    slot.lock()
        .map_err(|_| Error::internal("Attachment owner handoff lock poisoned"))?
        .take()
        .ok_or_else(|| Error::internal("Attachment owner chunk produced no outcome"))
}
/// The heartbeat's job progress: `{"after": <last visited key or null>, "refused": {<store key>:
/// <refusal code>}}`. `after` lets a yielded run resume after the store it last handled and is
/// null between cycles; `refused` holds the stores refused in the current cycle and, between
/// cycles, those of the last completed one. A completed cycle without refusals stores JSON null,
/// which an earlier binary also reads. Progress of earlier versions (JSON null, or the last
/// confirmed key as a string) still decodes and unknown object keys are ignored; any other shape
/// is a permanent error: the row is dead-lettered and the next server start schedules a fresh one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct HeartbeatProgress {
    after: Option<String>,
    refused: BTreeMap<String, String>,
}
impl HeartbeatProgress {
    fn decode(value: Option<Value>) -> std::result::Result<Self, erp_sdk::jobs::JobError> {
        let shape = || {
            erp_sdk::jobs::JobError::permanent(
                "owner_progress",
                "The stored attachment heartbeat progress has an unknown shape; this heartbeat row is dead-lettered and the next server start schedules a fresh one.",
            )
        };
        match value {
            None | Some(Value::Null) => Ok(Self::default()),
            Some(Value::String(after)) => Ok(Self {
                after: Some(after),
                refused: BTreeMap::new(),
            }),
            Some(Value::Object(map)) => {
                // Keys a later version adds are ignored, so a rollback still reads its progress.
                let after = match map.get("after") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(after)) => Some(after.clone()),
                    Some(_) => return Err(shape()),
                };
                let mut refused = BTreeMap::new();
                match map.get("refused") {
                    None => {}
                    Some(Value::Object(stores)) => {
                        for (key, code) in stores {
                            let Value::String(code) = code else {
                                return Err(shape());
                            };
                            refused.insert(key.clone(), code.clone());
                        }
                    }
                    Some(_) => return Err(shape()),
                }
                Ok(Self { after, refused })
            }
            Some(_) => Err(shape()),
        }
    }
    fn encode(&self) -> Value {
        let refused: Map<String, Value> = self
            .refused
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        json!({"after": self.after, "refused": refused})
    }
    /// The progress once `key` was beaten.
    fn with_beaten(&self, key: &str) -> Self {
        let mut next = self.clone();
        next.after = Some(key.to_owned());
        next.refused.remove(key);
        next
    }
    /// The progress once `key` was refused.
    fn with_refused(&self, key: &str, refusal: Refusal) -> Self {
        let mut next = self.clone();
        next.after = Some(key.to_owned());
        next.refused
            .insert(key.to_owned(), refusal.code().to_owned());
        next
    }
}

/// The refusal a settlement means for the heartbeat, `None` when the store was beaten.
/// `published` is true after the contest publication step: a failed publication leaves the
/// store `contest_pending`, not merely failing.
fn refusal_of(outcome: &Settlement, published: bool) -> Option<Refusal> {
    match outcome {
        Settlement::Confirmed { .. } => None,
        Settlement::ContestPending => Some(Refusal::ContestPending),
        Settlement::Contested => Some(Refusal::Contested),
        Settlement::Retry(_) if published => Some(Refusal::ContestPending),
        Settlement::Retry(_) => Some(Refusal::StoreFailed),
        Settlement::Superseded => Some(Refusal::Superseded),
    }
}

fn log_refusal(key: &str, refusal: Refusal, detail: &str) {
    tracing::error!(
        target: "erp::attachments",
        store = key,
        code = refusal.code(),
        error = detail,
        "the attachment heartbeat refused this store; the next stores are still beaten and its cleanup stays refused"
    );
}

impl erp_sdk::jobs::JobHandler for HeartbeatJob {
    fn run<'a>(
        &'a self,
        ctx: &'a mut erp_sdk::jobs::JobContext<'_>,
    ) -> erp_sdk::BoxFuture<
        'a,
        std::result::Result<erp_sdk::jobs::RunOutcome, erp_sdk::jobs::JobError>,
    > {
        use erp_sdk::jobs::{ChunkResult, JobError, RunOutcome};
        Box::pin(async move {
            // An absent progress value is the first run, as defined by JOBS. A completed cycle
            // resets `after`; a yielded run resumes after its last visited key. A new cycle
            // records its refusals from scratch, a resumed one keeps what it recorded so far.
            let stored = ctx.progress::<Value>()?;
            let decoded = HeartbeatProgress::decode(stored.clone())?;
            let mut state = if decoded.after.is_some() {
                decoded
            } else {
                HeartbeatProgress::default()
            };
            let resume_after = state.after.clone();
            let initial_progress = stored.unwrap_or(Value::Null);
            let keys = Arc::new(Mutex::new(None));
            let out = Arc::clone(&keys);
            let job_id = ctx.job().id();
            ctx.chunk(move |tx| {
                let out = Arc::clone(&out);
                let progress = initial_progress.clone();
                Box::pin(async move {
                    let rows = tx
                        .query("SELECT singleton_key FROM erp_job WHERE id=$1", &[&job_id])
                        .await?;
                    let singleton: Option<String> = decode(rows[0].try_get(0))?;
                    if singleton.as_deref() != Some(SINGLETON) {
                        return Err(JobError::permanent(
                            "owner_singleton",
                            "Attachment heartbeat requires the shared ownership singleton",
                        ));
                    }
                    let rt = runtime(&tx.env())?;
                    put_slot(&out, rt.store_keys().map(str::to_owned).collect::<Vec<_>>())?;
                    Ok(ChunkResult::more(progress, 0, None))
                })
            })
            .await?;
            for key in take_slot(&keys)?
                .into_iter()
                .filter(|key| resume_after.as_ref().is_none_or(|after| key > after))
            {
                if ctx.budget_exhausted() {
                    return Ok(RunOutcome::Continue);
                }
                // Each chunk computes the progress it commits; the loop's state follows only
                // after the commit, since a chunk body can run again on a retryable error.
                let preparing_key = key.clone();
                let current = state.clone();
                let pending = Arc::new(Mutex::new(None));
                let out = Arc::clone(&pending);
                ctx.chunk(move |tx| {
                    let out = Arc::clone(&out);
                    let key = preparing_key.clone();
                    let current = current.clone();
                    Box::pin(async move {
                        // Observe: settle opens a cleanup window only when none is open, and
                        // later beats keep it, so scheduled cleanup becomes admitted on its own.
                        // A refusal of this store is a Rust-level return after statements that
                        // all succeeded, so the chunk commits it with the progress.
                        let prepared = prepare_store(tx, &key, Purpose::Observe).await?;
                        let next = match &prepared {
                            Ok(_) => current,
                            Err(refused) => current.with_refused(&key, refused.refusal),
                        };
                        let progress = next.encode();
                        put_slot(&out, (prepared, next))?;
                        Ok(ChunkResult::more(progress, 0, None))
                    })
                })
                .await?;
                let (prepared, next) = take_slot(&pending)?;
                state = next;
                let p = match prepared {
                    Ok(p) => p,
                    Err(refused) => {
                        log_refusal(&key, refused.refusal, &refused.error.to_string());
                        continue;
                    }
                };
                let attempt = ctx
                    .external(async |cancel| {
                        if cancel.is_cancelled() {
                            return Err(JobError::transient(
                                "owner_cancelled",
                                "Attachment heartbeat cancelled before store mutation",
                            ));
                        }
                        Ok(p.execute().await)
                    })
                    .await?;
                let outcome = Arc::new(Mutex::new(None));
                let out = Arc::clone(&outcome);
                let prepared = p.clone();
                let settling_key = key.clone();
                let current = state.clone();
                ctx.chunk(move |tx| {
                    let out = Arc::clone(&out);
                    let p = prepared.clone();
                    let attempt = attempt.clone();
                    let key = settling_key.clone();
                    let current = current.clone();
                    Box::pin(async move {
                        let outcome = settle(tx, &p, attempt).await?;
                        let next = match (&outcome, refusal_of(&outcome, false)) {
                            // the contest is published next; the store is not settled yet
                            (Settlement::ContestPending, _) => current,
                            (_, None) => current.with_beaten(&key),
                            (_, Some(refusal)) => current.with_refused(&key, refusal),
                        };
                        let progress = next.encode();
                        put_slot(&out, (outcome, next))?;
                        Ok(ChunkResult::more(progress, 0, None))
                    })
                })
                .await?;
                let (mut result, next) = take_slot(&outcome)?;
                state = next;
                let mut published = false;
                if matches!(result, Settlement::ContestPending) {
                    let contest = p.clone();
                    let publication = ctx
                        .external(async |_cancel| Ok(contest.publish_contest().await))
                        .await?;
                    let out = Arc::clone(&outcome);
                    let publishing_key = key.clone();
                    let current = state.clone();
                    ctx.chunk(move |tx| {
                        let p = p.clone();
                        let publication = publication.clone();
                        let out = Arc::clone(&out);
                        let key = publishing_key.clone();
                        let current = current.clone();
                        Box::pin(async move {
                            let outcome = settle_contest(tx, &p, publication).await?;
                            let next = match (&outcome, refusal_of(&outcome, true)) {
                                (Settlement::Confirmed { .. } | Settlement::ContestPending, _)
                                | (_, None) => {
                                    return Err(JobError::from(Error::programming(
                                        "Attachment contest publication settled as neither contested, failed nor superseded",
                                    )));
                                }
                                (_, Some(refusal)) => current.with_refused(&key, refusal),
                            };
                            let progress = next.encode();
                            put_slot(&out, (outcome, next))?;
                            Ok(ChunkResult::more(progress, 0, None))
                        })
                    })
                    .await?;
                    let (contest_result, next) = take_slot(&outcome)?;
                    result = contest_result;
                    state = next;
                    published = true;
                }
                if let Some(refusal) = refusal_of(&result, published) {
                    let detail = match &result {
                        Settlement::Retry(error) => error.to_string(),
                        Settlement::Confirmed { .. }
                        | Settlement::ContestPending
                        | Settlement::Contested
                        | Settlement::Superseded => refusal.code().to_owned(),
                    };
                    log_refusal(&key, refusal, &detail);
                }
            }
            state.after = None;
            // A clean cycle stores null, the progress an earlier binary reads after a rollback.
            let done = if state.refused.is_empty() {
                Value::Null
            } else {
                state.encode()
            };
            ctx.checkpoint(&done, 0, None).await?;
            Ok(RunOutcome::RescheduleAfter(Duration::from_secs(
                HEARTBEAT_SECS,
            )))
        })
    }
}
