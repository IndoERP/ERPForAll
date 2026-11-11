//! Admission budget for parser state (feature `budget`).
//!
//! A [`Budget`] is consulted before the tokenizer or tree builder retains more
//! data.  The first refusal is sticky: [`Budget::tripped`] keeps returning it.

use std::fmt;

/// What a refused request was about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BudgetKind {
    /// Bytes retained by the parser while it works.
    WorkingBytes,
    /// Units of work (for example duplicate-attribute comparisons).
    Work,
    /// Depth of the open-element stack.
    Depth,
    /// Number of tree nodes.
    Nodes,
    /// Internal accounting fault: more bytes were released than were charged.
    Accounting,
}

impl fmt::Display for BudgetKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            BudgetKind::WorkingBytes => "working bytes",
            BudgetKind::Work => "work",
            BudgetKind::Depth => "depth",
            BudgetKind::Nodes => "nodes",
            BudgetKind::Accounting => "accounting",
        })
    }
}

/// A refused admission request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct BudgetExceeded {
    /// Which budget refused.
    pub kind: BudgetKind,
    /// The amount (or depth) that was requested when the refusal happened.
    pub requested: u64,
    /// The limit that was in force.
    pub limit: u64,
}

impl BudgetExceeded {
    /// Build a refusal record.
    pub fn new(kind: BudgetKind, requested: u64, limit: u64) -> Self {
        BudgetExceeded {
            kind,
            requested,
            limit,
        }
    }
}

impl fmt::Display for BudgetExceeded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "parser budget exceeded: {} requested {}, limit {}",
            self.kind, self.requested, self.limit
        )
    }
}

impl std::error::Error for BudgetExceeded {}

/// Admission control consulted by the tokenizer and tree builder.
///
/// Implementations are shared (`Arc`) between the tokenizer and the tree
/// builder, so they use interior mutability.  Once any method refuses,
/// [`Budget::tripped`] must keep returning that first refusal.
pub trait Budget: Send + Sync {
    /// Admit `n` more retained bytes.
    fn admit_bytes(&self, n: usize) -> Result<(), BudgetExceeded>;
    /// Give back `n` previously admitted bytes.
    fn release_bytes(&self, n: usize);
    /// Admit `n` units of work.
    fn admit_work(&self, n: u64) -> Result<(), BudgetExceeded>;
    /// Admit an open-element stack of the given depth.
    fn admit_depth(&self, depth: usize) -> Result<(), BudgetExceeded>;
    /// Admit one more tree node.
    fn admit_node(&self) -> Result<(), BudgetExceeded>;
    /// The first refusal, if any (sticky).
    fn tripped(&self) -> Option<BudgetExceeded>;
}

/// A validated budget limit: non-zero and at most [`Limit::MAX`].
///
/// A zero or overflowing limit is a configuration error, never "unlimited".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limit(u64);

/// Why a [`Limit`] was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LimitError {
    /// The limit was zero.
    Zero,
    /// The limit is larger than [`Limit::MAX`], so sums of charges could overflow.
    Overflow {
        /// The refused value.
        value: u64,
    },
}

impl fmt::Display for LimitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LimitError::Zero => f.write_str("parser budget limit must be greater than zero"),
            LimitError::Overflow { value } => write!(
                f,
                "parser budget limit {} exceeds the maximum {}",
                value,
                Limit::MAX
            ),
        }
    }
}

impl std::error::Error for LimitError {}

impl Limit {
    /// Largest accepted limit: `i64::MAX`, so two limits can be added without overflow, and on
    /// targets with a narrower `usize` (wasm32) `usize::MAX - 1`, so a byte request that saturates
    /// `usize` is always above every limit and is refused rather than admitted.
    pub const MAX: u64 = if usize::BITS < 64 {
        (usize::MAX - 1) as u64
    } else {
        i64::MAX as u64
    };

    /// Validate a limit.
    pub fn new(value: u64) -> Result<Limit, LimitError> {
        match value {
            0 => Err(LimitError::Zero),
            v if v > Limit::MAX => Err(LimitError::Overflow { value: v }),
            v => Ok(Limit(v)),
        }
    }

    /// The validated value.
    pub fn get(self) -> u64 {
        self.0
    }
}
