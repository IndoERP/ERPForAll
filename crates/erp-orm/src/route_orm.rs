//! The [`Orm`] side of read routing: which pool a
//! transaction starts on, the per-connection verification of the read pool, and
//! [`Orm::check_read_pool`].

use deadpool_postgres::{Object, Pool, PoolError};
use erp_fields::{Error, ErrorKind, Result};

use crate::route::{
    CachedFailure, ReadPoolFailure, ReadPoolStatus, ReadRoute, Reason, Route, UnitFlag, mark_write,
    unavailable_error, unavailable_with, unit_wrote,
};
use crate::transaction::{Orm, TxIntent, checkout_error};

/// What one connection tells about its server and role (run on the primary and on the read pool).
///
/// `can_write` looks at the login (`session_user`) and every role it can `SET ROLE` to
/// (`pg_has_role(.., 'MEMBER')`, the login itself included), not at `current_user`: a session
/// that logged in as a writable role and switched to a read-only one with `-c role=..` can switch
/// back with `RESET ROLE`, so the switch is no boundary.
const IDENTITY_SQL: &str = "\
SELECT current_database()::text,
       (SELECT system_identifier::text FROM pg_control_system()),
       current_setting('server_version_num')::int4,
       current_setting('server_version')::text,
       current_setting('search_path')::text,
       pg_is_in_recovery(),
       current_setting('default_transaction_read_only') = 'on',
       EXISTS (
         SELECT 1 FROM pg_roles r
         WHERE pg_has_role(session_user, r.oid, 'MEMBER')
           AND (has_any_column_privilege(r.oid, 'erp_signal_generation', 'INSERT,UPDATE')
                OR has_table_privilege(r.oid, 'erp_signal_generation', 'DELETE')))";

/// The answer of [`IDENTITY_SQL`].
struct Identity {
    database: String,
    system_identifier: String,
    version_num: i32,
    version: String,
    search_path: String,
    status: ReadPoolStatus,
}

/// Why a read pool cannot serve.
pub(crate) enum ReadFailure {
    /// A configuration error (another database, an unsupported server): never falls back.
    Fatal(Error),
    /// The primary pool failed while the read pool was being verified: the primary's error, not a
    /// read pool failure (the read fails whatever [`ReadPoolFailure`] says).
    Primary(Error),
    /// The pool cannot serve right now or must not be used (unreachable, not read-only): the
    /// failure setting decides. `cause` is for the log, `error` for the caller.
    Unavailable { cause: String, error: Error },
}

impl ReadFailure {
    fn unavailable(error: Error) -> Self {
        Self::Unavailable {
            cause: error_chain(&error),
            error,
        }
    }
}

/// The error and its sources, joined (for the log).
pub(crate) fn error_chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(next) = source {
        text.push_str(": ");
        text.push_str(&next.to_string());
        source = next.source();
    }
    text
}

/// What [`Orm::checkout`] decided for a transaction.
pub(crate) struct Checkout {
    pub(crate) conn: Object,
    pub(crate) route: Route,
    /// The primary pool a routed transaction can fall back to.
    pub(crate) primary: Option<Pool>,
    /// Why a read transaction runs on the primary; counted once its `BEGIN` succeeded.
    pub(crate) fallback: Option<Reason>,
}

impl Orm {
    /// Connect to the read pool once, check that it serves the primary's database and report
    /// whether it is read-only end to end. The same check runs on each connection of the pool
    /// before its first routed transaction.
    ///
    /// # Errors
    ///
    /// `Programming` when no read pool is configured or the pool serves another database or
    /// server than the primary (database name, `pg_control_system()` identifier or `search_path`
    /// differ); a read-pool-unavailable error ([`Error::is_read_pool_unavailable`]) when it cannot
    /// be reached; `Database` when the server is below the supported PostgreSQL version
    /// or its answer cannot be read.
    pub async fn check_read_pool(&self) -> Result<ReadPoolStatus> {
        let Some(read) = &self.shared.read else {
            return Err(Error::programming(
                "no read pool is configured (OrmBuilder::read_pool)",
            ));
        };
        let conn = self.read_connection(read).await.map_err(failure_error)?;
        self.inspect(&conn).await.map_err(failure_error)
    }

    /// A connection of the read pool (counting a saturated pool).
    async fn read_connection(&self, read: &ReadRoute) -> std::result::Result<Object, ReadFailure> {
        match read.pool.get().await {
            Ok(conn) => Ok(conn),
            Err(e) => {
                // an unsupported server (or a malformed hook payload) is not "unavailable"
                if !matches!(
                    crate::pool::UnsupportedPostgresError::from_pool_error(&e),
                    Ok(None)
                ) {
                    return Err(ReadFailure::Fatal(checkout_error(e)));
                }
                if matches!(e, PoolError::Timeout(_)) {
                    self.shared.routing.timeout();
                }
                let cause = e.to_string();
                Err(ReadFailure::Unavailable {
                    cause,
                    error: unavailable_error(e),
                })
            }
        }
    }

    /// Read what `conn` says about its server and role, compare it with the primary's and return
    /// the read-only status.
    async fn inspect(&self, conn: &Object) -> std::result::Result<ReadPoolStatus, ReadFailure> {
        let replica = identity(conn).await.map_err(ReadFailure::unavailable)?;
        if replica.version_num < crate::pool::MIN_POSTGRES_VERSION_NUM {
            // the primary path's gate, for a pool built without the post_create hook
            return Err(ReadFailure::Fatal(Error::new(
                ErrorKind::Database,
                crate::pool::UnsupportedPostgresError::new(
                    replica.version_num,
                    replica.version.clone(),
                )
                .to_string(),
            )));
        }
        let primary = self
            .pool
            .get()
            .await
            .map_err(|e| ReadFailure::Primary(checkout_error(e)))?;
        let primary = identity(&primary).await.map_err(ReadFailure::Primary)?;
        let mut differences = Vec::new();
        if replica.database != primary.database {
            differences.push(format!(
                "database {:?} vs {:?}",
                replica.database, primary.database
            ));
        }
        if replica.system_identifier != primary.system_identifier {
            differences.push("server (pg_control_system identifier)".to_owned());
        }
        if replica.search_path != primary.search_path {
            differences.push(format!(
                "search_path {:?} vs {:?}",
                replica.search_path, primary.search_path
            ));
        }
        if !differences.is_empty() {
            return Err(ReadFailure::Fatal(Error::programming(format!(
                "the read pool does not serve the primary's database ({}): refusing to route \
                 reads there",
                differences.join(", ")
            ))));
        }
        Ok(replica.status)
    }

    /// The check run on the first routed use of each connection of the pool: it serves the
    /// primary's database and cannot write (a standby, or a login whose roles have no write
    /// privileges on the ORM's tables). `default_transaction_read_only` is not a boundary and does
    /// not count. A connection is identified by when its pool created it; one that passed is
    /// trusted for its life. A connection that fails is remembered for
    /// [`crate::route::VERIFY_FAILURE_TTL`]: the reads that follow are refused (or sent to the
    /// primary) without verifying again.
    async fn verify_read_pool(
        &self,
        read: &ReadRoute,
        conn: &Object,
    ) -> std::result::Result<(), ReadFailure> {
        let id = Object::metrics(conn).created;
        if read.verify.is_verified(id) {
            return Ok(());
        }
        match read.verify.recent_failure() {
            Some(CachedFailure::Unavailable(cause)) => {
                return Err(ReadFailure::unavailable(unavailable_with(&cause, None)));
            }
            Some(CachedFailure::Fatal(kind, message)) => {
                return Err(ReadFailure::Fatal(Error::new(kind, message)));
            }
            None => {}
        }
        match self.inspect(conn).await {
            Ok(status) if status.is_read_only() => {
                read.verify.record_ok(id);
                Ok(())
            }
            Ok(_) => {
                let cause = "the read pool is neither a standby nor a login whose roles lack write \
                             privileges on the ORM's tables (default_transaction_read_only is not \
                             a boundary, and neither is a SET ROLE the session can undo)";
                read.verify
                    .record_failure(CachedFailure::Unavailable(cause.to_owned()));
                Err(ReadFailure::unavailable(unavailable_with(cause, None)))
            }
            Err(ReadFailure::Fatal(error)) => {
                read.verify.record_failure(CachedFailure::Fatal(
                    error.kind(),
                    error.message().to_owned(),
                ));
                Err(ReadFailure::Fatal(error))
            }
            // transient (the connection or the primary failed): the next read tries again
            Err(other) => Err(other),
        }
    }

    /// The connection for a transaction of `intent`: the read pool for a read when the routing
    /// rules allow, else the primary. `unit` is the unit of work the transaction belongs to.
    pub(crate) async fn checkout(
        &self,
        intent: &TxIntent,
        unit: Option<&UnitFlag>,
        on_primary: bool,
    ) -> Result<Checkout> {
        let primary = |fallback: Option<Reason>| async {
            self.pool
                .get()
                .await
                .map(|conn| Checkout {
                    conn,
                    route: Route::Primary,
                    primary: None,
                    fallback,
                })
                .map_err(checkout_error)
        };
        let TxIntent::Read { .. } = intent else {
            // the unit of work has written from here on: its later reads stay on the primary
            mark_write(unit);
            return primary(None).await;
        };
        let Some(read) = &self.shared.read else {
            return primary(None).await;
        };
        if on_primary {
            return primary(None).await;
        }
        if unit_wrote(unit) {
            return primary(Some(Reason::UnitOfWorkWrote)).await;
        }
        // A process that is not running its signal listener may not know of the changes a
        // replica has not replayed: it reads from the primary, unless the replay lag is bounded.
        if self.shared.signal.as_ref().is_some_and(|signal| {
            signal.listener_down() || (read.max_lag.is_none() && !signal.listener_running())
        }) {
            return primary(Some(Reason::ListenerDown)).await;
        }
        let served = match self.read_connection(read).await {
            Ok(conn) => self.verify_read_pool(read, &conn).await.map(|()| conn),
            Err(failure) => Err(failure),
        };
        match served {
            Ok(conn) => Ok(Checkout {
                conn,
                route: Route::ReadPool,
                primary: Some(self.pool.clone()),
                fallback: None,
            }),
            Err(ReadFailure::Fatal(error)) => {
                self.shared.routing.error(&error_chain(&error));
                Err(error)
            }
            Err(ReadFailure::Primary(error)) => {
                self.shared
                    .routing
                    .primary_failed_during_verification(&error_chain(&error));
                Err(error)
            }
            Err(ReadFailure::Unavailable { cause, error }) => match read.on_failure {
                ReadPoolFailure::Primary => primary(Some(Reason::ReadPoolUnavailable(cause))).await,
                ReadPoolFailure::Fail => {
                    self.shared.routing.error(&cause);
                    Err(error)
                }
            },
        }
    }
}

fn failure_error(failure: ReadFailure) -> Error {
    match failure {
        ReadFailure::Fatal(error)
        | ReadFailure::Primary(error)
        | ReadFailure::Unavailable { error, .. } => error,
    }
}

async fn identity(conn: &Object) -> Result<Identity> {
    let row = conn.query_one(IDENTITY_SQL, &[]).await.map_err(|e| {
        Error::new(ErrorKind::Database, "could not inspect the connection").with_source(e)
    })?;
    let unreadable = |e: tokio_postgres::Error| {
        Error::new(
            ErrorKind::Database,
            "could not read the connection's settings",
        )
        .with_source(e)
    };
    Ok(Identity {
        database: row.try_get(0).map_err(unreadable)?,
        system_identifier: row.try_get(1).map_err(unreadable)?,
        version_num: row.try_get(2).map_err(unreadable)?,
        version: row.try_get(3).map_err(unreadable)?,
        search_path: row.try_get(4).map_err(unreadable)?,
        status: ReadPoolStatus {
            in_recovery: row.try_get(5).map_err(unreadable)?,
            default_read_only: row.try_get(6).map_err(unreadable)?,
            can_write: row.try_get(7).map_err(unreadable)?,
        },
    })
}
