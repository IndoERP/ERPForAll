//! Allocation counting for tests: assert how many heap allocations and bytes an operation costs
//! (standards §5b, "allocation budgets are tested").
//!
//! A crate of its own (std, `dhat` and `thiserror` only) so any crate can dev-depend on it without
//! a dependency cycle; `erp_testkit::alloc` re-exports it for tests that already use the test kit.
//!
//! The counting is done by the [`dhat`] crate's allocator in testing mode, so no `unsafe` is
//! needed in ERPForAll code. A global allocator is chosen per binary, so the test binary installs
//! it with one line, then calls [`measure`]:
//!
//! ```ignore
//! // tests/alloc_domain.rs, declared `harness = false` in Cargo.toml (see "One profiler")
//! use erp_alloc_budget::{self as alloc, Budget};
//!
//! #[global_allocator]
//! static ALLOC: alloc::Alloc = alloc::Alloc;
//!
//! alloc::harness!(parsing_a_domain_stays_within_budget);
//!
//! fn parsing_a_domain_stays_within_budget() {
//!     let json = serde_json::json!([["name", "=", "x"]]);
//!     let (stats, domain) = alloc::measure(|| erp_domain::Domain::from_json(&json)).unwrap();
//!     domain.unwrap();
//!     stats.assert_within(&Budget::new(40, 4096));
//! }
//! ```
//!
//! # What is counted
//!
//! Every allocation of the whole process between the start and the end of the closure, **all
//! threads included**, as the difference of two snapshots. The value the closure returns stays alive
//! while the second snapshot is taken, so [`AllocStats::end_blocks`] and [`AllocStats::end_bytes`]
//! show what the operation retains. Allocations of the measuring code itself are ignored by dhat.
//!
//! Counting every thread is right only when every thread belongs to the test: the code under test
//! may allocate on threads it starts (a server under test, a runtime's workers), and those
//! allocations are part of its cost. So [`measure`] runs only on the main thread of a binary that
//! uses [`harness`](mod@crate::harness) (one test per process, on the main thread, no harness
//! thread of its own): in a process without it, it returns [`AllocError::NoHarness`], and on
//! another thread of a harness process [`AllocError::OffHarnessThread`]. Under libtest the main
//! thread keeps working after it spawned the test's thread and, on a loaded host, allocated inside
//! the window (lesson).
//!
//! # One profiler
//!
//! dhat keeps one profiler for the whole process and counts every thread, so:
//!
//! - Allocation tests live in their own `tests/alloc_*.rs` binaries declared `harness = false`
//!   (`[[test]] name = "alloc_x" harness = false` in the crate's `Cargo.toml`) that list their
//!   tests with [`harness!`](macro@crate::harness). Every test runs in a process of its own:
//!   `cargo nextest` (what `scripts/ci.sh` runs) starts one per test, and the harness starts one
//!   per test itself when a run (`cargo test`) selects several.
//! - Several sequential calls of [`measure`] in one test are fine and are how a per-row cost is
//!   found: measure with N rows and with 2N rows and divide the difference. A call inside another
//!   one's closure is refused ([`AllocError::Nested`]): the windows would overlap, and dhat panics
//!   on a second profiler.
//! - A `dhat::Profiler` started outside [`measure`] in the same binary (by the test itself) makes dhat
//!   panic when [`measure`] starts its own. [`measure`] catches that and returns
//!   [`AllocError::ProfilerAlreadyRunning`]; dhat's panic message still prints on stderr. Do not start
//!   profilers yourself in a binary that uses [`measure`].
//! - Without `#[global_allocator] static A: erp_alloc_budget::Alloc = erp_alloc_budget::Alloc;`
//!   in the binary nothing is counted; [`measure`] detects that with a probe allocation and returns
//!   [`AllocError::AllocatorNotInstalled`] rather than a zero.
//!
//! dhat records a backtrace per allocation, so a measured section runs far slower than normal:
//! never time anything inside [`measure`].

pub mod harness;

use std::fmt;
use std::sync::{Mutex, TryLockError};

pub use dhat::Alloc;

/// Held while a profiler runs: dhat panics when a second one starts while one runs, so a nested
/// [`measure`] is refused ([`AllocError::Nested`]).
static PROFILER_LOCK: Mutex<()> = Mutex::new(());

/// Why an allocation measurement could not be made.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AllocError {
    /// [`measure`] was called in a process where [`harness`](mod@crate::harness) does not run:
    /// under libtest, or in a binary without `harness!`. libtest's main thread allocates beside
    /// the test, right after it spawns the test's thread: the binary is wired wrong.
    #[error(
        "measure() counts every thread, so it runs only in a test binary declared \
         `harness = false` that lists its tests with erp_alloc_budget::harness!; this process \
         runs no such harness (under libtest, a harness thread could allocate inside the window)"
    )]
    NoHarness,
    /// [`measure`] was called on a thread of the test's own while the harness runs its test on
    /// the main thread: the test must call it from its own body, not from a worker or a spawned
    /// thread.
    #[error(
        "measure() was called on a thread the test started; call it from the test function \
         itself, which the harness runs on the main thread"
    )]
    OffHarnessThread,
    /// [`measure`] was called inside the closure of another [`measure`]: the windows would
    /// overlap.
    #[error(
        "measure() was called inside another measure(); measure the two parts one after the other"
    )]
    Nested,
    /// The test binary has no `#[global_allocator]` of type [`Alloc`]: a probe allocation was not
    /// counted.
    #[error(
        "no allocation was counted: the test binary must declare \
         `#[global_allocator] static ALLOC: erp_alloc_budget::Alloc = erp_alloc_budget::Alloc;`"
    )]
    AllocatorNotInstalled,
    /// A `dhat::Profiler` that this module did not start is already running in the process (a test
    /// started its own, or a `dhat-heap` build). dhat panics when a second profiler starts; the panic
    /// message still appears on stderr, but [`measure`] turns the failure into this error.
    #[error("another dhat::Profiler is already running in this process; measure() needs to own it")]
    ProfilerAlreadyRunning,
    /// The allocator reported figures that cannot be (a counter went backwards).
    #[error("inconsistent allocator statistics: {0}")]
    Inconsistent(&'static str),
}

/// Heap activity of one measured closure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct AllocStats {
    /// Allocations made (a `realloc` that moves counts as one).
    pub total_blocks: u64,
    /// Bytes requested by those allocations.
    pub total_bytes: u64,
    /// Most bytes live at once above what was live before the closure.
    pub peak_bytes: u64,
    /// Blocks still live after the closure returned (its result included).
    pub end_blocks: u64,
    /// Bytes still live after the closure returned.
    pub end_bytes: u64,
}

/// Upper bounds for [`AllocStats::assert_within`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Budget {
    /// Most allocations allowed.
    pub max_blocks: u64,
    /// Most bytes allowed.
    pub max_bytes: u64,
}

impl Budget {
    /// A budget of at most `max_blocks` allocations and `max_bytes` bytes.
    #[must_use]
    pub const fn new(max_blocks: u64, max_bytes: u64) -> Self {
        Self {
            max_blocks,
            max_bytes,
        }
    }

    /// A budget of `rows` rows at `blocks_per_row` allocations and `bytes_per_row` bytes each, plus
    /// a fixed part for the operation itself. Overflow is an error, not a wrapped number.
    ///
    /// # Errors
    /// The product or the sum does not fit `u64`.
    pub fn per_row(
        rows: u64,
        (blocks_per_row, bytes_per_row): (u64, u64),
        fixed: Budget,
    ) -> Result<Self, AllocError> {
        let overflow = || AllocError::Inconsistent("budget overflows u64");
        let total = |per_row: u64, fixed: u64| {
            rows.checked_mul(per_row)
                .and_then(|n| n.checked_add(fixed))
                .ok_or_else(overflow)
        };
        Ok(Self {
            max_blocks: total(blocks_per_row, fixed.max_blocks)?,
            max_bytes: total(bytes_per_row, fixed.max_bytes)?,
        })
    }
}

/// A budget that was exceeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverBudget {
    /// What was measured.
    pub stats: AllocStats,
    /// What was allowed.
    pub budget: Budget,
}

impl fmt::Display for OverBudget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "allocation budget exceeded: {} blocks (budget {}), {} bytes (budget {}); \
             peak {} bytes, {} blocks / {} bytes still live",
            self.stats.total_blocks,
            self.budget.max_blocks,
            self.stats.total_bytes,
            self.budget.max_bytes,
            self.stats.peak_bytes,
            self.stats.end_blocks,
            self.stats.end_bytes
        )
    }
}

impl std::error::Error for OverBudget {}

impl AllocStats {
    /// Whether the measured activity is within `budget`.
    ///
    /// # Errors
    /// More blocks or more bytes than the budget allows; the error carries both figures.
    pub fn check(&self, budget: &Budget) -> Result<(), OverBudget> {
        if self.total_blocks > budget.max_blocks || self.total_bytes > budget.max_bytes {
            return Err(OverBudget {
                stats: *self,
                budget: *budget,
            });
        }
        Ok(())
    }

    /// [`AllocStats::check`] for tests: panics with the measured and the allowed figures.
    ///
    /// # Panics
    /// The budget is exceeded.
    #[track_caller]
    #[allow(
        clippy::panic,
        reason = "an assertion helper for tests: failing the test is its job"
    )]
    pub fn assert_within(&self, budget: &Budget) {
        if let Err(over) = self.check(budget) {
            panic!("{over}");
        }
    }
}

fn delta(after: u64, before: u64, what: &'static str) -> Result<u64, AllocError> {
    after
        .checked_sub(before)
        .ok_or(AllocError::Inconsistent(what))
}

fn to_u64(value: usize, what: &'static str) -> Result<u64, AllocError> {
    u64::try_from(value).map_err(|_| AllocError::Inconsistent(what))
}

/// Runs `work` under a testing-mode heap profiler and returns what it allocated, with its result.
///
/// See the module documentation for what is counted and why each test runs in a process of its
/// own.
///
/// # Errors
/// [`AllocError::NoHarness`] in a process without [`harness`](mod@crate::harness),
/// [`AllocError::OffHarnessThread`] on any thread but the one the harness runs the test on,
/// [`AllocError::Nested`] inside another [`measure`],
/// [`AllocError::AllocatorNotInstalled`] when the binary does not use [`Alloc`] as its global
/// allocator, [`AllocError::Inconsistent`] when dhat's counters contradict each other.
pub fn measure<R>(work: impl FnOnce() -> R) -> Result<(AllocStats, R), AllocError> {
    match harness::standing() {
        harness::Standing::HarnessThread => {}
        harness::Standing::NoHarness => return Err(AllocError::NoHarness),
        harness::Standing::OtherThread => return Err(AllocError::OffHarnessThread),
    }
    // Only the harness thread gets here, so the lock is held only by an enclosing `measure` on
    // this same thread: re-entry is refused at once instead of waiting forever. A panic inside an
    // earlier `work` only poisons the lock; the profiler was dropped while unwinding, so the
    // state behind the lock is fine.
    let _guard = match PROFILER_LOCK.try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(TryLockError::WouldBlock) => return Err(AllocError::Nested),
    };
    // dhat panics when a profiler already runs; the panic happens before any state changes, so the
    // failure can be reported as an error.
    let _profiler = std::panic::catch_unwind(|| dhat::Profiler::builder().testing().build())
        .map_err(|_| AllocError::ProfilerAlreadyRunning)?;

    // The probe proves the allocator is installed: an uncounted block would otherwise read as
    // "this operation allocates nothing".
    let probe_before = dhat::HeapStats::get();
    let probe = std::hint::black_box(Box::new(0u64));
    let probe_after = dhat::HeapStats::get();
    drop(probe);
    if probe_after.total_blocks == probe_before.total_blocks {
        return Err(AllocError::AllocatorNotInstalled);
    }

    let before = dhat::HeapStats::get();
    let result = work();
    let after = dhat::HeapStats::get();

    let stats = AllocStats {
        total_blocks: delta(
            after.total_blocks,
            before.total_blocks,
            "blocks went backwards",
        )?,
        total_bytes: delta(
            after.total_bytes,
            before.total_bytes,
            "bytes went backwards",
        )?,
        peak_bytes: delta(
            to_u64(after.max_bytes, "peak bytes")?,
            to_u64(before.curr_bytes, "live bytes")?,
            "peak below the live bytes at the start",
        )?,
        end_blocks: delta(
            to_u64(after.curr_blocks, "live blocks")?,
            to_u64(before.curr_blocks, "live blocks")?,
            "live blocks below the start",
        )?,
        end_bytes: delta(
            to_u64(after.curr_bytes, "live bytes")?,
            to_u64(before.curr_bytes, "live bytes")?,
            "live bytes below the start",
        )?,
    };
    Ok((stats, result))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod tests {
    use super::*;

    fn stats(blocks: u64, bytes: u64) -> AllocStats {
        AllocStats {
            total_blocks: blocks,
            total_bytes: bytes,
            peak_bytes: bytes / 2,
            end_blocks: 1,
            end_bytes: 8,
        }
    }

    #[test]
    fn a_budget_is_inclusive_on_both_figures() {
        let budget = Budget::new(10, 1000);
        assert_eq!(stats(10, 1000).check(&budget), Ok(()));
        assert_eq!(stats(0, 0).check(&budget), Ok(()));
        for over in [stats(11, 1000), stats(10, 1001)] {
            assert_eq!(
                over.check(&budget),
                Err(OverBudget {
                    stats: over,
                    budget
                })
            );
        }
    }

    #[test]
    fn an_exceeded_budget_names_every_figure() {
        let over = stats(11, 1500).check(&Budget::new(10, 1000)).unwrap_err();
        assert_eq!(
            over.to_string(),
            "allocation budget exceeded: 11 blocks (budget 10), 1500 bytes (budget 1000); \
             peak 750 bytes, 1 blocks / 8 bytes still live"
        );
    }

    #[test]
    #[should_panic(
        expected = "allocation budget exceeded: 3 blocks (budget 2), 9 bytes (budget 9)"
    )]
    fn assert_within_panics_with_the_message() {
        stats(3, 9).assert_within(&Budget::new(2, 9));
    }

    #[test]
    fn measure_refuses_to_count_under_libtest() {
        // this test runs under libtest, on the thread libtest spawned for it
        assert_eq!(measure(|| ()).unwrap_err(), AllocError::NoHarness);
        assert_eq!(
            AllocError::NoHarness.to_string(),
            "measure() counts every thread, so it runs only in a test binary declared \
             `harness = false` that lists its tests with erp_alloc_budget::harness!; this process \
             runs no such harness (under libtest, a harness thread could allocate inside the \
             window)"
        );
    }

    #[test]
    fn per_row_budgets_add_the_fixed_part_and_refuse_overflow() {
        let budget = Budget::per_row(100, (3, 64), Budget::new(10, 512)).unwrap();
        assert_eq!(budget, Budget::new(310, 6912));
        assert_eq!(
            Budget::per_row(u64::MAX, (2, 1), Budget::new(0, 0)),
            Err(AllocError::Inconsistent("budget overflows u64"))
        );
        assert_eq!(
            Budget::per_row(1, (u64::MAX, 0), Budget::new(1, 0)),
            Err(AllocError::Inconsistent("budget overflows u64"))
        );
    }
}
