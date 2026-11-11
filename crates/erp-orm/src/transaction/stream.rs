//! Statements whose rows are decoded as the driver receives them (PERF.STREAM-CORE, design note
//! ).
//!
//! `tokio-postgres` hands the rows of a statement over one at a time (`query_raw`,
//! `query_typed_raw`); collecting them first (`Client::query`) holds every row, its column ranges
//! and the received wire bytes until the last one has arrived. Every ORM statement that fills the
//! cache goes through [`Transaction::query_each_with`] instead: a `sink` decodes each row into the cache
//! and the row is dropped, so the transient driver memory is one received batch whatever the row
//! count. Statements, flights and round trips are the same as with the collecting calls.
//!
//! Rows arrive as their `DataRow` bodies (`RowStream::poll_next_body` of the vendored driver, ADR
//! 0055) and are lent to the sink as a [`RawRow`] over field ranges the transaction reuses, so the
//! driver allocates nothing per row.
//!
//! Backpressure: the driver gives each request a channel of one batch and stops reading the
//! socket while that channel is full, so the rows of a statement must be consumed before anything
//! behind them on the connection can arrive. A stream therefore borrows the transaction mutably
//! (nothing else can start on the connection while rows flow; the sink gets the cache, not the
//! transaction), and a flight drains its statements strictly in order
//! ([`drain_flight`](super::pipelined)).

use std::fmt::Write as _;
use std::pin::pin;

use erp_fields::{Error, Result};
use erp_sql::param::Param;
use tokio_postgres::RowStream;
use tokio_postgres::types::{ToSql, Type};

use super::PipelinedQuery;
use super::raw_row::{RawRow, RowRanges};
use super::{Transaction, behind_closes, released_error};
use crate::cache::Cache;
use crate::search::{PlanPolicy, bind, param_type};

/// The bound parameters of one statement and how it is executed.
#[derive(Clone, Copy)]
pub(crate) enum Args<'a> {
    /// A named prepared statement cached on the connection.
    Prepared(&'a [&'a (dyn ToSql + Sync)]),
    /// An unnamed statement with typed parameters, planned for these values.
    Typed(&'a [(&'a (dyn ToSql + Sync), Type)]),
}

/// How the rows of one statement ended.
pub(super) enum Drained {
    /// Every row was handed to the sink.
    Done,
    /// The driver or the server failed (the server's error may come after some rows).
    Driver(tokio_postgres::Error),
    /// The sink refused a row; the rest of the answer is left to the driver, which discards it
    /// once the stream is dropped.
    Sink(Error),
    /// A received row did not parse (its field ranges or count are wrong): the connection is no
    /// longer trusted and is released. The driver refuses such rows before handing them over, so
    /// this is the second line of `RowRanges::read`.
    Protocol(Error),
}

/// Hand every row of `stream` to `sink`, one at a time, until the answer ends or fails. Each row
/// is lent as a [`RawRow`] over `ranges` and its body is dropped right after the sink returns.
pub(super) async fn drain<F>(
    stream: RowStream,
    cache: &mut Cache,
    ranges: &mut RowRanges,
    sink: &mut F,
) -> Drained
where
    F: FnMut(&mut Cache, &RawRow<'_>) -> Result<()>,
{
    let mut stream = pin!(stream);
    loop {
        match std::future::poll_fn(|cx| stream.as_mut().poll_next_body(cx)).await {
            None => return Drained::Done,
            Some(Err(e)) => return Drained::Driver(e),
            Some(Ok(body)) => {
                let row = match ranges.read(stream.columns(), &body) {
                    Ok(row) => row,
                    Err(e) => return Drained::Protocol(e),
                };
                if let Err(e) = sink(cache, &row) {
                    return Drained::Sink(e);
                }
            }
        }
    }
}

/// The planner setting of a stream's first flight (design note `perf-stream.md` §3.4 "Planner",
/// lead decision: the cursor gets the plan the single statement gets). Sent once per transaction
/// (`SET LOCAL` lasts until the transaction ends, or until a rollback to a savepoint opened
/// before it).
const SET_FRACTION: &str = "SET LOCAL cursor_tuple_fraction = 1";

/// Write `FETCH FORWARD window FROM erp_c<number>` into `fetch`, reusing its capacity.
pub(crate) fn write_fetch(fetch: &mut String, window: usize, number: u32) {
    fetch.clear();
    // writing into a `String` cannot fail
    let _ = write!(fetch, "FETCH FORWARD {window} FROM erp_c{number}");
}

/// The statements of a stream's first flight: the planner setting (only when the transaction
/// has not sent it yet), the `DECLARE` and the `FETCH` (last).
fn first_flight<'a>(
    with_fraction: bool,
    declare: &'a str,
    params: &[Param],
    policy: PlanPolicy,
    fetch: &'a str,
) -> Result<Vec<PipelinedQuery<'a>>> {
    let mut statements = Vec::with_capacity(3);
    if with_fraction {
        statements.push(PipelinedQuery::new(SET_FRACTION, &[], PlanPolicy::PerCall)?);
    }
    statements.push(PipelinedQuery::new(declare, params, policy)?);
    statements.push(PipelinedQuery::new(fetch, &[], PlanPolicy::PerCall)?);
    Ok(statements)
}

impl Transaction {
    /// The plan policy of a statement the ORM generated: unnamed when it carries literals, and
    /// always unnamed under a private policy snapshot (no prepared statement outlives the
    /// transaction's policy, `search` design note §2.3).
    pub(crate) fn plan_policy(&self, params: &[Param]) -> PlanPolicy {
        if self.policy().snapshot().is_shared() {
            PlanPolicy::of(params)
        } else {
            PlanPolicy::PerCall
        }
    }

    /// The plan policy of an ORM-produced id-list fetch whose only literal is the id list
    /// : a cached prepared statement, still unnamed under a private policy snapshot
    /// like every other statement ([`Transaction::plan_policy`]).
    pub(crate) fn plan_policy_id_list(&self) -> PlanPolicy {
        if self.policy().snapshot().is_shared() {
            PlanPolicy::Cached
        } else {
            PlanPolicy::PerCall
        }
    }

    /// Run one statement and hand each row to `sink` as the driver receives it (design note
    /// `perf-stream.md` §3.1). The statement is counted like [`Transaction::query`].
    ///
    /// # Errors
    ///
    /// A parameter that cannot be bound (nothing is sent); the statement's error, mapped as for
    /// [`Transaction::query`] (a server error aborts the transaction, also when it arrives after
    /// some rows: the rows cached before it are real values of the snapshot); the first error
    /// the sink returns, as it is: the driver then discards the rest of the answer, so the next
    /// statement on the transaction waits behind it and runs normally.
    pub(crate) async fn query_each_with<F>(
        &mut self,
        sql: &str,
        params: &[Param],
        policy: PlanPolicy,
        sink: F,
    ) -> Result<()>
    where
        F: FnMut(&mut Cache, &RawRow<'_>) -> Result<()>,
    {
        let bound = bind(params)?;
        let refs: Vec<&(dyn ToSql + Sync)> = bound
            .iter()
            .map(|p| p.as_ref() as &(dyn ToSql + Sync))
            .collect();
        match policy {
            PlanPolicy::Cached => self.query_each_args(sql, Args::Prepared(&refs), sink).await,
            PlanPolicy::PerCall => {
                let typed: Vec<(&(dyn ToSql + Sync), Type)> = refs
                    .iter()
                    .zip(params)
                    .map(|(value, param)| (*value, param_type(param)))
                    .collect();
                self.query_each_args(sql, Args::Typed(&typed), sink).await
            }
        }
    }

    /// Run a statement and lend each row to `each` as it arrives, without collecting the result
    /// (design note `perf-stream-window.md` §1): the memory a large answer costs is one received
    /// batch of the driver, whatever its row count. Prepared and cached per connection, counted
    /// and checked like [`Transaction::query`] (raw SQL: no access rules apply, the read-only
    /// guard does). Returns the number of rows lent.
    ///
    /// ```no_run
    /// # async fn demo(tx: &mut erp_orm::Transaction) -> erp_fields::Result<()> {
    /// let mut total = 0.0;
    /// tx.query_each(
    ///     "SELECT qty FROM bench_order_line WHERE order_id = $1",
    ///     &[&7i64],
    ///     |row| {
    ///         total += row.try_get::<Option<f64>>(0)?.unwrap_or(0.0);
    ///         Ok(())
    ///     },
    /// )
    /// .await?;
    /// # Ok(()) }
    /// ```
    ///
    /// # Errors
    ///
    /// As [`Transaction::query`]: a server error aborts the transaction, also after some rows were
    /// lent. The first error `each` returns stops the rows and is returned as it is; the rest of
    /// the answer is discarded and the transaction goes on.
    pub async fn query_each<F>(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
        mut each: F,
    ) -> Result<u64>
    where
        F: FnMut(&super::raw_row::SqlRow<'_>) -> Result<()>,
    {
        let mut rows = 0u64;
        self.query_each_args(sql, Args::Prepared(params), |_, raw| {
            rows += 1;
            each(&super::raw_row::SqlRow::new(raw))
        })
        .await?;
        Ok(rows)
    }

    /// [`Transaction::query_each_with`] with parameters already bound.
    ///
    /// # Errors
    ///
    /// As [`Transaction::query_each_with`].
    pub(crate) async fn query_each_args<F>(
        &mut self,
        sql: &str,
        args: Args<'_>,
        sink: F,
    ) -> Result<()>
    where
        F: FnMut(&mut Cache, &RawRow<'_>) -> Result<()>,
    {
        self.require_writable_sql(sql)?;
        // a `DELETE … RETURNING` streamed here forgets what the follow-delete triggers changed
        // (review C3-X2)
        self.follow_sql_deletes(sql);
        self.ensure_open().await?;
        self.open_isolation(Some(sql)).await?;
        self.each_args_open(sql, args, false, sink).await
    }

    /// [`Transaction::query_each_args`] once the transaction is open and the statement allowed:
    /// pending `CLOSE`s ride in front of it, the statement is counted and its rows drained. A
    /// stream's `FETCH` starts here with `own`: it skips the read-only guard's and the isolated
    /// scope's classifier, and is counted without the raw-SQL classifier
    /// ([`Transaction::record_own`]).
    pub(crate) async fn each_args_open<F>(
        &mut self,
        sql: &str,
        args: Args<'_>,
        own: bool,
        mut sink: F,
    ) -> Result<()>
    where
        F: FnMut(&mut Cache, &RawRow<'_>) -> Result<()>,
    {
        let closes = self.take_closes();
        if own {
            self.record_own(sql);
        } else {
            self.record(sql);
        }
        if matches!(args, Args::Prepared(_)) {
            let conn = self.conn()?;
            self.bound_statement_cache(conn);
        }
        let (closed, outcome) = {
            // the connection, the cache and the range buffer are disjoint fields: the stream reads
            // the first while the sink writes the cache, and nothing else can use the transaction
            // meanwhile
            let conn = self.conn.as_ref().ok_or_else(released_error)?;
            let cache = &mut self.cache;
            let ranges = &mut self.row_ranges;
            behind_closes!(conn, closes.as_ref(), async {
                let opened = match args {
                    Args::Prepared(params) => match conn.prepare_cached(sql).await {
                        Ok(statement) => conn.query_raw(&statement, params.iter().copied()).await,
                        Err(e) => Err(e),
                    },
                    Args::Typed(params) => {
                        conn.query_typed_raw(sql, params.iter().map(|(v, t)| (*v, t.clone())))
                            .await
                    }
                };
                match opened {
                    Ok(stream) => drain(stream, cache, ranges, &mut sink).await,
                    Err(e) => Drained::Driver(e),
                }
            })
        };
        if let Drained::Protocol(e) = outcome {
            self.release();
            return Err(e);
        }
        self.closes_sent(closes, closed)?;
        match outcome {
            Drained::Done => Ok(()),
            Drained::Driver(e) => Err(self.statement_failed_in(sql, e)),
            Drained::Sink(e) => Err(e),
            Drained::Protocol(e) => {
                self.release();
                Err(e)
            }
        }
    }

    /// The first window of a cursor stream (design note `perf-stream.md` §3.3,
    /// `perf-stream-window.md` §1.1 step 4): one flight of `SET LOCAL cursor_tuple_fraction = 1`
    /// (the transaction's first stream only), `DECLARE erp_cN NO SCROLL CURSOR WITHOUT HOLD FOR
    /// <select>` (prepared under `policy`) and `FETCH FORWARD window FROM erp_cN` (unnamed), with
    /// the pending `CLOSE`s of finished streams in front. The cursor takes its number after those
    /// `CLOSE`s were taken, so a number they free is used again. The `FETCH` text is written into
    /// `fetch` (the stream's reused buffer) and every fetched row goes to `sink`. The stream's
    /// statements skip the raw-SQL classifier (they are the ORM's own reads, allowed in a
    /// read-only transaction).
    ///
    /// # Errors
    ///
    /// `Programming` when the transaction has its limit of open cursors
    /// ([`Transaction::set_cursor_limit`]); a parameter that cannot be bound; the flight's errors
    /// ([`Transaction::query_flight`]). When the flight fails and leaves the transaction aborted
    /// or released, the cursor is forgotten (the transaction ends it); when only the sink refused
    /// a row, it exists and is closed in front of the next statement. A flight whose future is
    /// dropped leaves the cursor's number taken until the transaction ends (whether the server
    /// ran the `DECLARE` is unknown).
    pub(crate) async fn cursor_first_window<F>(
        &mut self,
        select: &str,
        params: &[Param],
        policy: PlanPolicy,
        window: usize,
        fetch: &mut String,
        mut sink: F,
    ) -> Result<super::cursors::CursorHandle>
    where
        F: FnMut(&mut Cache, &RawRow<'_>) -> Result<()>,
    {
        self.ensure_open().await?;
        let closes = self.take_closes();
        let depth = self.savepoint_depth();
        let handle = match self.cursors.declare(depth) {
            Ok(handle) => handle,
            Err(e) => {
                self.closes_unsent(closes);
                return Err(e);
            }
        };
        let declare = format!(
            "DECLARE erp_c{} NO SCROLL CURSOR WITHOUT HOLD FOR {select}",
            handle.number()
        );
        write_fetch(fetch, window, handle.number());
        let with_fraction = self.cursors.fraction_needed();
        let statements = match first_flight(with_fraction, &declare, params, policy, fetch) {
            Ok(statements) => statements,
            Err(e) => {
                handle.forget();
                self.closes_unsent(closes);
                return Err(e);
            }
        };
        let fetch_at = statements.len() - 1;
        let fetched = self
            .flight_with_closes(closes, &statements, true, |i, cache, row| {
                if i == fetch_at {
                    sink(cache, row)
                } else {
                    Err(Error::programming(format!(
                        "statement {i} of a stream's first flight returned a row"
                    )))
                }
            })
            .await;
        match fetched {
            Ok(()) => {
                handle.declared();
                if with_fraction {
                    self.cursors.fraction_sent(depth);
                }
                self.cursors.fetched(handle.number(), depth)?;
                Ok(handle)
            }
            Err(e) => {
                if self.is_aborted_or_released() {
                    handle.forget();
                } else {
                    // only the sink refused a row: every statement ran, the cursor exists and is
                    // closed in front of the next statement when the handle drops
                    handle.declared();
                    if with_fraction {
                        self.cursors.fraction_sent(depth);
                    }
                }
                Err(e)
            }
        }
    }

    /// A later window of a cursor stream: `fetch` (`FETCH FORWARD w FROM erp_c<number>`, unnamed)
    /// with the pending `CLOSE`s in front, every row to `sink`.
    ///
    /// # Errors
    ///
    /// As [`Transaction::query_each_with`]; an aborted transaction refuses before sending.
    pub(crate) async fn cursor_window<F>(&mut self, fetch: &str, number: u32, sink: F) -> Result<()>
    where
        F: FnMut(&mut Cache, &RawRow<'_>) -> Result<()>,
    {
        self.ensure_open().await?;
        self.each_args_open(fetch, Args::Typed(&[]), true, sink)
            .await?;
        let depth = self.savepoint_depth();
        self.cursors.fetched(number, depth)
    }

    /// Run one statement through [`Transaction::query_each_with`] for a test of its error rules, with
    /// `sink` called for every row; the rows handed over. Only with the `test-faults` feature.
    /// `cached` prepares the statement on the connection, else it is unnamed with typed
    /// parameters.
    ///
    /// # Errors
    ///
    /// As [`Transaction::query_each_with`].
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub async fn query_each_for_tests(
        &mut self,
        sql: &str,
        params: &[Param],
        cached: bool,
        mut sink: impl FnMut(&RawRow<'_>) -> Result<()>,
    ) -> Result<u64> {
        let policy = if cached {
            PlanPolicy::Cached
        } else {
            PlanPolicy::PerCall
        };
        let mut rows = 0u64;
        self.query_each_with(sql, params, policy, |_, row| {
            rows += 1;
            sink(row)
        })
        .await?;
        Ok(rows)
    }
}
