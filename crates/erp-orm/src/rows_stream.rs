//! Windowed reads (PERF.STREAM-WINDOW; design notes–§3.7, §4.1
//! and).
//!
//! [`Records::stream`] reads the records of a domain window by window through a SQL cursor, and
//! [`Records::stream_records`] the records of a recordset by id chunks. Each window is lent as a
//! [`Rows`] batch equal to `read_rows` of its records; before the next window is read, the
//! window's records leave the cache (it is cut back to the slot counts taken before the window)
//! and the batch's buffers are taken back for the next window. So a stream holds one window,
//! whatever the result size: the memory of a read of a million rows is that of one window.
//!
//! The cursor is the transaction's (`WITHOUT HOLD`): it reads the snapshot of its `DECLARE`
//! and ends with the transaction. What the transaction itself writes after the `DECLARE` is not
//! in that snapshot, so once a model whose cells a window caches changed (a flushed write, a
//! recompute, a raw statement, an invalidation, a rollback to a savepoint:
//! `transaction/write_gens.rs`), the windows take only their ids from the cursor and read the
//! values fresh by id, as the id-chunk source always does; for a restricted environment, once
//! the rule's inputs or the policy changed, each window's ids are checked against the rule again
//! before it is lent. A finished, closed or dropped stream asks the transaction to close its
//! cursor; the `CLOSE` goes out in front of the transaction's next statement, never as a round
//! trip of its own, and nothing is sent when the transaction ends first.

use std::sync::Arc;

use erp_domain::Domain;
use erp_fields::rows::{Rows, RowsBuilder};
use erp_fields::{Error, FieldType, RealId, RecId, Result};
use erp_registry::{FieldId, ModelId, ModelSpec};
use erp_sql::param::Param;

use crate::Transaction;
use crate::access::Operation;
use crate::cache::{CutRefused, KeptBy, Mark};
use crate::engine::MemoKey;
use crate::read::{Load, field_of};
use crate::records::{Ids, Records};
use crate::search::{Fetch, PlanPolicy, RowCacher, SearchOptions};
use crate::transaction::cursors::{CursorHandle, TxStamp};
use crate::transaction::write_fetch;

const MIN_WINDOW: usize = 1_000;
/// The largest window (the default `prefetch_chunk`).
const MAX_WINDOW: usize = 10_000;
/// The first adaptive window: psycopg2's and Django's default fetch size.
const FIRST_WINDOW: usize = 2_000;
/// The first adaptive window when a column can hold wide values (text, html, json).
const FIRST_WIDE_WINDOW: usize = 1_000;
/// The bytes an adaptive window aims at (received `DataRow` bytes).
const WINDOW_BYTES: u64 = 1024 * 1024;

/// How the window size is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Window {
    /// 2,000 rows first (1,000 with a wide column), then 1 MiB of received rows per window,
    /// between 1,000 rows and `prefetch_chunk` (design note §3.4).
    Adaptive,
    /// This many rows per window.
    Fixed(usize),
}

/// Options of a windowed read ([`Records::stream`], [`Records::stream_records`]).
///
/// ```
/// use erp_orm::{Load, StreamOptions};
///
/// let adaptive = StreamOptions::new().order("id").load(Load::Raw);
/// let fixed = StreamOptions::new().fixed_window(5_000)?;
/// assert!(StreamOptions::new().fixed_window(10).is_err());
/// # let _ = (adaptive, fixed);
/// # Ok::<(), erp_fields::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamOptions {
    window: Window,
    order: Option<String>,
    active_test: bool,
    load: Load,
}

impl Default for StreamOptions {
    fn default() -> Self {
        Self {
            window: Window::Adaptive,
            order: None,
            active_test: true,
            load: Load::Classic,
        }
    }
}

impl StreamOptions {
    /// Adaptive windows, the model's order, the active test on, many2one values as
    /// `[id, display_name]` ([`Load::Classic`]).
    pub fn new() -> Self {
        Self::default()
    }

    /// Windows of `rows` rows.
    ///
    /// # Errors
    ///
    /// A window above the ORM's
    /// [`OrmConfig::prefetch_chunk`](crate::OrmConfig::prefetch_chunk) is refused when the stream
    /// starts.
    pub fn fixed_window(mut self, rows: usize) -> Result<Self> {
        if !(MIN_WINDOW..=MAX_WINDOW).contains(&rows) {
            return Err(Error::programming(format!(
                "StreamOptions: a fixed window of {rows} rows is outside \
                 {MIN_WINDOW}..={MAX_WINDOW} (a window is never 0 = unlimited; never below 1,000 \
                 rows, the reference ERP's export batch)"
            )));
        }
        self.window = Window::Fixed(rows);
        Ok(self)
    }

    /// Windows of any size from 1 row, for tests of window boundaries. Only with the
    /// `test-faults` feature.
    ///
    /// # Errors
    ///
    /// `Programming` when `rows` is outside 1..=10,000.
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn fixed_window_for_tests(mut self, rows: usize) -> Result<Self> {
        if !(1..=MAX_WINDOW).contains(&rows) {
            return Err(Error::programming(format!(
                "StreamOptions: a fixed window of {rows} rows is outside 1..={MAX_WINDOW} (test \
                 windows)"
            )));
        }
        self.window = Window::Fixed(rows);
        Ok(self)
    }

    /// An
    /// id-chunk stream keeps its recordset's order and refuses this option.
    #[must_use]
    pub fn order(mut self, order: impl Into<String>) -> Self {
        self.order = Some(order.into());
        self
    }

    /// Apply the active test to a cursor stream's domain (on by default). An id-chunk stream reads
    /// its recordset as it is and refuses this option set to `false`.
    #[must_use]
    pub fn active_test(mut self, on: bool) -> Self {
        self.active_test = on;
        self
    }

    /// How relational values read ([`Load::Classic`] by default).
    #[must_use]
    pub fn load(mut self, load: Load) -> Self {
        self.load = load;
        self
    }

    /// The window sizes for `names` of `spec` under `prefetch_chunk`.
    fn sizes(&self, prefetch_chunk: usize, spec: &ModelSpec, names: &[&str]) -> Result<Sizing> {
        let max = MAX_WINDOW.min(prefetch_chunk.max(1));
        match self.window {
            Window::Fixed(rows) if rows > prefetch_chunk => Err(Error::programming(format!(
                "RowsStream: a fixed window of {rows} rows is above this ORM's prefetch_chunk \
                 ({prefetch_chunk})"
            ))),
            Window::Fixed(rows) => Ok(Sizing {
                size: rows,
                min: rows,
                max: rows,
            }),
            Window::Adaptive => {
                let min = MIN_WINDOW.min(max);
                let wide = |ty: &FieldType| {
                    matches!(ty, FieldType::Text | FieldType::Html | FieldType::Json)
                };
                let has_wide = if names.is_empty() {
                    spec.fields.iter().any(|f| f.has_column() && wide(&f.ty))
                } else {
                    names
                        .iter()
                        .filter_map(|name| spec.field(name))
                        .any(|f| wide(&f.ty))
                };
                let first = if has_wide {
                    FIRST_WIDE_WINDOW
                } else {
                    FIRST_WINDOW
                };
                Ok(Sizing {
                    size: first.clamp(min, max),
                    min,
                    max,
                })
            }
        }
    }
}

/// The size of the next window and its bounds.
#[derive(Debug, Clone, Copy)]
struct Sizing {
    size: usize,
    min: usize,
    max: usize,
}

/// The transaction a stream reads and its window sizes, fixed by its first window.
#[derive(Debug, Clone, Copy)]
struct Bound {
    stamp: TxStamp,
    sizing: Sizing,
}

/// The window a stream holds.
#[derive(Debug)]
enum WindowState {
    /// None: the next call fetches one.
    Empty,
    /// Fetched into the cache and the id buffer, not lent yet (also after a call cancelled
    /// while it was building the window: the next call builds it again).
    Fetched,
    /// Lent to the caller until the next call.
    Lent(Rows),
}

/// The record-rule watch of a cursor stream read by a restricted environment (review DB 2): the
/// rows came through the rule as it was at the `DECLARE`; once the policy (the binding's key) or
/// a row the rule reads changed, each window's ids are checked again before it is lent.
#[derive(Debug)]
struct AccessWatch {
    /// The binding and model the rule was decided for at the `DECLARE`.
    key: MemoKey,
    /// What the rule reads, remembered with each window's ids while nothing changed.
    inputs: Vec<(ModelId, FieldId)>,
    /// The rule's start set came from a search: any write may change its answer.
    volatile: bool,
    /// The models whose writes may change the rule's answer: the streamed model and those the
    /// rule reads.
    models: Vec<ModelId>,
}

/// Where the windows come from.
#[allow(
    clippy::large_enum_variant,
    reason = "one per stream, never in a collection: boxing the cursor would only add an allocation"
)]
enum Source {
    /// A SQL cursor over the search's `SELECT`.
    Cursor {
        /// The `FETCH` of one window, rewritten only when the window size changes.
        fetch: String,
        /// The rows `fetch` asks for.
        fetch_rows: usize,
        /// Decodes the fetched rows into the cache.
        cacher: RowCacher,
        /// Takes only the ids of the fetched rows (once the streamed model changed after the
        /// `DECLARE`).
        ids_only: RowCacher,
        /// The cursor (its number, and the state the transaction shares).
        cursor: CursorHandle,
        /// `erp_c<number>`.
        name: String,
        /// The transaction's write tick at the `DECLARE` (after its flush).
        declared_at: u64,
        /// The record-rule watch, for a restricted environment.
        access: Option<AccessWatch>,
    },
    /// Chunks of an explicit id list.
    Ids {
        /// Every id, in order (duplicates kept).
        all: Ids,
        /// The first id of the next chunk.
        next: usize,
    },
    /// A domain known to match nothing (`FALSE`): no statement at all.
    Nothing,
}

/// Where a stream is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// More windows may come.
    Open,
    /// The last window has been fetched (shorter than asked, or the last chunk).
    Last,
    /// Every window was lent, or the stream was closed.
    Done,
    /// A window failed: the cursor's position is unknown, so no further window is read.
    Failed,
}

/// A windowed read: [`Records::stream`] (SQL cursor) or [`Records::stream_records`] (id chunks).
///
/// Each [`RowsStream::next`] lends the next window as a [`Rows`] batch with an `id` key and the
/// requested fields, values exactly as [`Records::read_rows`] gives them for the window's
/// records. The window is valid until the next call: its records then leave the cache and its
/// buffers are reused (a clone of the batch kept by the caller stays valid; the stream then makes
/// new buffers and counts it in [`RowsStream::buffers_renewed`]).
///
/// A stream belongs to the transaction it started in and ends with it.
///
/// ```no_run
/// # async fn demo(tx: &mut erp_orm::Transaction) -> erp_fields::Result<()> {
/// use erp_orm::{Load, StreamOptions};
///
/// let lines = tx.env().records("bench.order.line")?;
/// let opts = StreamOptions::new().order("id").load(Load::Raw);
/// let domain = erp_domain::Domain::TRUE;
/// let mut stream = lines.stream(tx, &domain, &["code", "qty"], &opts).await?;
/// let mut total = 0.0;
/// while let Some(window) = stream.next(tx).await? {
///     for i in window.indices() {
///         total += window.f64(i, "qty")?.unwrap_or(0.0);
///     }
/// }
/// # let _ = total;
/// # Ok(()) }
/// ```
pub struct RowsStream {
    /// The model and environment of the windows.
    records: Records,
    /// The fields read (spec names; empty: every field the environment may read).
    names: Vec<&'static str>,
    load: Load,
    options: StreamOptions,
    source: Source,
    phase: Phase,
    /// The transaction and window sizes (`None` for an id-chunk stream before its first window).
    bound: Option<Bound>,
    /// The cache's slot counts (and clear count) before the current window (reused buffer).
    mark: Mark,
    /// The ids of the window fetched (reused buffer).
    ids: Vec<RecId>,
    window: WindowState,
    /// A `FETCH` was sent and its call has not finished: a call found with this set was
    /// cancelled mid-`FETCH`, and the cursor's position is unknown.
    in_flight: bool,
    /// The buffers of the last window, taken back for the next one.
    spare: Option<RowsBuilder>,
    /// The last window could not be taken back (the caller kept a clone).
    lost: bool,
    renewed: u64,
    /// Received `DataRow` bytes of the last fetched window.
    bytes: u64,
}

impl std::fmt::Debug for RowsStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RowsStream")
            .field("model", &self.records.model_name())
            .field("cursor", &self.cursor_name())
            .field("phase", &self.phase)
            .field("sizing", &self.bound.map(|b| b.sizing))
            .finish_non_exhaustive()
    }
}

// A stream is held across awaits by request handlers that run on spawned tasks (export,
// PERF.SERVE-EXPORT): it must stay `Send`, and so must its `next` future.
const _: () = {
    const fn send<T: Send>() {}
    send::<RowsStream>();
    send::<StreamOptions>();
};

#[allow(dead_code)]
fn next_future_is_send(stream: &mut RowsStream, tx: &mut Transaction) {
    fn send<T: Send>(_: &T) {}
    send(&stream.next(tx));
}

impl Records {
    /// Read the records matching `domain` window by window through a SQL cursor (design note
    /// `perf-stream.md` §3.3): the search of [`Records::search_with`] (access rules, active test,
    /// `opts`' order, else the model's) declared as a cursor, its stored columns fetched with the
    /// ids, `fields` read for each window as [`Records::read_rows`] reads them (every field the
    /// environment may read when empty).
    ///
    /// Pending writes are flushed first, and the first window is fetched now, in one flight
    /// (`SET LOCAL cursor_tuple_fraction = 1` for the transaction's first stream, `DECLARE`,
    /// `FETCH`); every further window costs one `FETCH`. The records and their order are those
    /// of the search at the `DECLARE` (rows other transactions commit meanwhile are not seen);
    /// the values are current: once this transaction changed the model after the `DECLARE`, the
    /// windows read their values fresh by id (rows changed so they no longer match `domain`
    /// still arrive, with their new values; rows deleted are left out). For a restricted
    /// environment, once the policy or a row its record rule reads changed, each window is
    /// checked against the rule again: a record it no longer lets the environment read is
    /// refused with the `Access` error of [`Records::read_rows`].
    ///
    /// # Errors
    ///
    /// `Programming` inside an isolated scope (onchange: its cache snapshot would bring the
    /// windows back), for an unknown field, a fixed window above
    /// [`OrmConfig::prefetch_chunk`](crate::OrmConfig::prefetch_chunk), or when the transaction
    /// has its limit of open cursors ([`Transaction::set_cursor_limit`]); `Access` for a field or
    /// a model the environment may not read; the flush's and the first window's errors.
    pub async fn stream(
        &self,
        tx: &mut Transaction,
        domain: &Domain,
        fields: &[&str],
        opts: &StreamOptions,
    ) -> Result<RowsStream> {
        refuse_isolated(tx)?;
        let spec = self.spec();
        let names = resolve(spec, fields)?;
        let sizing = opts.sizes(tx.config().prefetch_chunk, spec, &names)?;
        let fetch = if names.is_empty() {
            Fetch::Prefetchable
        } else {
            Fetch::Fields(names.iter().map(|n| (*n).to_owned()).collect())
        };
        let search = SearchOptions {
            order: opts.order.clone(),
            active_test: opts.active_test,
            fetch,
            ..SearchOptions::default()
        };
        let (query, columns) = self.search_plan(tx, domain, &search).await?;
        let mut stream = RowsStream {
            records: self.browse_ids([]),
            names,
            load: opts.load,
            options: opts.clone(),
            source: Source::Nothing,
            phase: Phase::Last,
            bound: Some(Bound {
                stamp: tx.cursors.stamp(),
                sizing,
            }),
            mark: Mark::default(),
            ids: Vec::new(),
            window: WindowState::Empty,
            in_flight: false,
            spare: None,
            lost: false,
            renewed: 0,
            bytes: 0,
        };
        if query.is_empty() {
            return Ok(stream);
        }
        let access = access_watch(tx, &stream.records).await?;
        // the cursor reads the database: what this transaction wrote must be there first
        tx.flush().await?;
        let declared_at = tx.write_gens.now();
        let (select, params) = query.statement(&columns)?;
        let policy = tx.plan_policy(&params);
        let cacher = RowCacher::new(spec, &columns);
        tx.cache.mark_into(&mut stream.mark);
        let mut fetch = String::new();
        let cursor = first_window(
            tx,
            &mut stream,
            &cacher,
            &select,
            &params,
            policy,
            &mut fetch,
        )
        .await?;
        stream.source = Source::Cursor {
            fetch,
            fetch_rows: sizing.size,
            cacher,
            ids_only: RowCacher::new(spec, &[]),
            name: format!("erp_c{}", cursor.number()),
            cursor,
            declared_at,
            access,
        };
        stream.phase = Phase::Open;
        stream.after_fetch(sizing.size);
        Ok(stream)
    }

    /// Read the records of this recordset window by window, in its order (duplicates read
    /// again), by id chunks: one fetch of `w` ids per window, read as [`Records::read_rows`]
    /// reads them (values current, record rules applied: a record the environment may not read
    /// is refused with its `Access` error; records that no longer exist left out), no cursor.
    /// Sends nothing until the first [`RowsStream::next`], which binds the stream to its
    /// transaction. The windows keep the first size of [`StreamOptions`] (no byte count without a
    /// cursor).
    ///
    /// # Errors
    ///
    /// `Programming` for an unknown field, or for an order or a disabled active test in `opts`
    /// (they apply to a domain; a recordset is read as it is). A fixed window above
    /// [`OrmConfig::prefetch_chunk`](crate::OrmConfig::prefetch_chunk) is refused by the first
    /// [`RowsStream::next`].
    pub fn stream_records(&self, fields: &[&str], opts: &StreamOptions) -> Result<RowsStream> {
        if let Some(order) = &opts.order {
            return Err(Error::programming(format!(
                "RowsStream: stream_records reads its recordset in the recordset's order; the \\
                 order '{order}' applies to a domain stream (Records::stream)"
            )));
        }
        if !opts.active_test {
            return Err(Error::programming(
                "RowsStream: stream_records reads its recordset as it is; the active test \\
                 applies to a domain stream (Records::stream)",
            ));
        }
        let names = resolve(self.spec(), fields)?;
        Ok(RowsStream {
            records: self.browse_ids([]),
            names,
            load: opts.load,
            options: opts.clone(),
            source: Source::Ids {
                all: Arc::clone(self.shared_ids()),
                next: 0,
            },
            phase: Phase::Open,
            bound: None,
            mark: Mark::default(),
            ids: Vec::new(),
            window: WindowState::Empty,
            in_flight: false,
            spare: None,
            lost: false,
            renewed: 0,
            bytes: 0,
        })
    }
}

/// The spec names of `fields` (unknown names refused).
fn resolve(spec: &ModelSpec, fields: &[&str]) -> Result<Vec<&'static str>> {
    fields
        .iter()
        .map(|name| field_of(spec, name).map(|f| f.name))
        .collect()
}

fn refuse_isolated(tx: &Transaction) -> Result<()> {
    if tx.in_isolated_scope() || tx.cache.has_snapshot() {
        return Err(Error::programming(
            "RowsStream: a stream cannot run inside an isolated scope (onchange): restoring the \
             scope's cache snapshot would bring its windows back",
        ));
    }
    Ok(())
}

/// The record-rule watch of a cursor stream of `records`' model in its environment, `None` in
/// superuser mode.
async fn access_watch(tx: &mut Transaction, records: &Records) -> Result<Option<AccessWatch>> {
    let env = records.env();
    if env.su() {
        return Ok(None);
    }
    tx.prepare_access(env).await?;
    let model = records.model_id();
    let key = tx.readable_key(env, model)?;
    let restriction = crate::search::read_restriction(tx, records).await?;
    let (inputs, volatile) = match restriction.as_deref() {
        Some(rule) => (rule.fields.clone(), rule.volatile),
        None => (Vec::new(), false),
    };
    let mut models = vec![model];
    for (m, _) in &inputs {
        if !models.contains(m) {
            models.push(*m);
        }
    }
    Ok(Some(AccessWatch {
        key,
        inputs,
        volatile,
        models,
    }))
}

/// The first window of a cursor stream, fetched into the cache and the stream's id buffer.
async fn first_window(
    tx: &mut Transaction,
    stream: &mut RowsStream,
    cacher: &RowCacher,
    select: &str,
    params: &[Param],
    policy: PlanPolicy,
    fetch: &mut String,
) -> Result<CursorHandle> {
    let size = stream.sizing().size;
    let ids = &mut stream.ids;
    let bytes = &mut stream.bytes;
    ids.clear();
    *bytes = 0;
    let mut rows = cacher.sink(size);
    tx.cursor_first_window(select, params, policy, size, fetch, |cache, row| {
        *bytes += row.wire_len() as u64;
        ids.push(rows.cache_row(cache, row)?);
        Ok(())
    })
    .await
}

impl RowsStream {
    /// The next window, `None` once every window was lent (and from then on). The window lent
    /// before leaves the cache first: pending writes are flushed, the cache is cut back to its
    /// slots before that window, and its buffers are taken back.
    ///
    /// # Errors
    ///
    /// `Programming` when called with another transaction than the stream's, after the stream's
    /// transaction ended (commit, rollback) or a rollback to a savepoint ended its cursor or
    /// undid one of its `FETCH`es (a cursor does not rewind), after a call cancelled while its
    /// `FETCH` was in flight, inside an isolated scope, after an earlier window failed, or when
    /// the caller created a new record (or left a computation of one) while the window was lent;
    /// `Access` for a record of the window the record rules no longer let the environment read;
    /// the flush's errors; the `FETCH`'s or the chunk's read errors (a server error aborts the
    /// transaction); the read errors of [`Records::read_rows`] for the window's records.
    pub async fn next(&mut self, tx: &mut Transaction) -> Result<Option<&Rows>> {
        tx.check_records(&self.records)?;
        if self.phase == Phase::Done {
            return Ok(None);
        }
        // the end of the transaction is named before an earlier failure (a failed window
        // usually aborted the transaction, and the caller rolled it back)
        self.check(tx)?;
        if self.phase == Phase::Failed {
            return Err(Error::programming(format!(
                "{} stopped at an error in an earlier window",
                self.label()
            )));
        }
        if self.in_flight {
            self.fail();
            return Err(Error::programming(format!(
                "{}: an earlier call to next was cancelled while its FETCH was in flight; the \
                 cursor's position is unknown, so the stream stops",
                self.label()
            )));
        }
        if self.rewound() {
            self.fail();
            return Err(Error::programming(format!(
                "{}: a window was fetched inside a savepoint that was rolled back since; a \
                 cursor does not rewind, so that window cannot be fetched again (fetch outside \
                 the savepoint, or start a new stream)",
                self.label()
            )));
        }
        if self.bound.is_none() {
            self.start_ids(tx)?;
        }
        // a window whose every record is gone (deleted after the `DECLARE`, or ids of deleted
        // records) is not lent: the next one is read (round 2 nit 6)
        loop {
            if !matches!(self.window, WindowState::Fetched) {
                self.end_window(tx).await?;
                if self.phase == Phase::Last {
                    self.finish();
                    return Ok(None);
                }
                tx.cache.mark_into(&mut self.mark);
                let asked = self.sizing().size;
                self.in_flight = true;
                let fetched = self.fetch_window(tx).await;
                self.in_flight = false;
                if let Err(e) = fetched {
                    self.fail();
                    return Err(e);
                }
                self.after_fetch(asked);
            }
            if self.ids.is_empty() {
                // nothing was fetched, so nothing is left in the cache past the mark
                self.finish();
                return Ok(None);
            }
            if let Err(e) = self.check_access(tx).await {
                self.fail();
                return Err(e);
            }
            let window = match self.build(tx).await {
                Ok(window) => window,
                Err(e) => {
                    self.fail();
                    return Err(e);
                }
            };
            let empty = window.is_empty();
            self.window = WindowState::Lent(window);
            if !empty {
                break;
            }
        }
        match &self.window {
            WindowState::Lent(rows) => Ok(Some(rows)),
            WindowState::Empty | WindowState::Fetched => Err(Error::programming(
                "RowsStream: the window just built is not lent (internal error in \
                 erp_orm::rows_stream)",
            )),
        }
    }

    /// End the stream now: the lent window leaves the cache (pending writes flushed first) and
    /// the cursor is closed in front of the transaction's next statement. Nothing is sent by
    /// itself. A stream whose transaction already ended just ends.
    ///
    /// # Errors
    ///
    /// `Programming` with another transaction than the stream's (the stream is ended, its window
    /// left in that transaction's cache), or when the caller created a new record while the
    /// window was lent (the window then stays cached); the flush's errors. The stream is ended
    /// in every case except refusal by a raw-statements-only scope, which preserves it for
    /// the caller to close after the scope ends.
    pub async fn close(&mut self, tx: &mut Transaction) -> Result<()> {
        tx.require_record_scope()?;
        let lent = !matches!(self.window, WindowState::Empty);
        self.finish();
        self.window = WindowState::Empty;
        let mine = match self.bound {
            Some(bound) => {
                let now = tx.cursors.stamp();
                if bound.stamp.owner != now.owner {
                    return Err(Error::programming(format!(
                        "{} belongs to another transaction",
                        self.label()
                    )));
                }
                bound.stamp.serial == now.serial
            }
            None => false,
        };
        if mine && lent && !tx.is_aborted_or_released() {
            tx.flush().await?;
            self.cut(tx)?;
            self.shed(tx);
        }
        Ok(())
    }

    /// The cursor's name (`erp_c1`), `None` for an id-chunk stream or a domain that matches
    /// nothing.
    pub fn cursor_name(&self) -> Option<&str> {
        match &self.source {
            Source::Cursor { name, .. } => Some(name),
            Source::Ids { .. } | Source::Nothing => None,
        }
    }

    /// The received `DataRow` bytes of the last window fetched through the cursor (0 for an
    /// id-chunk stream): what the adaptive window size follows.
    pub fn window_bytes(&self) -> u64 {
        self.bytes
    }

    /// How many windows got new buffers because the caller kept the window before (a clone of
    /// the batch); 0 when every window is let go before the next one.
    pub fn buffers_renewed(&self) -> u64 {
        self.renewed
    }

    /// The window sizes; set by `stream`, or by the first window of an id-chunk stream (before
    /// it, the bounds of the options without the configuration's cap).
    fn sizing(&self) -> Sizing {
        self.bound.map_or(
            Sizing {
                size: MIN_WINDOW,
                min: MIN_WINDOW,
                max: MAX_WINDOW,
            },
            |b| b.sizing,
        )
    }

    /// "RowsStream erp_c1", or "RowsStream (ids of bench.order.line)".
    fn label(&self) -> String {
        match &self.source {
            Source::Cursor { name, .. } => format!("RowsStream {name}"),
            Source::Ids { .. } => {
                format!("RowsStream (ids of {})", self.records.model_name())
            }
            Source::Nothing => format!("RowsStream (nothing of {})", self.records.model_name()),
        }
    }

    /// The stream may go on with `tx`.
    fn check(&self, tx: &Transaction) -> Result<()> {
        if let Some(bound) = self.bound {
            let now = tx.cursors.stamp();
            if bound.stamp.owner != now.owner {
                return Err(Error::programming(format!(
                    "{} belongs to another transaction",
                    self.label()
                )));
            }
            if bound.stamp.serial != now.serial {
                return Err(Error::programming(format!(
                    "{} ended with its transaction (commit, rollback or a lost connection)",
                    self.label()
                )));
            }
        }
        if matches!(self.phase, Phase::Open | Phase::Last)
            && let Source::Cursor { cursor, .. } = &self.source
            && cursor.is_ended()
        {
            return Err(Error::programming(format!(
                "{} ended with a rollback to a savepoint it was declared under",
                self.label()
            )));
        }
        refuse_isolated(tx)
    }

    /// Whether a rollback to a savepoint undid one of the cursor's `FETCH`es.
    fn rewound(&self) -> bool {
        matches!(&self.source, Source::Cursor { cursor, .. } if cursor.is_rewound())
    }

    /// The first window of an id-chunk stream binds it to `tx` and sizes its windows.
    fn start_ids(&mut self, tx: &Transaction) -> Result<()> {
        let sizing =
            self.options
                .sizes(tx.config().prefetch_chunk, self.records.spec(), &self.names)?;
        self.bound = Some(Bound {
            stamp: tx.cursors.stamp(),
            sizing,
        });
        Ok(())
    }

    /// The lent window leaves the cache and its buffers come back.
    async fn end_window(&mut self, tx: &mut Transaction) -> Result<()> {
        if !matches!(self.window, WindowState::Lent(_)) {
            return Ok(());
        }
        tx.flush().await?;
        self.cut(tx)?;
        self.shed(tx);
        if let WindowState::Lent(rows) = std::mem::replace(&mut self.window, WindowState::Empty) {
            match rows.try_recycle() {
                Ok(builder) => self.spare = Some(builder),
                Err(_kept) => {
                    self.spare = None;
                    self.lost = true;
                }
            }
        }
        Ok(())
    }

    /// The record-access decisions the window's reads remembered (rule decisions of x2many
    /// targets and of records read past the cursor's columns, the window's own ids) go once they
    /// outnumber the largest window, so they do not pile up over the stream.
    fn shed(&self, tx: &mut Transaction) {
        let keep = self.sizing().max;
        tx.engine.readable.shed_decisions(keep);
        tx.access.shed(keep);
    }

    /// Cut the cache back to the mark taken before the window.
    fn cut(&self, tx: &mut Transaction) -> Result<()> {
        match tx.cache.cut_to_mark(&self.mark) {
            Ok(()) => Ok(()),
            Err(CutRefused::Snapshot) => refuse_isolated(tx),
            Err(CutRefused::Kept { model, by }) => {
                let registry = tx.registry();
                let spec = registry.models().get(model);
                let model_name = spec.map_or("?", |s| s.name);
                let field_name = |field: usize| {
                    spec.and_then(|s| s.fields.get(field))
                        .map_or("?", |f| f.name)
                };
                let message = match by {
                    KeptBy::NewRecord => format!(
                        "{}: a new record of {model_name} was created while the window was lent; \
                         a stream drops its window from the cache and cannot drop a new record \
                         (create it outside the stream's loop)",
                        self.label()
                    ),
                    KeptBy::Pending(field) => format!(
                        "{}: {model_name}.{} is still to compute for a record cached since the \
                         window began, after the flush; a stream drops what the window cached \
                         and cannot drop a pending computation (read the field inside the loop)",
                        self.label(),
                        field_name(field)
                    ),
                    KeptBy::Dirty(field) => format!(
                        "{}: {model_name}.{} has a pending write on a record cached since the \
                         window began, after the flush (internal error in erp_orm::rows_stream)",
                        self.label(),
                        field_name(field)
                    ),
                };
                Err(Error::programming(message))
            }
        }
    }

    /// Fetch the next window into the cache and `ids` (only the ids, once the streamed model
    /// changed after the `DECLARE`: the window's values are then read fresh by id).
    async fn fetch_window(&mut self, tx: &mut Transaction) -> Result<()> {
        self.ids.clear();
        self.bytes = 0;
        let size = self.sizing().size;
        let model = self.records.model_id();
        match &mut self.source {
            Source::Cursor {
                fetch,
                fetch_rows,
                cacher,
                ids_only,
                cursor,
                declared_at,
                ..
            } => {
                if *fetch_rows != size {
                    write_fetch(fetch, size, cursor.number());
                    *fetch_rows = size;
                }
                let decoder = if tx.write_gens.moved_since(*declared_at, &[model]) {
                    &*ids_only
                } else {
                    &*cacher
                };
                let ids = &mut self.ids;
                let bytes = &mut self.bytes;
                let mut rows = decoder.sink(size);
                tx.cursor_window(fetch, cursor.number(), |cache, row| {
                    *bytes += row.wire_len() as u64;
                    ids.push(rows.cache_row(cache, row)?);
                    Ok(())
                })
                .await
            }
            Source::Ids { all, next } => {
                let end = all.len().min(next.saturating_add(size));
                self.ids.extend_from_slice(&all[*next..end]);
                *next = end;
                Ok(())
            }
            Source::Nothing => Ok(()),
        }
    }

    /// After a window of `asked` rows was fetched: a shorter one (or the last chunk) is the last,
    /// and the adaptive size follows its bytes.
    fn after_fetch(&mut self, asked: usize) {
        let last = match &self.source {
            Source::Cursor { .. } => self.ids.len() < asked,
            Source::Ids { all, next } => *next >= all.len(),
            Source::Nothing => true,
        };
        if last {
            self.phase = Phase::Last;
        }
        self.window = WindowState::Fetched;
        if matches!(self.options.window, Window::Adaptive)
            && matches!(self.source, Source::Cursor { .. })
            && self.bytes > 0
            && let Some(bound) = &mut self.bound
        {
            let rows = self.ids.len() as u64;
            let fit = usize::try_from(WINDOW_BYTES.saturating_mul(rows) / self.bytes)
                .unwrap_or(usize::MAX);
            bound.sizing.size = fit.clamp(bound.sizing.min, bound.sizing.max);
        }
    }

    /// For a restricted environment's cursor window: while neither the policy nor a row the rule
    /// reads changed since the `DECLARE`, the window's ids came through the rule and are
    /// remembered as readable (the window's read then asks nothing); otherwise they are checked
    /// against the rule again (one statement for the records not decided yet) and a record it
    /// hides is refused with the `Access` error of [`Records::read_rows`].
    async fn check_access(&mut self, tx: &mut Transaction) -> Result<()> {
        let Source::Cursor {
            access: Some(watch),
            declared_at,
            ..
        } = &self.source
        else {
            return Ok(());
        };
        let env = self.records.env();
        let model = self.records.model_id();
        tx.prepare_access(env).await?;
        let key = tx.readable_key(env, model)?;
        let changed = if watch.volatile {
            tx.write_gens.anything_since(*declared_at)
        } else {
            tx.write_gens.moved_since(*declared_at, &watch.models)
        };
        if key == watch.key && !changed {
            if !watch.volatile {
                tx.remember_read(
                    env,
                    model,
                    self.ids.iter().filter_map(|id| id.as_real()),
                    &watch.inputs,
                )?;
            }
            return Ok(());
        }
        let visible = tx.readable_ids(env, model, &self.ids).await?;
        if visible.len() < self.ids.len() {
            let hidden: Vec<RealId> = self
                .ids
                .iter()
                .filter(|id| !visible.contains(id))
                .filter_map(|id| id.as_real())
                .collect();
            if let Some(error) = tx
                .record_access_error(env, model, Operation::Read, &hidden)
                .await?
            {
                return Err(error);
            }
            // the others no longer exist: the window's read leaves them out
        }
        Ok(())
    }

    /// Read the fetched window's records into a batch, in the recycled buffers when there are.
    async fn build(&mut self, tx: &mut Transaction) -> Result<Rows> {
        if self.spare.is_none() && std::mem::take(&mut self.lost) {
            self.renewed += 1;
        }
        let ids: Ids = Arc::from(self.ids.as_slice());
        let records = Records::from_parts(
            self.records.env().clone(),
            self.records.model_id(),
            Arc::clone(&ids),
            ids,
        );
        let (_, level) = records
            .read_level_into(tx, &self.names, self.load, self.spare.as_mut())
            .await?;
        level.finish_reusing(self.spare.take())
    }

    /// Every window was lent, or the stream is closed: ask the transaction to close the cursor
    /// (in front of its next statement).
    fn finish(&mut self) {
        self.phase = Phase::Done;
        self.request_close();
    }

    fn request_close(&self) {
        if let Source::Cursor { cursor, .. } = &self.source {
            cursor.request_close();
        }
    }

    /// A window failed: no further window is read.
    fn fail(&mut self) {
        self.phase = Phase::Failed;
        self.window = WindowState::Empty;
        self.request_close();
    }
}
