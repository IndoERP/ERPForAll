// SPDX-License-Identifier: MIT OR Apache-2.0
//! Read-only verification of audit rows, seals and epoch links against an anchor inventory.
//!
//! The verifier runs in one REPEATABLE READ (or SERIALIZABLE) snapshot and takes no advisory lock, so a running sealer
//! never blocks it. Stored seals and inventory anchors are walked in `(epoch, batch_no)` order and the
//! FIRST defect in that order is reported. An integrity defect is `Ok(VerifyOutcome::Failed)`; `Err`
//! means the verifier could not run (admission, isolation, catalog drift or SQL failure).
//!
//! Assurance is the frontier of the last anchored seal in walk order. The chain links (previous seal
//! hash, inherited cursor, epoch link hash) tie every earlier seal to that frontier, so an unanchored
//! seal BEFORE a later anchored one is covered and is not a failure. Seals after the frontier are the
//! unanchored tail and rows after the head's last cursor are the unsealed tail; both are reported and
//! never called verified. `NoWitness` and `NoAnchors` are not external verification.
use super::{
    Anchor, AnchorRange, BatchDigest, RowDigest, SealCursor, SealIdentity, SealInput,
    SealerProblem, WitnessError, json, row,
    sealer::{MAX_ROWS, MAX_WIRE},
};
use crate::Transaction;
use erp_fields::{Error, Result};
use std::fmt;

const MAX_ANCHORS: usize = 65_536;
const PAGE: i64 = 512;

/// Refusals that stop the verifier before it can report: invalid input or wrong isolation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum VerifyRefusal {
    /// A range scope is not positive, within SQL bigint, and ordered.
    InvalidScope,
    /// Verification needs one snapshot: REPEATABLE READ or SERIALIZABLE.
    Isolation,
    /// A stored value could not be read in the expected type; the driver error is the source.
    Read,
}
impl fmt::Display for VerifyRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a later release: ")?;
        f.write_str(match self {
            Self::InvalidScope => {
                "range scope requires positive SQL bigint epoch and batch bounds with first <= last"
            }
            Self::Isolation => "verification requires REPEATABLE READ or SERIALIZABLE isolation",
            Self::Read => "a stored value could not be read in its expected type",
        })
    }
}
impl std::error::Error for VerifyRefusal {}
impl From<VerifyRefusal> for Error {
    fn from(value: VerifyRefusal) -> Self {
        Error::programming(value.to_string()).with_source(value)
    }
}

/// A validated, sorted set of externally witnessed anchors (at most 65,536).
///
/// Identical duplicates collapse; two anchors with one identity and different hashes are refused.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AnchorInventory {
    anchors: Vec<Anchor>,
}
fn anchor_key(a: &Anchor) -> (u64, u64, [u8; 16]) {
    let id = a.identity();
    (id.epoch(), id.batch_no(), *id.incarnation())
}
impl AnchorInventory {
    /// Validate and sort an inventory.
    /// # Errors
    /// [`WitnessError::Limit`] above 65,536 anchors; [`WitnessError::Conflict`] for one identity
    /// with different seal hashes or formats.
    pub fn new(mut anchors: Vec<Anchor>) -> std::result::Result<Self, WitnessError> {
        if anchors.len() > MAX_ANCHORS {
            return Err(WitnessError::Limit("anchor inventory"));
        }
        anchors.sort_by_key(anchor_key);
        let mut unique: Vec<Anchor> = Vec::with_capacity(anchors.len());
        for anchor in anchors {
            match unique.last() {
                Some(last) if anchor_key(last) == anchor_key(&anchor) => {
                    if *last != anchor {
                        return Err(WitnessError::Conflict);
                    }
                }
                _ => unique.push(anchor),
            }
        }
        Ok(Self { anchors: unique })
    }
    /// Number of distinct anchors.
    pub fn len(&self) -> usize {
        self.anchors.len()
    }
    /// Whether the inventory holds no anchor.
    pub fn is_empty(&self) -> bool {
        self.anchors.is_empty()
    }
    /// Anchors ordered by epoch, batch and incarnation.
    pub fn anchors(&self) -> &[Anchor] {
        &self.anchors
    }
}

/// External evidence handed to the verifier.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WitnessEvidence {
    /// No witness is configured; the result is [`Assurance::NoWitness`].
    NotConfigured,
    /// An independently read inventory (possibly empty).
    Inventory(AnchorInventory),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScopeRange {
    epoch: i64,
    first: i64,
    last: i64,
}
/// What to verify: the whole chain or inclusive batches `first..=last` of one epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifyScope {
    range: Option<ScopeRange>,
}
impl VerifyScope {
    /// Every epoch and seal.
    pub const fn full() -> Self {
        Self { range: None }
    }
    /// One epoch's inclusive batch range. The predecessor boundary is read as stored and the
    /// report is partial. Incarnation and chain-missing checks still cover the whole inventory;
    /// hash comparison covers anchors inside the range only.
    /// # Errors
    /// `Programming` with [`VerifyRefusal::InvalidScope`] for zero, reversed or out-of-bigint values.
    pub fn range(epoch: u64, first: u64, last: u64) -> Result<Self> {
        if epoch == 0 || epoch > i64::MAX as u64 {
            return Err(VerifyRefusal::InvalidScope.into());
        }
        AnchorRange::new(first, last).map_err(|_| Error::from(VerifyRefusal::InvalidScope))?;
        Ok(Self {
            range: Some(ScopeRange {
                epoch: epoch as i64,
                first: first as i64,
                last: last as i64,
            }),
        })
    }
    /// Whether the whole chain is verified.
    pub const fn is_full(&self) -> bool {
        self.range.is_none()
    }
    /// The epoch of a range scope.
    pub fn epoch(&self) -> Option<u64> {
        self.range.map(|r| r.epoch as u64)
    }
    /// Inclusive batch bounds of a range scope.
    pub fn batches(&self) -> Option<(u64, u64)> {
        self.range.map(|r| (r.first as u64, r.last as u64))
    }
}

/// Where a failure was found; never carries row contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FailurePoint {
    /// A seal, stored or expected, by epoch and batch number.
    Seal {
        /// Epoch number.
        epoch: u64,
        /// Batch number within the epoch.
        batch_no: u64,
    },
    /// An epoch row, or a requested epoch.
    Epoch(u64),
    /// The chain as a whole.
    Chain,
}
impl fmt::Display for FailurePoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Seal { epoch, batch_no } => write!(f, "seal epoch {epoch} batch {batch_no}"),
            Self::Epoch(epoch) => write!(f, "epoch {epoch}"),
            Self::Chain => f.write_str("chain"),
        }
    }
}

/// A detected integrity defect. Messages are stable and contain no row payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum VerifyProblem {
    /// Anchors exist but no chain is initialized.
    ChainMissing,
    /// An anchor names another chain incarnation.
    ForeignIncarnation,
    /// Epoch numbers are not contiguous from 1.
    ///
    /// Defensive: `ir_audit_chain_pkey`, `_previous`, `_previous_number`, `_genesis` and `_successor`
    /// make a gap or a first epoch other than 1 impossible without dropping constraints.
    EpochSequence,
    /// An epoch's incarnation differs from the chain's.
    EpochIncarnation,
    /// An epoch's format is not 1.
    ///
    /// Defensive: `ir_audit_chain_format` (`CHECK (format_version = 1)`) refuses any other value.
    EpochFormat,
    /// An epoch link differs from the predecessor epoch's final seal.
    EpochLink,
    /// A non-head epoch has no seal.
    EmptyEpoch,
    /// The requested epoch or range has no stored seal.
    RangeNotFound,
    /// The seal before a range is missing.
    RangePredecessor,
    /// Batch numbers are not contiguous.
    SealNumbering,
    /// A seal's format is not 1.
    ///
    /// Defensive: `ir_audit_seal_format` (`CHECK (format_version = 1)`) refuses any other value.
    SealFormat,
    /// Stored seal values cannot be decoded.
    ///
    /// Defensive: `ir_audit_seal_initial_cursor`, `_upper_id`, `_upper_xid`, `_digest`,
    /// `_predecessor` and `_hash` constrain cursors and 32-byte digests, so every stored seal decodes.
    SealMalformed,
    /// A lower cursor does not continue the previous boundary.
    CursorChain,
    /// `prev_seal_hash` differs from the previous seal.
    PredecessorHash,
    /// Stored row count exceeds the verifier limit (above 65,536; `ir_audit_seal_count` only
    /// refuses values of zero or less).
    RowCountLimit,
    /// Stored row count differs from the rows in the range.
    RowCountMismatch,
    /// Rows are not strictly increasing by cursor.
    RowOrder,
    /// The last row is not the stored upper cursor.
    UpperCursor,
    /// A row has an unsupported schema version.
    RowSchema,
    /// A row has an unsupported shape or type.
    RowValue,
    /// A row exceeds the payload safety limit.
    RowPayloadLimit,
    /// Rows exceed the cumulative transfer limit.
    WireLimit,
    /// A row cannot be canonically encoded (canonical nesting or size limits). No test reaches it.
    RowEncoding,
    /// The recomputed batch digest differs.
    BatchDigestMismatch,
    /// The seal envelope cannot be rebuilt from stored values.
    ///
    /// Defensive: every input of the envelope is validated by an earlier check or constraint.
    SealEnvelope,
    /// The recomputed seal hash differs.
    SealHashMismatch,
    /// An anchor has no stored seal.
    AnchorWithoutSeal,
    /// A stored seal hash differs from its anchor.
    AnchorHashMismatch,
}
impl fmt::Display for VerifyProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a later release: ")?;
        f.write_str(match self {
            Self::ChainMissing => "anchors exist but no audit chain is initialized",
            Self::ForeignIncarnation => "anchor belongs to a different chain incarnation",
            Self::EpochSequence => "epoch numbers are not contiguous from 1",
            Self::EpochIncarnation => "epoch incarnation differs from the chain's",
            Self::EpochFormat => "epoch format version is unsupported",
            Self::EpochLink => "epoch link does not match the predecessor epoch's final seal",
            Self::EmptyEpoch => "epoch has no seal but is not the head epoch",
            Self::RangeNotFound => "requested epoch or batch range has no stored seal",
            Self::RangePredecessor => "predecessor seal of the requested range is missing",
            Self::SealNumbering => "seal batch numbers are not contiguous",
            Self::SealFormat => "seal format version is unsupported",
            Self::SealMalformed => "stored seal values are malformed",
            Self::CursorChain => "seal lower cursor does not continue the previous boundary",
            Self::PredecessorHash => "seal predecessor hash does not match the previous seal",
            Self::RowCountLimit => "stored row count exceeds the verifier limit",
            Self::RowCountMismatch => "stored row count differs from the rows in the sealed range",
            Self::RowOrder => "sealed rows are not strictly increasing by cursor",
            Self::UpperCursor => "last sealed row does not match the stored upper cursor",
            Self::RowSchema => "a sealed row has an unsupported schema version",
            Self::RowValue => "a sealed row has an unsupported shape or type",
            Self::RowPayloadLimit => "a sealed row exceeds the payload safety limit",
            Self::WireLimit => "sealed rows exceed the cumulative transfer limit",
            Self::RowEncoding => "a sealed row cannot be canonically encoded",
            Self::BatchDigestMismatch => "recomputed batch digest differs from the stored digest",
            Self::SealEnvelope => "seal envelope cannot be recomputed from stored values",
            Self::SealHashMismatch => "recomputed seal hash differs from the stored hash",
            Self::AnchorWithoutSeal => "anchored seal is missing from the database",
            Self::AnchorHashMismatch => "stored seal hash differs from its anchor",
        })
    }
}

/// The first integrity defect found, with its position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifyFailure {
    at: FailurePoint,
    problem: VerifyProblem,
}
impl VerifyFailure {
    /// Where the walk stopped.
    pub const fn at(&self) -> FailurePoint {
        self.at
    }
    /// The defect.
    pub const fn problem(&self) -> VerifyProblem {
        self.problem
    }
}
impl fmt::Display for VerifyFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (at {})", self.problem, self.at)
    }
}
impl std::error::Error for VerifyFailure {}

/// How much external evidence backs a successful walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Assurance {
    /// No witness is configured; internal consistency only.
    NoWitness,
    /// A witness inventory exists but holds no anchor inside the scope.
    NoAnchors,
    /// The last anchored seal in walk order; earlier seals are covered by the chain links.
    AnchoredThrough {
        /// Epoch of the frontier seal.
        epoch: u64,
        /// Batch of the frontier seal.
        batch_no: u64,
    },
}

/// Count and UTC-microsecond `create_date` bounds of a tail. Bounds are `None` when the tail is
/// empty or a date is not finite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TailSummary {
    count: u64,
    first_micros: Option<i64>,
    last_micros: Option<i64>,
}
impl TailSummary {
    /// Rows (unsealed tail) or seals (unanchored tail).
    pub const fn count(&self) -> u64 {
        self.count
    }
    /// Earliest `create_date` in microseconds since the Unix epoch.
    pub const fn first_micros(&self) -> Option<i64> {
        self.first_micros
    }
    /// Latest `create_date` in microseconds since the Unix epoch.
    pub const fn last_micros(&self) -> Option<i64> {
        self.last_micros
    }
}

/// A walk without a detected defect. It is not proof of anything beyond [`Assurance`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    partial: bool,
    epochs: u64,
    seals: u64,
    rows: u64,
    assurance: Assurance,
    unanchored: TailSummary,
    unsealed: Option<TailSummary>,
    head_epoch_empty: bool,
}
impl VerifyReport {
    /// A range scope was verified.
    pub const fn partial(&self) -> bool {
        self.partial
    }
    /// Epochs walked.
    pub const fn epochs(&self) -> u64 {
        self.epochs
    }
    /// Seals recomputed.
    pub const fn seals(&self) -> u64 {
        self.seals
    }
    /// Rows rehashed.
    pub const fn rows(&self) -> u64 {
        self.rows
    }
    /// External assurance level.
    pub const fn assurance(&self) -> Assurance {
        self.assurance
    }
    /// Seals after the last anchored seal (all seals without anchors); seal `create_date` bounds.
    pub const fn unanchored_tail(&self) -> TailSummary {
        self.unanchored
    }
    /// Rows after the head's last cursor, or all rows when no chain exists. `None` for a range scope.
    pub const fn unsealed_tail(&self) -> Option<TailSummary> {
        self.unsealed
    }
    /// The head epoch has no seal yet (full scope only).
    pub const fn head_epoch_empty(&self) -> bool {
        self.head_epoch_empty
    }
}

/// Verification result. A defect is a value, not an error.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "a Failed outcome is an integrity defect"]
pub enum VerifyOutcome {
    /// No defect was found in the scope.
    Verified(VerifyReport),
    /// The first defect in walk order.
    Failed(VerifyFailure),
}

enum Stop {
    Failed(VerifyFailure),
    Error(Error),
}
impl From<VerifyRefusal> for Stop {
    fn from(e: VerifyRefusal) -> Self {
        Self::Error(e.into())
    }
}
impl From<Error> for Stop {
    fn from(e: Error) -> Self {
        Self::Error(e)
    }
}
type Flow<T> = std::result::Result<T, Stop>;
fn fail<T>(at: FailurePoint, problem: VerifyProblem) -> Flow<T> {
    Err(Stop::Failed(VerifyFailure { at, problem }))
}
fn cell<'a, T: tokio_postgres::types::FromSql<'a>>(
    row: &'a tokio_postgres::Row,
    index: usize,
) -> Result<T> {
    row.try_get(index)
        .map_err(|e| Error::from(VerifyRefusal::Read).with_source(e))
}

#[derive(Clone, Copy)]
struct Boundary {
    upper: (u64, i64),
    hash: [u8; 32],
}
const GENESIS: Boundary = Boundary {
    upper: (0, 0),
    hash: [0; 32],
};
struct EpochRow {
    epoch: i64,
    incarnation: Vec<u8>,
    format: i16,
    previous: Option<i64>,
    link: Option<Vec<u8>>,
}
struct Parsed {
    lower: (u64, i64),
    upper: (u64, i64),
    digest: [u8; 32],
    previous: [u8; 32],
    hash: [u8; 32],
}
struct StoredSeal {
    batch: i64,
    format: i16,
    count: i64,
    created: Option<i64>,
    parsed: Option<Parsed>,
}
struct RowRead {
    digests: Vec<RowDigest>,
    last: Option<(u64, i64)>,
    defect: Option<VerifyProblem>,
}

// Exact UTC microseconds; infinite dates have no finite reading.
fn micros(column: &str) -> String {
    format!(
        "CASE WHEN pg_catalog.isfinite({column}) THEN pg_catalog.trunc(EXTRACT(EPOCH FROM {column})*1000000)::bigint END"
    )
}
fn hash32(bytes: Vec<u8>) -> Option<[u8; 32]> {
    bytes.try_into().ok()
}
fn cursor(xid: &str, id: i64) -> Option<(u64, i64)> {
    let xid = xid.parse::<u64>().ok()?;
    SealCursor::new(xid, id).ok()?;
    Some((xid, id))
}
// Row adapter refusals are pure functions of the stored row; they fail that seal, never the run.
fn row_problem(error: &Error) -> VerifyProblem {
    match std::error::Error::source(error).and_then(|s| s.downcast_ref::<SealerProblem>()) {
        Some(SealerProblem::RowSchema) => VerifyProblem::RowSchema,
        Some(SealerProblem::RowPayload | SealerProblem::DecodeLimit) => {
            VerifyProblem::RowPayloadLimit
        }
        Some(SealerProblem::Codec(_)) => VerifyProblem::RowEncoding,
        _ => VerifyProblem::RowValue,
    }
}

#[derive(Default)]
struct Walk {
    anchors: Vec<Anchor>,
    next: usize,
    incarnation: [u8; 16],
    epochs: u64,
    seals: u64,
    rows: u64,
    frontier: Option<(u64, u64)>,
    tail: TailSummary,
}
impl Walk {
    // Anchors below `key` have no stored seal: the first one, in order, is the defect.
    fn drain_before(&mut self, key: (u64, u64)) -> Flow<()> {
        if let Some(a) = self.anchors.get(self.next) {
            let id = a.identity();
            if (id.epoch(), id.batch_no()) < key {
                return fail(
                    FailurePoint::Seal {
                        epoch: id.epoch(),
                        batch_no: id.batch_no(),
                    },
                    VerifyProblem::AnchorWithoutSeal,
                );
            }
        }
        Ok(())
    }
    // Compares anchors at exactly this seal; true when one existed.
    fn anchored(&mut self, at: FailurePoint, key: (u64, u64), hash: &[u8; 32]) -> Flow<bool> {
        let mut found = false;
        while let Some(a) = self.anchors.get(self.next) {
            let id = a.identity();
            if (id.epoch(), id.batch_no()) != key {
                break;
            }
            if a.seal_hash() != hash {
                return fail(at, VerifyProblem::AnchorHashMismatch);
            }
            found = true;
            self.next += 1;
        }
        Ok(found)
    }
    fn header(&self, e: &EpochRow, inherited: Option<Boundary>) -> Flow<()> {
        let at = FailurePoint::Epoch(e.epoch as u64);
        if e.incarnation != self.incarnation {
            return fail(at, VerifyProblem::EpochIncarnation);
        }
        if e.format != 1 {
            return fail(at, VerifyProblem::EpochFormat);
        }
        let linked = if e.epoch == 1 {
            e.previous.is_none() && e.link.is_none()
        } else {
            match (inherited, &e.link) {
                (Some(b), Some(link)) => e.previous == Some(e.epoch - 1) && link[..] == b.hash[..],
                _ => false,
            }
        };
        if linked {
            Ok(())
        } else {
            fail(at, VerifyProblem::EpochLink)
        }
    }

    // Verifies batches first..=last of one epoch. Returns the last boundary, `None` without a seal
    // (then `empty` names the defect unless the epoch may be empty).
    async fn seals(
        &mut self,
        tx: &mut Transaction,
        epoch: i64,
        window: (i64, i64),
        inherited: Boundary,
        empty: Option<VerifyProblem>,
    ) -> Flow<Option<Boundary>> {
        let mut prev = inherited;
        let mut expected = window.0;
        let mut after = window.0 - 1;
        let mut done = None;
        loop {
            let page = seal_page(tx, epoch, after, window.1).await?;
            for s in &page {
                self.drain_before((epoch as u64, s.batch as u64))?;
                if s.batch != expected {
                    return fail(
                        FailurePoint::Seal {
                            epoch: epoch as u64,
                            batch_no: expected as u64,
                        },
                        VerifyProblem::SealNumbering,
                    );
                }
                prev = self.check(tx, epoch, s, prev).await?;
                done = Some(prev);
                expected = expected.saturating_add(1);
                after = s.batch;
            }
            if (page.len() as i64) < PAGE {
                break;
            }
        }
        // Anchors of this epoch without a stored seal are reported before an empty epoch or range.
        self.drain_before((epoch as u64 + 1, 0))?;
        if done.is_none()
            && let Some(problem) = empty
        {
            return fail(FailurePoint::Epoch(epoch as u64), problem);
        }
        Ok(done)
    }

    async fn check(
        &mut self,
        tx: &mut Transaction,
        epoch: i64,
        s: &StoredSeal,
        prev: Boundary,
    ) -> Flow<Boundary> {
        let at = FailurePoint::Seal {
            epoch: epoch as u64,
            batch_no: s.batch as u64,
        };
        let key = (epoch as u64, s.batch as u64);
        let Some(p) = &s.parsed else {
            return fail(at, VerifyProblem::SealMalformed);
        };
        if s.format != 1 {
            return fail(at, VerifyProblem::SealFormat);
        }
        if p.lower != prev.upper {
            return fail(at, VerifyProblem::CursorChain);
        }
        if p.previous != prev.hash {
            return fail(at, VerifyProblem::PredecessorHash);
        }
        if s.count <= 0 || s.count > i64::from(MAX_ROWS) {
            return fail(at, VerifyProblem::RowCountLimit);
        }
        let read = read_rows(tx, p, s.count).await?;
        if let Some(problem) = read.defect {
            return fail(at, problem);
        }
        if read.digests.len() as i64 != s.count {
            return fail(at, VerifyProblem::RowCountMismatch);
        }
        if read.last != Some(p.upper) {
            return fail(at, VerifyProblem::UpperCursor);
        }
        let envelope = |_| {
            Stop::Failed(VerifyFailure {
                at,
                problem: VerifyProblem::SealEnvelope,
            })
        };
        let batch = BatchDigest::new(&read.digests).map_err(envelope)?;
        if *batch.as_bytes() != p.digest {
            return fail(at, VerifyProblem::BatchDigestMismatch);
        }
        let identity = SealIdentity::new(self.incarnation, key.0, key.1).map_err(envelope)?;
        let hash = SealInput::new(
            identity,
            SealCursor::new(p.lower.0, p.lower.1).map_err(envelope)?,
            SealCursor::new(p.upper.0, p.upper.1).map_err(envelope)?,
            batch,
            prev.hash,
        )
        .map_err(envelope)?
        .hash();
        if hash != p.hash {
            return fail(at, VerifyProblem::SealHashMismatch);
        }
        self.seals += 1;
        self.rows += s.count as u64;
        if self.anchored(at, key, &hash)? {
            self.frontier = Some(key);
            self.tail = TailSummary::default();
        } else {
            self.tail.count += 1;
            self.tail.first_micros = self.tail.first_micros.or(s.created);
            self.tail.last_micros = s.created;
        }
        Ok(Boundary {
            upper: p.upper,
            hash,
        })
    }
}

async fn epoch_page(
    tx: &mut Transaction,
    after: i64,
    through: i64,
    limit: i64,
) -> Result<Vec<EpochRow>> {
    let rows = tx
        .query(
            "SELECT epoch,pg_catalog.uuid_send(incarnation),format_version,previous_epoch,link_hash FROM erp_event.ir_audit_chain WHERE epoch>$1 AND epoch<=$2 ORDER BY epoch LIMIT $3",
            &[&after, &through, &limit],
        )
        .await?;
    rows.iter()
        .map(|r| {
            Ok(EpochRow {
                epoch: cell(r, 0)?,
                incarnation: cell(r, 1)?,
                format: cell(r, 2)?,
                previous: cell(r, 3)?,
                link: cell(r, 4)?,
            })
        })
        .collect()
}

async fn seal_page(
    tx: &mut Transaction,
    epoch: i64,
    after: i64,
    through: i64,
) -> Result<Vec<StoredSeal>> {
    let sql = format!(
        "SELECT batch_no,lower_xid::text,lower_id,upper_xid::text,upper_id,row_count,batch_digest,prev_seal_hash,seal_hash,format_version,{} FROM erp_event.ir_audit_seal WHERE chain_epoch=$1 AND batch_no>$2 AND batch_no<=$3 ORDER BY batch_no LIMIT $4",
        micros("create_date")
    );
    let rows = tx.query(&sql, &[&epoch, &after, &through, &PAGE]).await?;
    rows.iter()
        .map(|r| {
            let lower = cursor(&cell::<String>(r, 1)?, cell(r, 2)?);
            let upper = cursor(&cell::<String>(r, 3)?, cell(r, 4)?);
            let parsed = (|| {
                Some(Parsed {
                    lower: lower?,
                    upper: upper?,
                    digest: hash32(cell(r, 6).ok()?)?,
                    previous: hash32(cell(r, 7).ok()?)?,
                    hash: hash32(cell(r, 8).ok()?)?,
                })
            })();
            Ok(StoredSeal {
                batch: cell(r, 0)?,
                count: cell(r, 5)?,
                format: cell(r, 9)?,
                created: cell(r, 10)?,
                parsed,
            })
        })
        .collect()
}

enum Found {
    Absent,
    Malformed,
    At(Boundary),
}
impl Found {
    fn boundary(&self) -> Option<Boundary> {
        match self {
            Self::At(b) => Some(*b),
            _ => None,
        }
    }
}
async fn last_batch(tx: &mut Transaction, epoch: i64) -> Result<Option<i64>> {
    let rows = tx
        .query(
            "SELECT max(batch_no) FROM erp_event.ir_audit_seal WHERE chain_epoch=$1",
            &[&epoch],
        )
        .await?;
    cell(rows.first().ok_or(VerifyRefusal::Read)?, 0)
}
// Last seal of `epoch`, or the seal numbered `batch`, as stored.
async fn boundary_at(tx: &mut Transaction, epoch: i64, batch: Option<i64>) -> Result<Found> {
    let rows = tx
        .query(
            "SELECT upper_xid::text,upper_id,seal_hash FROM erp_event.ir_audit_seal WHERE chain_epoch=$1 AND ($2::bigint IS NULL OR batch_no=$2) ORDER BY batch_no DESC LIMIT 1",
            &[&epoch, &batch],
        )
        .await?;
    let Some(r) = rows.first() else {
        return Ok(Found::Absent);
    };
    let upper = cursor(&cell::<String>(r, 0)?, cell(r, 1)?);
    let hash = hash32(cell(r, 2)?);
    Ok(upper.zip(hash).map_or(Found::Malformed, |(upper, hash)| {
        Found::At(Boundary { upper, hash })
    }))
}

// Re-reads one seal's range with the sealer's projection and the same payload and wire caps.
// A refusal is recorded for the seal; only SQL or decoding of known columns returns `Err`.
async fn read_rows(tx: &mut Transaction, p: &Parsed, count: i64) -> Result<RowRead> {
    let sql = format!(
        "WITH candidates AS MATERIALIZED (
            SELECT tableoid AS relation_id,ctid AS tuple_id,xid,id FROM erp_event.ir_audit_entry
            WHERE (xid,id)>($1::text::xid8,$2) AND (xid,id)<=($3::text::xid8,$4) ORDER BY xid,id LIMIT $5
        ), payloads AS (
            SELECT c.xid,c.id,pg_catalog.row_to_json(p)::text AS payload
            FROM candidates c JOIN erp_event.ir_audit_entry a ON a.tableoid=c.relation_id AND a.ctid=c.tuple_id
            CROSS JOIN LATERAL (SELECT {}) p
        ), sizes AS (
            SELECT *,pg_catalog.octet_length(payload)::bigint AS payload_bytes FROM payloads
        ), bounded AS (
            SELECT *,(sum(payload_bytes) OVER (ORDER BY xid,id ROWS UNBOUNDED PRECEDING))::bigint AS wire_total FROM sizes
        ) SELECT xid::text AS xid_text,id,payload_bytes,wire_total,
            CASE WHEN payload_bytes<={} AND wire_total<={MAX_WIRE} THEN payload END AS payload
        FROM bounded ORDER BY xid,id",
        row::PROJECTION,
        json::MAX_PAYLOAD
    );
    let (lower_xid, upper_xid) = (p.lower.0.to_string(), p.upper.0.to_string());
    let limit = count + 1;
    let mut read = RowRead {
        digests: Vec::new(),
        last: None,
        defect: None,
    };
    let mut previous = p.lower;
    tx.query_each(
        &sql,
        &[&lower_xid, &p.lower.1, &upper_xid, &p.upper.1, &limit],
        |r| {
            if read.defect.is_some() {
                return Ok(());
            }
            let found = cursor(r.try_get::<&str>(0)?, r.try_get::<i64>(1)?);
            let payload_bytes: i64 = r.try_get(2)?;
            let wire_total: i64 = r.try_get(3)?;
            let payload: Option<&str> = r.try_get(4)?;
            read.defect = match found {
                None => Some(VerifyProblem::RowOrder),
                Some(c) if c <= previous => Some(VerifyProblem::RowOrder),
                Some(_) if read.digests.len() as i64 >= count => {
                    Some(VerifyProblem::RowCountMismatch)
                }
                Some(_) if payload_bytes > json::MAX_PAYLOAD as i64 => {
                    Some(VerifyProblem::RowPayloadLimit)
                }
                Some(_) if wire_total > MAX_WIRE => Some(VerifyProblem::WireLimit),
                Some(_) => match payload {
                    None => Some(VerifyProblem::RowValue),
                    Some(text) => match row::digest(text) {
                        Ok(d) => {
                            read.digests.push(d);
                            None
                        }
                        Err(e) => Some(row_problem(&e)),
                    },
                },
            };
            if read.defect.is_none() {
                previous = found.unwrap_or(previous);
                read.last = found;
            }
            Ok(())
        },
    )
    .await?;
    Ok(read)
}

async fn unsealed(tx: &mut Transaction, after: (u64, i64)) -> Result<TailSummary> {
    let sql = format!(
        "SELECT count(*),{},{} FROM erp_event.ir_audit_entry WHERE (xid,id)>($1::text::xid8,$2)",
        micros("min(create_date)"),
        micros("max(create_date)")
    );
    let rows = tx.query(&sql, &[&after.0.to_string(), &after.1]).await?;
    let r = rows.first().ok_or(VerifyRefusal::Read)?;
    Ok(TailSummary {
        count: u64::try_from(cell::<i64>(r, 0)?).map_err(|_| Error::from(VerifyRefusal::Read))?,
        first_micros: cell(r, 1)?,
        last_micros: cell(r, 2)?,
    })
}

impl Transaction {
    /// Verify audit rows, seals and epoch links in this REPEATABLE READ snapshot against `witness`.
    ///
    /// Requires System/full authority, REPEATABLE READ (SERIALIZABLE also gives one snapshot and is
    /// accepted; Read Committed is refused), compatible chain and audit storage. No lock
    /// is taken and nothing is written. The first defect in `(epoch, batch_no)` order is returned as
    /// [`VerifyOutcome::Failed`]; anchors without a stored seal (a deleted middle seal, suffix or
    /// epoch), differing hashes, foreign incarnations and anchors without a chain all fail. An
    /// absent epoch or range without anchors in a range scope fails with
    /// [`VerifyProblem::RangeNotFound`]. A range ending after the last stored seal verifies the
    /// stored seals; a range wholly beyond the last stored seal is `RangeNotFound`. An empty store
    /// or uninitialized chain without anchors gives a report with zero epochs (rows are then
    /// counted as unsealed from the start).
    /// # Errors
    /// `Access` before SQL for non-System/full; `Programming` with [`VerifyRefusal::Isolation`] or a
    /// catalog drift [`super::ChainProblem`]; `Database` for SQL failures. Roll back after an error.
    pub async fn verify_audit_chain(
        &mut self,
        scope: &VerifyScope,
        witness: &WitnessEvidence,
    ) -> Result<VerifyOutcome> {
        self.require_system_full("Transaction::verify_audit_chain")?;
        let isolation = self
            .query(
                "SELECT pg_catalog.current_setting('transaction_isolation')",
                &[],
            )
            .await?;
        let level: String = cell(isolation.first().ok_or(VerifyRefusal::Read)?, 0)?;
        if level != "repeatable read" && level != "serializable" {
            return Err(VerifyRefusal::Isolation.into());
        }
        self.check_chain_storage().await?;
        self.check_audit_seal_input().await?;
        match walk(self, scope, witness).await {
            Ok(report) => Ok(VerifyOutcome::Verified(report)),
            Err(Stop::Failed(failure)) => Ok(VerifyOutcome::Failed(failure)),
            Err(Stop::Error(error)) => Err(error),
        }
    }
}

async fn walk(
    tx: &mut Transaction,
    scope: &VerifyScope,
    witness: &WitnessEvidence,
) -> Flow<VerifyReport> {
    let inventory: &[Anchor] = match witness {
        WitnessEvidence::NotConfigured => &[],
        WitnessEvidence::Inventory(i) => i.anchors(),
    };
    let first = epoch_page(tx, 0, i64::MAX, 1).await?;
    let Some(genesis) = first.first() else {
        if !inventory.is_empty() {
            return fail(FailurePoint::Chain, VerifyProblem::ChainMissing);
        }
        if let Some(r) = scope.range {
            return fail(
                FailurePoint::Epoch(r.epoch as u64),
                VerifyProblem::RangeNotFound,
            );
        }
        // Without a chain every row is unsealed; show them instead of hiding them.
        let rows = unsealed(tx, (0, 0)).await?;
        return Ok(report(witness, false, &Walk::default(), Some(rows), false));
    };
    let incarnation: [u8; 16] = genesis
        .incarnation
        .clone()
        .try_into()
        .map_err(|_| Error::from(VerifyRefusal::Read))?;
    if let Some(a) = inventory
        .iter()
        .find(|a| *a.identity().incarnation() != incarnation)
    {
        return fail(
            FailurePoint::Seal {
                epoch: a.identity().epoch(),
                batch_no: a.identity().batch_no(),
            },
            VerifyProblem::ForeignIncarnation,
        );
    }
    let in_scope = |a: &&Anchor| {
        scope.range.is_none_or(|r| {
            let id = a.identity();
            id.epoch() == r.epoch as u64
                && (r.first as u64..=r.last as u64).contains(&id.batch_no())
        })
    };
    let mut w = Walk {
        anchors: inventory.iter().filter(in_scope).copied().collect(),
        incarnation,
        ..Walk::default()
    };
    let mut head_empty = false;
    let mut unsealed_tail = None;
    match scope.range {
        None => {
            let max = tx
                .query("SELECT max(epoch) FROM erp_event.ir_audit_chain", &[])
                .await?;
            let head: i64 = cell(max.first().ok_or(VerifyRefusal::Read)?, 0)?;
            let (mut after, mut expected, mut prev) = (0i64, 1i64, GENESIS);
            loop {
                let page = epoch_page(tx, after, i64::MAX, PAGE).await?;
                for e in &page {
                    w.drain_before((e.epoch as u64, 0))?;
                    if e.epoch != expected {
                        return fail(
                            FailurePoint::Epoch(e.epoch as u64),
                            VerifyProblem::EpochSequence,
                        );
                    }
                    w.header(e, Some(prev))?;
                    w.epochs += 1;
                    let problem = (e.epoch != head).then_some(VerifyProblem::EmptyEpoch);
                    match w.seals(tx, e.epoch, (1, i64::MAX), prev, problem).await? {
                        Some(end) => prev = end,
                        None => head_empty = true,
                    }
                    expected += 1;
                    after = e.epoch;
                }
                if (page.len() as i64) < PAGE {
                    break;
                }
            }
            w.drain_before((u64::MAX, u64::MAX))?;
            unsealed_tail = Some(unsealed(tx, prev.upper).await?);
        }
        Some(r) => {
            let rows = epoch_page(tx, r.epoch - 1, r.epoch, 1).await?;
            let at = FailurePoint::Epoch(r.epoch as u64);
            let Some(e) = rows.first().filter(|e| e.epoch == r.epoch) else {
                w.drain_before((u64::MAX, u64::MAX))?;
                return fail(at, VerifyProblem::RangeNotFound);
            };
            let inherited = if r.epoch == 1 {
                Some(GENESIS)
            } else {
                boundary_at(tx, r.epoch - 1, None).await?.boundary()
            };
            w.header(e, inherited)?;
            w.epochs = 1;
            let start = if r.first == 1 {
                inherited
            } else {
                match boundary_at(tx, r.epoch, Some(r.first - 1)).await? {
                    Found::At(b) => Some(b),
                    Found::Malformed => {
                        return fail(
                            FailurePoint::Seal {
                                epoch: r.epoch as u64,
                                batch_no: (r.first - 1) as u64,
                            },
                            VerifyProblem::SealMalformed,
                        );
                    }
                    Found::Absent => {
                        // A range wholly beyond the last seal is absent, not a broken predecessor.
                        if last_batch(tx, r.epoch).await?.is_none_or(|l| l < r.first) {
                            w.drain_before((u64::MAX, u64::MAX))?;
                            return fail(at, VerifyProblem::RangeNotFound);
                        }
                        None
                    }
                }
            };
            let Some(start) = start else {
                return fail(
                    FailurePoint::Seal {
                        epoch: r.epoch as u64,
                        batch_no: r.first as u64,
                    },
                    VerifyProblem::RangePredecessor,
                );
            };
            w.seals(
                tx,
                r.epoch,
                (r.first, r.last),
                start,
                Some(VerifyProblem::RangeNotFound),
            )
            .await?;
        }
    }
    Ok(report(
        witness,
        scope.range.is_some(),
        &w,
        unsealed_tail,
        head_empty,
    ))
}

fn report(
    witness: &WitnessEvidence,
    partial: bool,
    w: &Walk,
    unsealed: Option<TailSummary>,
    head_epoch_empty: bool,
) -> VerifyReport {
    let assurance = match (witness, w.frontier) {
        (WitnessEvidence::NotConfigured, _) => Assurance::NoWitness,
        (_, Some((epoch, batch_no))) => Assurance::AnchoredThrough { epoch, batch_no },
        (_, None) => Assurance::NoAnchors,
    };
    VerifyReport {
        partial,
        epochs: w.epochs,
        seals: w.seals,
        rows: w.rows,
        assurance,
        unanchored: w.tail,
        unsealed,
        head_epoch_empty,
    }
}
