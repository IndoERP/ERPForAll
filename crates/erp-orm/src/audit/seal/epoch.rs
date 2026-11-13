// SPDX-License-Identifier: MIT OR Apache-2.0
//! Explicit, transaction-owned epoch lifecycle. Sealing will reuse the same lock and head.
use super::{SealCursor, schema::ChainProblem};
use crate::Transaction;
use erp_fields::{Error, Result};

/// Bounded operator reason identifier; it is never SQL or arbitrary free text.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct EpochReason(String);
impl EpochReason {
    /// Accept 1..=128 ASCII letters, digits, dots, underscores or hyphens.
    /// # Errors
    /// `Programming` with [`ChainProblem::Reason`] for all other input.
    pub fn new(value: &str) -> Result<Self> {
        if value.is_empty()
            || value.len() > 128
            || !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        {
            return Err(ChainProblem::Reason.into());
        }
        Ok(Self(value.to_owned()))
    }
    /// The validated reason identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A validated current epoch boundary; hash consistency against audit rows is the verifier's duty.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct EpochState {
    epoch: i64,
    incarnation: [u8; 16],
    lower: (u64, i64),
    predecessor: [u8; 32],
    has_seal: bool,
}
impl EpochState {
    /// Positive SQL bigint epoch number.
    pub const fn epoch(&self) -> i64 {
        self.epoch
    }
    /// Stable UUID bytes in network order.
    pub const fn incarnation(&self) -> &[u8; 16] {
        &self.incarnation
    }
    /// Lower-exclusive cursor for the next batch, inherited across epoch boundaries.
    pub const fn lower_cursor(&self) -> (u64, i64) {
        self.lower
    }
    /// Predecessor seal hash for the next batch; zero only for the initial boundary.
    pub const fn predecessor_hash(&self) -> &[u8; 32] {
        &self.predecessor
    }
}

/// A competing maintenance transaction leaves this operation unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EpochChange {
    /// Another sealer or epoch operation holds the chain; retry in a new transaction.
    Busy,
    /// Metadata was inserted in the caller's transaction; commit remains the caller's duty.
    Created(EpochState),
}

// Stable database-local lock key shared by initialization, transitions and the future sealer.
const CHAIN_LOCK: &str = "erp_event.ir_audit_chain";
fn bad_head() -> Error {
    ChainProblem::InvalidHead.into()
}
fn digest(bytes: Vec<u8>) -> Result<[u8; 32]> {
    bytes.try_into().map_err(|_| bad_head())
}

struct SealBoundary {
    batch: i64,
    lower: (u64, i64),
    upper: (u64, i64),
    previous: [u8; 32],
    hash: [u8; 32],
}

impl Transaction {
    /// Initialize epoch 1 explicitly. Storage must already be installed by the owner.
    /// No commit is performed; rollback discards this initialization.
    /// # Errors
    /// `Access` before SQL unless System/full; `Programming` with [`ChainProblem`] for an existing
    /// chain, incompatible storage, or isolation other than Read Committed; `Database` for SQL.
    pub async fn initialize_audit_chain(&mut self, reason: &EpochReason) -> Result<EpochChange> {
        self.require_system_full("Transaction::initialize_audit_chain")?;
        if !self.try_audit_chain_lock().await? {
            return Ok(EpochChange::Busy);
        }
        self.check_chain_storage().await?;
        if self.audit_chain_head().await?.is_some() {
            return Err(ChainProblem::AlreadyInitialized.into());
        }
        let rows = self.query("INSERT INTO erp_event.ir_audit_chain(epoch,format_version,reason) VALUES(1,1,$1) RETURNING pg_catalog.uuid_send(incarnation)", &[&reason.as_str()]).await?;
        let bytes: Vec<u8> = rows[0].get(0);
        let incarnation: [u8; 16] = bytes.try_into().map_err(|_| bad_head())?;
        Ok(EpochChange::Created(EpochState {
            epoch: 1,
            incarnation,
            lower: (0, 0),
            predecessor: [0; 32],
            has_seal: false,
        }))
    }
    /// Start the next linked epoch using the current epoch's final seal hash and upper cursor.
    /// The incarnation is preserved. No commit or witness I/O is performed.
    /// # Errors
    /// As [`Self::initialize_audit_chain`], plus uninitialized/empty/corrupt predecessors or bigint
    /// exhaustion. A busy outcome changes nothing. The caller must use its owner connection.
    pub async fn new_audit_chain_epoch(&mut self, reason: &EpochReason) -> Result<EpochChange> {
        self.require_system_full("Transaction::new_audit_chain_epoch")?;
        if !self.try_audit_chain_lock().await? {
            return Ok(EpochChange::Busy);
        }
        self.check_chain_storage().await?;
        let mut head = self
            .audit_chain_head()
            .await?
            .ok_or(ChainProblem::NotInitialized)?;
        if !head.has_seal {
            return Err(ChainProblem::EmptyEpoch.into());
        }
        let epoch = head
            .epoch
            .checked_add(1)
            .ok_or(ChainProblem::EpochOverflow)?;
        self.query("INSERT INTO erp_event.ir_audit_chain(epoch,incarnation,format_version,previous_epoch,link_hash,reason) SELECT $1,incarnation,1,epoch,$2,$3 FROM erp_event.ir_audit_chain WHERE epoch=$4", &[&epoch,&&head.predecessor[..],&reason.as_str(),&head.epoch]).await?;
        head.epoch = epoch;
        head.has_seal = false;
        Ok(EpochChange::Created(head))
    }
    // These helpers deliberately remain crate-private; no arbitrary seal append API is exposed.
    pub(crate) async fn try_audit_chain_lock(&mut self) -> Result<bool> {
        self.require_system_full("Transaction::try_audit_chain_lock")?;
        let isolation: String = self
            .query(
                "SELECT pg_catalog.current_setting('transaction_isolation')",
                &[],
            )
            .await?[0]
            .get(0);
        if isolation != "read committed" {
            return Err(ChainProblem::Isolation.into());
        }
        Ok(self
            .query(
                "SELECT pg_catalog.pg_try_advisory_xact_lock(pg_catalog.hashtextextended($1,0))",
                &[&CHAIN_LOCK],
            )
            .await?[0]
            .get(0))
    }
    pub(crate) async fn audit_chain_head(&mut self) -> Result<Option<EpochState>> {
        self.require_system_full("Transaction::audit_chain_head")?;
        let rows = self.query("SELECT epoch,pg_catalog.uuid_send(incarnation),format_version,previous_epoch,link_hash FROM erp_event.ir_audit_chain ORDER BY epoch DESC LIMIT 1", &[]).await?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let epoch: i64 = row.get(0);
        let bytes: Vec<u8> = row.get(1);
        let incarnation: [u8; 16] = bytes.try_into().map_err(|_| bad_head())?;
        let format: i16 = row.get(2);
        let previous: Option<i64> = row.get(3);
        let link: Option<Vec<u8>> = row.get(4);
        if epoch <= 0 || incarnation == [0; 16] || format != 1 {
            return Err(bad_head());
        }
        let (lower, predecessor) = if epoch == 1 {
            if previous.is_some() || link.is_some() {
                return Err(bad_head());
            }
            ((0, 0), [0; 32])
        } else {
            if previous != Some(epoch - 1) {
                return Err(bad_head());
            }
            let prior=self.query("SELECT pg_catalog.uuid_send(incarnation),format_version FROM erp_event.ir_audit_chain WHERE epoch=$1", &[&(epoch-1)]).await?;
            if prior.len() != 1
                || prior[0].get::<_, Vec<u8>>(0).as_slice() != incarnation
                || prior[0].get::<_, i16>(1) != 1
            {
                return Err(bad_head());
            }
            let prior = self
                .chain_seal_boundary(epoch - 1, false)
                .await?
                .ok_or_else(bad_head)?;
            if Some(prior.hash.as_slice()) != link.as_deref() {
                return Err(bad_head());
            }
            (prior.upper, prior.hash)
        };
        let first = self.chain_seal_boundary(epoch, true).await?;
        let last = self.chain_seal_boundary(epoch, false).await?;
        match (first, last) {
            (None, None) => Ok(Some(EpochState {
                epoch,
                incarnation,
                lower,
                predecessor,
                has_seal: false,
            })),
            (Some(first), Some(last)) => {
                let count: i64 = self
                    .query(
                        "SELECT count(*) FROM erp_event.ir_audit_seal WHERE chain_epoch=$1",
                        &[&epoch],
                    )
                    .await?[0]
                    .get(0);
                if first.batch != 1
                    || count != last.batch
                    || first.lower != lower
                    || first.previous != predecessor
                    || last.hash == [0; 32]
                {
                    return Err(bad_head());
                }
                Ok(Some(EpochState {
                    epoch,
                    incarnation,
                    lower: last.upper,
                    predecessor: last.hash,
                    has_seal: true,
                }))
            }
            _ => Err(bad_head()),
        }
    }
    async fn chain_seal_boundary(
        &mut self,
        epoch: i64,
        first: bool,
    ) -> Result<Option<SealBoundary>> {
        let order = if first { "ASC" } else { "DESC" };
        let rows=self.query(&format!("SELECT batch_no,lower_xid::text,lower_id,upper_xid::text,upper_id,prev_seal_hash,seal_hash,format_version FROM erp_event.ir_audit_seal WHERE chain_epoch=$1 ORDER BY batch_no {order} LIMIT 1"), &[&epoch]).await?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let lower = (
            row.get::<_, String>(1)
                .parse::<u64>()
                .map_err(|_| bad_head())?,
            row.get::<_, i64>(2),
        );
        let upper = (
            row.get::<_, String>(3)
                .parse::<u64>()
                .map_err(|_| bad_head())?,
            row.get::<_, i64>(4),
        );
        SealCursor::new(lower.0, lower.1).map_err(|_| bad_head())?;
        SealCursor::new(upper.0, upper.1).map_err(|_| bad_head())?;
        let batch = row.get::<_, i64>(0);
        if batch <= 0 || lower >= upper || row.get::<_, i16>(7) != 1 {
            return Err(bad_head());
        }
        Ok(Some(SealBoundary {
            batch,
            lower,
            upper,
            previous: digest(row.get(5))?,
            hash: digest(row.get(6))?,
        }))
    }
}
