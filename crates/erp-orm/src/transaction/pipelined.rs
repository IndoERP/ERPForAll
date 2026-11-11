//! Independent statements sent in one flight on the transaction's connection (design
//! note), their rows decoded as they arrive (PERF.STREAM-CORE,
//! ).
//!
//! `tokio-postgres` writes the requests of futures polled together one after the other before
//! the first answer is read, so N independent reads cost one round trip instead of N. A
//! `Transaction` still has one owner: the flight is one `&mut self` call that returns when every
//! answer is in, so nothing else touches the transaction meanwhile.
//!
//! A flight has two phases. The cached statements that are not prepared on the connection yet are
//! prepared together first (one round trip for any number of them; nothing at all when every
//! one is already cached). Then every statement is executed, in the order given, in one round
//! trip. So the order on the wire is the order given, and a flight costs one round trip with a
//! warm connection and two with a cold one. The round trips can be more when the answers of
//! the executes come back in several network reads and the driver writes between them (the
//! client never waits for an answer between statements, but the writes may be split), and
//! parameter types that are not built in cost a type lookup the first time.
//!
//! **Draining order.** The driver gives each request a channel of one batch and stops reading the
//! socket while a channel is full (`tokio-postgres` 0.7.18 `client.rs` `InnerClient::send`,
//! `connection.rs` `poll_read`), so statement 2's first message sits behind statement 1's rows
//! until those are consumed. [`drain_flight`] is the one function that drains a flight: it puts
//! every request on the wire first (the first poll of each future sends it), then awaits and
//! drains statement 1 completely before it awaits statement 2, and so on. Awaiting the futures
//! together while their rows wait unconsumed would deadlock as soon as an answer exceeds the
//! driver's buffers. For the same reason the per-call statements of a flight read only built-in
//! column types: a column type the driver must look up would send its lookup behind the later
//! statements of the flight, whose rows wait for this one to be drained. Every flight the ORM
//! sends selects record columns, whose types are all built in.

use std::future::Future;
use std::pin::Pin;
use std::task::Poll;

use erp_fields::Result;
use erp_sql::param::Param;
use tokio_postgres::types::{ToSql, Type};
use tokio_postgres::{RowStream, Statement};

use super::raw_row::{RawRow, RowRanges};
use super::stream::{Drained, drain};
use super::{Transaction, behind_closes, released_error};
use crate::cache::Cache;
use crate::search::{Bound, PlanPolicy, typed_params};

/// The most statements one flight carries. The rows of each statement are decoded as they
/// arrive, one statement after the other, so the cap no longer bounds rows held in memory: it
/// bounds the requests and bound parameters in flight (each at most
/// [`OrmConfig::prefetch_chunk`](crate::OrmConfig::prefetch_chunk) ids) and the answers the
/// server keeps waiting while an earlier one is drained. Callers with more statements send
/// several flights (one more round trip per flight) or run them one by one.
pub(crate) const MAX_FLIGHT_STATEMENTS: usize = 16;

/// One statement of a flight, its parameters bound when it is built: a parameter that cannot be
/// bound fails before anything is counted or sent.
pub(crate) struct PipelinedQuery<'a> {
    sql: &'a str,
    values: Bound,
    types: Vec<Type>,
    policy: PlanPolicy,
}

impl<'a> PipelinedQuery<'a> {
    /// A statement with `params` bound now.
    ///
    /// # Errors
    ///
    /// A parameter that cannot be bound ([`crate::search::bind`]).
    pub(crate) fn new(sql: &'a str, params: &[Param], policy: PlanPolicy) -> Result<Self> {
        let (values, types) = typed_params(params)?;
        Ok(Self::bound(sql, values, types, policy))
    }

    /// A statement whose parameters are already bound, with their driver types (one per value).
    pub(crate) fn bound(sql: &'a str, values: Bound, types: Vec<Type>, policy: PlanPolicy) -> Self {
        Self {
            sql,
            values,
            types,
            policy,
        }
    }
}

type Pending<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
type Outcome<T> = std::result::Result<T, tokio_postgres::Error>;

/// How a statement of a flight is executed once its plan is settled.
#[derive(Clone)]
enum Plan {
    /// Prepared on the connection (cached).
    Prepared(Statement),
    /// Unnamed with typed parameters.
    PerCall,
}

/// Poll every future in order until all are done; the first poll of each puts its request on the
/// wire, so the requests leave in the order given. Every output is `Some` when this returns.
/// Only for futures that do not wait for each other's answers to be consumed (the prepares);
/// row answers go through [`drain_flight`].
async fn poll_in_order<T>(futures: Vec<Pending<'_, T>>) -> Vec<Option<T>> {
    let mut futures = futures;
    let mut out: Vec<Option<T>> = futures.iter().map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (future, slot) in futures.iter_mut().zip(out.iter_mut()) {
            if slot.is_none() {
                match future.as_mut().poll(cx) {
                    Poll::Ready(value) => *slot = Some(value),
                    Poll::Pending => pending = true,
                }
            }
        }
        if pending {
            Poll::Pending
        } else {
            Poll::Ready(std::mem::take(&mut out))
        }
    })
    .await
}

/// A statement of a flight between its first poll and its rows.
enum Opening<'a> {
    /// Sent; its answer has not started yet.
    Waiting(Pending<'a, Outcome<RowStream>>),
    /// The answer started (or the request failed) during the first poll.
    Started(Outcome<RowStream>),
}

/// What draining a flight found: the driver errors with their statement's index, the first
/// error a sink returned, and the first row that did not parse (the connection is then released).
struct FlightEnd {
    errors: Vec<(usize, tokio_postgres::Error)>,
    sink: Option<erp_fields::Error>,
    protocol: Option<erp_fields::Error>,
}

/// Drain the answers of a flight in order (see the module documentation, "Draining order"): every
/// request goes on the wire first, then statement `i`'s rows are handed to `sink(i, …)` until its
/// answer ends before statement `i + 1` is awaited. After the first error (a driver error or a
/// sink's), the remaining answers are still drained, without the sink, so a server error later
/// in the flight is known ([`root_cause`]) and the connection is left at a statement boundary.
async fn drain_flight<F>(
    opens: Vec<Pending<'_, Outcome<RowStream>>>,
    cache: &mut Cache,
    ranges: &mut RowRanges,
    sink: &mut F,
) -> FlightEnd
where
    F: FnMut(usize, &mut Cache, &RawRow<'_>) -> Result<()>,
{
    let mut opens: Vec<Opening<'_>> = opens.into_iter().map(Opening::Waiting).collect();
    // every request on the wire: one poll each, in order
    std::future::poll_fn(|cx| {
        for open in &mut opens {
            let started = match open {
                Opening::Waiting(future) => match future.as_mut().poll(cx) {
                    Poll::Ready(outcome) => Some(outcome),
                    Poll::Pending => None,
                },
                Opening::Started(_) => None,
            };
            if let Some(outcome) = started {
                *open = Opening::Started(outcome);
            }
        }
        Poll::Ready(())
    })
    .await;
    let mut end = FlightEnd {
        errors: Vec::new(),
        sink: None,
        protocol: None,
    };
    for (i, open) in opens.into_iter().enumerate() {
        let outcome = match open {
            Opening::Waiting(future) => future.await,
            Opening::Started(outcome) => outcome,
        };
        let stream = match outcome {
            Ok(stream) => stream,
            Err(e) => {
                end.errors.push((i, e));
                continue;
            }
        };
        let failed = !end.errors.is_empty() || end.sink.is_some() || end.protocol.is_some();
        let mut each = |cache: &mut Cache, row: &RawRow<'_>| {
            if failed {
                // the flight already failed: the rows are read and dropped
                Ok(())
            } else {
                sink(i, cache, row)
            }
        };
        match drain(stream, cache, ranges, &mut each).await {
            Drained::Done => {}
            Drained::Driver(e) => end.errors.push((i, e)),
            Drained::Sink(e) => end.sink = Some(e),
            Drained::Protocol(e) => {
                if end.protocol.is_none() {
                    end.protocol = Some(e);
                }
            }
        }
    }
    end
}

/// The error a failed flight reports. Server errors come before client-side ones (a parameter
/// that could not be encoded never reached the server, so it cannot be why the transaction is
/// aborted), and among server errors the first that is not "current transaction is aborted"
/// (`25P02`, which only reports an earlier failure of the same flight) is the root cause. A
/// flight with a server error always reports one, so the transaction is marked aborted whatever
/// else failed on the client.
fn root_cause(
    errors: Vec<(usize, tokio_postgres::Error)>,
) -> Option<(usize, tokio_postgres::Error)> {
    let rank = |e: &tokio_postgres::Error| match e.code() {
        Some(code) if code.code() != "25P02" => 0,
        Some(_) => 1,
        None => 2,
    };
    errors.into_iter().min_by_key(|(i, e)| (rank(e), *i))
}

impl Transaction {
    /// Run independent statements in one flight and hand each row to `sink` with its
    /// statement's index, statement by statement in the order given ([`drain_flight`]). The
    /// statements are counted like sequential ones ([`Transaction::query_count`]).
    ///
    /// When several fail, the error is the root cause ([`root_cause`]); the transaction is
    /// aborted like after any server error. A driver or server error comes before a sink's error;
    /// a sink's error stops the sink for the rest of the flight (the remaining rows are read and
    /// dropped) and is returned as it is. A future dropped mid-flight leaves the transaction
    /// open on the server in an unknown state, like a dropped sequential `query`: callers drop
    /// the whole `Transaction` on a deadline, and `Drop` then detaches the connection.
    ///
    /// # Errors
    ///
    /// `Programming` for more than [`MAX_FLIGHT_STATEMENTS`] statements; the statements' errors
    /// (when the sink also refused a row, its error is added to the message: "… (and …)"); the
    /// sink's first error.
    pub(crate) async fn query_flight<F>(
        &mut self,
        statements: &[PipelinedQuery<'_>],
        sink: F,
    ) -> Result<()>
    where
        F: FnMut(usize, &mut Cache, &RawRow<'_>) -> Result<()>,
    {
        if statements.is_empty() {
            return Ok(());
        }
        if statements.len() > MAX_FLIGHT_STATEMENTS {
            return Err(erp_fields::Error::programming(format!(
                "a flight carries at most {MAX_FLIGHT_STATEMENTS} statements, got {}",
                statements.len()
            )));
        }
        for statement in statements {
            self.require_writable_sql(statement.sql)?;
        }
        // a flight statement deleting from a followed table forgets what the follow-delete
        // triggers changed, as a sequential one does (review C3-X2)
        for statement in statements {
            self.follow_sql_deletes(statement.sql);
        }
        self.ensure_open().await?;
        for statement in statements {
            self.open_isolation(Some(statement.sql)).await?;
        }
        let closes = self.take_closes();
        self.flight_with_closes(closes, statements, false, sink)
            .await
    }

    /// [`Transaction::query_flight`] once the transaction is open and its statements allowed,
    /// with the pending `CLOSE`s already taken ([`Transaction::take_closes`]): they ride in front
    /// of the flight's executions, the statements are counted and run. A stream's first window
    /// starts here with `own`: it takes the closes first, so the number of a cursor they close is
    /// free for its `DECLARE`, and its statements are counted without the raw-SQL classifier
    /// ([`Transaction::record_own`]).
    ///
    /// The statements are classified before any is sent, but a per-call statement is parsed by
    /// the server only after the one before it completed: when the server reported a new string
    /// reading during the flight (`Transaction::note_string_reading`, read again once the flight
    /// returned, with or without an error), a later statement of it may have been read
    /// differently from the scanner, so the flight's effects become unknown after the fact (the
    /// remembered record-rule decisions end, every model counts as written; review round 6, L1).
    /// A savepoint decided before the flight is not revisited: flight statements are built by
    /// the ORM with their values bound, so none of them changes a setting today. A flight whose
    /// future is dropped mid-flight skips the re-read; `Drop` and the reroute to the primary
    /// still close the connection, since they compare the driver's count with 0.
    pub(crate) async fn flight_with_closes<F>(
        &mut self,
        closes: Option<super::cursors::Closes>,
        statements: &[PipelinedQuery<'_>],
        own: bool,
        sink: F,
    ) -> Result<()>
    where
        F: FnMut(usize, &mut Cache, &RawRow<'_>) -> Result<()>,
    {
        let result = self.run_flight(closes, statements, own, sink).await;
        if self.note_string_reading() {
            self.flight_effects_unknown();
        }
        result
    }

    /// The flight of [`Transaction::flight_with_closes`], before the string reading is read again.
    async fn run_flight<F>(
        &mut self,
        closes: Option<super::cursors::Closes>,
        statements: &[PipelinedQuery<'_>],
        own: bool,
        mut sink: F,
    ) -> Result<()>
    where
        F: FnMut(usize, &mut Cache, &RawRow<'_>) -> Result<()>,
    {
        for statement in statements {
            if own {
                self.record_own(statement.sql);
            } else {
                self.record(statement.sql);
            }
        }
        let (plans, errors) = {
            let conn = match self.conn() {
                Ok(conn) => conn,
                Err(e) => {
                    self.closes_unsent(closes);
                    return Err(e);
                }
            };
            self.bound_statement_cache(conn);
            // phase 1: prepare what is not cached yet, all together; hits answer at once and
            // send nothing. The cache can exceed its size by the misses of one flight. The chunks
            // of one comodel have the same text: it is prepared once and the statement shared.
            let first_of: Vec<usize> = statements
                .iter()
                .enumerate()
                .map(|(i, statement)| {
                    statements[..i]
                        .iter()
                        .position(|earlier| {
                            earlier.policy == PlanPolicy::Cached
                                && statement.policy == PlanPolicy::Cached
                                && earlier.sql == statement.sql
                        })
                        // no earlier statement with this text: it is its own first (not a fallback)
                        .unwrap_or(i)
                })
                .collect();
            let prepares: Vec<Pending<'_, Outcome<Plan>>> = statements
                .iter()
                .enumerate()
                .map(|(i, statement)| -> Pending<'_, Outcome<Plan>> {
                    match statement.policy {
                        // a repeat of an earlier text takes that statement's plan below
                        PlanPolicy::Cached if first_of[i] != i => {
                            Box::pin(std::future::ready(Ok(Plan::PerCall)))
                        }
                        PlanPolicy::Cached => Box::pin(async move {
                            conn.prepare_cached(statement.sql).await.map(Plan::Prepared)
                        }),
                        PlanPolicy::PerCall => Box::pin(std::future::ready(Ok(Plan::PerCall))),
                    }
                })
                .collect();
            let mut plans = Vec::with_capacity(statements.len());
            let mut errors = Vec::new();
            for (i, outcome) in poll_in_order(prepares).await.into_iter().enumerate() {
                match outcome {
                    Some(Ok(plan)) => plans.push(plan),
                    Some(Err(e)) => errors.push((i, e)),
                    None => {
                        self.closes_unsent(closes);
                        return Err(self.statement_without_answer(statements[i].sql));
                    }
                }
            }
            for (i, first) in first_of.iter().copied().enumerate() {
                if first != i && errors.is_empty() {
                    plans[i] = plans[first].clone();
                }
            }
            (plans, errors)
        };
        if let Some((i, e)) = root_cause(errors) {
            self.closes_unsent(closes);
            return Err(self.statement_failed_in(statements[i].sql, e));
        }
        // phase 2: every statement executes, in the order given, in one flight; the answers are
        // drained in that order
        let end = {
            // the connection and the cache are disjoint fields: the streams read one while the
            // sink writes the other
            let conn = self.conn.as_ref().ok_or_else(released_error)?;
            let cache = &mut self.cache;
            let ranges = &mut self.row_ranges;
            let refs: Vec<Vec<&(dyn ToSql + Sync)>> = statements
                .iter()
                .map(|s| {
                    s.values
                        .iter()
                        .map(|v| v.as_ref() as &(dyn ToSql + Sync))
                        .collect()
                })
                .collect();
            let opens: Vec<Pending<'_, Outcome<RowStream>>> = statements
                .iter()
                .zip(&plans)
                .zip(&refs)
                .map(
                    |((statement, plan), refs)| -> Pending<'_, Outcome<RowStream>> {
                        match plan {
                            Plan::Prepared(prepared) => {
                                Box::pin(conn.query_raw(prepared, refs.iter().copied()))
                            }
                            Plan::PerCall => Box::pin(conn.query_typed_raw(
                                statement.sql,
                                refs.iter().copied().zip(statement.types.iter().cloned()),
                            )),
                        }
                    },
                )
                .collect();
            behind_closes!(
                conn,
                closes.as_ref(),
                drain_flight(opens, cache, ranges, &mut sink)
            )
        };
        let (closed, end) = end;
        if let Some(e) = end.protocol {
            // a row that does not parse: nothing else the connection says can be trusted
            self.release();
            return Err(e);
        }
        self.closes_sent(closes, closed)?;
        if let Some((i, e)) = root_cause(end.errors) {
            let failed = self.statement_failed_in(statements[i].sql, e);
            // the sink refused a row before the statement failed: the statement's error is the
            // one returned (it decides the transaction's state; its source stays the driver's
            // error), the refusal rides in its message
            return Err(match &end.sink {
                Some(refused) => failed.and_also(refused),
                None => failed,
            });
        }
        match end.sink {
            None => Ok(()),
            Some(e) => Err(e),
        }
    }

    /// Run a flight of the given statements for a test of its rules, returning the rows each
    /// statement answered. Only with the `test-faults` feature. A statement is `(sql, params,
    /// cached)`: `cached` prepares it on the connection (`PlanPolicy::Cached`), else it is unnamed
    /// with typed parameters (`PlanPolicy::PerCall`).
    /// `refuse = Some((i, n))` makes the sink refuse row `n` (from 1) of statement `i` with the
    /// `Database` error "refused row n of statement i".
    ///
    /// # Errors
    ///
    /// As the flight's ([`Transaction::query_flight`]).
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub async fn flight_for_tests(
        &mut self,
        statements: &[(&str, &[Param], bool)],
        refuse: Option<(usize, u64)>,
    ) -> Result<Vec<u64>> {
        let queries = statements
            .iter()
            .map(|(sql, params, cached)| {
                PipelinedQuery::new(
                    sql,
                    params,
                    if *cached {
                        PlanPolicy::Cached
                    } else {
                        PlanPolicy::PerCall
                    },
                )
            })
            .collect::<Result<Vec<_>>>()?;
        let mut rows = vec![0u64; queries.len()];
        self.query_flight(&queries, |i, _, _| {
            let n = rows.get_mut(i).ok_or_else(|| {
                erp_fields::Error::programming(format!("a flight row named statement {i}"))
            })?;
            *n += 1;
            if refuse == Some((i, *n)) {
                return Err(erp_fields::Error::db(format!(
                    "refused row {n} of statement {i}"
                )));
            }
            Ok(())
        })
        .await?;
        Ok(rows)
    }

    /// A statement of a flight has no answer: the flight returned before polling it to the end,
    /// which only a bug in the flight can cause. The connection state is unknown, so it is dropped.
    fn statement_without_answer(&mut self, sql: &str) -> erp_fields::Error {
        self.release();
        erp_fields::Error::programming(format!(
            "internal error: a pipelined statement got no answer ({sql})"
        ))
    }
}

#[cfg(test)]
mod tests {
    use erp_fields::{ColumnType, RangeBounds};
    use tokio_postgres::types::{Kind, Type};

    /// The type the driver reads a record column of `column` as. Exhaustive on purpose: a new
    /// [`ColumnType`] variant does not compile until it has its row here.
    fn wire_type(column: ColumnType) -> Type {
        match column {
            ColumnType::Int4 => Type::INT4,
            ColumnType::Int8 => Type::INT8,
            ColumnType::Float8 => Type::FLOAT8,
            ColumnType::Numeric => Type::NUMERIC,
            ColumnType::Varchar(_) => Type::VARCHAR,
            ColumnType::Text => Type::TEXT,
            ColumnType::Bool => Type::BOOL,
            ColumnType::Date => Type::DATE,
            ColumnType::Timestamp => Type::TIMESTAMP,
            ColumnType::Uuid => Type::UUID,
            ColumnType::Xid8 => Type::XID8,
            ColumnType::TimestampTz => Type::TIMESTAMPTZ,

            ColumnType::Jsonb => Type::JSONB,
            ColumnType::DateRange(_) => Type::DATE_RANGE,
            ColumnType::TsRange(_) => Type::TS_RANGE,
        }
    }

    /// Module documentation, "Draining order": a flight reads its columns without a type lookup,
    /// so every column type is built in: the driver resolves its OID by itself, and it is the type
    /// the catalog names for the column (not an assumption of this table).
    #[test]
    fn every_column_type_is_built_in() {
        let all = [
            ColumnType::Int4,
            ColumnType::Int8,
            ColumnType::Float8,
            ColumnType::Numeric,
            ColumnType::Varchar(None),
            ColumnType::Varchar(Some(64)),
            ColumnType::Text,
            ColumnType::Bool,
            ColumnType::Date,
            ColumnType::Timestamp,
            ColumnType::Uuid,
            ColumnType::Xid8,
            ColumnType::TimestampTz,
            ColumnType::Jsonb,
            ColumnType::DateRange(RangeBounds::HalfOpen),
            ColumnType::DateRange(RangeBounds::Closed),
            ColumnType::TsRange(RangeBounds::HalfOpen),
            ColumnType::TsRange(RangeBounds::Closed),
        ];
        for column in all {
            let ty = wire_type(column);
            assert_eq!(Type::from_oid(ty.oid()), Some(ty.clone()), "{column:?}");
            assert_eq!(ty.name(), column.udt_name(), "{column:?}");
            assert!(
                matches!(ty.kind(), Kind::Simple | Kind::Range(_)),
                "{column:?}: {:?}",
                ty.kind()
            );
        }
        // the id parameters of the flights' chunk statements are built in too
        assert_eq!(
            Type::from_oid(Type::INT8_ARRAY.oid()),
            Some(Type::INT8_ARRAY)
        );
    }
}
