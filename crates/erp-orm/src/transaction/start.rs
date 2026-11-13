//! The opening flight of a transaction.
//!
//! One message carries `BEGIN` and the `SET LOCAL` limits; the generation read of the signals and
//! the principal resolver's statement follow it as unnamed statements with typed parameters. All
//! of it is written before the first answer is read, so opening costs one round trip however many
//! parts apply. A child module of `transaction` so it works on the transaction's private state.

use std::future::Future;
use std::task::Poll;

use erp_fields::Result;
use tokio_postgres::Row;
use tokio_postgres::types::{ToSql, Type};

use super::{CommitError, FinalStatement, State, Transaction};
use crate::access::ResolverStatement;
use crate::search::typed_params;

/// Poll `a`, `b`, `c` and `d` together, in that order, until all are done. The first poll of each
/// future is what puts its request on the wire, so the requests leave back to back in this order
/// and are answered in one round trip.
pub(crate) async fn pipeline4<A, B, C, D>(
    a: A,
    b: B,
    c: C,
    d: D,
) -> (A::Output, B::Output, C::Output, D::Output)
where
    A: Future,
    B: Future,
    C: Future,
    D: Future,
{
    let mut a = std::pin::pin!(a);
    let mut b = std::pin::pin!(b);
    let mut c = std::pin::pin!(c);
    let mut d = std::pin::pin!(d);
    let (mut a_out, mut b_out, mut c_out, mut d_out) = (None, None, None, None);
    std::future::poll_fn(|cx| {
        if a_out.is_none()
            && let Poll::Ready(r) = a.as_mut().poll(cx)
        {
            a_out = Some(r);
        }
        if b_out.is_none()
            && let Poll::Ready(r) = b.as_mut().poll(cx)
        {
            b_out = Some(r);
        }
        if c_out.is_none()
            && let Poll::Ready(r) = c.as_mut().poll(cx)
        {
            c_out = Some(r);
        }
        if d_out.is_none()
            && let Poll::Ready(r) = d.as_mut().poll(cx)
        {
            d_out = Some(r);
        }
        match (a_out.take(), b_out.take(), c_out.take(), d_out.take()) {
            (Some(w), Some(x), Some(y), Some(z)) => Poll::Ready((w, x, y, z)),
            (w, x, y, z) => {
                a_out = w;
                b_out = x;
                c_out = y;
                d_out = z;
                Poll::Pending
            }
        }
    })
    .await
}

/// What the parts of the flight answered.
struct Answers<'a> {
    begin: std::result::Result<(), tokio_postgres::Error>,
    signal: Option<std::result::Result<Vec<Row>, tokio_postgres::Error>>,
    /// The replica's replay lag, on a routed transaction with a bound.
    lag: Option<std::result::Result<Vec<Row>, tokio_postgres::Error>>,
    /// The resolver's statement text with its answer, so the text is never looked up again.
    resolver: Option<(
        &'a str,
        std::result::Result<Vec<Row>, tokio_postgres::Error>,
    )>,
}

impl Transaction {
    /// Open the transaction with the principal resolver's statement in the same flight; the rows
    /// are its answer ([`PrincipalResolver::finish`](crate::access::PrincipalResolver::finish)).
    pub(crate) async fn begin_resolving(
        &mut self,
        statement: &ResolverStatement,
    ) -> Result<Vec<Row>> {
        self.check_not_cancelled()?;
        match self.state {
            State::Idle => {}
            State::Beginning => {
                // an earlier start never completed (its future was dropped)
                self.release();
                return Err(super::released_error());
            }
            _ => {
                return Err(erp_fields::Error::programming(
                    "the principal is resolved while the transaction opens; this one is already \
                     open",
                ));
            }
        }
        Box::pin(self.start(Some(statement))).await
    }

    /// Open the transaction: `BEGIN` with the limits, the signal read when the ORM has signals,
    /// and `resolver`'s statement when given, in one round trip. Returns the resolver's rows
    /// (empty without one).
    ///
    /// The signal read is not counted in `query_count`; the resolver's statement is.
    pub(super) async fn start(&mut self, resolver: Option<&ResolverStatement>) -> Result<Vec<Row>> {
        if let Some(statement) = resolver {
            self.require_writable_sql("Orm::begin_with", statement.sql())?;
            // caller SQL like any other: a resolver should only read, but a `DELETE` it sends on
            // a followed table is forgotten here too (review C4-X1)
            self.follow_sql_deletes(statement.sql());
        }
        // a write began in the unit of work after this read was opened: read-your-writes
        if self.route == crate::route::Route::ReadPool
            && crate::route::unit_wrote(self.unit.as_ref())
        {
            self.reroute_to_primary(crate::route::Reason::UnitOfWorkWrote, false)
                .await?;
        }
        let routed = self.route == crate::route::Route::ReadPool;
        let signal = if self.signal_read {
            self.shared.signal.clone()
        } else {
            None
        };
        if routed && signal.is_none() {
            // the lag guard needs the generations read with BEGIN
            self.release();
            return Err(erp_fields::Error::programming(
                "a transaction on the read pool must read the signal generations at BEGIN",
            ));
        }
        if let Some(signal) = &signal
            && signal.is_superseded()
        {
            return Err(crate::signal::superseded_error());
        }
        // the seam only exists for the read route: on the primary it would silently run the
        // primary against an older snapshot
        #[cfg(feature = "test-faults")]
        if !routed && self.import_snapshot.is_some() {
            self.release();
            return Err(erp_fields::Error::programming(
                "BeginOptions::import_snapshot needs a transaction routed to the read pool",
            ));
        }
        // the seam stands for a replica that lacks the table: the primary after a reroute reads
        // the real table
        #[cfg(feature = "test-faults")]
        let signal_seam = self.signal_read_sql.clone().filter(|_| routed);
        #[cfg(feature = "test-faults")]
        let signal_sql: &str = signal_seam.as_deref().unwrap_or(crate::signal::READ_SQL);
        #[cfg(not(feature = "test-faults"))]
        let signal_sql: &str = crate::signal::READ_SQL;
        #[cfg(feature = "test-faults")]
        let lag_seam = self.lag_read_sql.clone();
        #[cfg(feature = "test-faults")]
        let lag_sql: &str = lag_seam.as_deref().unwrap_or(crate::route::LAG_SQL);
        #[cfg(not(feature = "test-faults"))]
        let lag_sql: &str = crate::route::LAG_SQL;
        let max_lag = self
            .shared
            .read
            .as_ref()
            .filter(|_| routed)
            .and_then(|read| read.max_lag);
        // bound before anything is sent: a parameter that cannot be encoded sends nothing
        let resolving = match resolver {
            Some(statement) => Some((statement, typed_params(statement.params())?)),
            None => None,
        };
        // A transaction object keeps its connection across commits: DDL committed by another
        // ORM of the database since the last BEGIN invalidates its statements.
        if let Some(conn) = &self.conn {
            self.shared.refresh_statements(conn);
        }
        // set before awaiting: if this future is dropped, Drop sees an unknown state
        self.state = State::Beginning;
        self.watch.began();
        // loaded before the read is sent: a lower generation in a read that started after the
        // last apply is a regression, not a race (Shared::apply_signal)
        let read_epoch = signal.as_ref().map(|s| s.apply_epoch());
        // loaded before the read too: a full refresh that starts after this load clears every
        // named cache, and this transaction's values are then refused at commit
        let full_refreshes = signal.as_ref().map(|s| s.full_refreshes());
        // test seam (replica lag guard): read the state of an exported snapshot, in the same
        // message as `BEGIN`, before any query takes the transaction's own
        #[cfg(feature = "test-faults")]
        let import = match self.import_snapshot.take() {
            Some(snapshot) => format!(
                "; SET TRANSACTION SNAPSHOT '{}'",
                snapshot.replace('\'', "''")
            ),
            None => String::new(),
        };
        #[cfg(not(feature = "test-faults"))]
        let import = "";
        let begin_sql = format!(
            "{}{import}; {}",
            self.intent.begin_sql(self.isolation),
            self.timeouts.set_local_sql()
        );
        let answers = {
            let client = self.conn()?;
            let begin = client.batch_execute(&begin_sql);
            let read = async {
                match &signal {
                    Some(_) => Some(client.query_typed(signal_sql, &[]).await),
                    None => None,
                }
            };
            let lag = async {
                match max_lag {
                    Some(_) => Some(client.query_typed(lag_sql, &[]).await),
                    None => None,
                }
            };
            let resolve = async {
                match &resolving {
                    Some((statement, (values, types))) => {
                        let typed: Vec<(&(dyn ToSql + Sync), Type)> = values
                            .iter()
                            .zip(types)
                            .map(|(v, t)| (v.as_ref() as &(dyn ToSql + Sync), t.clone()))
                            .collect();
                        Some((
                            statement.sql(),
                            client.query_typed(statement.sql(), &typed).await,
                        ))
                    }
                    None => None,
                }
            };
            let (begin, signal, lag, resolver) = pipeline4(begin, read, lag, resolve).await;
            Answers {
                begin,
                signal,
                lag,
                resolver,
            }
        };
        // a read connection that died between checkout and `BEGIN` (replica restart, failover)
        // follows the read pool failure setting like an unreachable pool
        if routed {
            // the first answer that shows a lost connection is the cause (a refused statement
            // before it is not); the resolver's answer counts like the others
            let lost = |e: Option<&tokio_postgres::Error>| e.is_some_and(super::connection_failed);
            let at = [
                lost(answers.begin.as_ref().err()),
                lost(answers.signal.as_ref().and_then(|r| r.as_ref().err())),
                lost(answers.lag.as_ref().and_then(|r| r.as_ref().err())),
                lost(
                    answers
                        .resolver
                        .as_ref()
                        .and_then(|(_, r)| r.as_ref().err()),
                ),
            ]
            .iter()
            .position(|dead| *dead);
            if let Some(at) = at {
                let error = match (
                    at,
                    answers.begin,
                    answers.signal,
                    answers.lag,
                    answers.resolver,
                ) {
                    (0, Err(e), ..)
                    | (1, _, Some(Err(e)), ..)
                    | (2, _, _, Some(Err(e)), _)
                    | (3, _, _, _, Some((_, Err(e)))) => e,
                    _ => {
                        self.release();
                        return Err(erp_fields::Error::programming(
                            "a dead read connection reported no error",
                        ));
                    }
                };
                return self.read_pool_died(resolver, error).await;
            }
        }
        if let Err(e) = answers.begin {
            let error = crate::dberror::map_pg_error(&self.shared.registry, e);
            self.release();
            return Err(error);
        }
        let observed = match answers.signal {
            Some(Ok(rows)) => Some(crate::signal::Generations::from_rows(&rows)),
            // A replica that has not replayed the signal table yet is a replica that is behind,
            // not a missing installation: the primary answers (and keeps the Programming error
            // that tells the operator to install the table when it is missing there too).
            Some(Err(e))
                if routed
                    && e.code() == Some(&tokio_postgres::error::SqlState::UNDEFINED_TABLE) =>
            {
                let why = format!(
                    "the signal table {} is not on the replica yet",
                    crate::signal::SIGNAL_TABLE
                );
                self.reroute_to_primary(crate::route::Reason::ReplicaBehind(why), false)
                    .await?;
                return Box::pin(self.start(resolver)).await;
            }
            Some(Err(e)) => {
                let error = crate::signal::read_error(e);
                self.release();
                return Err(error);
            }
            None => None,
        };
        let observed = match observed.transpose() {
            Ok(observed) => observed,
            Err(error) => {
                self.release();
                return Err(error);
            }
        };
        let lag_rows = match answers.lag {
            Some(Ok(rows)) => Some(rows),
            Some(Err(e)) => {
                let error = crate::route::lag_read_error(e);
                self.release();
                return Err(error);
            }
            None => None,
        };
        self.state = State::Open;
        if routed {
            // The replay lag is over the bound (or unknown): the replica may serve rows the
            // primary has changed long ago. The resolver's rows, read on the replica, are dropped
            // with the transaction; the primary answers again.
            let over = match (max_lag, &lag_rows) {
                (Some(max), Some(rows)) => match crate::route::lagging(rows, max) {
                    Ok(over) => over,
                    Err(error) => {
                        // never left Open on the replica without a guard
                        self.release();
                        return Err(error);
                    }
                },
                _ => None,
            };
            if let Some(detail) = over {
                self.reroute_to_primary(crate::route::Reason::Lagging(detail), false)
                    .await?;
                return Box::pin(self.start(resolver)).await;
            }
            // A replica that reads lower than what this process applied has not replayed that
            // change: its snapshot matches neither the registry nor the policy this process runs
            // on, and the lower value would read as a regression. Begin again on the primary.
            if let (Some(signal), Some(observed)) = (&signal, &observed)
                && let Some(keys) = signal.behind(observed)
            {
                self.reroute_to_primary(crate::route::Reason::ReplicaBehind(keys), false)
                    .await?;
                return Box::pin(self.start(resolver)).await;
            }
        }
        if let (Some(observed), Some(read_epoch), Some(full_refreshes)) =
            (observed, read_epoch, full_refreshes)
        {
            self.signal_state.began(observed.clone(), full_refreshes);
            if let Some(reason) = self.apply_observed(&observed, read_epoch).await? {
                // the policy pin on the read pool was refused: begin again on the primary
                self.reroute_to_primary(reason, false).await?;
                return Box::pin(self.start(resolver)).await;
            }
        }
        if let Some(statement) = resolver {
            self.record(statement.sql());
            // the resolver's statement left with the flight, not now
            self.watch.sent_at_begin();
        }
        self.count_begin();
        match answers.resolver {
            Some((_, Ok(rows))) => Ok(rows),
            Some((sql, Err(e))) => Err(self.statement_failed_in(sql, e)),
            None => Ok(Vec::new()),
        }
    }

    /// Open, run `last` and commit in one flight (`Orm::one_flight`, on a fresh
    /// primary transaction): `BEGIN` with the isolation and the limits, `last` and `COMMIT` are
    /// written before the first answer is read. No generation read rides along (the caller's
    /// contract), so there is nothing to apply after the commit.
    ///
    /// The outcome follows the answers. `BEGIN` and the limits are one simple-query message: when
    /// the limits are refused the transaction is open and aborted, so `last` fails and `COMMIT`
    /// answers `ROLLBACK`. A refused `last` makes `COMMIT` answer `ROLLBACK` (a definite
    /// rollback). A missing answer is [`CommitError::Unknown`]; so is a refused `BEGIN` itself
    /// whose `last` and `COMMIT` were answered without error (they may have run outside a
    /// transaction; not reachable with the fixed `BEGIN` text, reported honestly all the same).
    pub(super) async fn start_committing(
        &mut self,
        last: FinalStatement<'_>,
    ) -> std::result::Result<Vec<Row>, CommitError> {
        if let Err(error) = self.check_not_cancelled() {
            return Err(CommitError::RolledBack(error));
        }
        if let Err(error) = self.require_writable_sql("Orm::one_flight", last.sql) {
            return Err(CommitError::RolledBack(error));
        }
        #[cfg(feature = "test-faults")]
        if self.import_snapshot.is_some() {
            return Err(CommitError::RolledBack(erp_fields::Error::programming(
                "BeginOptions::import_snapshot needs a transaction routed to the read pool",
            )));
        }
        // A transaction object keeps its connection across commits: DDL committed by another
        // ORM of the database since the last BEGIN invalidates its statements.
        if let Some(conn) = &self.conn {
            self.shared.refresh_statements(conn);
        }
        let begin_sql = format!(
            "{}; {}",
            self.intent.begin_sql(self.isolation),
            self.timeouts.set_local_sql()
        );
        // set before awaiting: if this future is dropped, Drop sees an unknown state
        self.state = State::Beginning;
        self.watch.began();
        let (begin, statement, commit, ()) = {
            let client = match self.conn() {
                Ok(client) => client,
                Err(error) => return Err(CommitError::RolledBack(error)),
            };
            pipeline4(
                client.batch_execute(&begin_sql),
                client.query_typed(last.sql, last.params),
                client.batch_execute("COMMIT"),
                std::future::ready(()),
            )
            .await
        };
        self.count_begin();
        self.count(last.sql);
        let statement = match (begin, statement, commit) {
            (Ok(()), Ok(rows), Ok(())) => {
                self.state = State::Idle;
                self.publish_ormcache();
                return Ok(rows);
            }
            other => other,
        };
        let db = |r: &std::result::Result<(), tokio_postgres::Error>| {
            r.as_ref().err().is_none_or(|e| e.as_db_error().is_some())
        };
        let (begin, statement, commit) = (statement.0, statement.1.map(|_| ()), statement.2);
        // a missing answer anywhere, or a refused BEGIN whose statement and COMMIT went through
        // (they may have run outside a transaction): the outcome is not known
        let unknown = !db(&begin)
            || !db(&statement)
            || !db(&commit)
            || (begin.is_err() && statement.is_ok() && commit.is_ok());
        // the server answered COMMIT (with ROLLBACK after a refusal): the connection is clean
        let answered = commit.is_ok();
        // the first refusal is the cause
        let cause = match (begin, statement, commit) {
            (Err(e), _, _) | (Ok(()), Err(e), _) | (Ok(()), Ok(()), Err(e)) => e,
            (Ok(()), Ok(()), Ok(())) => {
                self.release();
                return Err(CommitError::Unknown(erp_fields::Error::programming(
                    "the one-flight commit reported an error without one",
                )));
            }
        };
        let error = self.commit_failed(last.sql, cause);
        if unknown {
            self.release();
            return Err(CommitError::Unknown(error));
        }
        if answered {
            self.state = State::Idle;
            self.reset();
        } else {
            self.release();
        }
        Err(CommitError::RolledBack(error))
    }

    /// Apply the generations the opening flight read before the first statement runs.
    /// `Some(reason)`: on the read pool, the policy cannot be pinned to the replica's snapshot
    /// (the caller begins again on the primary).
    async fn apply_observed(
        &mut self,
        observed: &crate::signal::Generations,
        read_epoch: u64,
    ) -> Result<Option<crate::route::Reason>> {
        let applied = self.shared.apply_signal(observed, read_epoch).await;
        match applied {
            Ok(crate::signal::Applied::Superseded) => {
                Err(self.discard_after(crate::signal::superseded_error()).await)
            }
            Ok(crate::signal::Applied::UpToDate | crate::signal::Applied::InPlace) => {
                // an applied registry change moved the schema epoch: this connection's cached
                // statements may describe old shapes
                let shared = std::sync::Arc::clone(&self.shared);
                shared.refresh_statements(self.conn()?);
                // the snapshot of the generation this transaction read (a policy change was
                // applied above). On the read pool it must not be newer than the replica's
                // snapshot: pinned with no refresh running and nothing applied past `observed`.
                if self.route == crate::route::Route::ReadPool {
                    let Some(signal) = shared.signal.as_ref() else {
                        // never left Open on the replica without a guard
                        self.release();
                        return Err(erp_fields::Error::programming(
                            "a transaction on the read pool must read the signal generations",
                        ));
                    };
                    match signal.pin_not_ahead(observed, || shared.policy.current()) {
                        Ok(policy) => self.policy = policy,
                        Err(why) => {
                            return Ok(Some(match why {
                                crate::signal::PinRefused::RefreshRunning => {
                                    crate::route::Reason::RefreshRunning
                                }
                                crate::signal::PinRefused::Behind(keys) => {
                                    crate::route::Reason::ReplicaBehind(keys)
                                }
                            }));
                        }
                    }
                } else {
                    self.policy = shared.policy.current();
                }
                Ok(None)
            }
            Err(error) => Err(self.discard_after(error).await),
        }
    }
}
