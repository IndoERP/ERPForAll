// SPDX-License-Identifier: MIT OR Apache-2.0
//! Transaction-owned sealing. A returned report is staged until the caller confirms COMMIT.
use super::{
    BatchDigest, ChainProblem, SealCursor, SealIdentity, SealInput, SealerProblem, problem::codec,
    row,
};
use crate::Transaction;
use erp_fields::{Error, Result};

pub(super) const MAX_ROWS: u32 = 65_536;
const MAX_BYTES: u64 = 8 * 1024 * 1024;
pub(super) const MAX_WIRE: i64 = 16 * 1024 * 1024;

/// Validated batch admission, separate from fixed canonical/transport safety limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct SealOptions {
    rows: u32,
    bytes: u64,
}
impl Default for SealOptions {
    fn default() -> Self {
        Self {
            rows: 10_000,
            bytes: MAX_BYTES,
        }
    }
}
impl SealOptions {
    /// Admit 1..=65,536 rows and 1..=8 MiB of cumulative persisted encoded_bytes.
    /// # Errors
    /// `Programming` with [`SealerProblem::Options`] outside those bounds.
    pub fn new(rows: u32, bytes: u64) -> Result<Self> {
        if rows == 0 || rows > MAX_ROWS || bytes == 0 || bytes > MAX_BYTES {
            return Err(SealerProblem::Options.into());
        }
        Ok(Self { rows, bytes })
    }
    /// Maximum rows in one batch.
    pub const fn max_rows(&self) -> u32 {
        self.rows
    }
    /// Maximum sum of persisted encoded_bytes in one batch.
    pub const fn max_stored_bytes(&self) -> u64 {
        self.bytes
    }
}
/// A maintenance outcome, never a declaration of external assurance or confirmed commit.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SealOutcome {
    /// Another maintenance transaction holds the shared lock; no rows were changed.
    Busy,
    /// No row lies in the safe prefix. A long transaction may be holding the frontier back.
    NoWork,
    /// One seal/checkpoint was inserted. The caller still owns commit/rollback.
    Staged(SealReport),
}
/// Safe metadata for a staged immutable seal; contains no audit row payload.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SealReport {
    identity: SealIdentity,
    lower: (u64, i64),
    upper: (u64, i64),
    count: u64,
    digest: [u8; 32],
    hash: [u8; 32],
}
impl SealReport {
    /// Database incarnation, epoch and batch identity. Publication requires confirmed commit.
    pub const fn identity(&self) -> SealIdentity {
        self.identity
    }
    /// Exclusive lower boundary, inherited from the previous seal/epoch.
    pub const fn lower_cursor(&self) -> (u64, i64) {
        self.lower
    }
    /// Inclusive last selected cursor.
    pub const fn upper_cursor(&self) -> (u64, i64) {
        self.upper
    }
    /// Number of persisted rows actually hashed.
    pub const fn row_count(&self) -> u64 {
        self.count
    }
    /// Ordered batch digest.
    pub const fn batch_digest(&self) -> &[u8; 32] {
        &self.digest
    }
    /// Format-1 seal envelope digest. This is not a witness anchor.
    pub const fn seal_hash(&self) -> &[u8; 32] {
        &self.hash
    }
}

impl Transaction {
    /// Stage one immutable safe-prefix seal/checkpoint in this owner transaction.
    ///
    /// Requires an explicitly initialized chain, System/full authority and Read Committed.
    /// The caller owns COMMIT/rollback and must roll back after an error. Never publish a witness
    /// from this report until COMMIT is confirmed. No request writer lock or witness I/O is added.
    /// # Errors
    /// `Access` before SQL for non-System/full; existing catalog/head failures; `Programming`
    /// with [`SealerProblem`] for invalid persisted values/bounds; `Database` for SQL failures.
    pub async fn seal_audit_batch(&mut self, options: &SealOptions) -> Result<SealOutcome> {
        self.require_system_full("Transaction::seal_audit_batch")?;
        if !self.try_audit_chain_lock().await? {
            return Ok(SealOutcome::Busy);
        }
        self.check_chain_storage().await?;
        self.check_audit_seal_input().await?;
        let head = self
            .audit_chain_head()
            .await?
            .ok_or(ChainProblem::NotInitialized)?;
        let epoch = head.epoch();
        let last=self.query("SELECT COALESCE(max(batch_no),0) FROM erp_event.ir_audit_seal WHERE chain_epoch=$1",&[&epoch]).await?;
        let batch_no = next_batch(
            last[0]
                .try_get::<_, i64>(0)
                .map_err(|_| Error::from(ChainProblem::InvalidHead))?,
        )?;
        let identity =
            SealIdentity::new(*head.incarnation(), epoch as u64, batch_no as u64).map_err(codec)?;
        let lower = head.lower_cursor();
        let xid = lower.0.to_string();
        let rows = i64::from(options.rows);
        let bytes = options.bytes as i64;
        // The xmin frontier and row visibility come from this ONE selection statement snapshot.
        // Materialized candidate metadata bounds count before payload/window processing. Status
        // markers retain the first excluded/invalid row; later small rows cannot fill a gap.
        // The text cursor is aliased xid_text and the final ORDER BY is qualified: a bare `xid` there
        // would bind to a `xid::text` output column and sort decimal text ("100" < "99").
        let sql=format!("WITH candidates AS MATERIALIZED (
            SELECT tableoid AS relation_id,ctid AS tuple_id,id,xid,encoded_bytes
            FROM erp_event.ir_audit_entry
            WHERE (xid,id)>($1::text::xid8,$2) AND xid<pg_catalog.pg_snapshot_xmin(pg_catalog.pg_current_snapshot())
            ORDER BY xid,id LIMIT ($3::bigint+1)
        ), estimates AS (
            SELECT *,row_number() OVER w AS ordinal,count(*) OVER (PARTITION BY xid,id) AS cursor_count,
            sum(CASE WHEN encoded_bytes>0 THEN encoded_bytes::bigint ELSE 0 END) OVER w AS stored_total,
            bool_or(encoded_bytes<=0) OVER w AS bad_estimate
            FROM candidates WINDOW w AS (ORDER BY xid,id ROWS UNBOUNDED PRECEDING)
        ), prefix AS MATERIALIZED (
            SELECT * FROM estimates WHERE ordinal<=$3 AND (stored_total<=$4::bigint OR bad_estimate
                OR (stored_total-CASE WHEN encoded_bytes>0 THEN encoded_bytes::bigint ELSE 0 END)<=$4::bigint)
        ), payloads AS (
            SELECT e.xid,e.id,e.encoded_bytes,e.stored_total,e.bad_estimate,e.cursor_count,pg_catalog.row_to_json(p)::text AS payload
            FROM prefix e JOIN erp_event.ir_audit_entry a ON a.tableoid=e.relation_id AND a.ctid=e.tuple_id
            CROSS JOIN LATERAL (SELECT {}) p
        ), sizes AS (
            SELECT *,pg_catalog.octet_length(payload)::bigint AS payload_bytes FROM payloads
        ), bounded AS (
            SELECT *,sum(payload_bytes) OVER (ORDER BY xid,id ROWS UNBOUNDED PRECEDING) AS wire_total FROM sizes
        ) SELECT xid::text AS xid_text,id,encoded_bytes,payload_bytes,
            CASE WHEN cursor_count<>1 THEN 5 WHEN encoded_bytes<=0 THEN 1 WHEN payload_bytes>{} THEN 2
                WHEN stored_total>$4::bigint THEN 3 WHEN wire_total>{MAX_WIRE} THEN 4 ELSE 0 END AS status,
            CASE WHEN cursor_count=1 AND NOT bad_estimate AND stored_total<=$4::bigint AND payload_bytes<={} AND wire_total<={MAX_WIRE}
                THEN payload END AS payload
        FROM bounded ORDER BY bounded.xid,bounded.id",row::PROJECTION,super::json::MAX_PAYLOAD,super::json::MAX_PAYLOAD);
        let mut digests = Vec::new();
        let mut upper = lower;
        let mut stopped = false;
        let mut received = 0i64;
        self.query_each(&sql, &[&xid, &lower.1, &rows, &bytes], |r| {
            // Rows beyond a normal budget boundary are intentionally deferred to the next batch.
            if stopped {
                return Ok(());
            }
            let status: i32 = r.try_get(4)?;
            match status {
                1 => return Err(SealerProblem::EncodedBytes.into()),
                5 => return Err(SealerProblem::Cursor.into()),
                2 => return Err(SealerProblem::RowPayload.into()),
                3 | 4 => {
                    if digests.is_empty() {
                        return Err(if status == 3 {
                            SealerProblem::FirstRow
                        } else {
                            SealerProblem::DecodeLimit
                        }
                        .into());
                    }
                    stopped = true;
                    return Ok(());
                }
                0 => {}
                _ => return Err(SealerProblem::RowValue.into()),
            }
            let cursor = (
                r.try_get::<&str>(0)?
                    .parse::<u64>()
                    .map_err(|_| Error::from(SealerProblem::Cursor))?,
                r.try_get::<i64>(1)?,
            );
            if cursor <= upper {
                return Err(SealerProblem::Cursor.into());
            }
            SealCursor::new(cursor.0, cursor.1).map_err(codec)?;
            let payload = r
                .try_get::<Option<&str>>(5)?
                .ok_or(SealerProblem::RowValue)?;
            received = received
                .checked_add(i64::try_from(payload.len()).map_err(|_| SealerProblem::DecodeLimit)?)
                .ok_or(SealerProblem::DecodeLimit)?;
            if received > MAX_WIRE {
                return Err(SealerProblem::DecodeLimit.into());
            }
            digests.push(row::digest(payload)?);
            upper = cursor;
            Ok(())
        })
        .await?;
        if digests.is_empty() {
            return Ok(SealOutcome::NoWork);
        }
        let batch = BatchDigest::new(&digests).map_err(codec)?;
        let hash = SealInput::new(
            identity,
            SealCursor::new(lower.0, lower.1).map_err(codec)?,
            SealCursor::new(upper.0, upper.1).map_err(codec)?,
            batch,
            *head.predecessor_hash(),
        )
        .map_err(codec)?
        .hash();
        let upper_xid = upper.0.to_string();
        let count = batch.row_count() as i64;
        let inserted=self.query("INSERT INTO erp_event.ir_audit_seal(chain_epoch,batch_no,lower_xid,lower_id,upper_xid,upper_id,row_count,batch_digest,prev_seal_hash,seal_hash,format_version)
            VALUES($1,$2,$3::text::xid8,$4,$5::text::xid8,$6,$7,$8,$9,$10,1) RETURNING id",
            &[&epoch,&batch_no,&xid,&lower.1,&upper_xid,&upper.1,&count,&&batch.as_bytes()[..],&&head.predecessor_hash()[..],&&hash[..]]).await?;
        if inserted.len() != 1 {
            return Err(SealerProblem::Insertion.into());
        }
        Ok(SealOutcome::Staged(SealReport {
            identity,
            lower,
            upper,
            count: batch.row_count(),
            digest: *batch.as_bytes(),
            hash,
        }))
    }
}

fn next_batch(last: i64) -> Result<i64> {
    if last < 0 {
        return Err(ChainProblem::InvalidHead.into());
    }
    last.checked_add(1)
        .ok_or_else(|| SealerProblem::BatchOverflow.into())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;
    #[test]
    fn batch_numbers_do_not_wrap_or_reset() {
        assert_eq!(next_batch(0).unwrap(), 1);
        assert_eq!(next_batch(i64::MAX - 1).unwrap(), i64::MAX);
        let e = next_batch(i64::MAX).unwrap_err();
        assert_eq!(e.kind(), erp_fields::ErrorKind::Programming);
        assert_eq!(
            e.message(),
            "a later release: batch number exhausted SQL bigint; owner investigation is required"
        );
    }
}
