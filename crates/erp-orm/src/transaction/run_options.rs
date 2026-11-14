// SPDX-License-Identifier: MIT OR Apache-2.0
//! Explicit native whole-unit policy.
use std::fmt;
use std::time::Duration;

use erp_fields::{Condition, Error, Result};

use super::{BeginOptions, OrmConfig, is_retryable};

/// A whole-unit retry budget. The 1–5 attempt and 10-second backoff ceilings are core hard
/// safety bounds, not operator defaults. Addons declare their configurable policies separately.
///
/// ```
/// use erp_orm::{BeginOptions, RetryPolicy, RunOptions, SUPERUSER_ID};
/// use std::time::Duration;
/// let policy = RetryPolicy::new(3, Duration::from_millis(50), Duration::from_secs(2))?;
/// let options = RunOptions::new(BeginOptions::new(SUPERUSER_ID), policy)
///     .retry_lock_timeouts()
///     .lock_timeout(Duration::from_secs(1));
/// # Ok::<(), erp_fields::Error>(())
/// ```
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct RetryPolicy {
    pub(super) attempts: u32,
    base: Duration,
    cap: Duration,
}

impl RetryPolicy {
    /// Validate a budget: 1–5 attempts and positive whole-millisecond backoffs, base <= cap,
    /// each at most 10 seconds. Zero never means unlimited.
    /// # Errors
    /// `Programming` naming retry policy when a hard safety bound is violated.
    pub fn new(attempts: u32, base: Duration, cap: Duration) -> Result<Self> {
        let valid_delay = |value: Duration| {
            !value.is_zero()
                && value <= Duration::from_secs(10)
                && value.subsec_nanos().is_multiple_of(1_000_000)
        };
        if !(1..=5).contains(&attempts) || !valid_delay(base) || !valid_delay(cap) || base > cap {
            return Err(Error::programming(
                "retry policy requires 1 to 5 attempts and positive whole-millisecond backoffs up to 10000 ms, with base <= cap.",
            ));
        }
        Ok(Self {
            attempts,
            base,
            cap,
        })
    }

    // The legacy entry retains its existing node configuration (U1).
    pub(super) fn legacy(config: &OrmConfig) -> Self {
        Self {
            attempts: config.max_attempts,
            base: config.retry_base,
            cap: config.retry_cap,
        }
    }

    pub(super) fn delay(&self, attempt: u32) -> Duration {
        let exp = self.base.saturating_mul(1u32 << (attempt - 1).min(16));
        let bound = exp.min(self.cap).as_millis() as u64;
        Duration::from_millis(super::jitter(bound))
    }
}

/// Explicit transaction options for [`super::Orm::run_with_options`]. This grants no authority:
/// principal resolution, transaction intent and isolation follow its [`BeginOptions`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RunOptions {
    pub(super) begin: BeginOptions,
    pub(super) retry: RetryPolicy,
    lock: Option<Duration>,
    lock_timeouts: bool,
    top_level: Option<&'static str>,
    pub(super) classified_commit: bool,
}

impl RunOptions {
    /// Use the given transaction options and validated retry policy.
    pub fn new(begin: BeginOptions, retry: RetryPolicy) -> Self {
        Self {
            begin,
            retry,
            lock: None,
            lock_timeouts: false,
            top_level: None,
            classified_commit: true,
        }
    }

    pub(super) fn legacy(begin: BeginOptions, retry: RetryPolicy) -> Self {
        Self {
            classified_commit: false,
            ..Self::new(begin, retry)
        }
    }

    /// Additionally retry only errors carrying both `55P03` and `Condition::LockTimeout`.
    /// A `NOWAIT` refusal with the same SQLSTATE remains terminal.
    #[must_use]
    pub fn retry_lock_timeouts(mut self) -> Self {
        self.lock_timeouts = true;
        self
    }

    /// Override only the lock limit. The runner validates its ordering against the effective
    /// statement/transaction limits before opening an attempt; it never silently clamps it.
    #[must_use]
    pub fn lock_timeout(mut self, lock: Duration) -> Self {
        self.lock = Some(lock);
        self
    }

    /// Require a top-level native unit. Refuse an outer ORM/RPC unit by this entry's name.
    ///
    /// The check is [`require_top_level_unit`]'s: it sees the task-local unit scope only. Work
    /// spawned from inside a unit must start with `spawn_in_unit_of_work` so the scope follows it,
    /// and a caller holding its own `begin_opts` transaction outside a unit is not detected.
    #[must_use]
    pub fn top_level(mut self, entry: &'static str) -> Self {
        self.top_level = Some(entry);
        self
    }

    pub(super) fn prepare(mut self, defaults: crate::Timeouts) -> Result<Self> {
        if let Some(entry) = self.top_level {
            require_top_level_unit(entry)?;
        }
        if let Some(lock) = self.lock {
            // BeginOptions explicitly specifies inheritance when timeouts are absent.
            let old = self.begin.timeouts.unwrap_or(defaults);
            self.begin = self.begin.timeouts(crate::Timeouts::new(
                lock,
                old.statement(),
                old.transaction(),
            )?);
        }
        Ok(self)
    }

    pub(super) fn accepts(&self, error: &Error) -> bool {
        is_retryable(error)
            || (self.lock_timeouts
                && error.sqlstate() == Some("55P03")
                && error.condition() == Some(Condition::LockTimeout))
    }
}

/// Refuse a nested whole-unit entry, including an RPC-owned transaction, before a policy read
/// or any business work. RPC scopes and `Orm::run` use the same task-local unit boundary.
///
/// The guard covers that task-local unit scope only. A task started with `tokio::spawn` does not
/// inherit it, so work spawned from inside a unit (an RPC handler, a hook) must be started with
/// `spawn_in_unit_of_work`; a native caller that holds its own `begin_opts` transaction outside
/// any unit is not seen either, and must not call a top-level entry while that transaction is open.
/// # Errors
/// `Programming` naming `entry` if another unit is active, or if its name is empty.
pub fn require_top_level_unit(entry: &'static str) -> Result<()> {
    if entry.is_empty() {
        return Err(Error::programming(
            "A top-level unit requires an entry name.",
        ));
    }
    if crate::route::current_unit().is_some() {
        return Err(Error::programming(format!(
            "{entry} cannot run inside another unit of work (including an RPC transaction)."
        )));
    }
    Ok(())
}

/// A failed invocation: its original error, actual attempts and whether eligible retries ran
/// out. A pre-attempt validation refusal has zero attempts; unrelated errors are not exhausted.
#[derive(Debug)]
#[non_exhaustive]
pub struct RunFailure {
    error: Error,
    attempts: u32,
    exhausted: bool,
}

impl RunFailure {
    pub(super) fn new(error: Error, attempts: u32, exhausted: bool) -> Self {
        Self {
            error,
            attempts,
            exhausted,
        }
    }
    /// The underlying error, including SQLSTATE, condition and rollback diagnostics.
    pub fn error(&self) -> &Error {
        &self.error
    }
    /// Attempts entered, including a refused opening (zero for options rejected before work).
    pub fn attempts(&self) -> u32 {
        self.attempts
    }
    /// Whether this failure exhausted the eligible retry budget.
    pub fn exhausted(&self) -> bool {
        self.exhausted
    }
    /// Preserve the underlying error for a caller that does not need retry metadata.
    pub fn into_error(self) -> Error {
        self.error
    }
}

// The cause is reached through `source()`, so the message does not repeat it (a chained printer
// would show it twice).
impl fmt::Display for RunFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.exhausted {
            write!(
                f,
                "unit of work failed after {} attempt(s); retries exhausted",
                self.attempts
            )
        } else {
            write!(f, "unit of work failed after {} attempt(s)", self.attempts)
        }
    }
}
impl std::error::Error for RunFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}
