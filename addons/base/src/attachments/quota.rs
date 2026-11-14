// SPDX-License-Identifier: MIT OR Apache-2.0
//! Database-authoritative byte reservations and soft storage quota.
use super::runtime::{blob_error, col_error};
use erp_sdk::blob::{AttachmentLimits, BlobError};
use erp_sdk::{Error, Result, Transaction};

/// Advisory lock classes are disjoint from GC holds. Database first, subject second; both
/// transaction scoped on the primary. No lock survives intent commit or spans body transfer.
const DATABASE_BUDGET_LOCK: i64 = 0x4552_5051_554f_5441;
pub const SUBJECT_BUDGET_CLASS: i32 = 0x4552_5153;

pub(crate) fn sql_bytes(bytes: u64) -> Result<i64> {
    i64::try_from(bytes)
        .map_err(|_| Error::validation("Attachment byte count exceeds the database range."))
}

/// The soft storage quota of an inline creation. Unknown legacy occupancy refuses it only when a
/// quota is configured: without one there is no budget those unknown bytes could exceed
/// (review S3). Staging admission refuses on unknown occupancy always ([`admit`]).
pub(crate) async fn check_storage(
    tx: &mut Transaction,
    limits: &AttachmentLimits,
    requested: u64,
) -> Result<()> {
    if limits.quota_bytes.is_some() {
        refuse_unknown_occupancy(tx).await?;
    }
    check_quota(tx, limits, requested).await
}

/// Unknown legacy occupancy must be resolved by maintenance first: an admission cannot assume
/// those bytes are zero.
async fn refuse_unknown_occupancy(tx: &mut Transaction) -> Result<()> {
    let unknown = tx
        .query(
            "SELECT EXISTS (SELECT 1 FROM ir_blob_upload WHERE reservation_unknown)",
            &[],
        )
        .await?;
    let unknown: bool = unknown
        .first()
        .ok_or_else(|| Error::db("Attachment recovery query returned no row."))?
        .try_get(0)
        .map_err(|e| col_error("attachment recovery", e))?;
    if unknown {
        return Err(blob_error(BlobError::AccountingUnavailable));
    }
    Ok(())
}

async fn check_quota(
    tx: &mut Transaction,
    limits: &AttachmentLimits,
    requested: u64,
) -> Result<()> {
    let policy = super::settings::Policy::load(tx).await?;
    let Some(limit) = limits.quota_bytes else {
        return Ok(());
    };
    // One snapshot for base and deltas. The extra row is an overflow sentinel, never
    // silently excluded from the sum. A stalled compactor cannot make admission scan history.
    let rows = tx.query("WITH pending AS MATERIALIZED (SELECT bytes FROM ir_blob_usage_delta ORDER BY id LIMIT $1) SELECT (b.bytes + COALESCE((SELECT sum(bytes) FROM pending),0))::text, (SELECT count(*) FROM pending)::int8 FROM ir_blob_usage_base b WHERE id=1", &[&(policy.delta_read + 1)]).await?;
    let count: i64 = rows
        .first()
        .ok_or_else(|| Error::db("Attachment usage base is missing."))?
        .try_get(1)
        .map_err(|e| col_error("attachment usage backlog", e))?;
    if count > policy.delta_read {
        return Err(blob_error(BlobError::AccountingUnavailable));
    }
    let text: String = rows
        .first()
        .ok_or_else(|| Error::db("Attachment usage base is missing."))?
        .try_get(0)
        .map_err(|e| col_error("attachment usage", e))?;
    let used = text
        .parse::<u64>()
        .map_err(|_| Error::db("Attachment usage is negative or outside the byte range."))?;
    check_budget("database quota", used, requested, limit)
}
fn check_budget(budget: &'static str, used: u64, requested: u64, limit: u64) -> Result<()> {
    if used
        .checked_add(requested)
        .is_none_or(|total| total > limit)
    {
        return Err(blob_error(BlobError::QuotaExceeded {
            budget,
            used,
            requested,
            limit,
        }));
    }
    Ok(())
}

pub(crate) async fn admit(
    tx: &mut Transaction,
    limits: &AttachmentLimits,
    subject: i64,
    reserved: u64,
    file_limit: u64,
) -> Result<()> {
    // The full i64 subject is an SQL parameter. Advisory hash collisions only serialize extra
    // subjects; identity and budget evaluation always use the original subject column.
    tx.query("SELECT pg_advisory_xact_lock($1)", &[&DATABASE_BUDGET_LOCK])
        .await?;
    tx.query(
        "SELECT pg_advisory_xact_lock($1, hashint8($2))",
        &[&SUBJECT_BUDGET_CLASS, &subject],
    )
    .await?;
    if !(1..=100).contains(&limits.pending_uploads_per_subject) {
        return Err(Error::programming(
            "attachments.pending_uploads_per_subject is outside 1 to 100.",
        ));
    }
    // A failed zero-byte intent still occupies a pending slot. Consumed rows retain external
    // staging bytes until cleanup but no longer count as pending uploads.
    // Only cleanup removes failed rows, so zero-length declarations cannot evade the row cap.
    let rows = tx.query("SELECT count(*) FILTER (WHERE subject_user=$1 AND state IN (0,1,3))::int8, COALESCE(sum(reserved_bytes) FILTER (WHERE subject_user=$1),0)::text, COALESCE(sum(reserved_bytes),0)::text FROM ir_blob_upload", &[&subject]).await?;
    let row = rows
        .first()
        .ok_or_else(|| Error::db("The staging budget query returned no row."))?;
    let pending: i64 = row
        .try_get(0)
        .map_err(|e| col_error("pending uploads", e))?;
    if pending >= i64::from(limits.pending_uploads_per_subject) {
        return Err(blob_error(BlobError::PendingUploads {
            limit: limits.pending_uploads_per_subject,
        }));
    }
    let bytes = |index| -> Result<u64> {
        let text: String = row
            .try_get(index)
            .map_err(|e| col_error("staging bytes", e))?;
        text.parse()
            .map_err(|_| Error::db("Staging bytes are outside the byte range."))
    };
    let subject_limit = file_limit
        .checked_mul(4)
        .ok_or_else(|| Error::validation("Subject staging limit exceeds the byte range."))?;
    check_budget("subject staging", bytes(1)?, reserved, subject_limit)?;
    check_budget(
        "database staging",
        bytes(2)?,
        reserved,
        limits.staging_max_bytes,
    )?;
    // Hard staging admission cannot assume unknown bytes are zero, quota or not.
    refuse_unknown_occupancy(tx).await?;
    check_quota(tx, limits, reserved).await
}

/// A bounded maintenance-only compaction. DELETE RETURNING chooses actual visible rows;
/// concurrent insertions and late commits keep their deltas, irrespective of allocated ids.
pub(crate) async fn compact(tx: &mut Transaction, limit: i64) -> Result<u64> {
    if !(1..=10_000).contains(&limit) {
        return Err(Error::validation(
            "Usage compaction batch is 1 to 10000 rows.",
        ));
    }
    let base = tx.query("SELECT current_setting('transaction_isolation') FROM ir_blob_usage_base WHERE id=1 FOR UPDATE", &[]).await?;
    let isolation: String = base
        .first()
        .ok_or_else(|| Error::db("Attachment usage base is missing."))?
        .try_get(0)
        .map_err(|e| col_error("usage compaction isolation", e))?;
    if isolation != "read committed" {
        return Err(Error::programming(
            "Usage compaction requires READ COMMITTED isolation.",
        ));
    }
    let rows = tx.query("WITH claimed AS (SELECT id FROM ir_blob_usage_delta ORDER BY id LIMIT $1 FOR UPDATE SKIP LOCKED), removed AS (DELETE FROM ir_blob_usage_delta d USING claimed c WHERE d.id=c.id RETURNING d.bytes), folded AS (UPDATE ir_blob_usage_base SET bytes=bytes+COALESCE((SELECT sum(bytes) FROM removed),0) WHERE id=1 RETURNING id) SELECT count(*)::int8 FROM removed WHERE EXISTS (SELECT 1 FROM folded)", &[&limit]).await?;
    let n: i64 = rows
        .first()
        .ok_or_else(|| Error::db("Usage compaction returned no row."))?
        .try_get(0)
        .map_err(|e| col_error("usage compaction", e))?;
    u64::try_from(n).map_err(|_| Error::db("Usage compaction count is negative."))
}
