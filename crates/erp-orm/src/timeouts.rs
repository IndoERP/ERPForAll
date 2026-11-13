//! Per-transaction time limits ("Timeouts").
//!
//! Every transaction sets `lock_timeout` and `statement_timeout` as `SET LOCAL` in the same
//! message as `BEGIN`, and `transaction_timeout` as a backstop well above them. The backstop ends
//! the *session* when it fires (`25P04`), which drops the pooled connection and its prepared
//! statements, so the two ordinary limits are meant to fire first.
//!
//! The ORM owns the limits of each unit of work: they replace whatever a role, a database, a
//! pooler or the pool's `options` configured (the effective values are logged once when the ORM
//! is built). Classifying a server error as one of the limits does not read the server's message
//! text (it is translated with `lc_messages`): it uses the SQLSTATE, the `routine` field of the
//! error (never translated), the SQL the ORM sent and the time it measured itself.

use std::time::{Duration, Instant};

use erp_fields::{Condition, Error, ErrorKind, Result};

/// PostgreSQL stores these settings as an `int` of milliseconds.
const MAX_MS: u128 = i32::MAX as u128;

/// The `routine` the server reports for a lock timeout, a statement timeout and a user cancel
/// (`ProcessInterrupts` in `tcop/postgres.c`); a `NOWAIT` refusal comes from the lock and heap
/// routines. The field is not translated, so it holds in any `lc_messages`.
pub(crate) const INTERRUPT_ROUTINE: &str = "ProcessInterrupts";

/// The three limits of a transaction, validated: whole milliseconds, each above zero and within
/// PostgreSQL's range, `lock` strictly below `statement` (a lock limit at or above the statement
/// limit could never fire) and `statement` strictly below the `transaction` backstop.
///
/// ```
/// use std::time::Duration;
/// use erp_orm::Timeouts;
/// let timeouts = Timeouts::new(
///     Duration::from_secs(5),
///     Duration::from_secs(30),
///     Duration::from_secs(600),
/// )?;
/// assert_eq!(timeouts.statement(), Duration::from_secs(30));
/// // a backstop that is not above the statement limit is refused, never adjusted
/// assert!(Timeouts::new(
///     Duration::from_secs(5),
///     Duration::from_secs(30),
///     Duration::from_secs(30),
/// ).is_err());
/// # Ok::<(), erp_fields::Error>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Timeouts {
    lock: Duration,
    statement: Duration,
    transaction: Duration,
}

impl Default for Timeouts {
    /// Lock waits 10 s, statements 5 min, the backstop 1 h: generous enough for installs and
    /// bulk work, which set their own limits; servers and jobs choose theirs per unit of work.
    fn default() -> Self {
        Self {
            lock: Duration::from_secs(10),
            statement: Duration::from_secs(300),
            transaction: Duration::from_secs(3600),
        }
    }
}

fn whole_ms(name: &str, value: Duration) -> Result<u128> {
    if !value.subsec_nanos().is_multiple_of(1_000_000) {
        return Err(Error::programming(format!(
            "{name} must be a whole number of milliseconds, got {value:?}"
        )));
    }
    let ms = value.as_millis();
    if ms == 0 {
        return Err(Error::programming(format!(
            "{name} must be positive (0 would mean no limit in PostgreSQL), got {value:?}"
        )));
    }
    if ms > MAX_MS {
        return Err(Error::programming(format!(
            "{name} is {ms} ms; PostgreSQL accepts at most {MAX_MS} ms"
        )));
    }
    Ok(ms)
}

impl Timeouts {
    /// Validate and build the limits.
    ///
    /// # Errors
    ///
    /// `Programming` naming the limit when one is zero, not a whole number of milliseconds or
    /// above PostgreSQL's range, or when `lock` is not below `statement` or `statement` is not
    /// below `transaction`.
    pub fn new(lock: Duration, statement: Duration, transaction: Duration) -> Result<Self> {
        let lock_ms = whole_ms("lock_timeout", lock)?;
        let statement_ms = whole_ms("statement_timeout", statement)?;
        let transaction_ms = whole_ms("transaction_timeout", transaction)?;
        if lock_ms >= statement_ms {
            return Err(Error::programming(format!(
                "lock_timeout ({lock_ms} ms) must be below statement_timeout ({statement_ms} ms): \
                 the statement limit would always fire first and the lock limit could never apply"
            )));
        }
        if statement_ms >= transaction_ms {
            return Err(Error::programming(format!(
                "statement_timeout ({statement_ms} ms) must be below the transaction_timeout \
                 backstop ({transaction_ms} ms): the backstop ends the session and drops the \
                 connection, the statement limit only cancels the statement"
            )));
        }
        Ok(Self {
            lock,
            statement,
            transaction,
        })
    }

    /// `lock_timeout`.
    #[must_use]
    pub fn lock(&self) -> Duration {
        self.lock
    }

    /// `statement_timeout`.
    #[must_use]
    pub fn statement(&self) -> Duration {
        self.statement
    }

    /// The `transaction_timeout` backstop.
    #[must_use]
    pub fn transaction(&self) -> Duration {
        self.transaction
    }

    /// The `SET LOCAL` statements, to send right after `BEGIN` in the same message. The values
    /// are integers of milliseconds produced by [`Timeouts::new`], never user text.
    ///
    /// `transaction_timeout` is set to 0 first and then to its value. PostgreSQL arms the timer
    /// at the start of the transaction from the session's value, and a positive `SET LOCAL`
    /// while the timer is already running is ignored (checked on PostgreSQL 18, test
    /// `a_role_default_for_transaction_timeout_does_not_replace_the_backstop`): with a role or
    /// database default of, say, 30 s the backstop would stay at 30 s. Setting 0 stops that timer
    /// and the second `SET LOCAL` starts one with the ORM's value.
    pub(crate) fn set_local_sql(&self) -> String {
        format!(
            "SET LOCAL lock_timeout = {}; SET LOCAL statement_timeout = {}; \
             SET LOCAL transaction_timeout = 0; SET LOCAL transaction_timeout = {}",
            self.lock.as_millis(),
            self.statement.as_millis(),
            self.transaction.as_millis()
        )
    }

    /// The typed error for a server error that is one of the three limits firing, `None` for
    /// any other. The server's message text is not read (it depends on `lc_messages`).
    ///
    /// - `55P03` is a lock timeout when the `routine` is [`INTERRUPT_ROUTINE`]; immediate `NOWAIT`
    ///   row refusals come from heap routines (e.g. `heap_lock_tuple`) without an interrupt and
    ///   keep their own error.
    /// - `57014` is a statement timeout when the `routine` is [`INTERRUPT_ROUTINE`] and the time
    ///   the ORM measured since it sent the statement reached the smallest statement limit that
    ///   was in force; the client always sees at least the server's time, so a timeout is never
    ///   missed. The ORM never sends a cancel request, so a `57014` before the limit is someone
    ///   else's `pg_cancel_backend` and keeps its own error.
    /// - `25P04` is the code of the backstop alone.
    pub(crate) fn classify(&self, failure: &Failure<'_>, watch: &LimitWatch) -> Option<Timeout> {
        let interrupt = failure.routine == Some(INTERRUPT_ROUTINE);
        let (condition, limit, name) = match failure.sqlstate {
            "55P03" if interrupt => (Condition::LockTimeout, self.lock, "lock_timeout"),
            "57014"
                if interrupt
                    && failure
                        .elapsed
                        .is_some_and(|elapsed| elapsed >= watch.statement_floor(self)) =>
            {
                (
                    Condition::StatementTimeout,
                    self.statement,
                    "statement_timeout",
                )
            }
            "25P04" => (
                Condition::TransactionTimeout,
                self.transaction,
                "transaction_timeout",
            ),
            _ => return None,
        };
        Some(Timeout {
            condition,
            limit,
            name,
            changed: watch.changed,
        })
    }

    /// The limit a failed statement hit, if any: a server error that is one of the limits
    /// ([`Timeouts::classify`]), or a connection that closed with no error from the server after
    /// the transaction had been open past the backstop. The `25P04` that ends the session
    /// arrives when no request is waiting for it, so the driver then only reports a closed
    /// connection to the next statement.
    pub(crate) fn hit(&self, watch: &LimitWatch, error: &tokio_postgres::Error) -> Option<Hit> {
        if let Some(db) = error.as_db_error() {
            let failure = Failure {
                sqlstate: db.code().code(),
                routine: db.routine(),
                elapsed: watch.since_sent(),
            };
            return self.classify(&failure, watch).map(|timeout| Hit::Server {
                timeout,
                sqlstate: db.code().code().to_owned(),
                message: db.message().to_owned(),
            });
        }
        if error.is_closed()
            && let Some(open_for) = watch.open_for()
            && open_for >= self.transaction
        {
            return Some(Hit::ClosedAfterBackstop {
                open_for,
                limit: self.transaction,
            });
        }
        None
    }
}

/// A limit that fired, with what is needed to build its error.
#[derive(Debug)]
pub(crate) enum Hit {
    /// The server reported it.
    Server {
        timeout: Timeout,
        sqlstate: String,
        message: String,
    },
    /// The connection closed after the backstop had passed.
    ClosedAfterBackstop { open_for: Duration, limit: Duration },
}

impl Hit {
    /// True when the session ended, so the connection must be dropped.
    pub(crate) fn ends_session(&self) -> bool {
        match self {
            Self::Server { timeout, .. } => timeout.ends_session(),
            Self::ClosedAfterBackstop { .. } => true,
        }
    }

    /// The typed error; the driver's error is its source.
    pub(crate) fn into_error(self, source: tokio_postgres::Error) -> Error {
        match self {
            Self::Server {
                timeout,
                sqlstate,
                message,
            } => timeout.into_error(&sqlstate, &message, source),
            Self::ClosedAfterBackstop { open_for, limit } => Error::new(
                ErrorKind::Database,
                format!(
                    "the connection was closed after the transaction had been open for {} ms, \
                     past the transaction_timeout backstop (the likely cause; transaction_timeout \
                     was {} ms when the transaction began); the connection was discarded, split \
                     the work into shorter transactions",
                    open_for.as_millis(),
                    limit.as_millis()
                ),
            )
            .with_condition(Condition::TransactionTimeout)
            .with_source(source),
        }
    }
}

/// What the ORM knows about a failed statement besides the SQLSTATE.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Failure<'a> {
    pub(crate) sqlstate: &'a str,
    /// The error's `routine` field (not translated).
    pub(crate) routine: Option<&'a str>,
    /// Time since the ORM sent it, measured on the client.
    pub(crate) elapsed: Option<Duration>,
}

/// What a transaction tracks to name the limit that fired: when it began and when the last
/// statement left, and whether code inside it changed a limit with `set_config` (the schema
/// steps do).
#[derive(Debug, Default, Clone)]
pub(crate) struct LimitWatch {
    began_at: Option<Instant>,
    sent_at: Option<Instant>,
    lowest_statement: Option<Duration>,
    changed: bool,
}

impl LimitWatch {
    /// `BEGIN` is about to be sent.
    pub(crate) fn began(&mut self) {
        let now = Instant::now();
        // the whole state: limits one transaction changed must not leak into the next one
        *self = LimitWatch {
            began_at: Some(now),
            sent_at: Some(now),
            ..LimitWatch::default()
        };
    }

    /// A statement is about to be sent.
    pub(crate) fn sent(&mut self) {
        self.sent_at = Some(Instant::now());
    }

    /// The opening flight's start counts as the send time of the statements riding in it.
    pub(crate) fn sent_at_begin(&mut self) {
        self.sent_at = self.began_at;
    }

    pub(crate) fn since_sent(&self) -> Option<Duration> {
        self.sent_at.map(|t| t.elapsed())
    }

    pub(crate) fn open_for(&self) -> Option<Duration> {
        self.began_at.map(|t| t.elapsed())
    }

    /// Code inside the transaction changes the limits (`statement` is the new statement limit
    /// when it is one the ORM knows).
    pub(crate) fn limits_changed(&mut self, statement: Option<Duration>) {
        self.changed = true;
        if let Some(statement) = statement {
            self.lowest_statement = Some(
                self.lowest_statement
                    .map_or(statement, |l| l.min(statement)),
            );
        }
    }

    /// The smallest statement limit that was in force.
    fn statement_floor(&self, timeouts: &Timeouts) -> Duration {
        self.lowest_statement
            .map_or(timeouts.statement, |l| l.min(timeouts.statement))
    }
}

/// A limit that fired.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timeout {
    condition: Condition,
    limit: Duration,
    name: &'static str,
    /// Code in the transaction changed a limit after `BEGIN`.
    changed: bool,
}

impl Timeout {
    /// True for the backstop: the server ended the session, so the connection must be dropped.
    pub(crate) fn ends_session(&self) -> bool {
        self.condition == Condition::TransactionTimeout
    }

    /// The error: a lock timeout is a `Lock` error, the others `Database`. The message is the
    /// server's with the limit named and the value it had when the transaction began; when code
    /// in the transaction changed a limit since (the schema steps do), the message says so
    /// instead of claiming a value. The server's error is the source.
    pub(crate) fn into_error(
        self,
        sqlstate: &str,
        server_message: &str,
        source: tokio_postgres::Error,
    ) -> Error {
        let kind = if self.condition == Condition::LockTimeout {
            ErrorKind::Lock
        } else {
            ErrorKind::Database
        };
        let hint = if self.ends_session() {
            "; the connection was discarded, split the work into shorter transactions"
        } else {
            ""
        };
        let changed = if self.changed {
            "; changed inside the transaction since"
        } else {
            ""
        };
        Error::new(
            kind,
            format!(
                "{server_message} ({name} was {limit} ms when the transaction began{changed}){hint}",
                name = self.name,
                limit = self.limit.as_millis()
            ),
        )
        .with_sqlstate(Some(sqlstate.to_owned()))
        .with_condition(self.condition)
        .with_source(source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn valid_limits_round_trip() {
        let t = Timeouts::new(ms(5), ms(30), ms(600)).unwrap();
        assert_eq!(
            (t.lock(), t.statement(), t.transaction()),
            (ms(5), ms(30), ms(600))
        );
        assert_eq!(
            t.set_local_sql(),
            "SET LOCAL lock_timeout = 5; SET LOCAL statement_timeout = 30; \
             SET LOCAL transaction_timeout = 0; SET LOCAL transaction_timeout = 600"
        );
        assert!(Timeouts::new(ms(5), ms(30), ms(31)).is_ok());
        assert!(Timeouts::new(ms(29), ms(30), ms(31)).is_ok());
    }

    #[test]
    fn invalid_limits_are_errors_naming_the_limit() {
        let cases: [(Duration, Duration, Duration, &str); 8] = [
            (
                ms(0),
                ms(30),
                ms(600),
                "lock_timeout must be positive (0 would mean no limit in PostgreSQL), got 0ns",
            ),
            (
                ms(5),
                ms(0),
                ms(600),
                "statement_timeout must be positive (0 would mean no limit in PostgreSQL), got 0ns",
            ),
            (
                ms(5),
                ms(30),
                ms(0),
                "transaction_timeout must be positive (0 would mean no limit in PostgreSQL), got 0ns",
            ),
            (
                Duration::from_micros(1500),
                ms(30),
                ms(600),
                "lock_timeout must be a whole number of milliseconds, got 1.5ms",
            ),
            (
                ms(5),
                Duration::from_secs(u64::from(u32::MAX)),
                Duration::from_secs(u64::MAX / 2000),
                "statement_timeout is 4294967295000 ms; PostgreSQL accepts at most 2147483647 ms",
            ),
            (
                ms(5),
                ms(30),
                ms(30),
                "statement_timeout (30 ms) must be below the transaction_timeout backstop (30 ms): \
                 the backstop ends the session and drops the connection, the statement limit only \
                 cancels the statement",
            ),
            (
                ms(30),
                ms(30),
                ms(600),
                "lock_timeout (30 ms) must be below statement_timeout (30 ms): the statement limit \
                 would always fire first and the lock limit could never apply",
            ),
            (
                ms(31),
                ms(30),
                ms(600),
                "lock_timeout (31 ms) must be below statement_timeout (30 ms): the statement limit \
                 would always fire first and the lock limit could never apply",
            ),
        ];
        for (lock, statement, transaction, expected) in cases {
            let error = Timeouts::new(lock, statement, transaction).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Programming);
            assert_eq!(error.message(), expected);
        }
    }

    fn failure<'a>(
        sqlstate: &'a str,
        routine: Option<&'a str>,
        elapsed: Option<Duration>,
    ) -> Failure<'a> {
        Failure {
            sqlstate,
            routine,
            elapsed,
        }
    }

    fn condition(timeouts: &Timeouts, f: Failure<'_>, watch: &LimitWatch) -> Option<Condition> {
        timeouts.classify(&f, watch).map(|t| t.condition)
    }

    /// The classification reads no message text: it holds for a server in any language.
    #[test]
    fn classify_uses_the_sqlstate_the_routine_and_the_clock() {
        let t = Timeouts::new(ms(100), ms(1_000), ms(60_000)).unwrap();
        let w = LimitWatch::default();
        let interrupt = Some(INTERRUPT_ROUTINE);
        // a lock timeout: raised by the interrupt routine (including relation lock timeout on NOWAIT)
        assert_eq!(
            condition(&t, failure("55P03", interrupt, None), &w),
            Some(Condition::LockTimeout)
        );
        assert_eq!(
            condition(&t, failure("55P03", interrupt, None), &w),
            Some(Condition::LockTimeout)
        );
        // NOWAIT row refusal: another routine (heap_lock_tuple), keeps its own error
        assert_eq!(
            condition(&t, failure("55P03", Some("heap_lock_tuple"), None), &w),
            None
        );
        assert_eq!(condition(&t, failure("55P03", None, None), &w), None);
        // a statement timeout: the routine, and the client waited at least the limit
        assert_eq!(
            condition(&t, failure("57014", interrupt, Some(ms(1_000))), &w),
            Some(Condition::StatementTimeout)
        );
        assert_eq!(
            condition(&t, failure("57014", interrupt, Some(ms(999))), &w),
            None,
            "a cancel before the limit is someone else's"
        );
        assert_eq!(condition(&t, failure("57014", interrupt, None), &w), None);
        assert_eq!(
            condition(&t, failure("57014", Some("other"), Some(ms(5_000))), &w),
            None
        );
        // the backstop is its code alone
        assert_eq!(
            condition(&t, failure("25P04", None, None), &w),
            Some(Condition::TransactionTimeout)
        );
        assert_eq!(
            condition(&t, failure("23505", interrupt, Some(ms(5_000))), &w),
            None
        );
    }

    #[test]
    fn a_lowered_statement_limit_is_the_one_a_cancel_is_measured_against() {
        let t = Timeouts::new(ms(100), ms(10_000), ms(60_000)).unwrap();
        let mut w = LimitWatch::default();
        let f = failure("57014", Some(INTERRUPT_ROUTINE), Some(ms(300)));
        assert_eq!(condition(&t, f, &w), None);
        w.limits_changed(Some(ms(250)));
        assert_eq!(condition(&t, f, &w), Some(Condition::StatementTimeout));
        // a raised limit never raises the floor above the one the transaction began with
        w.limits_changed(Some(ms(50_000)));
        assert_eq!(condition(&t, f, &w), Some(Condition::StatementTimeout));
    }

    #[test]
    fn the_message_says_when_a_limit_was_changed_inside_the_transaction() {
        let t = Timeouts::new(ms(100), ms(1_000), ms(60_000)).unwrap();
        let mut w = LimitWatch::default();
        let lock = failure("55P03", Some(INTERRUPT_ROUTINE), None);
        assert!(!t.classify(&lock, &w).unwrap().changed);
        w.limits_changed(None);
        assert!(t.classify(&lock, &w).unwrap().changed);
    }

    #[test]
    fn a_new_transaction_starts_with_a_clean_watch() {
        let t = Timeouts::new(ms(100), ms(1_000), ms(60_000)).unwrap();
        let mut w = LimitWatch::default();
        w.began();
        w.limits_changed(Some(ms(250)));
        let lock = failure("55P03", Some(INTERRUPT_ROUTINE), None);
        assert!(t.classify(&lock, &w).unwrap().changed);
        assert_eq!(w.statement_floor(&t), ms(250));
        w.began();
        assert!(!t.classify(&lock, &w).unwrap().changed);
        assert_eq!(w.statement_floor(&t), ms(1_000));
    }
}
