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
//! **Lock order.** A sweep transaction takes [`GC_HOLD_LOCK`] (shared) first, then the `ir_blob`
//! row locks (`FOR UPDATE SKIP LOCKED`); [`hold`] takes only the advisory lock (exclusive) and no
//! row lock, and a writer takes only row locks (`FOR KEY SHARE`), so no cycle exists. The shared
//! lock is held across Phase B's one `delete_many` call (at most [`CHUNK`] keys, no byte
//! streamed), because the hold must wait until those keys are gone. Deep verification streams
//! with no lock of this module, but inside its job chunk's transaction, which holds the queue's
//! own fence and singleton lock for the chunk; those exclude only other runs of the
//! same kind, and the stream admission bounds the reads.

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
/// The database's. Meant to run in `READ COMMITTED` (the job kind's isolation) so the
/// `NOT EXISTS` sees committed references; the foreign key protects under any isolation.
pub async fn mark_unreferenced(tx: &mut Transaction, limit: i64) -> Result<MarkReport> {
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

/// Phase B, first transaction: mark the `deleting` rows older than the grace as swept
/// (`sweep_attempted_at`), so a later revive must prove their bytes, and return their ids. No
/// rows while any hold is active. Takes [`GC_HOLD_LOCK`] shared until the transaction ends.
///
/// # Errors
///
/// The database's.
pub async fn sweep_mark(tx: &mut Transaction, grace_secs: u64, limit: i64) -> Result<Vec<i64>> {
    if held(tx).await? {
        return Ok(Vec::new());
    }
    let grace = grace_secs as f64;
    let rows = tx
        .query(
            "UPDATE ir_blob SET sweep_attempted_at = now() WHERE id IN ( \
                SELECT id FROM ir_blob WHERE state = 1 \
                  AND deleting_since < now() - make_interval(secs => $1::float8) \
                  AND NOT EXISTS (SELECT 1 FROM ir_attachment a WHERE a.blob_id = ir_blob.id) \
                ORDER BY id LIMIT $2 FOR UPDATE SKIP LOCKED) RETURNING id",
            &[&grace, &limit],
        )
        .await?;
    let mut ids = Vec::with_capacity(rows.len());
    for row in &rows {
        ids.push(row.try_get(0).map_err(|e| col_error("gc", e))?);
    }
    ids.sort_unstable();
    Ok(ids)
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
    if ids.is_empty() || held(tx).await? {
        return Ok(SweepReport::default());
    }
    let rt = runtime(env)?;
    let grace = grace_secs as f64;
    let locked = tx
        .query(
            "SELECT b.id, b.sha256, b.store_id FROM ir_blob b \
             WHERE b.id = ANY($1::int8[]) AND b.state = 1 \
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
        let done = store.store.delete_many(&keys).await.map_err(blob_error)?;
        report.keys += done.deleted as u64;
        report.already_absent += done.already_absent as u64;
    }
    Ok(report)
}

/// What one staging cleanup chunk did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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
}

/// A file of any other model with `res_id` 0 is never removed here.
const PENDING_PLACEHOLDERS: [&str; 1] = ["mail.compose.message"];

/// One chunk of the staging cleaner: consumed uploads (their object, if still staged, and row),
/// then expired ones. An expired upload whose digest has no blob row may have had its bytes
/// promoted by a transaction that rolled back: a `deleting` row is created for it so the sweep
/// removes the key.
///
/// # Errors
///
/// Store errors as `Internal`; the database's.
pub async fn clean_staging(tx: &mut Transaction, env: &Env, limit: i64) -> Result<CleanReport> {
    let rt = runtime(env)?;
    let mut report = CleanReport::default();
    for (state_sql, consumed) in [
        ("state = 2", true),
        ("state IN (0, 1) AND expires_at < now()", false),
    ] {
        let rows = tx
            .query(
                &format!(
                    "SELECT id::text, store_id, sha256, size, (inline_data IS NOT NULL) \
                     FROM ir_blob_upload WHERE {state_sql} ORDER BY id LIMIT $1 \
                     FOR UPDATE SKIP LOCKED"
                ),
                &[&limit],
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
                   AND create_date < now() - interval '24 hours' \
                 ORDER BY id LIMIT $2 FOR UPDATE SKIP LOCKED)",
            &[&placeholders, &limit],
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

/// How often the recurring kinds run again.
const GC_EVERY: Duration = Duration::from_secs(900);
const CLEAN_EVERY: Duration = Duration::from_secs(900);
const RECONCILE_EVERY: Duration = Duration::from_secs(6 * 3600);

struct GcJob;
struct CleanJob;
struct ReconcileJob;
struct VerifyJob;

impl JobHandler for GcJob {
    fn run<'a>(
        &'a self,
        ctx: &'a mut JobContext<'_>,
    ) -> BoxFuture<'a, std::result::Result<RunOutcome, JobError>> {
        Box::pin(async move {
            // Phase A: until the queue is empty
            loop {
                let report = ctx
                    .chunk(|tx| {
                        Box::pin(async move {
                            let report = mark_unreferenced(tx, CHUNK).await?;
                            Ok(ChunkResult::more(
                                erp_sdk::serde_json::json!({"phase": "mark", "claimed": report.claimed, "marked": report.marked}),
                                report.claimed,
                                None,
                            ))
                        })
                    })
                    .await?;
                if report.processed < CHUNK as u64 {
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
                ctx.chunk(move |tx| {
                    let sink = Arc::clone(&sink);
                    Box::pin(async move {
                        let env = tx.env();
                        let grace = runtime(&env)?.limits().gc_grace_secs;
                        let ids = sweep_mark(tx, grace, CHUNK).await?;
                        let count = ids.len() as u64;
                        *sink.lock().unwrap_or_else(PoisonError::into_inner) = ids;
                        Ok(ChunkResult::more(
                            erp_sdk::serde_json::json!({"phase": "sweep_mark", "ids": count}),
                            count,
                            None,
                        ))
                    })
                })
                .await?;
                let ids =
                    std::mem::take(&mut *pending.lock().unwrap_or_else(PoisonError::into_inner));
                if ids.is_empty() {
                    break;
                }
                ctx.chunk(move |tx| {
                    let ids = ids.clone();
                    Box::pin(async move {
                        let env = tx.env();
                        let grace = runtime(&env)?.limits().gc_grace_secs;
                        let report = sweep_remove(tx, &env, &ids, grace).await?;
                        Ok(ChunkResult::more(
                            erp_sdk::serde_json::json!({"phase": "sweep_remove", "rows": report.rows, "keys": report.keys, "already_absent": report.already_absent}),
                            report.rows,
                            None,
                        ))
                    })
                })
                .await?;
                if ctx.budget_exhausted() {
                    return Ok(RunOutcome::Continue);
                }
            }
            Ok(RunOutcome::RescheduleAfter(GC_EVERY))
        })
    }
}

impl JobHandler for CleanJob {
    fn run<'a>(
        &'a self,
        ctx: &'a mut JobContext<'_>,
    ) -> BoxFuture<'a, std::result::Result<RunOutcome, JobError>> {
        Box::pin(async move {
            loop {
                let report = ctx
                    .chunk(|tx| {
                        Box::pin(async move {
                            let env = tx.env();
                            let r = clean_staging(tx, &env, CHUNK).await?;
                            let n = r.consumed + r.expired + r.abandoned;
                            Ok(ChunkResult::more(
                                erp_sdk::serde_json::json!({"consumed": r.consumed, "expired": r.expired, "adopted": r.adopted, "abandoned": r.abandoned}),
                                n,
                                None,
                            ))
                        })
                    })
                    .await?;
                if report.processed == 0 {
                    break;
                }
                if ctx.budget_exhausted() {
                    return Ok(RunOutcome::Continue);
                }
            }
            Ok(RunOutcome::RescheduleAfter(CLEAN_EVERY))
        })
    }
}

impl JobHandler for ReconcileJob {
    fn run<'a>(
        &'a self,
        ctx: &'a mut JobContext<'_>,
    ) -> BoxFuture<'a, std::result::Result<RunOutcome, JobError>> {
        Box::pin(async move {
            let after = Arc::new(Mutex::new(0i64));
            loop {
                let cursor = Arc::clone(&after);
                let report = ctx
                    .chunk(move |tx| {
                        let cursor = Arc::clone(&cursor);
                        Box::pin(async move {
                            let env = tx.env();
                            let from = *cursor.lock().unwrap_or_else(PoisonError::into_inner);
                            let r = reconcile_chunk(tx, &env, from, CHUNK).await?;
                            *cursor.lock().unwrap_or_else(PoisonError::into_inner) = r.last_id;
                            Ok(ChunkResult::more(
                                erp_sdk::serde_json::json!({"after": r.last_id, "queued": r.queued, "revived": r.revived}),
                                r.looked_at,
                                None,
                            ))
                        })
                    })
                    .await?;
                if report.processed < CHUNK as u64 {
                    break;
                }
                if ctx.budget_exhausted() {
                    return Ok(RunOutcome::Continue);
                }
            }
            Ok(RunOutcome::RescheduleAfter(RECONCILE_EVERY))
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

erp_sdk::jobs::register_job_kind!(
    GC_KIND,
    JobKindDecl::new("base", "base.attachment_gc", HandlerRef::Native(&GC_JOB))
        .principal(PrincipalPolicy::System)
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
    .timeouts(TimeoutClass::Maintenance)
    .isolation(KindIsolation::ReadCommitted)
    .max_attempts(1)
);

/// Keep the recurring schedules of the attachment jobs: garbage collection, staging cleanup and
/// the reconcile walk, each keyed so there is at most one row and at most one run at a time on
/// any node. Idempotent; the composition root calls it at startup, as it does for the queue's own
/// schedules.
///
/// # Errors
///
/// The database's; `Access` for a principal other than the system.
pub async fn ensure_schedules(orm: &Orm) -> Result<()> {
    let mut tx = orm.begin(SUPERUSER_ID).await?;
    let env = tx.env();
    super::topology::verify(&mut tx, &*runtime(&env)?).await?;
    for (kind, delay) in [
        ("base.attachment_gc", 60),
        ("base.attachment_clean_staging", 30),
        ("base.attachment_reconcile", 600),
    ] {
        let spec = RecurringSpec::new(kind, JobKey::new(kind)?)
            .first_run(When::After(Duration::from_secs(delay)));
        tx.job_upsert_recurring(&env, spec).await?;
    }
    tx.commit().await?;
    Ok(())
}
