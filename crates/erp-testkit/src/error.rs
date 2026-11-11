//! Error type for erp-testkit.
//!
//! Display strings describe only the failing step; the cause is kept as `source()` (driver errors
//! keep their SQLSTATE there). Print a whole chain with [`error_chain`].

use std::error::Error as StdError;

use tokio_postgres::error::SqlState;

/// Everything that can go wrong while creating, using or dropping a throwaway database.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TestkitError {
    /// Neither `ERP_TEST_DATABASE_URLS` nor `DATABASE_URL` is set in the environment.
    #[error(
        "neither ERP_TEST_DATABASE_URLS nor DATABASE_URL is set; on a build host run `. ./.env` \
         and export DATABASE_URL before running database tests"
    )]
    MissingDatabaseUrl,

    /// `ERP_TEST_DATABASE_URLS` is malformed. `position` is the 1-based entry, 0 for the whole list.
    #[error("ERP_TEST_DATABASE_URLS entry {position}: {reason}")]
    InvalidDatabaseUrls {
        /// 1-based entry, or 0 when the list as a whole is wrong.
        position: usize,
        /// What is wrong with it.
        reason: String,
    },

    /// `ERP_TEST_DB_MAX_USAGE_PCT` is not an integer from 1 to 100.
    #[error("ERP_TEST_DB_MAX_USAGE_PCT must be an integer from 1 to 100, got {value:?}")]
    InvalidUsageThreshold {
        /// The rejected value.
        value: String,
    },

    /// Every configured cluster is at or above the usage threshold, or unreachable.
    #[error(
        "no test database cluster is below {threshold}% of max_connections: {}",
        crate::cluster::render_skipped(.skipped)
    )]
    NoClusterAvailable {
        /// The threshold in percent.
        threshold: u8,
        /// Each cluster and why it was passed over, in configured order.
        skipped: Vec<crate::cluster::SkippedCluster>,
    },

    /// Connecting to a listed cluster failed for a reason other than it being unreachable (wrong
    /// password, unknown admin database, …); the next cluster is not tried.
    #[error("could not connect to {cluster}")]
    ClusterConnect {
        /// `cluster <n>/<count> (<host:port/database>)`.
        cluster: String,
        /// Underlying driver error (carries the SQLSTATE).
        #[source]
        source: tokio_postgres::Error,
    },

    /// `pg_stat_activity` reported numbers that cannot be connection counts.
    #[error("{cluster} reported {connections} connections of max_connections {max_connections}")]
    InvalidUsage {
        /// `cluster <n>/<count> (<host:port/database>)`.
        cluster: String,
        /// Reported client connections.
        connections: i64,
        /// Reported `max_connections`.
        max_connections: i64,
    },

    /// `ERP_TESTKIT_CLUSTER_LOG` is set to the empty string (absent disables the log; empty is a
    /// mistake, e.g. an unset shell variable expanded into it).
    #[error("ERP_TESTKIT_CLUSTER_LOG is set but empty; unset it to disable the cluster log")]
    EmptyClusterLogPath,

    /// The file named by `ERP_TESTKIT_CLUSTER_LOG` could not be appended to.
    #[error("could not append to the cluster log {}", path.display())]
    ClusterLog {
        /// The log file.
        path: std::path::PathBuf,
        /// The I/O error.
        #[source]
        source: std::io::Error,
    },

    /// `DATABASE_URL` could not be parsed as a PostgreSQL connection string.
    #[error("DATABASE_URL is not a valid PostgreSQL connection string")]
    InvalidDatabaseUrl(#[source] tokio_postgres::Error),

    /// A parsed database name contains another URL, usually from a comma-joined list.
    /// No input is retained: the embedded URL may carry a second password.
    #[error("database name contains another URL; separate admin URLs with whitespace, not commas")]
    NestedDatabaseUrl,

    /// `DATABASE_URL` does not name a database to use for administrative statements.
    #[error("DATABASE_URL names no database; the admin connection needs one (e.g. `/erpforall`)")]
    MissingAdminDatabase,

    /// A caller-supplied name (run id, template) contains characters we refuse to splice into SQL.
    #[error("invalid {what} {value:?}: {rule}")]
    InvalidName {
        /// Which input was rejected.
        what: &'static str,
        /// The rejected value.
        value: String,
        /// The rule it broke.
        rule: &'static str,
    },

    /// Opening a connection to the server failed.
    #[error("could not connect to PostgreSQL")]
    Connect(#[source] tokio_postgres::Error),

    /// A statement failed.
    #[error("PostgreSQL error while {action}")]
    Postgres {
        /// What the testkit was doing.
        action: String,
        /// Underlying driver error (carries the SQLSTATE).
        #[source]
        source: tokio_postgres::Error,
    },

    /// Every candidate database name was already taken.
    #[error("could not find a free test database name after {attempts} attempts")]
    NamesExhausted {
        /// How many names were tried.
        attempts: u32,
    },

    /// The system clock reads before the Unix epoch, so creation times cannot be recorded.
    #[error("system clock is before the Unix epoch")]
    Clock(#[source] std::time::SystemTimeError),

    /// PostgreSQL server is below the minimum required version.
    #[error(
        "PostgreSQL {found_version} (server_version_num {server_version_num}) is below the \
         minimum required version 18 (server_version_num {min_required}); ERPForAll requires PostgreSQL 18 or later"
    )]
    UnsupportedPostgres {
        /// Server version number (e.g. 170002).
        server_version_num: i32,
        /// Minimum version number required (e.g. 180000).
        min_required: i32,
        /// Full version string from `version()`.
        found_version: String,
    },

    /// A test cluster raised its timeouts too late in every attempt, for tests that expect a
    /// blocked statement to be cancelled within a short grace (`Cluster::probe_with_timeouts`).
    #[error(
        "{cluster} raises its timeouts late in all {} attempts, {} apart (earlier: {}; last: \
         statement_timeout median {} of {}, lock_timeout median {} of {}; at most {} allowed): \
         tests that expect a statement blocked behind a lock to end with SQLSTATE 57014 or 55P03 \
         within their grace fail on it. If the host is macOS, start the postmaster from launchd \
         with ProcessType Interactive",
        .earlier.len() + 1,
        crate::timeout_check::millis(crate::timeout_check::REMEASURE_PAUSE),
        crate::timeout_check::render_earlier(.earlier),
        crate::timeout_check::millis(crate::timeout_check::median(.statement)),
        crate::timeout_check::render_samples(.statement),
        crate::timeout_check::millis(crate::timeout_check::median(.lock)),
        crate::timeout_check::render_samples(.lock),
        crate::timeout_check::millis(crate::MAX_TIMEOUT_LATENESS)
    )]
    LateTimeouts {
        /// `cluster <n>/<count> (<host:port/database>)`.
        cluster: String,
        /// The earlier attempts' (`statement_timeout`, `lock_timeout`) medians, in order.
        earlier: Vec<(std::time::Duration, std::time::Duration)>,
        /// The last attempt's `statement_timeout` lateness, sorted.
        statement: Vec<std::time::Duration>,
        /// The last attempt's `lock_timeout` lateness, sorted.
        lock: Vec<std::time::Duration>,
    },

    /// A statement armed with a timeout finished instead of being cancelled.
    #[error(
        "{cluster} did not cancel a statement at its {setting} of {}; the statement finished \
         after {}: its server timeouts do not work, and tests that rely on them cannot pass",
        crate::timeout_check::millis(crate::timeout_check::ARMED),
        crate::timeout_check::millis(*.waited)
    )]
    TimeoutNotRaised {
        /// `cluster <n>/<count> (<host:port/database>)`.
        cluster: String,
        /// `statement_timeout` or `lock_timeout`.
        setting: &'static str,
        /// How long the statement ran.
        waited: std::time::Duration,
    },

    /// A statement armed with a timeout was cancelled before the timeout was due: something else
    /// cancelled it, so the sample says nothing about the timer.
    #[error(
        "{cluster}: a statement armed with {setting} = {} was cancelled after {}, before its \
         timeout was due: something else cancelled it (pg_cancel_backend, a pooler), so the \
         measurement is void",
        crate::timeout_check::millis(crate::timeout_check::ARMED),
        crate::timeout_check::millis(*.after)
    )]
    CancelledEarly {
        /// `cluster <n>/<count> (<host:port/database>)`.
        cluster: String,
        /// `statement_timeout` or `lock_timeout`.
        setting: &'static str,
        /// When the cancel arrived.
        after: std::time::Duration,
    },

    /// A step of the timeout check got no answer within its client bound.
    #[error(
        "no answer within {} while {step} on {cluster}: the connection stalled (an ssh tunnel \
         that stopped forwarding) or the server did not respond; this says nothing about its \
         timers",
        crate::timeout_check::millis(*.waited)
    )]
    TimeoutCheckStalled {
        /// `cluster <n>/<count> (<host:port/database>)`.
        cluster: String,
        /// The step that stalled (`arming statement_timeout (SET)`, …).
        step: &'static str,
        /// The client bound.
        waited: std::time::Duration,
    },

    /// The deadpool pool could not be built.
    #[error("could not build the connection pool")]
    PoolBuild(#[from] deadpool_postgres::BuildError),

    /// The pool could not hand out a connection (includes wait/create timeouts).
    #[error("could not get a connection from the pool")]
    Pool(deadpool_postgres::PoolError),

    /// The runtime used by the synchronous drop path could not start.
    #[error("could not start the cleanup runtime")]
    Runtime(#[source] std::io::Error),

    /// A configuration error occurred (e.g. invalid pool size or base install failure).
    #[error("configuration error: {0}")]
    Configuration(String),
}

impl From<deadpool_postgres::PoolError> for TestkitError {
    fn from(error: deadpool_postgres::PoolError) -> Self {
        if let Ok(Some(err)) = erp_orm::pool::UnsupportedPostgresError::from_pool_error(&error) {
            return Self::UnsupportedPostgres {
                server_version_num: err.server_version_num,
                min_required: err.min_required,
                found_version: err.found_version,
            };
        }
        Self::Pool(error)
    }
}

impl From<erp_orm::pool::UnsupportedPostgresError> for TestkitError {
    fn from(err: erp_orm::pool::UnsupportedPostgresError) -> Self {
        Self::UnsupportedPostgres {
            server_version_num: err.server_version_num,
            min_required: err.min_required,
            found_version: err.found_version,
        }
    }
}

impl From<erp_orm::pool::PoolBuildError> for TestkitError {
    fn from(err: erp_orm::pool::PoolBuildError) -> Self {
        match err {
            erp_orm::pool::PoolBuildError::ZeroMaxSize => Self::Configuration(err.to_string()),
            erp_orm::pool::PoolBuildError::Build(b) => Self::PoolBuild(b),
        }
    }
}

impl TestkitError {
    /// Why this error means the server is unreachable right now (connection refused, timed out,
    /// closed before answering, starting up, out of connection slots), or `None` for any other
    /// error. `erp-testkit sweep --if-reachable` uses it to skip a cluster that is stopped.
    pub fn unreachable_reason(&self) -> Option<crate::cluster::Unreachable> {
        match self {
            Self::Connect(error) | Self::ClusterConnect { source: error, .. } => {
                crate::cluster::unreachable_reason(error)
            }
            // Every other variant is not a failure to reach a server.
            _ => None,
        }
    }

    /// SQLSTATE of the server error behind this error, if there is one.
    pub fn sqlstate(&self) -> Option<&SqlState> {
        let mut cause: Option<&(dyn StdError + 'static)> = Some(self);
        while let Some(error) = cause {
            if let Some(pg) = error.downcast_ref::<tokio_postgres::Error>() {
                return pg.code();
            }
            cause = error.source();
        }
        None
    }
}

/// Result alias for this crate.
pub type Result<T, E = TestkitError> = std::result::Result<T, E>;

/// Render an error and all of its causes as `outer: cause: root cause`.
pub fn error_chain(error: &(dyn StdError + 'static)) -> String {
    let mut text = error.to_string();
    let mut cause = error.source();
    while let Some(next) = cause {
        text.push_str(": ");
        text.push_str(&next.to_string());
        cause = next.source();
    }
    text
}

/// Wrap a driver error with the action that failed.
pub(crate) fn pg_error(
    action: impl Into<String>,
) -> impl FnOnce(tokio_postgres::Error) -> TestkitError {
    let action = action.into();
    move |source| TestkitError::Postgres { action, source }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_unsupported_postgres_hook_message() {
        let orm_err = erp_orm::pool::UnsupportedPostgresError::new(170002, "17.2 (Debian 17.2-1)");
        let hook_err = deadpool_postgres::HookError::message(orm_err.to_hook_message());
        let pool_err = deadpool_postgres::PoolError::PostCreateHook(hook_err);
        let testkit_err = TestkitError::from(pool_err);
        match testkit_err {
            TestkitError::UnsupportedPostgres {
                server_version_num,
                min_required,
                found_version,
            } => {
                assert_eq!(server_version_num, 170002);
                assert_eq!(min_required, 180000);
                assert_eq!(found_version, "17.2 (Debian 17.2-1)");
            }
            other => panic!("expected UnsupportedPostgres, got: {other:?}"),
        }
    }
}
