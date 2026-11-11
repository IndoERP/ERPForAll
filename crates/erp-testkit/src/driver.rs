//! The PostgreSQL driver's per-batch heap blocks, as allocation tests take them out of what they
//! measure (amendment A3, lesson).
//!
//! The driver delivers a result in batches of response messages, and how many depends on how much
//! the server had sent each time the connection read: the count grows on a loaded host and
//! differs between two runs of the same read. Per delivered batch the driver allocates the
//! response channel's node ([`DRIVER_MIN_BLOCKS_PER_BATCH`]); a delivery that finds the channel
//! holding a batch already also parks the connection, which allocates one more node (up to
//! [`DRIVER_MAX_BLOCKS_PER_BATCH`]). The driver counts both
//! (`erp_orm::Transaction::driver_response_batches` or an isolated process reading from
//! `erp_orm::ResponseBatches::process_wide`), so [`DriverBlocks`] is exact, with no
//! slack for a loaded host to widen: a test takes it out of what it measures, and what is left is
//! the cost of everything else.
//!
//! The parked count is exact only when the connection and the reader run on a current-thread
//! runtime, so [`DriverBlocks::new`] refuses any other ([`DriverCountError::NotCurrentThread`]).
//!
//! The other driver blocks of a read, the read buffer and its shared header, come once per 8 KiB
//! received whatever the batch sizes. The per-batch
//! term goes to 0 with PERF.STREAM-RAW-BATCH.

use erp_orm::ResponseBatches;
use tokio::runtime::{Handle, RuntimeFlavor};

/// Heap blocks the driver allocates for a delivered batch that parks the connection: the
/// response channel's node and the parked-sender node. Pinned by
/// `crates/erp-test-models/tests/alloc_driver_batches.rs`.
pub const DRIVER_MAX_BLOCKS_PER_BATCH: u64 = 2;

/// Heap blocks the driver allocates for a delivered batch that does not park the connection: the
/// response channel's node. Pinned by `crates/erp-test-models/tests/alloc_driver_batches.rs`.
pub const DRIVER_MIN_BLOCKS_PER_BATCH: u64 = 1;

/// Why the driver's blocks of a window cannot be given exactly.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DriverCountError {
    /// The runtime is not current-thread: a reader on another worker can take a batch between
    /// the driver's delivery and its check, hiding a park, so the parked count can be low.
    #[error(
        "the driver's parked batches are counted exactly only on a current-thread runtime; \
         this runtime is {flavor}: build the test's runtime with \
         tokio::runtime::Builder::new_current_thread()"
    )]
    NotCurrentThread {
        /// The runtime's flavor, as tokio names it.
        flavor: String,
    },
    /// The window has more parked batches than delivered ones, which one connection never shows:
    /// a reading was taken while a request was being answered (the two counts are read one after
    /// the other), so the window is not one the test can measure against.
    #[error(
        "the window counts {parked} parked batches but only {delivered} delivered: read the \
         driver's counts between requests, not while one is being answered"
    )]
    ParkedAboveDelivered {
        /// Batches delivered in the window.
        delivered: u64,
        /// Batches counted as parked in the window.
        parked: u64,
    },
}

/// The driver's per-batch blocks in a window of one connection or an isolated process, exact: one per delivered batch,
/// one more per batch that parked the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriverBlocks {
    delivered: u64,
    parked: u64,
}

#[allow(
    clippy::cast_precision_loss,
    reason = "batch counts are far below 2^53, where f64 is exact"
)]
impl DriverBlocks {
    /// The blocks of `window` (a [`ResponseBatches::since`] difference), counted on `runtime`,
    /// the current-thread runtime that ran the connection and the reader.
    ///
    /// # Errors
    /// [`DriverCountError::NotCurrentThread`] for any other runtime;
    /// [`DriverCountError::ParkedAboveDelivered`] for a window read in the middle of a request.
    pub fn new(runtime: &Handle, window: ResponseBatches) -> Result<Self, DriverCountError> {
        require_current_thread(runtime)?;
        Self::from_counts(window.delivered(), window.parked())
    }

    fn from_counts(delivered: u64, parked: u64) -> Result<Self, DriverCountError> {
        if parked > delivered {
            return Err(DriverCountError::ParkedAboveDelivered { delivered, parked });
        }
        Ok(Self { delivered, parked })
    }

    /// Batches delivered in the window.
    #[must_use]
    pub fn delivered(&self) -> u64 {
        self.delivered
    }

    /// Of those, the batches that parked the connection.
    #[must_use]
    pub fn parked(&self) -> u64 {
        self.parked
    }

    /// The driver's per-batch blocks in the window.
    #[must_use]
    pub fn blocks(&self) -> f64 {
        let parking = DRIVER_MAX_BLOCKS_PER_BATCH - DRIVER_MIN_BLOCKS_PER_BATCH;
        self.delivered as f64 * DRIVER_MIN_BLOCKS_PER_BATCH as f64
            + self.parked as f64 * parking as f64
    }

    /// The blocks this window has more than `earlier` (fewer: negative).
    #[must_use]
    pub fn more_than(&self, earlier: &DriverBlocks) -> f64 {
        self.blocks() - earlier.blocks()
    }
}

impl std::fmt::Display for DriverBlocks {
    /// `40 batches (15 parked)`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} batches ({} parked)", self.delivered, self.parked)
    }
}

fn require_current_thread(runtime: &Handle) -> Result<(), DriverCountError> {
    match runtime.runtime_flavor() {
        RuntimeFlavor::CurrentThread => Ok(()),
        other => Err(DriverCountError::NotCurrentThread {
            flavor: format!("{other:?}"),
        }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod tests {
    use super::*;

    #[test]
    fn one_block_per_delivered_batch_and_one_more_per_parked_one() {
        let run = DriverBlocks::from_counts(40, 15).unwrap();
        assert_eq!((run.delivered(), run.parked()), (40, 15));
        assert_eq!(run.blocks(), 55.0);
        let other = DriverBlocks::from_counts(30, 20).unwrap();
        assert_eq!(run.more_than(&other), 5.0);
        assert_eq!(other.more_than(&run), -5.0);
        assert_eq!(DriverBlocks::from_counts(0, 0).unwrap().blocks(), 0.0);
        assert_eq!(run.to_string(), "40 batches (15 parked)");
    }

    #[test]
    fn a_window_with_more_parked_than_delivered_batches_is_refused() {
        let refused = DriverBlocks::from_counts(3, 4).unwrap_err();
        assert_eq!(
            refused,
            DriverCountError::ParkedAboveDelivered {
                delivered: 3,
                parked: 4
            }
        );
        assert!(refused.to_string().contains("between requests"));
        assert!(DriverBlocks::from_counts(4, 4).is_ok());
    }

    #[test]
    fn only_a_current_thread_runtime_is_accepted() {
        let current = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        assert_eq!(require_current_thread(current.handle()), Ok(()));
        let multi = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .unwrap();
        let refused = require_current_thread(multi.handle()).unwrap_err();
        assert_eq!(
            refused,
            DriverCountError::NotCurrentThread {
                flavor: "MultiThread".to_owned()
            }
        );
        assert!(refused.to_string().contains("new_current_thread"));
    }
}
