//! PERF.STREAM-WINDOW (–§3.7, §4.1, §9.3 and
//! ): windowed reads through a SQL cursor (`Records::stream`)
//! or id chunks (`Records::stream_records`). Each window equals `read_rows` of its records; the
//! statements and round trips are pinned; a window leaves nothing in the cache; the cursor is
//! closed in front of the transaction's next statement, never at `COMMIT`.
//!
//! The windows here are a few rows (`StreamOptions::fixed_window_for_tests`, `test-faults`): a
//! production window is never below 1,000 rows (lead decision 7).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod bench_fixture;

use erp_fields::rows::Rows;
use std::future::Future as _;
use std::task::{Context, Waker};

use erp_orm::{BeginOptions, Load, OrmConfig, RawVals, SUPERUSER_ID, StreamOptions};
use erp_sdk::{Cell, Domain, ErrorKind, Records, Transaction, Value};
use erp_test_models::{base_users, bench};
use erp_testkit::{Harness, WireCounter, assert_round_trips};

const LINE_FIELDS: [&str; 5] = ["code", "order_id", "product_name", "qty", "price_unit"];

async fn harness() -> Harness {
    harness_with(OrmConfig::default()).await
}

async fn harness_with(config: OrmConfig) -> Harness {
    Harness::builder([&base_users::ADDON, &bench::ADDON])
        .config(config)
        .count_round_trips()
        .build()
        .await
        .expect("harness")
}

fn wire(h: &Harness) -> WireCounter {
    h.wire().expect("built with count_round_trips").clone()
}

fn dom(json: erp_sdk::serde_json::Value) -> Domain {
    Domain::from_json(&json).expect("domain")
}

fn lines(tx: &Transaction) -> Records {
    tx.env().records("bench.order.line").unwrap()
}

fn orders(tx: &Transaction) -> Records {
    tx.env().records("bench.order").unwrap()
}

/// Windows of `w` rows (any size: the test-only constructor).
fn window(w: usize) -> StreamOptions {
    StreamOptions::new().fixed_window_for_tests(w).unwrap()
}

/// The ids of the lines in id order.
async fn line_ids(tx: &mut Transaction) -> Vec<i64> {
    lines(tx)
        .search_with(
            tx,
            &Domain::TRUE,
            &erp_sdk::SearchOptions::new().order("id"),
        )
        .await
        .unwrap()
        .ids()
}

/// Every row of a batch as `(key, value)` pairs in layout order.
fn values(rows: &Rows) -> Vec<Vec<(String, Value)>> {
    let keys: Vec<String> = rows.keys().map(str::to_owned).collect();
    rows.indices()
        .map(|i| {
            keys.iter()
                .enumerate()
                .map(|(pos, key)| (key.clone(), rows.get_at(i, pos).unwrap().unwrap()))
                .collect()
        })
        .collect()
}

/// Every window of `stream` read to the end, its rows as values, and the window sizes.
async fn drain(
    stream: &mut erp_orm::RowsStream,
    tx: &mut Transaction,
) -> (Vec<Vec<(String, Value)>>, Vec<usize>) {
    let mut all = Vec::new();
    let mut sizes = Vec::new();
    while let Some(window) = stream.next(tx).await.unwrap() {
        sizes.push(window.len());
        all.extend(values(window));
    }
    (all, sizes)
}

/// Cursors open on the transaction's connection (`pg_cursors` also lists the unnamed portal of
/// the query reading it, under the empty name).
async fn cursors(tx: &mut Transaction) -> Vec<String> {
    tx.query(
        "SELECT name FROM pg_cursors WHERE name <> '' ORDER BY name",
        &[],
    )
    .await
    .unwrap()
    .iter()
    .map(|row| row.get::<_, String>(0))
    .collect()
}

/// Windows equal `read_rows` of the same records, in the model's order (`date_order desc, id
/// desc` for orders), for a char with non-ASCII text, a many2one in both loads, x2many ids, a
/// stored compute and a non-stored related field, through the cursor; and through id chunks for
/// explicit ids in any order with duplicates.
#[tokio::test]
async fn windows_equal_read_rows_of_the_same_records() {
    let h = harness().await;
    bench_fixture::dataset(&h, 7, 3, 3, 2).await;
    let mut tx = h.begin().await.unwrap();
    let all = orders(&tx).search(&mut tx, &Domain::TRUE).await.unwrap();
    all.at(2)
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("name", Cell::Str("Pésanan – 東京 ✓".into())),
        )
        .await
        .unwrap();
    let fields = [
        "name",
        "code",
        "partner_id",
        "line_ids",
        "tag_ids",
        "amount_total",
        "partner_name",
        "date_order",
    ];
    for load in [Load::Classic, Load::Raw] {
        let expected = values(&all.read_rows(&mut tx, &fields, load).await.unwrap());
        for w in [1, 2, 3, 7, 8] {
            let mut stream = orders(&tx)
                .stream(&mut tx, &Domain::TRUE, &fields, &window(w).load(load))
                .await
                .unwrap();
            let (got, sizes) = drain(&mut stream, &mut tx).await;
            assert_eq!(got, expected, "{load:?}, w = {w}");
            assert!(sizes.iter().all(|s| *s <= w && *s > 0), "{sizes:?}");
            assert_eq!(sizes.iter().sum::<usize>(), 7);
        }
        let ids = all.ids();
        let picked = [ids[4], ids[0], ids[4], ids[6], ids[1]];
        let records = orders(&tx).browse(picked).unwrap();
        let expected = values(&records.read_rows(&mut tx, &fields, load).await.unwrap());
        assert_eq!(expected.len(), 5, "duplicates read twice");
        for w in [1, 2, 5, 6] {
            let mut stream = records
                .stream_records(&fields, &window(w).load(load))
                .unwrap();
            let (got, sizes) = drain(&mut stream, &mut tx).await;
            assert_eq!(got, expected, "ids, {load:?}, w = {w}");
            assert_eq!(sizes.iter().sum::<usize>(), 5);
        }
    }
    tx.rollback().await.unwrap();
}

/// perf-stream §9.3: a cursor stream sends `DECLARE` and ⌊n / w⌋ + 1 `FETCH`es (n = 0, n a
/// multiple of w, n not a multiple), and the transaction's first stream `SET LOCAL` in front
/// (review DB 6: once per transaction); its `CLOSE` rides the next statement (counted there),
/// after which no cursor is left. An id-chunk stream sends one statement per window.
#[tokio::test]
async fn statements_are_three_plus_one_fetch_per_full_window() {
    let h = harness().await;
    bench_fixture::dataset(&h, 5, 3, 2, 1).await;
    let mut tx = h.begin().await.unwrap();
    let ids = line_ids(&mut tx).await;
    assert_eq!(ids.len(), 15);
    let opts = window(2).load(Load::Raw);
    for n in [0usize, 4, 5, 6, 15] {
        let domain = match n {
            0 => dom(erp_sdk::serde_json::json!([["qty", "<", 0]])),
            n => dom(erp_sdk::serde_json::json!([["id", "<=", ids[n - 1]]])),
        };
        tx.capture_statements(true);
        let before = tx.query_count();
        let mut stream = lines(&tx)
            .stream(&mut tx, &domain, &LINE_FIELDS, &opts)
            .await
            .unwrap();
        let (got, _) = drain(&mut stream, &mut tx).await;
        assert_eq!(got.len(), n);
        // only the transaction's first stream sends the planner setting
        let first = usize::from(n == 0);
        assert_eq!(
            tx.query_count() - before,
            (2 + first + n / 2) as u64,
            "n = {n}"
        );
        let captured = tx.captured().to_vec();
        assert_eq!(
            captured
                .iter()
                .filter(|s| s.starts_with("SET LOCAL cursor_tuple_fraction = 1"))
                .count(),
            first,
            "{captured:?}"
        );
        assert!(
            captured[first].starts_with("DECLARE erp_c")
                && captured[first].contains(" NO SCROLL CURSOR WITHOUT HOLD FOR SELECT "),
            "{captured:?}"
        );
        assert!(
            captured[first + 1..]
                .iter()
                .all(|s| s.starts_with("FETCH FORWARD 2 FROM erp_c")),
            "{captured:?}"
        );
        let name = stream.cursor_name().unwrap().to_owned();
        let before = tx.query_count();
        assert_eq!(cursors(&mut tx).await, Vec::<String>::new(), "n = {n}");
        assert_eq!(
            tx.query_count() - before,
            2,
            "the CLOSE rides the next statement"
        );
        assert!(
            tx.captured().contains(&format!("CLOSE {name}")),
            "{:?}",
            tx.captured()
        );
        tx.capture_statements(false);
    }
    // explicit ids: ⌈n / w⌉ statements, no cursor
    let records = lines(&tx).browse(ids[..5].iter().copied()).unwrap();
    tx.invalidate_all().await.unwrap();
    let before = tx.query_count();
    let mut stream = records.stream_records(&LINE_FIELDS, &opts).unwrap();
    let (got, sizes) = drain(&mut stream, &mut tx).await;
    assert_eq!(got.len(), 5);
    assert_eq!(sizes, [2, 2, 1]);
    assert_eq!(tx.query_count() - before, 3);
    assert_eq!(stream.cursor_name(), None);
    tx.rollback().await.unwrap();
}

/// perf-stream §9.3: the first window rides one flight with its `SET LOCAL` and `DECLARE` (and the
/// `CLOSE` of the stream before it), every further window costs one round trip: 1 for n < w, 3
/// for n = 2.5·w, 4 for n = 3·w; a prepared `DECLARE` once warm costs the same.
#[tokio::test]
async fn round_trips_are_one_flight_then_one_per_window() {
    let h = harness().await;
    bench_fixture::dataset(&h, 4, 3, 2, 3).await;
    let wire = wire(&h);
    let mut tx = h.begin().await.unwrap();
    let ids = line_ids(&mut tx).await;
    tx.open().await.unwrap();
    let opts = window(4).load(Load::Raw);
    for (n, trips) in [(3usize, 1u64), (10, 3), (12, 4), (3, 1)] {
        let domain = dom(erp_sdk::serde_json::json!([["id", "<=", ids[n - 1]]]));
        let got = assert_round_trips!(wire, == trips, {
            let mut stream = lines(&tx)
                .stream(&mut tx, &domain, &LINE_FIELDS, &opts)
                .await
                .unwrap();
            drain(&mut stream, &mut tx).await.0
        });
        assert_eq!(got.len(), n);
    }
    // a prepared DECLARE (no literal in the search): warm once, then one flight
    let tags = tx.env().records("bench.tag").unwrap();
    let mut warm = tags
        .stream(&mut tx, &Domain::TRUE, &["code"], &opts)
        .await
        .unwrap();
    drain(&mut warm, &mut tx).await;
    let got = assert_round_trips!(wire, == 1, {
        let mut stream = tags.stream(&mut tx, &Domain::TRUE, &["code"], &opts).await.unwrap();
        drain(&mut stream, &mut tx).await.0
    });
    assert_eq!(got.len(), 3);
    tx.rollback().await.unwrap();
}

/// Window boundaries under REPEATABLE READ: rows another transaction inserts or deletes between
/// two windows are not seen (the cursor reads the transaction's snapshot), and the deleted row's
/// values come from the snapshot.
#[tokio::test]
async fn rows_committed_between_windows_are_not_seen() {
    let h = harness().await;
    let order_ids = bench_fixture::dataset(&h, 3, 3, 2, 1).await;
    let mut tx = h.begin().await.unwrap();
    let ids = line_ids(&mut tx).await;
    let expected = values(
        &lines(&tx)
            .browse(ids.iter().copied())
            .unwrap()
            .read_rows(&mut tx, &LINE_FIELDS, Load::Raw)
            .await
            .unwrap(),
    );
    let mut stream = lines(&tx)
        .stream(
            &mut tx,
            &Domain::TRUE,
            &LINE_FIELDS,
            &window(2).order("id").load(Load::Raw),
        )
        .await
        .unwrap();
    let mut got = values(stream.next(&mut tx).await.unwrap().unwrap());
    // another transaction deletes the last line and adds one, and commits
    let mut other = h.begin().await.unwrap();
    lines(&other)
        .browse([*ids.last().unwrap()])
        .unwrap()
        .unlink(&mut other)
        .await
        .unwrap();
    bench_fixture::create(
        &mut other,
        "bench.order.line",
        &[bench_fixture::vals(vec![
            ("code", Value::str("L-late")),
            ("order_id", Value::Int(order_ids[0])),
            ("product_name", Value::str("late")),
            ("qty", Value::Float(1.0)),
        ])],
    )
    .await;
    other.commit().await.unwrap();
    while let Some(window) = stream.next(&mut tx).await.unwrap() {
        got.extend(values(window));
    }
    assert_eq!(got, expected);
    tx.rollback().await.unwrap();
}

/// Pending writes and creations of the transaction are flushed before the `DECLARE` (the cursor
/// reads current values, perf-stream §3.5 "Computed fields"), stored computes included.
#[tokio::test]
async fn pending_writes_are_flushed_before_the_cursor_is_declared() {
    let h = harness().await;
    let order_ids = bench_fixture::dataset(&h, 2, 2, 1, 1).await;
    let mut tx = h.begin().await.unwrap();
    let ids = line_ids(&mut tx).await;
    lines(&tx)
        .browse([ids[1]])
        .unwrap()
        .write(&mut tx, RawVals::new().set_cell("qty", Cell::Float(9.0)))
        .await
        .unwrap();
    let made = lines(&tx)
        .create_values(
            &mut tx,
            &[bench_fixture::vals(vec![
                ("code", Value::str("L-new")),
                ("order_id", Value::Int(order_ids[1])),
                ("product_name", Value::str("new")),
                ("qty", Value::Float(4.0)),
                ("price_unit", Value::Float(2.5)),
            ])],
        )
        .await
        .unwrap();
    let fields = ["code", "qty", "price_subtotal"];
    let mut stream = lines(&tx)
        .stream(
            &mut tx,
            &Domain::TRUE,
            &fields,
            &window(2).order("id").load(Load::Raw),
        )
        .await
        .unwrap();
    let (got, _) = drain(&mut stream, &mut tx).await;
    let mut all = ids.clone();
    all.extend(made.ids());
    // read in a fresh transaction after commit: the database values the cursor had to see
    tx.commit().await.unwrap();
    let mut check = h.begin().await.unwrap();
    let expected = values(
        &lines(&check)
            .browse(all.iter().copied())
            .unwrap()
            .read_rows(&mut check, &fields, Load::Raw)
            .await
            .unwrap(),
    );
    assert_eq!(got, expected);
    let qty = |rows: &[Vec<(String, Value)>], i: usize| rows[i][2].1.clone();
    assert_eq!(qty(&got, 1), Value::Float(9.0));
    assert_eq!(got.len(), 5);
    assert_eq!(qty(&got, 4), Value::Float(4.0));
    assert_eq!(
        got[4][3].1,
        Value::Float(10.0),
        "stored compute of the new line"
    );
    check.rollback().await.unwrap();
}

/// perf-stream §3.5: after each window the cache holds no slot past the mark taken before it (for
/// the streamed model and for the comodel a many2one label read), records cached before the
/// stream keep their slots and values, and a read holding those records reads them correctly
/// after a window.
#[tokio::test]
async fn the_cache_is_truncated_back_to_the_mark_after_every_window() {
    let h = harness().await;
    bench_fixture::dataset(&h, 4, 3, 2, 1).await;
    let mut tx = h.begin().await.unwrap();
    let ids = line_ids(&mut tx).await;
    let early = lines(&tx).browse(ids[..2].iter().copied()).unwrap();
    let before_rows = values(
        &early
            .read_rows(&mut tx, &LINE_FIELDS, Load::Raw)
            .await
            .unwrap(),
    );
    let line_slots = tx.cached_slots_for_tests("bench.order.line").unwrap();
    let order_slots = tx.cached_slots_for_tests("bench.order").unwrap();
    assert_eq!(line_slots, 2);
    let mut stream = lines(&tx)
        .stream(
            &mut tx,
            &Domain::TRUE,
            &LINE_FIELDS,
            &window(3).order("id").load(Load::Classic),
        )
        .await
        .unwrap();
    let mut seen = 0;
    while let Some(window) = stream.next(&mut tx).await.unwrap() {
        seen += window.len();
        // the window's own records are cached while it is lent
        assert!(tx.cached_slots_for_tests("bench.order.line").unwrap() > line_slots);
        assert_eq!(
            values(
                &early
                    .read_rows(&mut tx, &LINE_FIELDS, Load::Raw)
                    .await
                    .unwrap()
            ),
            before_rows
        );
    }
    assert_eq!(seen, 12);
    assert_eq!(
        tx.cached_slots_for_tests("bench.order.line").unwrap(),
        line_slots
    );
    assert_eq!(
        tx.cached_slots_for_tests("bench.order").unwrap(),
        order_slots
    );
    assert_eq!(
        values(
            &early
                .read_rows(&mut tx, &LINE_FIELDS, Load::Raw)
                .await
                .unwrap()
        ),
        before_rows
    );
    tx.rollback().await.unwrap();
}

/// A new (in-memory) record created while a window is lent cannot be dropped with the window: the
/// next window refuses with a `Programming` error naming the stream and the model.
#[tokio::test]
async fn a_new_record_left_in_a_window_is_refused() {
    let h = harness().await;
    bench_fixture::dataset(&h, 2, 3, 1, 1).await;
    let mut tx = h.begin().await.unwrap();
    let mut stream = lines(&tx)
        .stream(
            &mut tx,
            &Domain::TRUE,
            &LINE_FIELDS,
            &window(2).load(Load::Raw),
        )
        .await
        .unwrap();
    assert!(stream.next(&mut tx).await.unwrap().is_some());
    let model = lines(&tx);
    tx.new_record(
        &model,
        RawVals::new().set_cell("qty", Cell::Float(2.0)),
        None,
        None,
    )
    .await
    .unwrap();
    let error = stream.next(&mut tx).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "RowsStream erp_c1: a new record of bench.order.line was created while the window was \
         lent; a stream drops its window from the cache and cannot drop a new record (create it \
         outside the stream's loop)"
    );
    tx.rollback().await.unwrap();
}

/// perf-stream §3.5: a stream cannot start or go on inside an isolated scope (an onchange's cache
/// snapshot), whose restore would bring its windows back.
#[tokio::test]
async fn a_stream_is_refused_inside_an_isolated_scope() {
    let h = harness().await;
    bench_fixture::dataset(&h, 2, 2, 1, 1).await;
    let mut tx = h.begin().await.unwrap();
    let message = "RowsStream: a stream cannot run inside an isolated scope (onchange): restoring \
                   the scope's cache snapshot would bring its windows back";
    let started = tx
        .isolated_for_tests(async |tx: &mut Transaction| {
            let records = lines(tx);
            records
                .stream(tx, &Domain::TRUE, &LINE_FIELDS, &window(2))
                .await
                .map(|_| ())
        })
        .await
        .unwrap_err();
    assert_eq!(started.kind(), ErrorKind::Programming);
    assert_eq!(started.message(), message);
    let mut stream = lines(&tx)
        .stream(
            &mut tx,
            &Domain::TRUE,
            &LINE_FIELDS,
            &window(2).load(Load::Raw),
        )
        .await
        .unwrap();
    assert!(stream.next(&mut tx).await.unwrap().is_some());
    let next = tx
        .isolated_for_tests(async |tx: &mut Transaction| stream.next(tx).await.map(|_| ()))
        .await
        .unwrap_err();
    assert_eq!(next.kind(), ErrorKind::Programming);
    assert_eq!(next.message(), message);
    // outside the scope the stream goes on: the second window of 2 lines, then the end
    assert_eq!(stream.next(&mut tx).await.unwrap().map(Rows::len), Some(2));
    assert!(stream.next(&mut tx).await.unwrap().is_none());
    tx.rollback().await.unwrap();
}

/// perf-stream §3.3 "Closing": an exhausted, a closed and a dropped stream leave their cursor
/// open until the transaction's next statement, which carries the `CLOSE`; at `COMMIT` and
/// `ROLLBACK` nothing is sent (the transaction ends the cursor), and the next transaction on the
/// same object sends no `CLOSE` either.
#[tokio::test]
async fn closed_streams_leave_no_cursor_after_the_next_statement() {
    let h = harness().await;
    bench_fixture::dataset(&h, 2, 3, 1, 1).await;
    let mut tx = h.begin().await.unwrap();
    let opts = window(2).load(Load::Raw);
    // dropped after one window
    let mut stream = lines(&tx)
        .stream(&mut tx, &Domain::TRUE, &LINE_FIELDS, &opts)
        .await
        .unwrap();
    assert!(stream.next(&mut tx).await.unwrap().is_some());
    assert_eq!(cursors(&mut tx).await, ["erp_c1"]);
    drop(stream);
    assert_eq!(cursors(&mut tx).await, Vec::<String>::new());
    // closed after one window; the number of a closed cursor is taken again (one `DECLARE` text,
    // so one prepared statement, per stream shape), a stream open beside it takes the next one
    let mut stream = lines(&tx)
        .stream(&mut tx, &Domain::TRUE, &LINE_FIELDS, &opts)
        .await
        .unwrap();
    assert!(stream.next(&mut tx).await.unwrap().is_some());
    let mut beside = lines(&tx)
        .stream(&mut tx, &Domain::TRUE, &LINE_FIELDS, &opts)
        .await
        .unwrap();
    assert!(beside.next(&mut tx).await.unwrap().is_some());
    assert_eq!(cursors(&mut tx).await, ["erp_c1", "erp_c2"]);
    drop(beside);
    stream.close(&mut tx).await.unwrap();
    assert_eq!(cursors(&mut tx).await, Vec::<String>::new());
    // exhausted
    let mut stream = lines(&tx)
        .stream(&mut tx, &Domain::TRUE, &LINE_FIELDS, &opts)
        .await
        .unwrap();
    drain(&mut stream, &mut tx).await;
    assert_eq!(cursors(&mut tx).await, Vec::<String>::new());
    // dropped, then COMMIT: no CLOSE anywhere, in this transaction or the next one
    let mut stream = lines(&tx)
        .stream(&mut tx, &Domain::TRUE, &LINE_FIELDS, &opts)
        .await
        .unwrap();
    assert!(stream.next(&mut tx).await.unwrap().is_some());
    drop(stream);
    tx.capture_statements(true);
    tx.commit().await.unwrap();
    assert_eq!(cursors(&mut tx).await, Vec::<String>::new());
    assert!(
        tx.captured().iter().all(|s| !s.starts_with("CLOSE")),
        "{:?}",
        tx.captured()
    );
    // the next transaction numbers its cursors from 1 (nothing of the last one is held)
    let mut stream = lines(&tx)
        .stream(&mut tx, &Domain::TRUE, &LINE_FIELDS, &opts)
        .await
        .unwrap();
    assert!(stream.next(&mut tx).await.unwrap().is_some());
    assert_eq!(stream.cursor_name(), Some("erp_c1"));
    tx.rollback().await.unwrap();
    assert_eq!(cursors(&mut tx).await, Vec::<String>::new());
    assert!(tx.captured().iter().all(|s| !s.starts_with("CLOSE")));
    tx.rollback().await.unwrap();
}

/// A server error in the middle of a stream (its cursor closed behind its back) is the typed
/// database error and aborts the transaction; after the rollback the stream is ended and no
/// `CLOSE` is sent for it.
#[tokio::test]
async fn a_server_error_mid_stream_is_returned_and_ends_the_stream() {
    let h = harness().await;
    bench_fixture::dataset(&h, 2, 3, 1, 1).await;
    let mut tx = h.begin().await.unwrap();
    let mut stream = lines(&tx)
        .stream(
            &mut tx,
            &Domain::TRUE,
            &LINE_FIELDS,
            &window(2).load(Load::Raw),
        )
        .await
        .unwrap();
    assert!(stream.next(&mut tx).await.unwrap().is_some());
    tx.execute("CLOSE erp_c1", &[]).await.unwrap();
    let error = stream.next(&mut tx).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Database);
    assert_eq!(error.sqlstate(), Some("34000"));
    assert_eq!(error.message(), "cursor \"erp_c1\" does not exist");
    tx.rollback().await.unwrap();
    tx.capture_statements(true);
    assert_eq!(cursors(&mut tx).await, Vec::<String>::new());
    assert!(tx.captured().iter().all(|s| !s.starts_with("CLOSE")));
    let ended = stream.next(&mut tx).await.unwrap_err();
    assert_eq!(ended.kind(), ErrorKind::Programming);
    assert_eq!(
        ended.message(),
        "RowsStream erp_c1 ended with its transaction (commit, rollback or a lost connection)"
    );
    tx.rollback().await.unwrap();
}

/// perf-stream §3.7: a stream used with another transaction, or after its own ended, is a
/// `Programming` error naming it; an id-chunk stream is bound to the transaction of its first
/// window.
#[tokio::test]
async fn a_stream_belongs_to_its_transaction() {
    let h = harness().await;
    bench_fixture::dataset(&h, 2, 3, 1, 1).await;
    let mut tx = h.begin().await.unwrap();
    let mut other = h.begin().await.unwrap();
    let opts = window(2).load(Load::Raw);
    let mut stream = lines(&tx)
        .stream(&mut tx, &Domain::TRUE, &LINE_FIELDS, &opts)
        .await
        .unwrap();
    let error = stream.next(&mut other).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "RowsStream erp_c1 belongs to another transaction"
    );
    assert!(stream.next(&mut tx).await.unwrap().is_some());
    tx.commit().await.unwrap();
    let error = stream.next(&mut tx).await.unwrap_err();
    assert_eq!(
        error.message(),
        "RowsStream erp_c1 ended with its transaction (commit, rollback or a lost connection)"
    );
    let ids = line_ids(&mut tx).await;
    let mut chunks = lines(&tx)
        .browse(ids.iter().copied())
        .unwrap()
        .stream_records(&LINE_FIELDS, &opts)
        .unwrap();
    assert!(chunks.next(&mut tx).await.unwrap().is_some());
    let error = chunks.next(&mut other).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "RowsStream (ids of bench.order.line) belongs to another transaction"
    );
    tx.rollback().await.unwrap();
    let error = chunks.next(&mut tx).await.unwrap_err();
    assert_eq!(
        error.message(),
        "RowsStream (ids of bench.order.line) ended with its transaction (commit, rollback or a \
         lost connection)"
    );
    other.rollback().await.unwrap();
}

/// A rollback to a savepoint ends the cursors declared under it (the server closes them, no
/// `CLOSE` is sent and the stream says so); a stream declared before the savepoint goes on after
/// it, with every row.
#[tokio::test]
async fn a_rollback_to_a_savepoint_ends_the_cursors_declared_under_it() {
    let h = harness().await;
    bench_fixture::dataset(&h, 2, 3, 1, 1).await;
    let mut tx = h.begin().await.unwrap();
    let opts = window(2).order("id").load(Load::Raw);
    let mut before = lines(&tx)
        .stream(&mut tx, &Domain::TRUE, &LINE_FIELDS, &opts)
        .await
        .unwrap();
    let mut seen = before.next(&mut tx).await.unwrap().unwrap().len();
    let mut inner = None;
    let rolled_back = tx
        .savepoint(async |tx: &mut Transaction| {
            let records = lines(tx);
            let mut stream = records
                .stream(tx, &Domain::TRUE, &LINE_FIELDS, &opts)
                .await?;
            assert!(stream.next(tx).await?.is_some());
            inner = Some(stream);
            Err::<(), _>(erp_sdk::Error::db("roll the savepoint back"))
        })
        .await
        .unwrap_err();
    assert_eq!(rolled_back.message(), "roll the savepoint back");
    let mut inner = inner.unwrap();
    assert_eq!(inner.cursor_name(), Some("erp_c2"));
    tx.capture_statements(true);
    assert_eq!(cursors(&mut tx).await, ["erp_c1"]);
    let error = inner.next(&mut tx).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "RowsStream erp_c2 ended with a rollback to a savepoint it was declared under"
    );
    drop(inner);
    while let Some(window) = before.next(&mut tx).await.unwrap() {
        seen += window.len();
    }
    assert_eq!(seen, 6);
    assert!(
        tx.captured().iter().all(|s| s != "CLOSE erp_c2"),
        "{:?}",
        tx.captured()
    );
    tx.rollback().await.unwrap();
}

/// A read-only transaction (the read pool's kind) streams: `DECLARE` of a `SELECT` is allowed
/// read-only; a write in it is still refused.
#[tokio::test]
async fn a_read_only_transaction_streams() {
    let h = harness().await;
    bench_fixture::dataset(&h, 2, 3, 1, 1).await;
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::new(SUPERUSER_ID).read_only("test.stream"))
        .await
        .unwrap();
    let mut stream = lines(&tx)
        .stream(
            &mut tx,
            &Domain::TRUE,
            &LINE_FIELDS,
            &window(4).load(Load::Raw),
        )
        .await
        .unwrap();
    let (got, sizes) = drain(&mut stream, &mut tx).await;
    assert_eq!(got.len(), 6);
    assert_eq!(sizes, [4, 2]);
    let error = tx
        .execute("UPDATE bench_order_line SET qty = 1", &[])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "'test.stream' is declared effect = read but tried to write through SQL; declare effect = \
         write (or wider)."
    );
    tx.rollback().await.unwrap();
}

/// perf-stream §3.3: the `DECLARE` is a prepared statement when the search has no literal (kept on
/// the connection, reused by the next stream of the same shape) and unnamed with typed parameters
/// otherwise; `FETCH` is always unnamed and returns the cursor's columns.
#[tokio::test]
async fn prepared_and_per_call_declare_both_stream() {
    let h = harness().await;
    bench_fixture::dataset(&h, 3, 2, 1, 1).await;
    let mut tx = h.begin().await.unwrap();
    let opts = window(4).order("id").load(Load::Raw);
    let ids = line_ids(&mut tx).await;
    let expected = values(
        &lines(&tx)
            .browse(ids.iter().copied())
            .unwrap()
            .read_rows(&mut tx, &LINE_FIELDS, Load::Raw)
            .await
            .unwrap(),
    );
    let literal = dom(erp_sdk::serde_json::json!([["qty", ">=", 0]]));
    for (domain, prepared) in [(&Domain::TRUE, 1usize), (&literal, 0)] {
        for round in 0..2 {
            let before = tx.prepared_statements();
            let mut stream = lines(&tx)
                .stream(&mut tx, domain, &LINE_FIELDS, &opts)
                .await
                .unwrap();
            let (got, _) = drain(&mut stream, &mut tx).await;
            assert_eq!(got, expected);
            let added = tx.prepared_statements() - before;
            let want = if round == 0 { prepared } else { 0 };
            assert_eq!(added, want, "prepared {prepared}, round {round}");
        }
    }
    tx.rollback().await.unwrap();
}

/// perf-stream §3.7, lead decision 7: a window of 0, below 1,000 outside tests, or above 10,000 is
/// refused when the options are built; a fixed window above the configuration's
/// `prefetch_chunk` when the stream starts.
#[tokio::test]
async fn window_options_are_validated() {
    for (rows, message) in [
        (
            0usize,
            "StreamOptions: a fixed window of 0 rows is outside 1000..=10000 (a window is never 0 = unlimited; never below 1,000 rows, the reference ERP's export batch)",
        ),
        (
            999,
            "StreamOptions: a fixed window of 999 rows is outside 1000..=10000 (a window is never 0 = unlimited; never below 1,000 rows, the reference ERP's export batch)",
        ),
        (
            10_001,
            "StreamOptions: a fixed window of 10001 rows is outside 1000..=10000 (a window is never 0 = unlimited; never below 1,000 rows, the reference ERP's export batch)",
        ),
    ] {
        let error = StreamOptions::new().fixed_window(rows).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(error.message(), message);
    }
    assert!(StreamOptions::new().fixed_window(1_000).is_ok());
    assert!(StreamOptions::new().fixed_window(10_000).is_ok());
    let error = StreamOptions::new().fixed_window_for_tests(0).unwrap_err();
    assert_eq!(
        error.message(),
        "StreamOptions: a fixed window of 0 rows is outside 1..=10000 (test windows)"
    );
    let h = harness_with(OrmConfig {
        prefetch_chunk: 1_500,
        ..OrmConfig::default()
    })
    .await;
    let mut tx = h.begin().await.unwrap();
    let opts = StreamOptions::new().fixed_window(2_000).unwrap();
    let error = lines(&tx)
        .stream(&mut tx, &Domain::TRUE, &LINE_FIELDS, &opts)
        .await
        .map(|_| ())
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "RowsStream: a fixed window of 2000 rows is above this ORM's prefetch_chunk (1500)"
    );
    let error = lines(&tx)
        .browse([1])
        .unwrap()
        .stream_records(&LINE_FIELDS, &opts)
        .unwrap()
        .next(&mut tx)
        .await
        .map(|_| ())
        .unwrap_err();
    assert_eq!(
        error.message(),
        "RowsStream: a fixed window of 2000 rows is above this ORM's prefetch_chunk (1500)"
    );
    tx.rollback().await.unwrap();
}

/// Edge cases: an empty explicit-id stream ends at once without a statement; a domain matching
/// nothing costs its one flight and ends; an unknown field is refused when the stream starts.
#[tokio::test]
async fn empty_streams_and_unknown_fields() {
    let h = harness().await;
    bench_fixture::dataset(&h, 1, 2, 1, 1).await;
    let mut tx = h.begin().await.unwrap();
    tx.open().await.unwrap();
    let before = tx.query_count();
    let mut empty = lines(&tx)
        .browse([])
        .unwrap()
        .stream_records(&LINE_FIELDS, &window(2))
        .unwrap();
    assert!(empty.next(&mut tx).await.unwrap().is_none());
    assert!(empty.next(&mut tx).await.unwrap().is_none());
    assert_eq!(tx.query_count(), before);
    let none = dom(erp_sdk::serde_json::json!([["qty", "<", 0]]));
    let mut stream = lines(&tx)
        .stream(&mut tx, &none, &LINE_FIELDS, &window(2))
        .await
        .unwrap();
    assert!(stream.next(&mut tx).await.unwrap().is_none());
    assert!(stream.next(&mut tx).await.unwrap().is_none());
    assert_eq!(tx.query_count() - before, 3);
    let error = lines(&tx)
        .stream(&mut tx, &Domain::TRUE, &["code", "nope"], &window(2))
        .await
        .map(|_| ())
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "Invalid field 'nope' on 'bench.order.line'"
    );
    tx.rollback().await.unwrap();
}

/// perf-stream §3.4: the default window starts at 2,000 rows and then follows the received bytes
/// (1 MiB per window, between 1,000 and `prefetch_chunk` rows).
#[tokio::test]
async fn adaptive_windows_follow_the_received_bytes() {
    let h = harness().await;
    bench_fixture::dataset(&h, 1_300, 10, 10, 1).await;
    let mut tx = h.begin().await.unwrap();
    let mut stream = lines(&tx)
        .stream(
            &mut tx,
            &Domain::TRUE,
            &["code", "qty"],
            &StreamOptions::new().load(Load::Raw),
        )
        .await
        .unwrap();
    let first = stream.next(&mut tx).await.unwrap().unwrap().len();
    assert_eq!(first, 2_000);
    // 1 MiB over the bytes per row of the first window, in rows
    let expected = usize::try_from(1024 * 1024 * first as u64 / stream.window_bytes())
        .unwrap()
        .clamp(1_000, 10_000);
    let second = stream.next(&mut tx).await.unwrap().unwrap().len();
    assert_eq!(second, expected.min(13_000 - first));
    let mut total = first + second;
    while let Some(window) = stream.next(&mut tx).await.unwrap() {
        total += window.len();
    }
    assert_eq!(total, 13_000);
    tx.rollback().await.unwrap();
}

/// A consumer that keeps a clone of a window gets correct later windows in new buffers, counted
/// by `buffers_renewed` (perf-stream §3.6).
#[tokio::test]
async fn a_kept_window_is_never_overwritten() {
    let h = harness().await;
    bench_fixture::dataset(&h, 2, 3, 1, 1).await;
    let mut tx = h.begin().await.unwrap();
    let ids = line_ids(&mut tx).await;
    let expected = values(
        &lines(&tx)
            .browse(ids.iter().copied())
            .unwrap()
            .read_rows(&mut tx, &LINE_FIELDS, Load::Raw)
            .await
            .unwrap(),
    );
    let mut stream = lines(&tx)
        .stream(
            &mut tx,
            &Domain::TRUE,
            &LINE_FIELDS,
            &window(2).order("id").load(Load::Raw),
        )
        .await
        .unwrap();
    let mut kept = Vec::new();
    while let Some(window) = stream.next(&mut tx).await.unwrap() {
        kept.push(window.clone());
    }
    let got: Vec<_> = kept.iter().flat_map(values).collect();
    assert_eq!(got, expected);
    assert_eq!(stream.buffers_renewed(), 2);
    tx.rollback().await.unwrap();
}

/// The record-rule decisions remembered for a restricted user (the read-access memo of x2many
/// reads) do not pile up across windows: on both sources (a stream's own `FETCH` ends nothing,
/// review M2) they are dropped at a window's end once they outnumber the largest window, and the
/// windows still show only the lines the rule lets the user read.
#[tokio::test]
async fn record_rule_decisions_do_not_pile_up_across_windows() {
    let h = qty_rule_harness().build().await.expect("harness");
    let order_ids = bench_fixture::dataset(&h, 12, 3, 2, 2).await;
    let fields = ["code", "line_ids"];
    let expected = {
        let mut tx = begin_user(&h).await;
        let rows = orders(&tx)
            .browse(order_ids.iter().copied())
            .unwrap()
            .read_rows(&mut tx, &fields, Load::Raw)
            .await
            .unwrap();
        // the rule hides the third line of every order
        assert!(rows.indices().all(
            |i| matches!(rows.get(i, "line_ids").unwrap(), Value::List(ids) if ids.len() == 2)
        ));
        values(&rows)
    };
    for by_ids in [true, false] {
        let mut tx = begin_user(&h).await;
        let mut stream = if by_ids {
            orders(&tx)
                .browse(order_ids.iter().copied())
                .unwrap()
                .stream_records(&fields, &window(2).load(Load::Raw))
                .unwrap()
        } else {
            orders(&tx)
                .stream(
                    &mut tx,
                    &Domain::TRUE,
                    &fields,
                    &window(2).order("id").load(Load::Raw),
                )
                .await
                .unwrap()
        };
        let mut got = Vec::new();
        let mut most = 0;
        while let Some(window) = stream.next(&mut tx).await.unwrap() {
            got.extend(values(window));
            most = most.max(tx.readable_decisions_for_tests());
        }
        assert_eq!(got, expected, "by ids: {by_ids}");
        // one window asks about its 2 orders' 6 lines; at most the largest window (2) is kept
        // from the windows before
        assert!(
            most <= 2 + 6,
            "by ids: {by_ids}: {most} decisions remembered at once over 36 lines"
        );
        tx.rollback().await.unwrap();
    }
}

/// Review H1/DB 1: what the transaction writes after the `DECLARE` (a line of a later order, whose
/// stored subtotal and order total recompute; a later order renamed out of the domain; a later
/// order deleted) is in the later windows, the same on both sources and equal to `read_rows`
/// after the loop: the renamed order still arrives with its new name, the deleted one is left
/// out.
#[tokio::test]
async fn writes_after_the_declare_are_read_fresh_on_both_sources() {
    let h = harness().await;
    bench_fixture::dataset(&h, 6, 2, 2, 1).await;
    let fields = ["name", "amount_total", "line_ids"];
    let domain = dom(erp_sdk::serde_json::json!([["name", "=like", "SO%"]]));
    let mut results = Vec::new();
    for by_ids in [false, true] {
        let mut tx = h.begin().await.unwrap();
        let all = orders(&tx)
            .search_with(&mut tx, &domain, &erp_sdk::SearchOptions::new().order("id"))
            .await
            .unwrap();
        let ids = all.ids();
        assert_eq!(ids.len(), 6);
        let later_line = lines(&tx)
            .search(
                &mut tx,
                &dom(erp_sdk::serde_json::json!([["order_id", "=", ids[4]]])),
            )
            .await
            .unwrap()
            .at(0)
            .unwrap();
        let opts = window(2).order("id").load(Load::Raw);
        let mut stream = if by_ids {
            all.stream_records(&fields, &window(2).load(Load::Raw))
                .unwrap()
        } else {
            orders(&tx)
                .stream(&mut tx, &domain, &fields, &opts)
                .await
                .unwrap()
        };
        let mut got = Vec::new();
        let mut first = true;
        while let Some(window) = stream.next(&mut tx).await.unwrap() {
            got.extend(values(window));
            if std::mem::take(&mut first) {
                later_line
                    .write(&mut tx, RawVals::new().set_cell("qty", Cell::Float(40.0)))
                    .await
                    .unwrap();
                orders(&tx)
                    .browse([ids[5]])
                    .unwrap()
                    .write(
                        &mut tx,
                        RawVals::new().set_cell("name", Cell::Str("renamed".into())),
                    )
                    .await
                    .unwrap();
                orders(&tx)
                    .browse([ids[3]])
                    .unwrap()
                    .unlink(&mut tx)
                    .await
                    .unwrap();
            }
        }
        let expected = values(
            &orders(&tx)
                .browse(ids.iter().copied())
                .unwrap()
                .read_rows(&mut tx, &fields, Load::Raw)
                .await
                .unwrap(),
        );
        assert_eq!(got, expected, "by ids: {by_ids}");
        assert_eq!(
            got.len(),
            5,
            "by ids: {by_ids}: the deleted order is left out"
        );
        assert!(
            got.iter().any(|row| row
                .iter()
                .any(|(k, v)| k == "name" && *v == Value::str("renamed"))),
            "by ids: {by_ids}: the renamed order arrives with its new name"
        );
        // 40 × 1.25 (the line's price) + 2 × 2.5 (the other line)
        assert!(
            got.iter().any(|row| row
                .iter()
                .any(|(k, v)| k == "amount_total" && *v == Value::Float(55.0))),
            "by ids: {by_ids}: the recomputed total: {got:?}"
        );
        results.push(got);
        tx.rollback().await.unwrap();
    }
    assert_eq!(results[0], results[1], "both sources give the same values");
}

/// The policy of the access tests below: a line is readable when its quantity is below 3.
fn qty_rule_harness() -> erp_testkit::HarnessBuilder {
    let rule = Domain::condition("qty", erp_sdk::Operator::Lt, 3.0).unwrap();
    let policy = erp_testkit::DomainPolicy::builder(erp_testkit::Otherwise::Allow)
        .rule_all("bench.order.line", &rule)
        .build()
        .unwrap();
    Harness::builder([&base_users::ADDON, &bench::ADDON]).policy(policy)
}

/// Review DB 2: a row the record rule reads changed mid-stream (a later line's quantity now fails
/// the rule): the window holding it is refused with the `Access` error of `read_rows`, the same
/// on both sources; the windows before it are lent.
#[tokio::test]
async fn a_rule_input_changed_mid_stream_refuses_the_record_on_both_sources() {
    let h = qty_rule_harness().build().await.expect("harness");
    bench_fixture::dataset(&h, 4, 3, 1, 1).await;
    seed_demo_user(&h).await;
    let mut outcomes = Vec::new();
    for by_ids in [false, true] {
        let mut tx = begin_user(&h).await;
        let visible = lines(&tx)
            .search_with(
                &mut tx,
                &Domain::TRUE,
                &erp_sdk::SearchOptions::new().order("id"),
            )
            .await
            .unwrap();
        let ids = visible.ids();
        assert_eq!(ids.len(), 8, "the rule hides the third line of every order");
        let mut stream = if by_ids {
            visible
                .stream_records(&LINE_FIELDS, &window(2).load(Load::Raw))
                .unwrap()
        } else {
            lines(&tx)
                .stream(
                    &mut tx,
                    &Domain::TRUE,
                    &LINE_FIELDS,
                    &window(2).order("id").load(Load::Raw),
                )
                .await
                .unwrap()
        };
        assert_eq!(stream.next(&mut tx).await.unwrap().unwrap().len(), 2);
        lines(&tx)
            .browse([ids[5]])
            .unwrap()
            .sudo(true)
            .write(&mut tx, RawVals::new().set_cell("qty", Cell::Float(5.0)))
            .await
            .unwrap();
        assert_eq!(stream.next(&mut tx).await.unwrap().unwrap().len(), 2);
        let error = stream.next(&mut tx).await.unwrap_err();
        assert_eq!(
            error.kind(),
            ErrorKind::Access,
            "by ids: {by_ids}: {error:?}"
        );
        outcomes.push(error.message().to_owned());
        tx.rollback().await.unwrap();
    }
    assert_eq!(outcomes[0], outcomes[1], "both entry points agree");
}

/// Review M2: a restricted user's cursor windows cost one `FETCH` each; the rule is not asked
/// again per window (the window's ids came through it at the `DECLARE`).
#[tokio::test]
async fn a_restricted_window_costs_one_fetch() {
    let h = qty_rule_harness().build().await.expect("harness");
    bench_fixture::dataset(&h, 4, 3, 1, 1).await;
    let mut tx = begin_user(&h).await;
    let mut stream = lines(&tx)
        .stream(
            &mut tx,
            &Domain::TRUE,
            &LINE_FIELDS,
            &window(2).order("id").load(Load::Raw),
        )
        .await
        .unwrap();
    let mut seen = 0;
    loop {
        let before = tx.query_count();
        let window = stream.next(&mut tx).await.unwrap().map(Rows::len);
        // the first window came with the DECLARE
        let want = u64::from(seen > 0);
        assert_eq!(tx.query_count() - before, want, "after {seen} rows");
        match window {
            Some(n) => seen += n,
            None => break,
        }
    }
    assert_eq!(seen, 8);
    tx.rollback().await.unwrap();
}

/// Review DB 3: a `FETCH` inside a savepoint that is released keeps the stream going; one inside
/// a savepoint that is rolled back is not undone by the server (a cursor does not rewind), so the
/// next call refuses, and the cursor is closed in front of the next statement.
#[tokio::test]
async fn a_fetch_undone_by_a_savepoint_rollback_stops_the_stream() {
    let h = harness().await;
    bench_fixture::dataset(&h, 4, 2, 1, 1).await;
    let mut tx = h.begin().await.unwrap();
    let mut stream = lines(&tx)
        .stream(
            &mut tx,
            &Domain::TRUE,
            &LINE_FIELDS,
            &window(2).order("id").load(Load::Raw),
        )
        .await
        .unwrap();
    assert!(stream.next(&mut tx).await.unwrap().is_some());
    tx.savepoint(async |tx: &mut Transaction| {
        assert!(stream.next(tx).await?.is_some());
        Ok(())
    })
    .await
    .unwrap();
    assert!(stream.next(&mut tx).await.unwrap().is_some());
    let rolled_back = tx
        .savepoint(async |tx: &mut Transaction| {
            assert!(stream.next(tx).await?.is_some());
            Err::<(), _>(erp_sdk::Error::db("roll the savepoint back"))
        })
        .await
        .unwrap_err();
    assert_eq!(rolled_back.message(), "roll the savepoint back");
    let error = stream.next(&mut tx).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "RowsStream erp_c1: a window was fetched inside a savepoint that was rolled back since; a \
         cursor does not rewind, so that window cannot be fetched again (fetch outside the \
         savepoint, or start a new stream)"
    );
    assert_eq!(cursors(&mut tx).await, Vec::<String>::new());
    let again = stream.next(&mut tx).await.unwrap_err();
    assert_eq!(
        again.message(),
        "RowsStream erp_c1 stopped at an error in an earlier window"
    );
    tx.rollback().await.unwrap();
}

/// Review H2: a call to `next` dropped while its `FETCH` is in flight leaves the cursor's position
/// unknown: the next call refuses and the stream stops.
#[tokio::test]
async fn a_next_cancelled_during_its_fetch_stops_the_stream() {
    let h = harness().await;
    bench_fixture::dataset(&h, 3, 2, 1, 1).await;
    let mut tx = h.begin().await.unwrap();
    let mut stream = lines(&tx)
        .stream(
            &mut tx,
            &Domain::TRUE,
            &LINE_FIELDS,
            &window(2).load(Load::Raw),
        )
        .await
        .unwrap();
    assert!(stream.next(&mut tx).await.unwrap().is_some());
    {
        // one poll sends the FETCH (its answer needs the connection task, which cannot run
        // during the poll), then the call is dropped
        let mut call = std::pin::pin!(stream.next(&mut tx));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(
            call.as_mut().poll(&mut cx).is_pending(),
            "the FETCH is in flight"
        );
    }
    let error = stream.next(&mut tx).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "RowsStream erp_c1: an earlier call to next was cancelled while its FETCH was in flight; \
         the cursor's position is unknown, so the stream stops"
    );
    let again = stream.next(&mut tx).await.unwrap_err();
    assert_eq!(
        again.message(),
        "RowsStream erp_c1 stopped at an error in an earlier window"
    );
    tx.rollback().await.unwrap();
}

/// Review L6: a window whose rows cannot be read (a malformed row: the connection is released,
/// which ends its cursor) returns the error; the stream then names the lost transaction.
#[tokio::test]
async fn a_failed_window_ends_the_stream() {
    let h = harness().await;
    bench_fixture::dataset(&h, 3, 2, 1, 1).await;
    let mut tx = h.begin().await.unwrap();
    let mut stream = lines(&tx)
        .stream(
            &mut tx,
            &Domain::TRUE,
            &LINE_FIELDS,
            &window(2).load(Load::Raw),
        )
        .await
        .unwrap();
    assert!(stream.next(&mut tx).await.unwrap().is_some());
    tx.refuse_next_row_for_tests();
    let error = stream.next(&mut tx).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Database);
    assert_eq!(error.message(), "error parsing response from server");
    let again = stream.next(&mut tx).await.unwrap_err();
    assert_eq!(again.kind(), ErrorKind::Programming);
    assert_eq!(
        again.message(),
        "RowsStream erp_c1 ended with its transaction (commit, rollback or a lost connection)"
    );
    assert!(tx.is_released());
    tx.rollback().await.unwrap();
}

/// Review L3: two streams read in turns in one transaction each equal `read_rows` of their
/// records; a window's cut drops what the other stream cached after its mark, never a lent
/// window (a batch owns its values).
#[tokio::test]
async fn interleaved_streams_each_equal_read_rows() {
    let h = harness().await;
    let order_ids = bench_fixture::dataset(&h, 5, 3, 2, 1).await;
    let mut tx = h.begin().await.unwrap();
    let ids = line_ids(&mut tx).await;
    let order_fields = ["code", "amount_total", "line_ids"];
    let expected_lines = values(
        &lines(&tx)
            .browse(ids.iter().copied())
            .unwrap()
            .read_rows(&mut tx, &LINE_FIELDS, Load::Raw)
            .await
            .unwrap(),
    );
    let mut sorted = order_ids.clone();
    sorted.sort_unstable();
    let expected_orders = values(
        &orders(&tx)
            .browse(sorted)
            .unwrap()
            .read_rows(&mut tx, &order_fields, Load::Raw)
            .await
            .unwrap(),
    );
    let mut a = lines(&tx)
        .stream(
            &mut tx,
            &Domain::TRUE,
            &LINE_FIELDS,
            &window(4).order("id").load(Load::Raw),
        )
        .await
        .unwrap();
    let mut b = orders(&tx)
        .stream(
            &mut tx,
            &Domain::TRUE,
            &order_fields,
            &window(2).order("id").load(Load::Raw),
        )
        .await
        .unwrap();
    let (mut got_a, mut got_b) = (Vec::new(), Vec::new());
    loop {
        let more_a = match a.next(&mut tx).await.unwrap() {
            Some(window) => {
                got_a.extend(values(window));
                true
            }
            None => false,
        };
        let more_b = match b.next(&mut tx).await.unwrap() {
            Some(window) => {
                got_b.extend(values(window));
                true
            }
            None => false,
        };
        if !more_a && !more_b {
            break;
        }
    }
    assert_eq!(got_a, expected_lines);
    assert_eq!(got_b, expected_orders);
    assert_eq!(cursors(&mut tx).await, Vec::<String>::new());
    tx.rollback().await.unwrap();
}

/// Review DB 4: a transaction holds at most its cursor limit (32 by default, set per
/// transaction); one more stream is refused naming the setting, and a closed stream frees its
/// cursor for the next one.
#[tokio::test]
async fn a_transaction_holds_at_most_its_cursor_limit() {
    let h = harness().await;
    bench_fixture::dataset(&h, 2, 3, 1, 1).await;
    let mut tx = h.begin().await.unwrap();
    let zero = tx.set_cursor_limit(0).unwrap_err();
    assert_eq!(zero.kind(), ErrorKind::Programming);
    assert_eq!(
        zero.message(),
        "Transaction::set_cursor_limit: the limit is at least 1 (a stream holds one cursor)"
    );
    tx.set_cursor_limit(2).unwrap();
    let opts = window(2).load(Load::Raw);
    let mut a = lines(&tx)
        .stream(&mut tx, &Domain::TRUE, &LINE_FIELDS, &opts)
        .await
        .unwrap();
    let mut b = lines(&tx)
        .stream(&mut tx, &Domain::TRUE, &LINE_FIELDS, &opts)
        .await
        .unwrap();
    let error = lines(&tx)
        .stream(&mut tx, &Domain::TRUE, &LINE_FIELDS, &opts)
        .await
        .map(|_| ())
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "RowsStream: this transaction already has 2 open SQL cursors, its limit \
         (Transaction::set_cursor_limit, default 32); finish or close a stream before starting \
         another"
    );
    a.close(&mut tx).await.unwrap();
    let mut c = lines(&tx)
        .stream(&mut tx, &Domain::TRUE, &LINE_FIELDS, &opts)
        .await
        .unwrap();
    assert_eq!(c.cursor_name(), Some("erp_c1"));
    assert_eq!(drain(&mut c, &mut tx).await.0.len(), 6);
    assert_eq!(drain(&mut b, &mut tx).await.0.len(), 6);
    tx.rollback().await.unwrap();
}

/// The demo user's `res.users` row (uid 2): the writer of its writes, the subject named by its
/// access errors.
async fn seed_demo_user(h: &Harness) {
    let mut tx = h.begin().await.unwrap();
    tx.batch(
        "INSERT INTO res_users (id, name, login, active) VALUES (2, 'demo', 'demo', true); \
         SELECT setval(pg_get_serial_sequence('res_users', 'id'), 2)",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

/// A transaction of the demo user (uid 2) in company 1, without groups.
async fn begin_user(h: &Harness) -> Transaction {
    let company = erp_orm::CompanyId::new(1).unwrap();
    let principal = erp_orm::PrincipalBuilder::user(erp_orm::UserId::new(2).unwrap())
        .resolution(
            erp_orm::Resolution::in_id_order(
                erp_orm::GroupSet::default(),
                [company],
                std::num::NonZeroU64::MIN,
            )
            .unwrap(),
        )
        .companies(vec![company])
        .build()
        .unwrap();
    h.orm()
        .begin_opts(BeginOptions::for_principal(principal))
        .await
        .unwrap()
}

/// The public `query_each` lends each row as an ORM row type: values by index with the driver's
/// rules, column names and type names, and the driver's texts for a wrong type or a column past the
/// last.
#[tokio::test]
async fn query_each_lends_sql_rows() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut seen = Vec::new();
    let rows = tx
        .query_each(
            "SELECT g AS n, CASE WHEN g = 2 THEN NULL ELSE 'é' || g END AS label \
             FROM generate_series(1, $1::int4) g",
            &[&3i32],
            |row| {
                assert_eq!(row.len(), 2);
                assert_eq!(row.column_name(0), Some("n"));
                assert_eq!(row.column_name(1), Some("label"));
                assert_eq!(row.column_name(2), None);
                assert_eq!(row.column_type_name(0), Some("int4"));
                assert_eq!(row.column_type_name(1), Some("text"));
                assert_eq!(row.column_type_name(2), None);
                let n: i32 = row.try_get(0)?;
                let label: Option<&str> = row.try_get(1)?;
                seen.push((n, label.map(str::to_owned)));
                let wrong = row.try_get::<i64>(0).unwrap_err();
                assert_eq!(wrong.kind(), ErrorKind::Database);
                assert_eq!(wrong.message(), "error deserializing column 0");
                assert_eq!(
                    std::error::Error::source(&wrong)
                        .map(ToString::to_string)
                        .as_deref(),
                    Some("cannot convert between the Rust type `i64` and the Postgres type `int4`")
                );
                let past = row.try_get::<i32>(5).unwrap_err();
                assert_eq!(past.kind(), ErrorKind::Database);
                assert_eq!(past.message(), "invalid column `5`");
                Ok(())
            },
        )
        .await
        .unwrap();
    assert_eq!(rows, 3);
    assert_eq!(
        seen,
        [
            (1, Some("é1".to_owned())),
            (2, None),
            (3, Some("é3".to_owned()))
        ]
    );
    let error = tx
        .query_each("SELECT 1", &[], |_| {
            Err(erp_sdk::Error::db("refused by the closure"))
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Database);
    assert_eq!(error.message(), "refused by the closure");
    // the transaction goes on after a refused row
    assert_eq!(tx.query_each("SELECT 1", &[], |_| Ok(())).await.unwrap(), 1);
    tx.rollback().await.unwrap();
}

// ----- review round 2: what moves the streamed model's write tick ------------------------------

/// The windows of `ids` (in id order) of `model`, two rows each, through the cursor (`by_ids`
/// false: the whole model in id order, as the transaction's user sees it) or through id chunks;
/// `act` runs once, after the first window. The values of every window and the window sizes, or
/// the error of the window that failed.
async fn stream_acting(
    tx: &mut Transaction,
    model: &str,
    ids: &[i64],
    fields: &[&str],
    by_ids: bool,
    act: impl AsyncFnOnce(&mut Transaction),
) -> Result<(Vec<Vec<(String, Value)>>, Vec<usize>), erp_sdk::Error> {
    let records = tx.env().records(model).unwrap();
    let mut stream = if by_ids {
        records
            .browse(ids.iter().copied())
            .unwrap()
            .stream_records(fields, &window(2).load(Load::Raw))?
    } else {
        records
            .stream(
                tx,
                &Domain::TRUE,
                fields,
                &window(2).order("id").load(Load::Raw),
            )
            .await?
    };
    let mut all = Vec::new();
    let mut sizes = Vec::new();
    let mut act = Some(act);
    while let Some(window) = stream.next(tx).await? {
        sizes.push(window.len());
        all.extend(values(window));
        if let Some(act) = act.take() {
            act(tx).await;
        }
    }
    Ok((all, sizes))
}

/// The ids of `model` in id order, as the transaction's user sees them.
async fn ids_of(tx: &mut Transaction, model: &str) -> Vec<i64> {
    tx.env()
        .records(model)
        .unwrap()
        .search_with(
            tx,
            &Domain::TRUE,
            &erp_sdk::SearchOptions::new().order("id"),
        )
        .await
        .unwrap()
        .ids()
}

/// `read_rows` of `ids` of `model` from the database (the cache dropped first).
async fn fresh_rows(
    tx: &mut Transaction,
    model: &str,
    ids: &[i64],
    fields: &[&str],
) -> Vec<Vec<(String, Value)>> {
    tx.invalidate_all().await.unwrap();
    values(
        &tx.env()
            .records(model)
            .unwrap()
            .browse(ids.iter().copied())
            .unwrap()
            .read_rows(tx, fields, Load::Raw)
            .await
            .unwrap(),
    )
}

/// Review round 2, finding 1: an order deleted mid-stream takes its lines with it (`ON DELETE
/// CASCADE`). The order's `DELETE` moves the lines' tick, so the cursor's later windows read by id
/// and leave the lines out, like the id chunks and `read_rows`; the window they filled is not lent
/// empty, the next one is read instead (nit 6).
#[tokio::test]
async fn an_order_deleted_mid_stream_takes_its_lines_out_of_both_sources() {
    let h = harness().await;
    let order_ids = bench_fixture::dataset(&h, 4, 2, 1, 1).await;
    let mut results = Vec::new();
    for by_ids in [false, true] {
        let mut tx = h.begin().await.unwrap();
        let ids = line_ids(&mut tx).await;
        assert_eq!(ids.len(), 8);
        let (got, sizes) = stream_acting(
            &mut tx,
            "bench.order.line",
            &ids,
            &LINE_FIELDS,
            by_ids,
            async |tx: &mut Transaction| {
                orders(tx)
                    .browse([order_ids[2]])
                    .unwrap()
                    .unlink(tx)
                    .await
                    .unwrap();
            },
        )
        .await
        .unwrap();
        let expected = fresh_rows(&mut tx, "bench.order.line", &ids, &LINE_FIELDS).await;
        assert_eq!(got, expected, "by ids: {by_ids}");
        assert_eq!(
            sizes,
            [2, 2, 2],
            "by ids: {by_ids}: the third order's lines are gone and no window is empty"
        );
        results.push(got);
        tx.rollback().await.unwrap();
    }
    assert_eq!(results[0], results[1], "both sources give the same values");
}

/// Review round 2, finding 1: a parent whose many2one into it is `SET NULL` is deleted
/// mid-stream, directly (`chain_b`) and through a cascade (`chain_a` deletes its `chain_b`): the
/// streamed `chain_c` rows that pointed at them arrive with an empty many2one on both sources,
/// equal to `read_rows`.
#[tokio::test]
async fn a_set_null_parent_deleted_mid_stream_empties_the_many2one_on_both_sources() {
    let h = Harness::builder([&base_users::ADDON, &erp_test_models::test_rel::ADDON])
        .build()
        .await
        .expect("harness");
    let (a_ids, b_ids) = {
        let mut tx = h.begin().await.unwrap();
        let a_rows: Vec<_> = (0..4)
            .map(|i| bench_fixture::vals(vec![("name", Value::str(format!("a{i}")))]))
            .collect();
        let a_ids = bench_fixture::create(&mut tx, "test_rel.chain_a", &a_rows).await;
        let b_rows: Vec<_> = a_ids
            .iter()
            .enumerate()
            .map(|(i, a)| {
                bench_fixture::vals(vec![
                    ("name", Value::str(format!("b{i}"))),
                    ("a_id", Value::Int(*a)),
                ])
            })
            .collect();
        let b_ids = bench_fixture::create(&mut tx, "test_rel.chain_b", &b_rows).await;
        // c0..c3 point at b0..b3, c4 at b3, c5 at b2
        let c_rows: Vec<_> = [0, 1, 2, 3, 3, 2]
            .iter()
            .enumerate()
            .map(|(i, b)| {
                bench_fixture::vals(vec![
                    ("name", Value::str(format!("c{i}"))),
                    ("b_id", Value::Int(b_ids[*b])),
                ])
            })
            .collect();
        bench_fixture::create(&mut tx, "test_rel.chain_c", &c_rows).await;
        tx.commit().await.unwrap();
        (a_ids, b_ids)
    };
    let fields = ["name", "b_id"];
    let mut results = Vec::new();
    for by_ids in [false, true] {
        let mut tx = h.begin().await.unwrap();
        let ids = ids_of(&mut tx, "test_rel.chain_c").await;
        assert_eq!(ids.len(), 6);
        let (got, _) = stream_acting(
            &mut tx,
            "test_rel.chain_c",
            &ids,
            &fields,
            by_ids,
            async |tx: &mut Transaction| {
                for (model, id) in [
                    ("test_rel.chain_a", a_ids[2]),
                    ("test_rel.chain_b", b_ids[3]),
                ] {
                    tx.env()
                        .records(model)
                        .unwrap()
                        .browse([id])
                        .unwrap()
                        .unlink(tx)
                        .await
                        .unwrap();
                }
            },
        )
        .await
        .unwrap();
        let expected = fresh_rows(&mut tx, "test_rel.chain_c", &ids, &fields).await;
        assert_eq!(got, expected, "by ids: {by_ids}");
        let emptied = got
            .iter()
            .filter(|row| row.iter().any(|(k, v)| k == "b_id" && *v == Value::False))
            .count();
        assert_eq!(
            emptied, 4,
            "by ids: {by_ids}: c2..c5 lost their parent: {got:?}"
        );
        results.push(got);
        tx.rollback().await.unwrap();
    }
    assert_eq!(results[0], results[1], "both sources give the same values");
}

/// How the many2many rule input of the test below is changed mid-stream.
#[derive(Debug, Clone, Copy, PartialEq)]
enum LinkWrite {
    /// The ORM, from the mirror field (`test_rel.category.partner_ids`).
    Mirror,
    /// Raw SQL on the link table, followed by the invalidation the raw-SQL rule asks for.
    RawSql,
    /// Raw SQL on the link table alone: the cursor still sees it (its tick moves with the
    /// statement); the documented contract only promises it with the invalidation.
    RawSqlUninvalidated,
}

/// Review round 2, finding 2: the record rule of `test_rel.partner` reads a many2many
/// (`category_id.name = 'visible'`). Removing a later partner's category mid-stream, from the
/// mirror side through the ORM or through raw SQL on the link table, refuses the window holding it
/// with the `Access` error `read_rows` gives for the same records, on both sources.
#[tokio::test]
async fn a_many2many_rule_input_written_mid_stream_refuses_on_both_sources() {
    let rule = dom(erp_sdk::serde_json::json!([[
        "category_id.name",
        "=",
        "visible"
    ]]));
    let policy = erp_testkit::DomainPolicy::builder(erp_testkit::Otherwise::Allow)
        .rule_all("test_rel.partner", &rule)
        .build()
        .unwrap();
    let h = Harness::builder([&base_users::ADDON, &erp_test_models::test_rel::ADDON])
        .policy(policy)
        .build()
        .await
        .expect("harness");
    let visible = {
        let mut tx = h.begin().await.unwrap();
        let categories = bench_fixture::create(
            &mut tx,
            "test_rel.category",
            &[
                bench_fixture::vals(vec![("name", Value::str("visible"))]),
                bench_fixture::vals(vec![("name", Value::str("hidden"))]),
            ],
        )
        .await;
        let partners: Vec<_> = (0..6)
            .map(|i| {
                bench_fixture::vals(vec![
                    ("name", Value::str(format!("p{i}"))),
                    ("category_id", bench_fixture::set_ids(&[categories[0]])),
                ])
            })
            .collect();
        bench_fixture::create(&mut tx, "test_rel.partner", &partners).await;
        tx.commit().await.unwrap();
        categories[0]
    };
    seed_demo_user(&h).await;
    let fields = ["name"];
    let mut messages = Vec::new();
    for (by_ids, how) in [
        (false, LinkWrite::Mirror),
        (true, LinkWrite::Mirror),
        (false, LinkWrite::RawSql),
        (true, LinkWrite::RawSql),
        (false, LinkWrite::RawSqlUninvalidated),
    ] {
        let mut tx = begin_user(&h).await;
        let ids = ids_of(&mut tx, "test_rel.partner").await;
        assert_eq!(ids.len(), 6, "every partner is in the visible category");
        let gone = ids[4];
        let error = stream_acting(
            &mut tx,
            "test_rel.partner",
            &ids,
            &fields,
            by_ids,
            async |tx: &mut Transaction| match how {
                LinkWrite::Mirror => {
                    let remove = Value::list([Value::list([
                        Value::Int(3),
                        Value::Int(gone),
                        Value::Int(0),
                    ])]);
                    tx.env()
                        .records("test_rel.category")
                        .unwrap()
                        .browse([visible])
                        .unwrap()
                        .sudo(true)
                        .write_values(tx, &bench_fixture::vals(vec![("partner_ids", remove)]))
                        .await
                        .unwrap();
                }
                LinkWrite::RawSql | LinkWrite::RawSqlUninvalidated => {
                    let deleted = tx
                        .execute(
                            "DELETE FROM test_rel_category_partner_rel \
                             WHERE partner_id = $1::bigint",
                            &[&gone],
                        )
                        .await
                        .unwrap();
                    assert_eq!(deleted, 1);
                    if how == LinkWrite::RawSql {
                        tx.invalidate_model("test_rel.partner", Some(&["category_id"]))
                            .await
                            .unwrap();
                    }
                }
            },
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.kind(),
            ErrorKind::Access,
            "{by_ids} {how:?}: {error:?}"
        );
        let direct = tx
            .env()
            .records("test_rel.partner")
            .unwrap()
            .browse([ids[4], ids[5]])
            .unwrap()
            .read_rows(&mut tx, &fields, Load::Raw)
            .await
            .unwrap_err();
        assert_eq!(
            error.message(),
            direct.message(),
            "{by_ids} {how:?}: the error of read_rows"
        );
        messages.push(error.message().to_owned());
        tx.rollback().await.unwrap();
    }
    assert!(
        messages.windows(2).all(|pair| pair[0] == pair[1]),
        "every source and write path agrees: {messages:?}"
    );
}

/// Review round 2, test list: the record rule of the lines reads their order
/// (`order_id.state = 'draft'`); a later order confirmed mid-stream refuses the window holding its
/// lines with the `Access` error of `read_rows`, on both sources.
#[tokio::test]
async fn a_rule_through_a_many2one_refuses_after_the_parent_changes_on_both_sources() {
    let rule = dom(erp_sdk::serde_json::json!([[
        "order_id.state",
        "=",
        "draft"
    ]]));
    let policy = erp_testkit::DomainPolicy::builder(erp_testkit::Otherwise::Allow)
        .rule_all("bench.order.line", &rule)
        .build()
        .unwrap();
    let h = Harness::builder([&base_users::ADDON, &bench::ADDON])
        .policy(policy)
        .build()
        .await
        .expect("harness");
    let order_ids = bench_fixture::dataset(&h, 4, 2, 1, 1).await;
    seed_demo_user(&h).await;
    let mut messages = Vec::new();
    for by_ids in [false, true] {
        let mut tx = begin_user(&h).await;
        let ids = line_ids(&mut tx).await;
        assert_eq!(ids.len(), 8);
        let error = stream_acting(
            &mut tx,
            "bench.order.line",
            &ids,
            &LINE_FIELDS,
            by_ids,
            async |tx: &mut Transaction| {
                orders(tx)
                    .browse([order_ids[2]])
                    .unwrap()
                    .sudo(true)
                    .write(
                        tx,
                        RawVals::new().set_cell("state", Cell::Str("done".into())),
                    )
                    .await
                    .unwrap();
            },
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.kind(),
            ErrorKind::Access,
            "by ids: {by_ids}: {error:?}"
        );
        let direct = lines(&tx)
            .browse([ids[4], ids[5]])
            .unwrap()
            .read_rows(&mut tx, &LINE_FIELDS, Load::Raw)
            .await
            .unwrap_err();
        assert_eq!(error.message(), direct.message(), "by ids: {by_ids}");
        messages.push(error.message().to_owned());
        tx.rollback().await.unwrap();
    }
    assert_eq!(messages[0], messages[1], "both sources agree");
}

/// Review round 2, test list: a raw `UPDATE` of the streamed table mid-loop, without any
/// invalidation: the cursor's later windows read the new value (the statement names the table, so
/// its model's tick moves), like the id chunks and `read_rows`.
#[tokio::test]
async fn a_raw_update_of_the_streamed_table_is_read_on_both_sources() {
    let h = harness().await;
    bench_fixture::dataset(&h, 4, 2, 1, 1).await;
    let mut results = Vec::new();
    for by_ids in [false, true] {
        let mut tx = h.begin().await.unwrap();
        let ids = line_ids(&mut tx).await;
        tx.invalidate_all().await.unwrap();
        let target = ids[5];
        let (got, _) = stream_acting(
            &mut tx,
            "bench.order.line",
            &ids,
            &LINE_FIELDS,
            by_ids,
            async |tx: &mut Transaction| {
                let updated = tx
                    .execute(
                        "UPDATE bench_order_line SET qty = 99 WHERE id = $1::bigint",
                        &[&target],
                    )
                    .await
                    .unwrap();
                assert_eq!(updated, 1);
            },
        )
        .await
        .unwrap();
        let expected = fresh_rows(&mut tx, "bench.order.line", &ids, &LINE_FIELDS).await;
        assert_eq!(got, expected, "by ids: {by_ids}");
        assert!(
            got.iter().any(|row| row
                .iter()
                .any(|(k, v)| k == "qty" && *v == Value::Float(99.0))),
            "by ids: {by_ids}: the new quantity arrives"
        );
        results.push(got);
        tx.rollback().await.unwrap();
    }
    assert_eq!(results[0], results[1], "both sources give the same values");
}

/// Review round 2, test list: a write the statement classifier cannot see (a function called
/// from a `SELECT`) followed by the invalidation the documented contract asks for: the
/// invalidation alone moves the tick, so the cursor's later windows read the new value, like the
/// id chunks and `read_rows`.
#[tokio::test]
async fn an_invalidation_alone_moves_the_tick_on_both_sources() {
    let h = harness().await;
    bench_fixture::dataset(&h, 4, 2, 1, 1).await;
    let mut results = Vec::new();
    for by_ids in [false, true] {
        let mut tx = h.begin().await.unwrap();
        tx.batch(
            "CREATE FUNCTION pg_temp.stream_bump(line bigint) RETURNS void LANGUAGE sql \
             AS 'UPDATE bench_order_line SET qty = 55 WHERE id = line'",
        )
        .await
        .unwrap();
        let ids = line_ids(&mut tx).await;
        tx.invalidate_all().await.unwrap();
        let target = ids[5];
        let (got, _) = stream_acting(
            &mut tx,
            "bench.order.line",
            &ids,
            &LINE_FIELDS,
            by_ids,
            async |tx: &mut Transaction| {
                tx.query("SELECT pg_temp.stream_bump($1::bigint)", &[&target])
                    .await
                    .unwrap();
                tx.invalidate_model("bench.order.line", Some(&["qty"]))
                    .await
                    .unwrap();
            },
        )
        .await
        .unwrap();
        let expected = fresh_rows(&mut tx, "bench.order.line", &ids, &LINE_FIELDS).await;
        assert_eq!(got, expected, "by ids: {by_ids}");
        assert!(
            got.iter().any(|row| row
                .iter()
                .any(|(k, v)| k == "qty" && *v == Value::Float(55.0))),
            "by ids: {by_ids}: the new quantity arrives"
        );
        results.push(got);
        tx.rollback().await.unwrap();
    }
    assert_eq!(results[0], results[1], "both sources give the same values");
}

/// Review round 2, decision 2: a data statement on a table the registry does not know (an
/// updatable view over the streamed table) moves every model's tick (fail closed): the cursor's
/// later windows read the new value, like the id chunks and `read_rows`.
#[tokio::test]
async fn a_write_to_an_unknown_table_moves_every_tick() {
    let h = harness().await;
    bench_fixture::dataset(&h, 4, 2, 1, 1).await;
    let mut results = Vec::new();
    for by_ids in [false, true] {
        let mut tx = h.begin().await.unwrap();
        tx.batch("CREATE TEMP VIEW stream_lines_v AS SELECT id, qty FROM bench_order_line")
            .await
            .unwrap();
        let ids = line_ids(&mut tx).await;
        tx.invalidate_all().await.unwrap();
        let target = ids[5];
        let (got, _) = stream_acting(
            &mut tx,
            "bench.order.line",
            &ids,
            &LINE_FIELDS,
            by_ids,
            async |tx: &mut Transaction| {
                let updated = tx
                    .execute(
                        "UPDATE stream_lines_v SET qty = 77 WHERE id = $1::bigint",
                        &[&target],
                    )
                    .await
                    .unwrap();
                assert_eq!(updated, 1);
            },
        )
        .await
        .unwrap();
        let expected = fresh_rows(&mut tx, "bench.order.line", &ids, &LINE_FIELDS).await;
        assert_eq!(got, expected, "by ids: {by_ids}");
        assert!(
            got.iter().any(|row| row
                .iter()
                .any(|(k, v)| k == "qty" && *v == Value::Float(77.0))),
            "by ids: {by_ids}: the new quantity arrives"
        );
        results.push(got);
        tx.rollback().await.unwrap();
    }
    assert_eq!(results[0], results[1], "both sources give the same values");
}

/// The record-rule decisions a restricted user's read of `orders` remembers (the rule decides on
/// their lines), and how many there are now.
async fn remember_decisions(tx: &mut Transaction, order_ids: &[i64]) -> usize {
    orders(tx)
        .browse(order_ids.iter().copied())
        .unwrap()
        .read_rows(tx, &["code", "line_ids"], Load::Raw)
        .await
        .unwrap();
    tx.readable_decisions_for_tests()
}

/// Review round 3, finding 8: the record-rule decisions a restricted user's reads remember
/// survive cursor and savepoint statements (raw `DECLARE`, `FETCH`, `CLOSE`, `SAVEPOINT`,
/// `RELEASE`, a plain `SELECT`, and a stream's own `FETCH`es); they end with a locking `SELECT`, a
/// raw rollback to a savepoint, a raw `SET` (round 4, finding 5) and a write.
#[tokio::test]
async fn record_rule_decisions_survive_cursor_and_savepoint_statements() {
    let h = qty_rule_harness().build().await.expect("harness");
    let order_ids = bench_fixture::dataset(&h, 4, 3, 1, 1).await;
    seed_demo_user(&h).await;
    let mut tx = begin_user(&h).await;
    let held = remember_decisions(&mut tx, &order_ids).await;
    assert!(held > 0, "the read remembers decisions");
    for sql in [
        "DECLARE raw_c NO SCROLL CURSOR FOR SELECT 1",
        "FETCH 1 FROM raw_c",
        "CLOSE raw_c",
        "SAVEPOINT raw_s",
        "RELEASE SAVEPOINT raw_s",
        "SELECT 1",
    ] {
        tx.execute(sql, &[]).await.unwrap();
        assert_eq!(tx.readable_decisions_for_tests(), held, "after {sql}");
    }
    let mut stream = lines(&tx)
        .stream(
            &mut tx,
            &Domain::TRUE,
            &LINE_FIELDS,
            &window(100).order("id").load(Load::Raw),
        )
        .await
        .unwrap();
    while stream.next(&mut tx).await.unwrap().is_some() {
        assert!(
            tx.readable_decisions_for_tests() >= held,
            "a stream's own statements end no decision"
        );
    }
    tx.execute("SAVEPOINT raw_back", &[]).await.unwrap();
    for (sql, what) in [
        (
            "SELECT id FROM bench_order_line ORDER BY id LIMIT 1 FOR UPDATE",
            "a locking SELECT",
        ),
        ("ROLLBACK TO SAVEPOINT raw_back", "a rollback"),
        // round 4, finding 5: a raw setting may change what later SQL reads
        ("SET LOCAL statement_timeout = '1min'", "a raw SET"),
        ("UPDATE bench_order SET name = name WHERE id = 0", "a write"),
    ] {
        assert!(remember_decisions(&mut tx, &order_ids).await > 0);
        tx.execute(sql, &[]).await.unwrap();
        assert_eq!(tx.readable_decisions_for_tests(), 0, "{what} ends them");
    }
    tx.rollback().await.unwrap();
}

/// Review round 4, lead decision (b): a read-intent transaction refuses SQL the ORM cannot read
/// with certainty (a `U&` name that spells a session-state function, a backslash in a plain
/// string, unterminated text) with its own error, before it is sent.
#[tokio::test]
async fn a_read_transaction_refuses_sql_it_cannot_read_with_certainty() {
    let h = harness().await;
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::new(SUPERUSER_ID).read_only("test.stream"))
        .await
        .unwrap();
    for sql in [
        r#"SELECT U&"pg\005fadvisory_lock"(1)"#,
        r"SELECT 'a\' AS x",
        "SELECT 'open",
    ] {
        let error = tx.query(sql, &[]).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming, "{sql}");
        assert_eq!(
            error.message(),
            "'test.stream' is declared effect = read but sent SQL the ORM cannot read with \
             certainty (a U& string or name, a backslash outside an E'' string, a stray $, or \
             unterminated text), so it cannot tell it changes nothing; declare effect = write (or \
             wider), or write it without those.",
            "{sql}"
        );
    }
    assert_eq!(tx.query("SELECT 1", &[]).await.unwrap().len(), 1);
    tx.rollback().await.unwrap();
}

/// A function that changes how the server reads strings, which no SQL text that calls it shows.
const SCS_OFF_FUNCTION: &str = "CREATE FUNCTION erp_test_scs_off() RETURNS int LANGUAGE plpgsql AS \
     $$ BEGIN SET LOCAL standard_conforming_strings = off; RETURN 1; END $$";

/// How a test sends a raw statement.
#[derive(Debug, Clone, Copy)]
enum Via {
    Execute,
    Query,
    Batch,
}

async fn send_via(tx: &mut Transaction, how: Via, sql: &str) {
    match how {
        Via::Execute => drop(tx.execute(sql, &[]).await.unwrap()),
        Via::Query => drop(tx.query(sql, &[]).await.unwrap()),
        Via::Batch => tx.batch(sql).await.unwrap(),
    }
}

/// Review rounds 4 and 5 (finding 6, M3, L3): once the server reported a new
/// `standard_conforming_strings` or `client_encoding` on the connection, every later raw statement
/// of the transaction object is unknown: a plain `SELECT` then ends the remembered record-rule
/// decisions, which it keeps otherwise. The change is seen whatever sends it (a `SET`,
/// `set_config`, dynamic SQL in a `DO` block, a function, through `execute`, `query` or `batch`);
/// a setting set to the value it has, or another setting, changes nothing.
#[tokio::test]
async fn a_change_of_string_reading_makes_later_statements_unknown() {
    let h = qty_rule_harness().build().await.expect("harness");
    let order_ids = bench_fixture::dataset(&h, 4, 3, 1, 1).await;
    seed_demo_user(&h).await;
    let mut setup = h.begin().await.unwrap();
    setup.batch(SCS_OFF_FUNCTION).await.unwrap();
    setup
        .batch("GRANT EXECUTE ON FUNCTION erp_test_scs_off() TO PUBLIC")
        .await
        .unwrap();
    setup.commit().await.unwrap();
    drop(setup);
    for (how, change) in [
        (Via::Execute, "SET LOCAL standard_conforming_strings = off"),
        (
            Via::Query,
            "SELECT set_config('standard_conforming_strings', 'off', true)",
        ),
        (
            Via::Execute,
            "DO $$ BEGIN EXECUTE 'SET LOCAL standard_' || 'conforming_strings = off'; END $$",
        ),
        (Via::Query, "SELECT erp_test_scs_off()"),
        (
            Via::Query,
            "SELECT set_config('client_encoding', 'SQL_ASCII', true)",
        ),
        (Via::Batch, "SET LOCAL client_encoding = 'SQL_ASCII'"),
        // review round 6, item 4: the server reads a batch whole under the settings before it, as
        // the scanner does; the statements after the batch are unknown
        (
            Via::Batch,
            "SET LOCAL standard_conforming_strings = off; SELECT '\\' AS x",
        ),
    ] {
        let mut tx = begin_user(&h).await;
        assert!(remember_decisions(&mut tx, &order_ids).await > 0);
        tx.execute("SELECT 1", &[]).await.unwrap();
        assert!(
            tx.readable_decisions_for_tests() > 0,
            "a plain SELECT keeps them"
        );
        send_via(&mut tx, how, change).await;
        assert!(remember_decisions(&mut tx, &order_ids).await > 0);
        let tick = tx.every_model_tick_for_tests();
        tx.execute("SELECT 1", &[]).await.unwrap();
        assert_eq!(
            tx.readable_decisions_for_tests(),
            0,
            "after {change}: a plain SELECT is unknown"
        );
        assert!(
            tx.every_model_tick_for_tests() > tick,
            "after {change}: a plain SELECT may change every model (Target::Unknown)"
        );
        // inside an onchange scope it opens the scope's savepoint
        tx.capture_statements(true);
        tx.isolated_for_tests(async |tx: &mut Transaction| tx.query("SELECT 1", &[]).await)
            .await
            .unwrap();
        let captured = tx.captured().to_vec();
        tx.capture_statements(false);
        assert!(
            captured.iter().any(|sql| sql.starts_with("SAVEPOINT")),
            "after {change}: {captured:?}"
        );
        tx.rollback().await.unwrap();
    }
    // no new value reported: the ORM's own limits, the value the setting has, another setting
    for change in [
        "SELECT set_config('lock_timeout', '5s', true), set_config('statement_timeout', '0', true)",
        "SET LOCAL standard_conforming_strings = on",
        "SET LOCAL client_encoding = 'unicode'",
        "SET LOCAL application_name = 'erp-test'",
    ] {
        let mut tx = begin_user(&h).await;
        tx.execute(change, &[]).await.unwrap();
        assert!(remember_decisions(&mut tx, &order_ids).await > 0);
        let tick = tx.every_model_tick_for_tests();
        tx.execute("SELECT 1", &[]).await.unwrap();
        assert!(
            tx.readable_decisions_for_tests() > 0,
            "after {change} a plain SELECT keeps them"
        );
        assert_eq!(tx.every_model_tick_for_tests(), tick, "after {change}");
        tx.capture_statements(true);
        tx.isolated_for_tests(async |tx: &mut Transaction| tx.query("SELECT 1", &[]).await)
            .await
            .unwrap();
        let captured = tx.captured().to_vec();
        tx.capture_statements(false);
        assert!(
            !captured.iter().any(|sql| sql.starts_with("SAVEPOINT")),
            "after {change}: {captured:?}"
        );
        tx.rollback().await.unwrap();
    }
}
