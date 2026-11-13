// SPDX-License-Identifier: MIT OR Apache-2.0
//! Domain-separated row, batch and seal digests from the format-1 grammar.
use super::grammar::{
    CanonicalValue, CodecError, encode_members, item_count, put_lp, sorted_members,
};
use sha2::{Digest, Sha256};

/// SHA-256 of every schema-1 persisted audit column, sorted by its UTF-8 name.
/// This is a codec result, not evidence that a database row or witness was verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct RowDigest([u8; 32]);
impl RowDigest {
    /// Hash a complete schema-1 column set, including generated columns.
    /// Values must already be redacted and mapped to their SQL types by the caller.
    ///
    /// # Errors
    /// `ColumnSet` for missing/extra names, `DuplicateName`, or an encoding `Limit`.
    pub fn schema1(columns: &[(&str, CanonicalValue)]) -> Result<Self, CodecError> {
        let sorted = sorted_members(columns)?;
        let expected = crate::audit::AUDIT_TABLE.columns;
        if sorted.len() != expected.len()
            || sorted
                .iter()
                .any(|(name, _)| !expected.iter().any(|c| c.name == *name))
        {
            return Err(CodecError::ColumnSet);
        }
        let mut bytes = Vec::new();
        put_lp(&mut bytes, b"erp.audit.row.v1")?;
        encode_members(&mut bytes, &sorted)?;
        Ok(Self(Sha256::digest(&bytes).into()))
    }
    /// Digest bytes in SHA-256 output order.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Digest and count of an ordered nonempty batch of row digests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct BatchDigest {
    hash: [u8; 32],
    count: u64,
}
impl BatchDigest {
    /// Hash rows in the supplied order. The database adapter must supply `(xid,id)` order.
    ///
    /// # Errors
    /// `EmptyBatch` or `Limit` above 65,536 rows. This codec cap is not the sealer default.
    pub fn new(rows: &[RowDigest]) -> Result<Self, CodecError> {
        if rows.is_empty() {
            return Err(CodecError::EmptyBatch);
        }
        let count = u64::from(item_count(rows.len())?);
        let mut hash = Sha256::new();
        hash.update((b"erp.audit.batch.v1".len() as u32).to_be_bytes());
        hash.update(b"erp.audit.batch.v1");
        hash.update(count.to_be_bytes());
        for row in rows {
            hash.update(row.0);
        }
        Ok(Self {
            hash: hash.finalize().into(),
            count,
        })
    }
    /// Digest bytes in SHA-256 output order.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.hash
    }
    /// Number of row digests included in the hash.
    pub fn row_count(&self) -> u64 {
        self.count
    }
}

/// Ordered `(xid8,id)` cursor. `(0,0)` is the initial lower exclusive bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub struct SealCursor {
    xid: u64,
    id: i64,
}
impl SealCursor {
    /// Preserve xid8 without signed narrowing. Zero id is permitted only at the initial cursor.
    ///
    /// # Errors
    /// `InvalidCursor` for negative ids or a partially zero cursor.
    pub fn new(xid: u64, id: i64) -> Result<Self, CodecError> {
        if id < 0 || ((xid == 0) != (id == 0)) {
            return Err(CodecError::InvalidCursor);
        }
        Ok(Self { xid, id })
    }
    fn encode(self, hash: &mut Sha256) {
        hash.update(self.xid.to_be_bytes());
        hash.update(self.id.to_be_bytes());
    }
}

/// Validated database incarnation, epoch and batch number. Epoch and batch fit positive SQL bigint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct SealIdentity {
    incarnation: [u8; 16],
    epoch: u64,
    batch_no: u64,
}
impl SealIdentity {
    /// UUID network-order incarnation bytes.
    pub fn incarnation(&self) -> &[u8; 16] {
        &self.incarnation
    }
    /// Positive SQL-bigint epoch.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
    /// Positive SQL-bigint batch number.
    pub fn batch_no(&self) -> u64 {
        self.batch_no
    }

    /// Build a format-1 identity. The incarnation is UUID network-order bytes.
    ///
    /// # Errors
    /// `InvalidIdentity` for nil incarnation or non-positive/out-of-bigint-range numbers.
    pub fn new(incarnation: [u8; 16], epoch: u64, batch_no: u64) -> Result<Self, CodecError> {
        if incarnation == [0; 16]
            || epoch == 0
            || batch_no == 0
            || epoch > i64::MAX as u64
            || batch_no > i64::MAX as u64
        {
            return Err(CodecError::InvalidIdentity);
        }
        Ok(Self {
            incarnation,
            epoch,
            batch_no,
        })
    }
}

/// Validated format-1 envelope. Database continuity and actual row bounds are verifier duties.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct SealInput {
    identity: SealIdentity,
    lower: SealCursor,
    upper: SealCursor,
    batch: BatchDigest,
    predecessor: [u8; 32],
}
impl SealInput {
    /// Construct an envelope with lower-exclusive and upper-inclusive cursors.
    /// Only epoch 1/batch 1 uses a zero predecessor, and it must start at `(0,0)`.
    ///
    /// # Errors
    /// `InvalidCursor` for reversed/equal bounds or a nonzero genesis lower bound;
    /// `InvalidPredecessor` for a wrong zero/nonzero predecessor.
    pub fn new(
        identity: SealIdentity,
        lower: SealCursor,
        upper: SealCursor,
        batch: BatchDigest,
        predecessor: [u8; 32],
    ) -> Result<Self, CodecError> {
        let genesis = identity.epoch == 1 && identity.batch_no == 1;
        if lower >= upper || (genesis && lower != (SealCursor { xid: 0, id: 0 })) {
            return Err(CodecError::InvalidCursor);
        }
        if genesis != (predecessor == [0; 32]) {
            return Err(CodecError::InvalidPredecessor);
        }
        Ok(Self {
            identity,
            lower,
            upper,
            batch,
            predecessor,
        })
    }
    /// SHA-256 of the complete format-1 envelope; performs no persistence or publication.
    pub fn hash(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update((b"erp.audit.seal.v1".len() as u32).to_be_bytes());
        hash.update(b"erp.audit.seal.v1");
        hash.update(1_u32.to_be_bytes());
        hash.update(self.identity.incarnation);
        hash.update(self.identity.epoch.to_be_bytes());
        hash.update(self.identity.batch_no.to_be_bytes());
        self.lower.encode(&mut hash);
        self.upper.encode(&mut hash);
        hash.update(self.batch.count.to_be_bytes());
        hash.update(self.batch.hash);
        hash.update(self.predecessor);
        hash.finalize().into()
    }
}
