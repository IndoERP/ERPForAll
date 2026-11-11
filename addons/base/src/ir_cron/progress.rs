//! The progress API of a scheduled action (3).
//!
//! Here the call is a chunk
//! boundary: it records how much was processed and how much remains, and the numbers commit
//! with the chunk's effects when the action returns, so a failing action rolls back its own
//! progress and its work together (is `divergent`, bug-free either way).

use std::time::{Duration, Instant};

use erp_sdk::Transaction;

/// What an action reported during the chunk that runs it. Lives in the chunk's transaction.
#[derive(Debug, Default)]
pub struct CronProgress {
    active: bool,
    done: u64,
    remaining: u64,
    deactivate: bool,
    end: Option<Instant>,
}

impl CronProgress {
    pub(crate) fn start(tx: &mut Transaction, end: Instant) {
        let state = tx.extension::<CronProgress>();
        *state = CronProgress {
            active: true,
            end: Some(end),
            ..CronProgress::default()
        };
    }

    /// `(done, remaining, deactivate)` reported so far.
    pub(crate) fn take(tx: &mut Transaction) -> (u64, u64, bool) {
        let state = tx.extension::<CronProgress>();
        let out = (state.done, state.remaining, state.deactivate);
        *state = CronProgress::default();
        out
    }
}

/// Report progress from a scheduled action: `processed` items were done, `remaining` are left
/// (`None` subtracts `processed` from the previous remaining count), `deactivate` asks the cron
/// to deactivate itself when the run ends.
///
/// ```
/// # let mut tx: Option<&mut erp_sdk::Transaction> = None;
/// # if let Some(tx) = tx.as_deref_mut() {
/// let left = erp_addon_base::ir_cron::commit_progress(tx, 10, Some(90), false);
/// assert!(left > std::time::Duration::ZERO);
/// # }
/// ```
pub fn commit_progress(
    tx: &mut Transaction,
    processed: u64,
    remaining: Option<u64>,
    deactivate: bool,
) -> Duration {
    let state = tx.extension::<CronProgress>();
    if !state.active {
        return Duration::MAX;
    }
    state.done = state.done.saturating_add(processed);
    state.remaining = remaining.unwrap_or_else(|| state.remaining.saturating_sub(processed));
    state.deactivate |= deactivate;
    state.end.map_or(Duration::MAX, |end| {
        end.saturating_duration_since(Instant::now())
    })
}
