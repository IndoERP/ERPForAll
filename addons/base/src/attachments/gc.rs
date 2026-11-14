//! Garbage collection, staging cleanup, reconcile and verification of the blob store, as jobs on
//! the durable queue. No scheduler of its own, no leader: every
//! kind is a leased job that any node may claim, and a singleton key keeps one run per database.
//!
//! **Rule R.** A content key is removed only by the sweep, and only while it holds the `FOR
//! UPDATE` lock on the `ir_blob` row of that digest. Everything else records what it wants gone
//! durably (a `deleting` row, a candidate) and lets the sweep decide.
//!
//! **Never deleting what another node is still writing.** A writer holds `FOR KEY SHARE` on the
//! blob row from its link until commit, and the sweep takes `FOR UPDATE ... SKIP LOCKED`, so a row
//! a writer holds is skipped, not removed. A blob nobody references is first only marked
//! `deleting` (Phase A); its bytes leave after `attachments.gc_grace_secs` and only if, in a second
//! transaction, the row is still unreferenced, still `deleting` and not locked (Phase B). A writer
//! that arrives in between revives the row after proving its bytes (see [`super::link`]). The
//! upload row of a staged object is the write-ahead marker: it commits before the first byte
//! reaches the store and outlives the link, so an object promoted by a transaction that rolled
//! back is found through its expired upload row, never by listing the store.
//!
//! **Limits across nodes.** Everything that must hold for the whole cluster is decided in
//! PostgreSQL, never by a process-local primitive: one run per kind is the job queue's singleton
//! key and lease, the hold/sweep exclusion is the advisory lock [`GC_HOLD_LOCK`], and the
//! row locks of rule R. The only process-local bound is the node's stream admission
//! (`attachments.stream_permits_per_node`), used by deep verification here.
//!
//! **Lock order.** Destructive transactions first take the ownership coordinator, store metadata
//! and owner rows in store-id order, then [`GC_HOLD_LOCK`] (shared), then the `ir_blob`/upload
//! row locks (`FOR UPDATE SKIP LOCKED`); [`hold`] takes only the advisory lock (exclusive) and no
//! row lock, and a writer takes only row locks (`FOR KEY SHARE`), so no cycle exists. The shared
//! lock is held across Phase B's one `delete_many` call (at most [`CHUNK`] keys, no byte
//! streamed), because the hold must wait until those keys are gone. Deep verification streams
//! with no lock of this module, but inside its job chunk's transaction, which holds the queue's
//! own fence and singleton lock for the chunk; those exclude only other runs of the
//! same kind, and the stream admission bounds the reads.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use erp_sdk::blob::{BlobError, ContentKey, StagingKey};
use erp_sdk::jobs::{
    ChunkResult, HandlerRef, JobContext, JobError, JobHandler, JobKey, JobKindDecl, KindIsolation,
    PrincipalPolicy, RecurringSpec, RunOutcome, TimeoutClass, When,
};
use erp_sdk::prelude::*;
use erp_sdk::{BoxFuture, Env, Orm, PrincipalKind, SUPERUSER_ID};

use super::link::{BlobRow, prove_present};
use super::runtime::{blob_error, col_error, runtime, store_by_id};

/// Candidates one Phase A chunk claims, and keys one Phase B chunk removes (the `DeleteObjects`
/// limit of S3).
pub const CHUNK: i64 = 1000;

/// What one Phase A chunk did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MarkReport {
    /// Candidates taken off the queue.
    pub claimed: u64,
    /// Blobs marked `deleting` (or whose grace restarted).
    pub marked: u64,
}

/// What one Phase B removal did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SweepReport {
    /// Blob rows removed.
    pub rows: u64,
    /// Content keys removed from external stores.
    pub keys: u64,
    /// Keys that were already absent (a lost commit finished).
    pub already_absent: u64,
}

fn is_system(env: &Env) -> bool {
    env.principal().kind() == PrincipalKind::System || env.su()
}

async fn require_maintenance(tx: &mut Transaction, env: &Env) -> Result<()> {
    if is_system(env) || tx.has_group(env, "base.group_system").await? {
        return Ok(());
    }
    Err(Error::access(
        "Attachment garbage collection is maintenance: only the system group may run it.",
    ))
}

/// Phase A, one chunk: claim candidates, lock their blob rows (`FOR UPDATE SKIP LOCKED`), and
/// mark the unreferenced ones `deleting`, restarting the grace of rows already `deleting`.
/// Candidates whose rows a writer holds are queued again; candidates without a row are dropped.
///
/// # Errors
///
/// Requires maintenance authority and READ COMMITTED before mutation. Holds pause marking;
/// ownership admission remains mandatory at the external removal phases. SQL failures propagate.
pub async fn mark_unreferenced(tx: &mut Transaction, limit: i64) -> Result<MarkReport> {
    if !(1..=CHUNK).contains(&limit) {
        return Err(Error::validation(
            "Attachment cleanup batch is 1 to 1000 rows.",
        ));
    }
    require_maintenance(tx, &tx.env()).await?;
    super::cleanup::start(tx).await?;
    if held(tx).await? {
        return Ok(MarkReport::default());
    }
    // Metadata marking removes no object. External removal still requires per-store admission
    // at both sweep phases; it cannot be authorized by a marking result.
    let rows = tx
        .query(
            "WITH claimed AS ( \
                DELETE FROM ir_blob_gc_candidate WHERE id IN ( \
                    SELECT id FROM ir_blob_gc_candidate ORDER BY id LIMIT $1 \
                    FOR UPDATE SKIP LOCKED) RETURNING blob_id), \
             locked AS ( \
                SELECT b.id FROM ir_blob b WHERE b.id IN (SELECT blob_id FROM claimed) \
                ORDER BY b.id FOR UPDATE SKIP LOCKED), \
             marked AS ( \
                UPDATE ir_blob b SET state = 1, deleting_since = now() FROM locked l \
                WHERE b.id = l.id \
                  AND NOT EXISTS (SELECT 1 FROM ir_attachment a WHERE a.blob_id = b.id) \
                RETURNING b.id), \
             requeued AS ( \
                INSERT INTO ir_blob_gc_candidate (blob_id) \
                SELECT DISTINCT c.blob_id FROM claimed c \
                WHERE c.blob_id NOT IN (SELECT id FROM locked) \
                  AND EXISTS (SELECT 1 FROM ir_blob b WHERE b.id = c.blob_id) \
                RETURNING 1) \
             SELECT (SELECT count(*) FROM claimed), (SELECT count(*) FROM marked)",
            &[&limit],
        )
        .await?;
    let row = rows
        .first()
        .ok_or_else(|| Error::db("the garbage collection statement returned no row"))?;
    let claimed: i64 = row.try_get(0).map_err(|e| col_error("gc", e))?;
    let marked: i64 = row.try_get(1).map_err(|e| col_error("gc", e))?;
    Ok(MarkReport {
        claimed: u64::try_from(claimed).unwrap_or(0),
        marked: u64::try_from(marked).unwrap_or(0),
    })
}

/// The transaction advisory lock that orders garbage collection holds against the sweep:
/// `(class, key)` = (`"ERPF"`, 7), the class of `erp_modules::system::LOCK_CLASS` (keys 1 to 6
/// belong to the module system, node readiness, the authentication collector, the UI asset
/// store and the node feature registry).
///
/// [`sweep_mark`] and [`sweep_remove`] take it **shared** before they read the holds and keep it
/// until their transaction ends, so sweeps never wait for each other; [`hold`] takes it
/// **exclusive** before it inserts, so it waits for every sweep that already read the holds. A
/// hold that has committed is therefore seen by every later sweep, and no sweep that missed it is
/// still removing keys: a backup that starts after its hold commits never references a key a
/// sweep is about to delete ("Holds").
pub const GC_HOLD_LOCK: (i32, i32) = (0x4552_5046, 7);

/// Take [`GC_HOLD_LOCK`] shared, for the rest of the transaction. The sweep must run in
/// `READ COMMITTED` (its job kinds do): a `REPEATABLE READ` snapshot is taken before the lock
/// wait, so it could miss the hold it waited for.
async fn lock_for_sweep(tx: &mut Transaction) -> Result<()> {
    let rows = tx
        .query(
            "SELECT current_setting('transaction_isolation') = 'read committed', \
             pg_advisory_xact_lock_shared($1::int4, $2::int4)::text",
            &[&GC_HOLD_LOCK.0, &GC_HOLD_LOCK.1],
        )
        .await?;
    let read_committed: bool = rows
        .first()
        .ok_or_else(|| Error::db("the sweep lock statement returned no row"))?
        .try_get(0)
        .map_err(|e| col_error("gc", e))?;
    if !read_committed {
        return Err(Error::programming(
            "The garbage collection sweep runs in a READ COMMITTED transaction (its job kinds \
             do): a REPEATABLE READ snapshot predates the hold lock and could miss a hold.",
        ));
    }
    Ok(())
}

/// Whether a hold is active, read after [`lock_for_sweep`].
async fn held(tx: &mut Transaction) -> Result<bool> {
    lock_for_sweep(tx).await?;
    let rows = tx
        .query(
            "SELECT EXISTS (SELECT 1 FROM ir_blob_gc_hold WHERE until > now())",
            &[],
        )
        .await?;
    rows.first()
        .ok_or_else(|| Error::db("the hold query returned no row"))?
        .try_get(0)
        .map_err(|e| col_error("gc", e))
}

/// A store that cleanup left out of one call, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct RefusedStore {
    /// The store's `ir_blob_store` id.
    pub store_id: i16,
    /// The fixed reason of the refusal (the text of `BlobError::CleanupRefused`), or
    /// `store_error` when the store layer failed in another way; never store response text.
    pub reason: &'static str,
}

fn refused_stores(refused: Vec<(i16, &'static str)>) -> Vec<RefusedStore> {
    refused
        .into_iter()
        .map(|(store_id, reason)| RefusedStore { store_id, reason })
        .collect()
}

/// What one [`sweep_mark`] call did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct SweepMark {
    /// The ids marked as swept, ascending: pass them to [`sweep_remove`].
    pub ids: Vec<i64>,
    /// The stores with due rows that were not admitted; their rows stay `deleting`, unswept.
    pub refused: Vec<RefusedStore>,
    /// An active hold stopped the call before any store was checked (`ids` and `refused` are
    /// empty because nothing was looked at, not because every store was admitted).
    pub held: bool,
}

/// Phase B, first transaction: mark the `deleting` rows older than the grace as swept
/// (`sweep_attempted_at`), so a later revive must prove their bytes, and return their ids. No
/// rows while any hold is active. Takes [`GC_HOLD_LOCK`] shared until the transaction ends.
/// Each external store is admitted once per call; the rows of a store that is not admitted
/// (ownership, observation, capability or contest) are left out, the refusal is logged at
/// error level and returned in [`SweepMark::refused`], so inline rows and other stores are
/// still swept.
///
/// # Errors
///
/// The database's. The caller must roll back on error. Requires System/system-group
/// maintenance and READ COMMITTED.
pub async fn sweep_mark(tx: &mut Transaction, grace_secs: u64, limit: i64) -> Result<SweepMark> {
    if !(1..=CHUNK).contains(&limit) {
        return Err(Error::validation(
            "Attachment cleanup batch is 1 to 1000 rows.",
        ));
    }
    require_maintenance(tx, &tx.env()).await?;
    super::cleanup::start(tx).await?;
    if held(tx).await? {
        return Ok(SweepMark {
            held: true,
            ..SweepMark::default()
        });
    }
    let grace = grace_secs as f64;
    let due = "state=1 AND deleting_since < now()-make_interval(secs=>$1::float8) \
               AND NOT EXISTS(SELECT 1 FROM ir_attachment a WHERE a.blob_id=b.id)";
    let stores = tx
        .query(
            &format!(
                "SELECT DISTINCT store_id FROM ir_blob b WHERE {due} AND store_id IS NOT NULL \
                 ORDER BY store_id"
            ),
            &[&grace],
        )
        .await?;
    let stores = stores
        .iter()
        .map(|r| r.try_get::<_, i16>(0).map_err(|e| col_error("gc", e)))
        .collect::<Result<Vec<_>>>()?;
    // Rows of a store that is not admitted stay `deleting`, unswept, until a later run.
    let (admitted, refused) = super::cleanup::admit(tx, stores).await?;
    let rows = tx
        .query(
            &format!(
                "UPDATE ir_blob SET sweep_attempted_at=now() WHERE id IN ( \
                 SELECT id FROM ir_blob b WHERE {due} \
                 AND (store_id IS NULL OR store_id = ANY($3::int2[])) \
                 ORDER BY id LIMIT $2 FOR UPDATE SKIP LOCKED) RETURNING id"
            ),
            &[&grace, &limit, &admitted],
        )
        .await?;
    let mut ids = Vec::with_capacity(rows.len());
    for row in &rows {
        ids.push(row.try_get(0).map_err(|e| col_error("gc", e))?);
    }
    ids.sort_unstable();
    Ok(SweepMark {
        ids,
        refused: refused_stores(refused),
        held: false,
    })
}

/// Phase B, second transaction: lock the same rows again, re-check state, grace and references,
/// re-read the holds, remove the rows, then remove the keys of external stores before the commit.
/// A store error fails the chunk: the rows stay `deleting` with `sweep_attempted_at` set, so no
/// writer revives them without proving the bytes. Takes [`GC_HOLD_LOCK`] shared before it reads
/// the holds, until the transaction ends, so a [`hold`] waits for the keys it removes.
///
/// # Errors
///
/// Store errors as `Internal`; the database's.
pub async fn sweep_remove(
    tx: &mut Transaction,
    env: &Env,
    ids: &[i64],
    grace_secs: u64,
) -> Result<SweepReport> {
    if ids.len() > CHUNK as usize {
        return Err(Error::validation(
            "Attachment cleanup batch is 1 to 1000 rows.",
        ));
    }
    require_maintenance(tx, env).await?;
    super::cleanup::start(tx).await?;
    if ids.is_empty() || held(tx).await? {
        return Ok(SweepReport::default());
    }
    let rt = runtime(env)?;
    let stores = tx.query("SELECT DISTINCT store_id FROM ir_blob WHERE id=ANY($1::int8[]) AND store_id IS NOT NULL ORDER BY store_id", &[&ids]).await?;
    for row in stores {
        super::cleanup::check(tx, row.try_get(0).map_err(|e| col_error("gc", e))?).await?;
    }
    let grace = grace_secs as f64;
    let locked = tx
        .query(
            "SELECT b.id, b.sha256, b.store_id FROM ir_blob b \
             WHERE b.id = ANY($1::int8[]) AND b.state = 1 AND b.sweep_attempted_at IS NOT NULL \
               AND b.deleting_since < now() - make_interval(secs => $2::float8) \
               AND NOT EXISTS (SELECT 1 FROM ir_attachment a WHERE a.blob_id = b.id) \
             ORDER BY b.id FOR UPDATE SKIP LOCKED",
            &[&ids, &grace],
        )
        .await?;
    let mut rows: Vec<(i64, [u8; 32], Option<i16>)> = Vec::with_capacity(locked.len());
    for row in &locked {
        let sha: Vec<u8> = row.try_get(1).map_err(|e| col_error("gc", e))?;
        let sha = <[u8; 32]>::try_from(sha.as_slice())
            .map_err(|_| Error::db("an ir_blob digest is not 32 bytes"))?;
        rows.push((
            row.try_get(0).map_err(|e| col_error("gc", e))?,
            sha,
            row.try_get(2).map_err(|e| col_error("gc", e))?,
        ));
    }
    if rows.is_empty() {
        return Ok(SweepReport::default());
    }
    let remove: Vec<i64> = rows.iter().map(|r| r.0).collect();
    let removed = tx
        .execute("DELETE FROM ir_blob WHERE id = ANY($1::int8[])", &[&remove])
        .await?;
    let mut report = SweepReport {
        rows: removed,
        ..SweepReport::default()
    };
    // external keys, grouped by store, before the commit
    let mut by_store: Vec<(i16, Vec<ContentKey>)> = Vec::new();
    for (_, sha, store_id) in &rows {
        let Some(store_id) = store_id else { continue };
        let store = store_by_id(tx, &rt, *store_id).await?;
        let key = ContentKey::new(store.namespace.clone(), *sha);
        match by_store.iter_mut().find(|(id, _)| id == store_id) {
            Some((_, keys)) => keys.push(key),
            None => by_store.push((*store_id, vec![key])),
        }
    }
    for (store_id, keys) in by_store {
        let store = store_by_id(tx, &rt, store_id).await?;
        super::cleanup::check(tx, store_id).await?;
        let done = store.store.delete_many(&keys).await.map_err(blob_error)?;
        report.keys += done.deleted as u64;
        report.already_absent += done.already_absent as u64;
    }
    Ok(report)
}

/// What one staging cleanup chunk did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct CleanReport {
    /// Consumed uploads removed with their staging objects.
    pub consumed: u64,
    /// Expired uploads removed with their staging objects.
    pub expired: u64,
    /// `deleting` rows created for keys an expired upload may have promoted (rule R).
    pub adopted: u64,
    /// Files uploaded for a composer and never posted for a day, removed (their blobs become
    /// garbage candidates through the table's statement trigger).
    pub abandoned: u64,
    /// Store-backed uploads left in place, with their reservations, because their store is not
    /// admitted for cleanup now (each refusal is logged at error level with the store and the
    /// reason); a later run retries them.
    pub skipped: u64,
    /// The stores with work that were not admitted, each with its reason.
    pub refused: Vec<RefusedStore>,
    /// An active hold stopped the chunk before any store was checked.
    pub held: bool,
}

/// A file of any other model with `res_id` 0 is never removed here.
const PENDING_PLACEHOLDERS: [&str; 1] = ["mail.compose.message"];

/// One chunk of the staging cleaner: consumed uploads (their object, if still staged, and row),
/// then expired ones. An expired upload whose digest has no blob row may have had its bytes
/// promoted by a transaction that rolled back: a `deleting` row is created for it so the sweep
/// removes the key. Each external store is admitted once per chunk; the rows of a store that is
/// not admitted stay with their reservations, are counted in [`CleanReport::skipped`] and the
/// store is named in [`CleanReport::refused`], so inline rows and the rows of admitted stores are
/// still cleaned.
///
/// # Errors
///
/// Store deletion errors as `Internal`; the database's. Roll back on error to retain tracking
/// and reservations. Requires System/system-group maintenance and READ COMMITTED.
pub async fn clean_staging(tx: &mut Transaction, env: &Env, limit: i64) -> Result<CleanReport> {
    if !(1..=CHUNK).contains(&limit) {
        return Err(Error::validation(
            "Attachment cleanup batch is 1 to 1000 rows.",
        ));
    }
    require_maintenance(tx, env).await?;
    super::cleanup::start(tx).await?;
    if held(tx).await? {
        return Ok(CleanReport {
            held: true,
            ..CleanReport::default()
        });
    }
    let rt = runtime(env)?;
    let retention = super::settings::Policy::load(tx).await?.composer_retention as f64;
    let mut report = CleanReport::default();
    // Each store is admitted once per chunk, before any row lock (the coordinator lock is held
    // for the whole chunk); rows of a refused store are left in place and counted as skipped.
    let stores = tx
        .query(
            "SELECT DISTINCT store_id FROM ir_blob_upload WHERE store_id IS NOT NULL \
             AND inline_data IS NULL AND (state IN (2, 3) OR (state IN (0, 1) AND expires_at < now())) \
             ORDER BY store_id",
            &[],
        )
        .await?;
    let stores = stores
        .iter()
        .map(|r| r.try_get::<_, i16>(0).map_err(|e| col_error("gc", e)))
        .collect::<Result<Vec<_>>>()?;
    let (admitted, refused) = super::cleanup::admit(tx, stores).await?;
    report.refused = refused_stores(refused);
    let refused: Vec<i16> = report.refused.iter().map(|r| r.store_id).collect();
    for (state_sql, consumed) in [
        ("state = 2", true),
        (
            "state = 3 OR (state IN (0, 1) AND expires_at < now())",
            false,
        ),
    ] {
        if !refused.is_empty() {
            let rows = tx
                .query(
                    &format!(
                        "SELECT count(*) FROM ir_blob_upload WHERE ({state_sql}) \
                         AND inline_data IS NULL AND store_id = ANY($1::int2[])"
                    ),
                    &[&refused],
                )
                .await?;
            let skipped: i64 = rows
                .first()
                .ok_or_else(|| Error::db("the skipped upload count returned no row"))?
                .try_get(0)
                .map_err(|e| col_error("gc", e))?;
            report.skipped += u64::try_from(skipped).unwrap_or(0);
        }
        let rows = tx
            .query(
                &format!(
                    "SELECT id::text, store_id, sha256, size, (inline_data IS NOT NULL) \
                     FROM ir_blob_upload WHERE ({state_sql}) \
                     AND (store_id IS NULL OR inline_data IS NOT NULL OR store_id = ANY($2::int2[])) \
                     ORDER BY id LIMIT $1 FOR UPDATE SKIP LOCKED"
                ),
                &[&limit, &admitted],
            )
            .await?;
        let mut ids: Vec<String> = Vec::with_capacity(rows.len());
        for row in &rows {
            let id: String = row.try_get(0).map_err(|e| col_error("gc", e))?;
            let store_id: Option<i16> = row.try_get(1).map_err(|e| col_error("gc", e))?;
            let sha: Option<Vec<u8>> = row.try_get(2).map_err(|e| col_error("gc", e))?;
            let size: Option<i64> = row.try_get(3).map_err(|e| col_error("gc", e))?;
            let inline: bool = row.try_get(4).map_err(|e| col_error("gc", e))?;
            if let (Some(store_id), false) = (store_id, inline) {
                let store = store_by_id(tx, &rt, store_id).await?;
                let key = StagingKey::new(store.namespace.clone(), &id).map_err(blob_error)?;
                store.store.delete_staged(&key).await.map_err(blob_error)?;
                if !consumed && let (Some(sha), Some(size)) = (&sha, size) {
                    let adopted = tx
                        .execute(
                            "INSERT INTO ir_blob (sha256, size, store_id, state, deleting_since, \
                             sweep_attempted_at) VALUES ($1, $2, $3, 1, now(), now()) \
                             ON CONFLICT (sha256) DO NOTHING",
                            &[sha, &size, &store_id],
                        )
                        .await?;
                    report.adopted += adopted;
                }
            }
            ids.push(id);
        }
        if !ids.is_empty() {
            tx.execute(
                "DELETE FROM ir_blob_upload WHERE id = ANY($1::text[]::uuid[])",
                &[&ids],
            )
            .await?;
        }
        if consumed {
            report.consumed += ids.len() as u64;
        } else {
            report.expired += ids.len() as u64;
        }
    }
    let placeholders: Vec<String> = PENDING_PLACEHOLDERS
        .iter()
        .map(|m| (*m).to_owned())
        .collect();
    report.abandoned = tx
        .execute(
            "DELETE FROM ir_attachment WHERE id IN ( \
                 SELECT id FROM ir_attachment \
                 WHERE res_model = ANY($1::text[]) AND coalesce(res_id, 0) = 0 AND res_field IS NULL \
                   AND create_date < (now() AT TIME ZONE 'UTC') - make_interval(secs=>$3::float8) \
                 ORDER BY id LIMIT $2 FOR UPDATE SKIP LOCKED)",
            &[&placeholders, &limit, &retention],
        )
        .await?;
    Ok(report)
}

/// What one reconcile chunk did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReconcileReport {
    /// The highest blob id looked at (the next chunk starts after it).
    pub last_id: i64,
    /// Blob rows looked at.
    pub looked_at: u64,
    /// Unreferenced `live` rows queued as candidates.
    pub queued: u64,
    /// Referenced `deleting` rows returned to `live` after their bytes were proved.
    pub revived: u64,
}

/// One chunk of the reconcile walk: blobs after `after_id` by id. Triggers do not fire under
/// `session_replication_role = replica` or `pg_restore --disable-triggers`; this walk is the path
/// that finds unreferenced blobs then. A referenced `deleting` row (only a raw `blob_id` write
/// can make one) goes back to `live` after a full read and SHA-256 check under its row lock.
///
/// # Errors
///
/// `Internal` naming the blob when a referenced `deleting` row's bytes cannot be proved; `Lock`
/// (retry) when the node's stream admission is full; store errors; the database's.
pub async fn reconcile_chunk(
    tx: &mut Transaction,
    env: &Env,
    after_id: i64,
    limit: i64,
) -> Result<ReconcileReport> {
    if !(1..=CHUNK).contains(&limit) {
        return Err(Error::validation(
            "Attachment cleanup batch is 1 to 1000 rows.",
        ));
    }
    require_maintenance(tx, env).await?;
    super::cleanup::start(tx).await?;
    if held(tx).await? {
        return Ok(ReconcileReport {
            last_id: after_id,
            ..ReconcileReport::default()
        });
    }
    // Reconcile reads external bytes to prove referenced content; it never removes store objects.
    let rt = runtime(env)?;
    let rows = tx
        .query(
            "SELECT id, state, sha256, store_id, size, \
                    EXISTS (SELECT 1 FROM ir_attachment a WHERE a.blob_id = ir_blob.id) \
             FROM ir_blob WHERE id > $1 ORDER BY id LIMIT $2",
            &[&after_id, &limit],
        )
        .await?;
    let mut report = ReconcileReport {
        last_id: after_id,
        ..ReconcileReport::default()
    };
    let mut queue: Vec<i64> = Vec::new();
    let mut referenced_deleting: Vec<i64> = Vec::new();
    for row in &rows {
        let id: i64 = row.try_get(0).map_err(|e| col_error("gc", e))?;
        let state: i16 = row.try_get(1).map_err(|e| col_error("gc", e))?;
        let referenced: bool = row.try_get(5).map_err(|e| col_error("gc", e))?;
        report.last_id = id;
        report.looked_at += 1;
        match (state, referenced) {
            (0, false) => queue.push(id),
            (1, true) => referenced_deleting.push(id),
            _ => {}
        }
    }
    if !queue.is_empty() {
        report.queued = tx
            .execute(
                "INSERT INTO ir_blob_gc_candidate (blob_id) SELECT unnest($1::int8[])",
                &[&queue],
            )
            .await?;
    }
    if !referenced_deleting.is_empty() {
        let locked = tx
            .query(
                "SELECT id, sha256, store_id, size FROM ir_blob \
                 WHERE id = ANY($1::int8[]) AND state = 1 ORDER BY id FOR UPDATE SKIP LOCKED",
                &[&referenced_deleting],
            )
            .await?;
        let mut proved: Vec<i64> = Vec::new();
        for row in &locked {
            let sha: Vec<u8> = row.try_get(1).map_err(|e| col_error("gc", e))?;
            let candidate = BlobRow {
                id: row.try_get(0).map_err(|e| col_error("gc", e))?,
                sha256: <[u8; 32]>::try_from(sha.as_slice())
                    .map_err(|_| Error::db("an ir_blob digest is not 32 bytes"))?,
                deleting: true,
                swept: true,
                store_id: row.try_get(2).map_err(|e| col_error("gc", e))?,
                size: row.try_get(3).map_err(|e| col_error("gc", e))?,
                mimetype: None,
            };
            prove_present(tx, &rt, &candidate).await.map_err(|e| {
                // a busy node (stream admission full) is not an integrity alarm: pass it through
                if e.kind() == erp_sdk::ErrorKind::Lock {
                    return e;
                }
                Error::internal(format!(
                    "Blob {} is referenced by an attachment but marked deleting and its bytes \
                     cannot be proved present: {e}",
                    candidate.id
                ))
            })?;
            proved.push(candidate.id);
        }
        if !proved.is_empty() {
            report.revived = tx
                .execute(
                    "UPDATE ir_blob SET state = 0, deleting_since = NULL, sweep_attempted_at = NULL \
                     WHERE id = ANY($1::int8[])",
                    &[&proved],
                )
                .await?;
        }
    }
    Ok(report)
}

/// One problem `verify` found.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct VerifyProblem {
    /// The blob row.
    pub blob_id: i64,
    /// What is wrong: `missing`, `size` or `digest`.
    pub kind: &'static str,
}

/// What one verify chunk did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VerifyReport {
    /// The highest blob id looked at.
    pub last_id: i64,
    /// `live` rows checked.
    pub checked: u64,
    /// What was wrong.
    pub problems: Vec<VerifyProblem>,
}

/// One chunk of verification: every `live` external row after `after_id` is checked against the
/// store (size from a `head`; with `deep`, the bytes are read and hashed). Nothing is repaired.
///
/// # Errors
///
/// Store errors other than a missing object; the database's.
pub async fn verify_chunk(
    tx: &mut Transaction,
    env: &Env,
    after_id: i64,
    limit: i64,
    deep: bool,
) -> Result<VerifyReport> {
    let rt = runtime(env)?;
    let rows = tx
        .query(
            "SELECT id, sha256, store_id, size FROM ir_blob \
             WHERE id > $1 AND state = 0 AND store_id IS NOT NULL ORDER BY id LIMIT $2",
            &[&after_id, &limit],
        )
        .await?;
    let mut report = VerifyReport {
        last_id: after_id,
        ..VerifyReport::default()
    };
    for row in &rows {
        let id: i64 = row.try_get(0).map_err(|e| col_error("verify", e))?;
        let sha: Vec<u8> = row.try_get(1).map_err(|e| col_error("verify", e))?;
        let store_id: i16 = row.try_get(2).map_err(|e| col_error("verify", e))?;
        let size: i64 = row.try_get(3).map_err(|e| col_error("verify", e))?;
        let sha = <[u8; 32]>::try_from(sha.as_slice())
            .map_err(|_| Error::db("an ir_blob digest is not 32 bytes"))?;
        report.last_id = id;
        report.checked += 1;
        let store = store_by_id(tx, &rt, store_id).await?;
        let key = ContentKey::new(store.namespace.clone(), sha);
        match store.store.head(&key).await {
            Err(BlobError::NotFound { .. }) => {
                report.problems.push(VerifyProblem {
                    blob_id: id,
                    kind: "missing",
                });
                continue;
            }
            Err(other) => return Err(blob_error(other)),
            Ok(meta) if i64::try_from(meta.size).ok() != Some(size) => {
                report.problems.push(VerifyProblem {
                    blob_id: id,
                    kind: "size",
                });
                continue;
            }
            Ok(_) => {}
        }
        if deep {
            let _permit = rt.try_stream_permit().map_err(blob_error)?;
            let mut read = store.store.get(&key, None).await.map_err(blob_error)?;
            let (digest, _) = erp_sdk::blob::digest_stream(&mut read.reader)
                .await
                .map_err(|e| Error::internal(format!("A blob could not be read: {e}")))?;
            if digest != sha {
                report.problems.push(VerifyProblem {
                    blob_id: id,
                    kind: "digest",
                });
            }
        }
    }
    Ok(report)
}

/// Prevent garbage collection until `seconds` from now (a backup relies on this and renews it
/// while it runs, then checks it never lapsed). Takes [`GC_HOLD_LOCK`] exclusive first, so it
/// waits for every running sweep to commit: once the hold commits, no sweep that missed it is
/// still removing keys.
///
/// # Errors
///
/// `Access` unless the system group; `Validation` for a reason outside 1 to 500 bytes or a
/// duration outside one second to 30 days.
pub async fn hold(tx: &mut Transaction, seconds: u64, reason: &str) -> Result<i64> {
    let env = tx.env();
    require_maintenance(tx, &env).await?;
    if reason.is_empty() || reason.len() > 500 || seconds == 0 || seconds > 30 * 86_400 {
        return Err(Error::validation(
            "A garbage collection hold has a reason of 1 to 500 bytes and lasts 1 second to 30 days.",
        ));
    }
    let secs = seconds as f64;
    tx.query(
        "SELECT pg_advisory_xact_lock($1::int4, $2::int4)",
        &[&GC_HOLD_LOCK.0, &GC_HOLD_LOCK.1],
    )
    .await?;
    let rows = tx
        .query(
            "INSERT INTO ir_blob_gc_hold (until, reason) \
             VALUES (now() + make_interval(secs => $1::float8), $2) RETURNING id",
            &[&secs, &reason],
        )
        .await?;
    rows.first()
        .ok_or_else(|| Error::db("the hold was not created"))?
        .try_get(0)
        .map_err(|e| col_error("gc", e))
}

/// End a hold now.
///
/// # Errors
///
/// `Access` unless the system group.
pub async fn release_hold(tx: &mut Transaction, id: i64) -> Result<()> {
    let env = tx.env();
    require_maintenance(tx, &env).await?;
    tx.execute(
        "UPDATE ir_blob_gc_hold SET until = now() WHERE id = $1 AND until > now()",
        &[&id],
    )
    .await?;
    Ok(())
}

// ---- the jobs ----------------------------------------------------------------------------

struct GcJob;
struct CleanJob;
struct ReconcileJob;
struct VerifyJob;

/// [`super::ownership::OBSERVATION_SECS`] as the signed epoch difference it is compared with.
const OBSERVATION_LIMIT_SECS: i64 = super::ownership::OBSERVATION_SECS as i64;

/// The stores cleanup keeps refusing, as the GC and staging-cleanup jobs carry them in their
/// progress: `"refused": {"<store id>": {"reason": <fixed reason>, "since": <epoch seconds>}}`.
/// `since` is the database clock at the first refusal of an unbroken run of refusals.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RefusedSince(BTreeMap<i16, (String, i64)>);

impl RefusedSince {
    /// The map in the job's stored progress; progress without one (a first run, or written by
    /// an earlier version) has none. Any other shape fails the run loudly.
    fn from_progress(
        progress: Option<erp_sdk::serde_json::Value>,
    ) -> std::result::Result<Self, JobError> {
        use erp_sdk::serde_json::Value;
        let shape = || {
            JobError::permanent(
                "cleanup_progress",
                "The stored attachment cleanup progress has an unknown 'refused' shape; this schedule row is dead-lettered and the next server start schedules a fresh one.",
            )
        };
        let Some(Value::Object(progress)) = progress else {
            return Ok(Self::default());
        };
        let Some(refused) = progress.get("refused") else {
            return Ok(Self::default());
        };
        let Value::Object(stores) = refused else {
            return Err(shape());
        };
        let mut map = BTreeMap::new();
        for (store, entry) in stores {
            let store: i16 = store.parse().map_err(|_| shape())?;
            let reason = entry
                .get("reason")
                .and_then(Value::as_str)
                .ok_or_else(shape)?;
            let since = entry
                .get("since")
                .and_then(Value::as_i64)
                .ok_or_else(shape)?;
            map.insert(store, (reason.to_owned(), since));
        }
        Ok(Self(map))
    }

    fn to_json(&self) -> erp_sdk::serde_json::Value {
        let stores: erp_sdk::serde_json::Map<String, erp_sdk::serde_json::Value> = self
            .0
            .iter()
            .map(|(store, (reason, since))| {
                (
                    store.to_string(),
                    erp_sdk::serde_json::json!({"reason": reason, "since": since}),
                )
            })
            .collect();
        erp_sdk::serde_json::Value::Object(stores)
    }

    /// The stores refused for longer than [`super::ownership::OBSERVATION_SECS`] at `now`.
    fn overdue(&self, now: i64) -> Vec<String> {
        self.0
            .iter()
            .filter(|(_, (_, since))| now.saturating_sub(*since) > OBSERVATION_LIMIT_SECS)
            .map(|(store, (reason, _))| format!("store {store} ({reason})"))
            .collect()
    }
}

/// The one helper behind every progress the GC and staging-cleanup jobs write: `fields` (an
/// object) gets the `refused` map, and the map the job carries on is returned with it.
///
/// `checked` is `Some((now, refused))` for a chunk that checked its stores, `now` the database
/// clock read in that chunk: a store refused again keeps its prior `since`, a newly refused one
/// gets `now`, and every other store leaves the map. That includes a store the chunk did not
/// check because it had no due rows: a store is only listed while a check refuses it. `None` (a
/// chunk that checks no store: Phase A, the strict removal, or a chunk an active hold stopped)
/// carries the prior map unchanged, so no chunk drops it and a hold does not restart `since`.
fn merge_refused(
    mut fields: erp_sdk::serde_json::Value,
    prior: &RefusedSince,
    checked: Option<(i64, &[RefusedStore])>,
) -> (erp_sdk::serde_json::Value, RefusedSince) {
    let next = match checked {
        None => prior.clone(),
        Some((now, refused)) => RefusedSince(
            refused
                .iter()
                .map(|r| {
                    let since = prior.0.get(&r.store_id).map_or(now, |(_, since)| *since);
                    (r.store_id, (r.reason.to_owned(), since))
                })
                .collect(),
        ),
    };
    if let erp_sdk::serde_json::Value::Object(map) = &mut fields {
        map.insert("refused".to_owned(), next.to_json());
    }
    (fields, next)
}

/// The strict removal's error as a job error. A store whose admission was lost after the marking
/// (a heartbeat preparation committed in between, a contest appeared) refuses with
/// `BlobError::CleanupRefused`: that is a retry, not a broken job, so the run fails transient
/// with `cleanup_refused`; the marked rows stay marked and the next run sweeps them again. Every
/// other error keeps its usual classification.
fn admission_lost(error: Error) -> JobError {
    let refused = std::error::Error::source(&error)
        .and_then(|s| s.downcast_ref::<BlobError>())
        .is_some_and(|e| matches!(e, BlobError::CleanupRefused { .. }));
    if !refused {
        return JobError::from(error);
    }
    JobError::Transient {
        code: "cleanup_refused",
        message: error.message().to_owned().into_boxed_str(),
        source: Some(Box::new(error)),
    }
}

/// The database clock in whole epoch seconds, read in the chunk's transaction.
async fn db_epoch(tx: &mut Transaction) -> Result<i64> {
    let rows = tx
        .query(
            "SELECT floor(extract(epoch FROM clock_timestamp()))::int8",
            &[],
        )
        .await?;
    rows.first()
        .ok_or_else(|| Error::db("the database clock returned no row"))?
        .try_get(0)
        .map_err(|e| col_error("gc", e))
}

/// After a run's admitted work has committed: fail it for retry when a store has been refused
/// for longer than the observation period, naming each such store and its reason. Chunks have
/// committed, so the runner counts this as the first attempt since progress and retries it with
/// backoff; the job is not dead-lettered by a long refusal.
fn refused_too_long(refused: &RefusedSince, now: Option<i64>) -> std::result::Result<(), JobError> {
    let Some(now) = now else {
        return Ok(());
    };
    let overdue = refused.overdue(now);
    if overdue.is_empty() {
        return Ok(());
    }
    Err(JobError::transient(
        "cleanup_refused",
        format!(
            "Attachment cleanup has refused these stores for longer than {} seconds; their rows and reservations are kept: {}",
            super::ownership::OBSERVATION_SECS,
            overdue.join(", ")
        ),
    ))
}

/// The refused map and the database clock of the last chunk that checked stores, handed from a
/// chunk to the run once it committed.
type Carried = Arc<Mutex<Option<(RefusedSince, Option<i64>)>>>;

fn take_carried(slot: &Carried) -> std::result::Result<(RefusedSince, Option<i64>), JobError> {
    slot.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
        .ok_or_else(|| {
            JobError::from(Error::internal(
                "an attachment cleanup chunk produced no progress",
            ))
        })
}

/// The policy a chunk of the running handler loaded last, for the run between its chunks.
type PolicySlot = Arc<Mutex<Option<super::settings::Policy>>>;

/// Load the validated policy inside a working chunk and keep it for the run. A chunk of its own
/// would commit before any work and reset the attempt count, so a handler that fails every run
/// would never reach its `max_attempts` nor back off (review S2; `CompactUsageJob` does the same).
async fn load_policy(tx: &mut Transaction, slot: &PolicySlot) -> Result<super::settings::Policy> {
    let policy = super::settings::Policy::load(tx).await?;
    *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(policy);
    Ok(policy)
}

/// The policy the last committed chunk loaded.
fn loaded_policy(slot: &PolicySlot) -> std::result::Result<super::settings::Policy, JobError> {
    let policy = *slot.lock().unwrap_or_else(PoisonError::into_inner);
    policy.ok_or_else(|| {
        JobError::from(Error::programming(
            "Attachment maintenance produced no policy snapshot.",
        ))
    })
}

impl JobHandler for GcJob {
    fn run<'a>(
        &'a self,
        ctx: &'a mut JobContext<'_>,
    ) -> BoxFuture<'a, std::result::Result<RunOutcome, JobError>> {
        Box::pin(async move {
            let slot = PolicySlot::default();
            let mut refused = RefusedSince::from_progress(ctx.progress()?)?;
            let mut now: Option<i64>;
            // Phase A: until the queue is empty
            loop {
                let carried: Carried = Arc::new(Mutex::new(None));
                let sink = Arc::clone(&carried);
                let prior = refused.clone();
                let policy_slot = Arc::clone(&slot);
                let report = ctx
                    .chunk(move |tx| {
                        let sink = Arc::clone(&sink);
                        let prior = prior.clone();
                        let policy_slot = Arc::clone(&policy_slot);
                        Box::pin(async move {
                            let policy = load_policy(tx, &policy_slot).await?;
                            let report = mark_unreferenced(tx, policy.batch).await?;
                            let (progress, next) = merge_refused(
                                erp_sdk::serde_json::json!({"phase": "mark", "claimed": report.claimed, "marked": report.marked}),
                                &prior,
                                None,
                            );
                            *sink.lock().unwrap_or_else(PoisonError::into_inner) = Some((next, None));
                            Ok(ChunkResult::more(progress, report.claimed, None))
                        })
                    })
                    .await?;
                refused = take_carried(&carried)?.0;
                if report.processed < loaded_policy(&slot)?.batch as u64 {
                    break;
                }
                if ctx.budget_exhausted() {
                    return Ok(RunOutcome::Continue);
                }
            }
            // Phase B: mark in one transaction, remove in the next
            loop {
                let pending: Arc<Mutex<Vec<i64>>> = Arc::new(Mutex::new(Vec::new()));
                let sink = Arc::clone(&pending);
                let carried: Carried = Arc::new(Mutex::new(None));
                let carry = Arc::clone(&carried);
                let prior = refused.clone();
                let policy_slot = Arc::clone(&slot);
                ctx.chunk(move |tx| {
                    let sink = Arc::clone(&sink);
                    let carry = Arc::clone(&carry);
                    let prior = prior.clone();
                    let policy_slot = Arc::clone(&policy_slot);
                    Box::pin(async move {
                        let env = tx.env();
                        let grace = runtime(&env)?.limits().gc_grace_secs;
                        let policy = load_policy(tx, &policy_slot).await?;
                        let mark = sweep_mark(tx, grace, policy.batch).await?;
                        let at = db_epoch(tx).await?;
                        let count = mark.ids.len() as u64;
                        let checked = (!mark.held).then_some((at, mark.refused.as_slice()));
                        let (progress, next) = merge_refused(
                            erp_sdk::serde_json::json!({"phase": "sweep_mark", "ids": count}),
                            &prior,
                            checked,
                        );
                        *sink.lock().unwrap_or_else(PoisonError::into_inner) = mark.ids;
                        *carry.lock().unwrap_or_else(PoisonError::into_inner) =
                            Some((next, Some(at)));
                        Ok(ChunkResult::more(progress, count, None))
                    })
                })
                .await?;
                let (next, at) = take_carried(&carried)?;
                refused = next;
                now = at;
                let ids =
                    std::mem::take(&mut *pending.lock().unwrap_or_else(PoisonError::into_inner));
                if ids.is_empty() {
                    break;
                }
                let prior = refused.clone();
                ctx.chunk(move |tx| {
                    let ids = ids.clone();
                    let prior = prior.clone();
                    Box::pin(async move {
                        let env = tx.env();
                        let grace = runtime(&env)?.limits().gc_grace_secs;
                        let report = sweep_remove(tx, &env, &ids, grace)
                            .await
                            .map_err(admission_lost)?;
                        let (progress, _) = merge_refused(
                            erp_sdk::serde_json::json!({"phase": "sweep_remove", "rows": report.rows, "keys": report.keys, "already_absent": report.already_absent}),
                            &prior,
                            None,
                        );
                        Ok(ChunkResult::more(progress, report.rows, None))
                    })
                })
                .await?;
                if ctx.budget_exhausted() {
                    return Ok(RunOutcome::Continue);
                }
            }
            refused_too_long(&refused, now)?;
            Ok(RunOutcome::RescheduleAfter(Duration::from_secs(
                loaded_policy(&slot)?.gc_interval,
            )))
        })
    }
}

impl JobHandler for CleanJob {
    fn run<'a>(
        &'a self,
        ctx: &'a mut JobContext<'_>,
    ) -> BoxFuture<'a, std::result::Result<RunOutcome, JobError>> {
        Box::pin(async move {
            let slot = PolicySlot::default();
            let mut refused = RefusedSince::from_progress(ctx.progress()?)?;
            let mut now: Option<i64>;
            loop {
                let carried: Carried = Arc::new(Mutex::new(None));
                let carry = Arc::clone(&carried);
                let prior = refused.clone();
                let policy_slot = Arc::clone(&slot);
                let report = ctx
                    .chunk(move |tx| {
                        let carry = Arc::clone(&carry);
                        let prior = prior.clone();
                        let policy_slot = Arc::clone(&policy_slot);
                        Box::pin(async move {
                            let env = tx.env();
                            let policy = load_policy(tx, &policy_slot).await?;
                            let r = clean_staging(tx, &env, policy.batch).await?;
                            let at = db_epoch(tx).await?;
                            let n = r.consumed + r.expired + r.abandoned;
                            let checked = (!r.held).then_some((at, r.refused.as_slice()));
                            let (progress, next) = merge_refused(
                                erp_sdk::serde_json::json!({"consumed": r.consumed, "expired": r.expired, "adopted": r.adopted, "abandoned": r.abandoned, "skipped": r.skipped}),
                                &prior,
                                checked,
                            );
                            *carry.lock().unwrap_or_else(PoisonError::into_inner) = Some((next, Some(at)));
                            Ok(ChunkResult::more(progress, n, None))
                        })
                    })
                    .await?;
                let (next, at) = take_carried(&carried)?;
                refused = next;
                now = at;
                if report.processed == 0 {
                    break;
                }
                if ctx.budget_exhausted() {
                    return Ok(RunOutcome::Continue);
                }
            }
            refused_too_long(&refused, now)?;
            Ok(RunOutcome::RescheduleAfter(Duration::from_secs(
                loaded_policy(&slot)?.cleanup_interval,
            )))
        })
    }
}

impl JobHandler for ReconcileJob {
    fn run<'a>(
        &'a self,
        ctx: &'a mut JobContext<'_>,
    ) -> BoxFuture<'a, std::result::Result<RunOutcome, JobError>> {
        Box::pin(async move {
            let slot = PolicySlot::default();
            let after = Arc::new(Mutex::new(0i64));
            loop {
                let cursor = Arc::clone(&after);
                let policy_slot = Arc::clone(&slot);
                let report = ctx
                    .chunk(move |tx| {
                        let cursor = Arc::clone(&cursor);
                        let policy_slot = Arc::clone(&policy_slot);
                        Box::pin(async move {
                            let env = tx.env();
                            let from = *cursor.lock().unwrap_or_else(PoisonError::into_inner);
                            let policy = load_policy(tx, &policy_slot).await?;
                            let r = reconcile_chunk(tx, &env, from, policy.batch).await?;
                            *cursor.lock().unwrap_or_else(PoisonError::into_inner) = r.last_id;
                            Ok(ChunkResult::more(
                                erp_sdk::serde_json::json!({"after": r.last_id, "queued": r.queued, "revived": r.revived}),
                                r.looked_at,
                                None,
                            ))
                        })
                    })
                    .await?;
                if report.processed < loaded_policy(&slot)?.batch as u64 {
                    break;
                }
                if ctx.budget_exhausted() {
                    return Ok(RunOutcome::Continue);
                }
            }
            Ok(RunOutcome::RescheduleAfter(Duration::from_secs(
                loaded_policy(&slot)?.reconcile_interval,
            )))
        })
    }
}

impl JobHandler for VerifyJob {
    fn run<'a>(
        &'a self,
        ctx: &'a mut JobContext<'_>,
    ) -> BoxFuture<'a, std::result::Result<RunOutcome, JobError>> {
        Box::pin(async move {
            let deep = ctx
                .payload::<erp_sdk::serde_json::Value>()?
                .get("deep")
                .and_then(erp_sdk::serde_json::Value::as_bool)
                .unwrap_or(false);
            let after = Arc::new(Mutex::new(0i64));
            let problems: Arc<Mutex<Vec<VerifyProblem>>> = Arc::new(Mutex::new(Vec::new()));
            loop {
                let cursor = Arc::clone(&after);
                let sink = Arc::clone(&problems);
                let report = ctx
                    .chunk(move |tx| {
                        let cursor = Arc::clone(&cursor);
                        let sink = Arc::clone(&sink);
                        Box::pin(async move {
                            let env = tx.env();
                            let from = *cursor.lock().unwrap_or_else(PoisonError::into_inner);
                            let r = verify_chunk(tx, &env, from, 200, deep).await?;
                            *cursor.lock().unwrap_or_else(PoisonError::into_inner) = r.last_id;
                            let mut all = sink.lock().unwrap_or_else(PoisonError::into_inner);
                            if all.len() < 20 {
                                all.extend(r.problems.iter().cloned());
                            }
                            Ok(ChunkResult::more(
                                erp_sdk::serde_json::json!({"after": r.last_id, "problems": r.problems.len()}),
                                r.checked,
                                None,
                            ))
                        })
                    })
                    .await?;
                if report.processed < 200 {
                    break;
                }
                if ctx.budget_exhausted() {
                    return Ok(RunOutcome::Continue);
                }
            }
            let found = problems
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            if found.is_empty() {
                return Ok(RunOutcome::Done);
            }
            let listed: Vec<String> = found
                .iter()
                .take(20)
                .map(|p| format!("blob {} {}", p.blob_id, p.kind))
                .collect();
            Err(JobError::from(Error::validation(format!(
                "Attachment verification found problems (nothing was repaired): {}",
                listed.join(", ")
            ))))
        })
    }
}

static GC_JOB: GcJob = GcJob;
static CLEAN_JOB: CleanJob = CleanJob;
static RECONCILE_JOB: ReconcileJob = ReconcileJob;
static VERIFY_JOB: VerifyJob = VerifyJob;
struct CompactUsageJob;
static COMPACT_USAGE_JOB: CompactUsageJob = CompactUsageJob;
impl JobHandler for CompactUsageJob {
    fn run<'a>(
        &'a self,
        ctx: &'a mut JobContext<'_>,
    ) -> BoxFuture<'a, std::result::Result<RunOutcome, JobError>> {
        Box::pin(async move {
            let interval = Arc::new(std::sync::atomic::AtomicU64::new(0));
            loop {
                let interval = Arc::clone(&interval);
                let report = ctx
                    .chunk(move |tx| {
                        let interval = Arc::clone(&interval);
                        Box::pin(async move {
                            let policy = super::settings::Policy::load(tx).await?;
                            let folded = compact_usage(tx, policy.compact_batch).await?;
                            interval.store(
                                policy.compact_interval,
                                std::sync::atomic::Ordering::Release,
                            );
                            Ok(ChunkResult::more(
                                erp_sdk::serde_json::json!({"folded":folded}),
                                folded,
                                None,
                            ))
                        })
                    })
                    .await?;
                if report.processed == 0 || ctx.budget_exhausted() {
                    break;
                }
            }
            let seconds = interval.load(std::sync::atomic::Ordering::Acquire);
            if seconds == 0 {
                return Err(JobError::from(Error::programming(
                    "Usage compaction produced no interval.",
                )));
            }
            Ok(RunOutcome::RescheduleAfter(Duration::from_secs(seconds)))
        })
    }
}
erp_sdk::jobs::register_job_kind!(
    COMPACT_KIND,
    JobKindDecl::new(
        "base",
        "base.attachment_usage_compact",
        HandlerRef::Native(&COMPACT_USAGE_JOB)
    )
    .principal(PrincipalPolicy::System)
    .elevated()
    .timeouts(TimeoutClass::Maintenance)
    .isolation(KindIsolation::ReadCommitted)
    .max_attempts(5)
);

/// Fixed compiled owner allowlist. An addon's elevated flag never joins this set.
/// Every declaration is validated again by the job runner before it can claim or execute.
pub fn maintenance_kinds() -> [&'static JobKindDecl; 6] {
    [
        &GC_KIND,
        &CLEAN_KIND,
        &RECONCILE_KIND,
        &VERIFY_KIND,
        &HEARTBEAT_KIND,
        &COMPACT_KIND,
    ]
}

static HEARTBEAT_JOB: super::ownership::HeartbeatJob = super::ownership::HeartbeatJob::new();

// The owner heartbeat: without it no store-backed cleanup is ever admitted.
erp_sdk::jobs::register_job_kind!(
    HEARTBEAT_KIND,
    JobKindDecl::new(
        "base",
        "base.attachment_owner_heartbeat",
        HandlerRef::Native(&HEARTBEAT_JOB)
    )
    .principal(PrincipalPolicy::System)
    .elevated()
    .timeouts(TimeoutClass::Maintenance)
    .isolation(KindIsolation::ReadCommitted)
    .max_attempts(5)
);

erp_sdk::jobs::register_job_kind!(
    GC_KIND,
    JobKindDecl::new("base", "base.attachment_gc", HandlerRef::Native(&GC_JOB))
        .principal(PrincipalPolicy::System)
        .elevated()
        .timeouts(TimeoutClass::Maintenance)
        .isolation(KindIsolation::ReadCommitted)
        .max_attempts(5)
);
erp_sdk::jobs::register_job_kind!(
    CLEAN_KIND,
    JobKindDecl::new(
        "base",
        "base.attachment_clean_staging",
        HandlerRef::Native(&CLEAN_JOB)
    )
    .principal(PrincipalPolicy::System)
    .elevated()
    .timeouts(TimeoutClass::Maintenance)
    .isolation(KindIsolation::ReadCommitted)
    .max_attempts(5)
);
erp_sdk::jobs::register_job_kind!(
    RECONCILE_KIND,
    JobKindDecl::new(
        "base",
        "base.attachment_reconcile",
        HandlerRef::Native(&RECONCILE_JOB)
    )
    .principal(PrincipalPolicy::System)
    .elevated()
    .timeouts(TimeoutClass::Maintenance)
    .isolation(KindIsolation::ReadCommitted)
    .max_attempts(5)
);
erp_sdk::jobs::register_job_kind!(
    VERIFY_KIND,
    JobKindDecl::new(
        "base",
        "base.attachment_verify",
        HandlerRef::Native(&VERIFY_JOB)
    )
    .principal(PrincipalPolicy::System)
    .elevated()
    .timeouts(TimeoutClass::Maintenance)
    .isolation(KindIsolation::ReadCommitted)
    .max_attempts(1)
);

/// The recurring attachment schedules: `(kind, delay before the first run in seconds)`. Each
/// is keyed by its kind, so there is at most one live row and at most one run at a time on any
/// node. [`ensure_schedules`] and [`ensure_missing_schedules`] keep exactly this list.
const SCHEDULES: [(&str, u64); 3] = [
    ("base.attachment_gc", 60),
    ("base.attachment_clean_staging", 30),
    ("base.attachment_reconcile", 600),
];

/// The elevated attachment maintenance kinds: native handlers that run as the
/// System principal with maintenance isolation and touch the attachment store. Only an isolated
/// maintenance runner may run them; the ordinary runner of a served node excludes them
/// (`JobRunnerBuilder::exclude_elevated`, by the declarations' flag; this list is the pinned record
/// a test compares with the flags). `base.attachment_verify` belongs here too: nothing in this
/// code base enqueues it (an operator does), but its handler reads every blob's bytes from the
/// store as the System principal, which is the same elevated store access D5 isolates.
/// The owner heartbeat is elevated as well: on a served node its ordinary runner would claim it and fail
/// on the runtime role's read-only grant until the row is a dead letter.
pub const ELEVATED_KINDS: [&str; 6] = [
    "base.attachment_gc",
    "base.attachment_clean_staging",
    "base.attachment_reconcile",
    "base.attachment_verify",
    // writes the store's owner marker and the owner state as the System principal
    "base.attachment_owner_heartbeat",
    "base.attachment_usage_compact",
];

/// The transaction advisory lock [`ensure_missing_schedules`] takes before it looks for the
/// rows: `(class, key)` = (`"ERPF"`, 8), the class of `erp_modules::system::LOCK_CLASS` (keys 1
/// to 7 are the module operation lock, the module post-commit lock, node readiness, the
/// authentication collector, the UI asset store, the node feature registry and [`GC_HOLD_LOCK`]).
/// Two nodes starting at once check and create the rows one after the other. A base upgrade is
/// serialized with a start too, but its REPEATABLE READ snapshot predates the lock, so if a start
/// commits the rows while the upgrade waits, the upgrade aborts with a serialization error and
/// must be run again. The module system reports that abort as
/// `erp_modules::Error::ConcurrentChange`, whose message says to retry the module operation.
pub const SCHEDULES_LOCK: (i32, i32) = (0x4552_5046, 8);

/// Keep the recurring schedules of the attachment jobs (garbage collection, staging cleanup and
/// the reconcile walk) for a server start: check the attachment store topology of this node,
/// then create every schedule that has no live row ([`ensure_missing_schedules`]) in one
/// committed transaction. Idempotent; an existing row keeps its due time.
///
/// For a caller that wants the topology refusal with the schedules (tests, tools). The served
/// binary does not use it: its start calls [`ensure_missing_schedules`] without the topology
/// check, which stays at upload and before every destructive store call. The rows only run where
/// a job runner runs.
///
/// # Errors
///
/// `UserError` when the ORM carries no attachment runtime; `Programming` when a node-local store
/// is shared by more than one live node; as [`ensure_missing_schedules`]; the database's.
pub async fn ensure_schedules(orm: &Orm) -> Result<()> {
    let mut tx = orm.begin(SUPERUSER_ID).await?;
    let env = tx.env();
    super::topology::verify(&mut tx, &*runtime(&env)?).await?;
    ensure_missing_schedules(&mut tx, &env).await?;
    tx.commit().await?;
    Ok(())
}

/// Create, inside `tx`, the recurring row of every attachment schedule that has no live row
/// (a row whose state is not `dead`, keyed by kind and dedup key). An existing row, running or
/// not, is left exactly as it is: its due time does not move and a running row is not an error.
/// A dead letter does not count as live, so it gets a fresh row next to it. The check and the
/// inserts run under [`SCHEDULES_LOCK`], held until `tx` ends.
///
/// This is the transaction-level part of [`ensure_schedules`], without the topology check, for
/// callers that already hold a transaction: the base install/upgrade lifecycle hook of the
/// native compositions calls it in the module operation's transaction, and `erp server` calls it
/// at every start on the owner connection, in `READ COMMITTED` so a concurrent starter coalesces
/// into the first one's row. Nothing is committed here.
///
/// # Errors
///
/// `Access` unless `env` is the system principal; `Programming` when an attachment kind is not
/// registered in this binary; the database's.
pub async fn ensure_missing_schedules(tx: &mut Transaction, env: &Env) -> Result<()> {
    if env.principal().kind() != PrincipalKind::System {
        return Err(Error::access(
            "The attachment schedules are kept by the system principal only.",
        ));
    }
    tx.query(
        "SELECT pg_advisory_xact_lock($1::int4, $2::int4)",
        &[&SCHEDULES_LOCK.0, &SCHEDULES_LOCK.1],
    )
    .await?;
    for (kind, delay) in SCHEDULES {
        let live = tx
            .query(
                "SELECT 1 FROM erp_job WHERE kind = $1 AND dedup_key = $1 AND state <> 'dead'",
                &[&kind],
            )
            .await?;
        if !live.is_empty() {
            continue;
        }
        let spec = RecurringSpec::new(kind, JobKey::new(kind)?)
            .first_run(When::After(Duration::from_secs(delay)));
        tx.job_upsert_recurring(env, spec).await?;
    }
    let policy = super::settings::Policy::load(tx).await?;
    let compact_kind = "base.attachment_usage_compact";
    if tx
        .query(
            "SELECT 1 FROM erp_job WHERE kind=$1 AND state<>'dead'",
            &[&compact_kind],
        )
        .await?
        .is_empty()
    {
        tx.job_upsert_recurring(
            env,
            RecurringSpec::new(compact_kind, JobKey::new(compact_kind)?)
                .first_run(When::After(Duration::from_secs(policy.compact_interval))),
        )
        .await?;
    }
    // The owner heartbeat (it opens the observation window that admits store-backed cleanup) runs under the
    // ownership singleton its handler requires, not under its kind, so it is kept apart from SCHEDULES.
    let live = tx
        .query(
            "SELECT 1 FROM erp_job WHERE kind = $1 AND state <> 'dead'",
            &[&"base.attachment_owner_heartbeat"],
        )
        .await?;
    if live.is_empty() {
        let spec = RecurringSpec::new(
            "base.attachment_owner_heartbeat",
            JobKey::new(super::ownership::SINGLETON)?,
        )
        .first_run(When::After(Duration::from_secs(5)));
        tx.job_upsert_recurring(env, spec).await?;
    }
    Ok(())
}

/// Fold a bounded batch of usage deltas into the base without changing visible usage.
/// The isolated native usage-compaction job schedules this primitive.
///
/// # Errors
/// Refuses non-System/non-READ-COMMITTED maintenance and an invalid batch size; propagates SQL errors.
pub async fn compact_usage(tx: &mut Transaction, limit: i64) -> Result<u64> {
    let env = tx.env();
    if !is_system(&env) {
        return Err(Error::access(
            "Usage compaction requires the System maintenance principal.",
        ));
    }
    super::quota::compact(tx, limit).await
}

/// Validate destructive admission for a configured store in this maintenance transaction.
/// This acquires coordinator/metadata locks until transaction end, never waits for observation.
/// A successful call is not a reusable permit: sweep and cleaner entry points check again.
/// The scheduled heartbeat (`base.attachment_owner_heartbeat`, [`ensure_schedules`]) opens the
/// observation window; `ownership::observation` reports its progress without taking a lock.
///
/// # Errors
/// Access, isolation, configuration, owner/observation/capability refusal, and store/SQL failures.
pub async fn check_cleanup(tx: &mut Transaction, key: &str) -> Result<()> {
    require_maintenance(tx, &tx.env()).await?;
    super::cleanup::start(tx).await?;
    let rows = tx
        .query(
            "SELECT id FROM ir_blob_store WHERE store_key=$1 AND role='own'",
            &[&key],
        )
        .await?;
    let row = rows
        .first()
        .ok_or_else(|| Error::internal("Attachment cleanup store is not registered."))?;
    super::cleanup::check(tx, row.try_get(0).map_err(|e| col_error("cleanup", e))?).await
}
