//! PERF.STREAM-CORE (design note, §3.7, §9.1): rows decoded as
//! the driver receives them. A flight drains its statements in order, so answers larger than the
//! driver's buffers cannot deadlock it; an error in the middle of an answer is returned as it is,
//! and the transaction goes on (a decode error) or is aborted (a server error) exactly as after a
//! collected answer.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod bench_fixture;

use std::time::Duration;

use erp_fields::{Cell, Condition, Error, ErrorKind};
use erp_orm::{BeginOptions, OrmConfig, Spec, SpecItem, Timeouts};
use erp_sdk::{Domain, SearchOptions};
use erp_sql::param::Param;
use erp_test_models::{base_users, bench};
use erp_testkit::Harness;

/// A flight that would deadlock if its answers were awaited together waits forever; this bounds
/// how long a test waits for it.
const DEADLOCK_GUARD: Duration = Duration::from_secs(120);

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

async fn harness(config: OrmConfig) -> Harness {
    Harness::builder([&base_users::ADDON, &bench::ADDON])
        .config(config)
        .build()
        .await
        .expect("harness")
}

/// Two statements of 60,000 rows each in one flight, prepared and unnamed: each answer is far
/// larger than the driver's channel of one batch, so statement 2's first message sits behind
/// statement 1's rows until they are consumed. Draining in order completes; awaiting both
/// answers before consuming either would hang.
#[tokio::test]
async fn a_flight_of_answers_larger_than_the_driver_buffers_completes() {
    let h = harness(OrmConfig::default()).await;
    let mut tx = h.begin().await.unwrap();
    let none: &[Param] = &[];
    let first = "SELECT g, repeat('x', 40) FROM generate_series(1, 60000) g";
    let second = "SELECT g, repeat('y', 40), g * 2 FROM generate_series(1, 60000) g";
    for cached in [true, false] {
        let rows = tokio::time::timeout(
            DEADLOCK_GUARD,
            tx.flight_for_tests(&[(first, none, cached), (second, none, cached)], None),
        )
        .await
        .expect("the flight completes (no deadlock)")
        .unwrap();
        assert_eq!(rows, [60_000, 60_000], "cached = {cached}");
    }
    tx.rollback().await.unwrap();
}

/// A flight whose sink refuses a row of statement 1 and whose statement 2 then fails on the
/// server: the server's error is returned (it aborts the transaction), and the refusal is not
/// lost: it is added to the message (review F6). Both execution forms.
#[tokio::test]
async fn a_refused_row_before_a_server_error_rides_in_the_flight_error() {
    let h = harness(OrmConfig::default()).await;
    let none: &[Param] = &[];
    let rows = "SELECT g, repeat('x', 40) FROM generate_series(1, 50000) g";
    let fails = "SELECT 1 / (SELECT 0)";
    for cached in [true, false] {
        let mut tx = h.begin().await.unwrap();
        let error = tokio::time::timeout(
            DEADLOCK_GUARD,
            tx.flight_for_tests(
                &[(rows, none, cached), (fails, none, cached)],
                Some((0, 100)),
            ),
        )
        .await
        .expect("the flight completes (no deadlock)")
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Database, "cached = {cached}");
        assert_eq!(error.sqlstate(), Some("22012"));
        assert_eq!(
            error.message(),
            "division by zero (and refused row 100 of statement 0)"
        );
        // the driver's error stays the direct source (review D4)
        let source = std::error::Error::source(&error).expect("a source");
        let driver = source
            .downcast_ref::<tokio_postgres::Error>()
            .expect("the driver's error is the direct source");
        assert_eq!(
            driver.code(),
            Some(&tokio_postgres::error::SqlState::DIVISION_BY_ZERO)
        );
        let next = tx.query("SELECT 1", &[]).await.unwrap_err();
        assert_eq!(
            next.message(),
            "current transaction is aborted after an earlier error (division by zero); roll \
             back first"
        );
        tx.rollback().await.unwrap();
        // the refusal alone: returned as it is, and the transaction goes on
        let mut tx = h.begin().await.unwrap();
        let error = tx
            .flight_for_tests(
                &[(rows, none, cached), (rows, none, cached)],
                Some((0, 100)),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Database);
        assert_eq!(error.sqlstate(), None);
        assert_eq!(error.message(), "refused row 100 of statement 0");
        assert_eq!(tx.query("SELECT 1", &[]).await.unwrap().len(), 1);
        tx.rollback().await.unwrap();
    }
}

/// The BENCH-002 shape at a size where both comodel answers of `web_read`'s flight (partners and
/// tags, one statement each with the prefetch chunk raised to 60,000 ids) exceed the driver's
/// buffers: the flight completes, and each answer lands in its own model (the codes read back from
/// the cache without a statement equal the database's).
#[tokio::test]
async fn web_read_flight_with_two_large_answers_caches_each_in_its_model() {
    const N: i64 = 50_000;
    let config = OrmConfig {
        prefetch_chunk: 60_000,
        ..OrmConfig::default()
    };
    let h = harness(config).await;
    let mut setup = h.begin().await.unwrap();
    setup
        .batch(&format!(
            "INSERT INTO bench_partner (code, name, country, active) \
               SELECT 'P' || g, 'Partner ' || g, 'id', true FROM generate_series(1, {N}) g; \
             INSERT INTO bench_tag (code, name) \
               SELECT 'T' || g, 'Tag ' || g FROM generate_series(1, {N}) g; \
             INSERT INTO bench_order (code, name, partner_id, state, date_order, amount_total) \
               SELECT 'O' || p.id, 'SO' || p.id, p.id, 'draft', now(), 0 FROM bench_partner p; \
             INSERT INTO bench_order_bench_tag_rel (bench_order_id, bench_tag_id) \
               SELECT o.id, t.id \
               FROM (SELECT id, row_number() OVER (ORDER BY id) AS n FROM bench_order) o \
               JOIN (SELECT id, row_number() OVER (ORDER BY id) AS n FROM bench_tag) t USING (n)"
        ))
        .await
        .unwrap();
    setup.commit().await.unwrap();

    let mut tx = h.begin().await.unwrap();
    // what the database holds, read before the ORM reads anything
    let expected = tx
        .query(
            "SELECT o.id, p.id, p.code, t.id, t.code FROM bench_order o \
             JOIN bench_partner p ON p.id = o.partner_id \
             JOIN bench_order_bench_tag_rel r ON r.bench_order_id = o.id \
             JOIN bench_tag t ON t.id = r.bench_tag_id ORDER BY o.id",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(expected.len(), N as usize);
    let order_ids: Vec<i64> = expected.iter().map(|r| r.get(0)).collect();
    let partner_ids: Vec<i64> = expected.iter().map(|r| r.get(1)).collect();
    let partner_codes: Vec<String> = expected.iter().map(|r| r.get(2)).collect();
    let tag_ids: Vec<i64> = expected.iter().map(|r| r.get(3)).collect();
    let tag_codes: Vec<String> = expected.iter().map(|r| r.get(4)).collect();

    let spec = Spec::new()
        .with("partner_id", SpecItem::fields(Spec::new().field("code")))
        .with("tag_ids", SpecItem::fields(Spec::new().field("code")));
    let orders = bench_fixture::orders(&tx, &order_ids);
    let rows = tokio::time::timeout(DEADLOCK_GUARD, orders.web_read(&mut tx, spec))
        .await
        .expect("web_read completes (no deadlock)")
        .unwrap();
    assert_eq!(rows.len(), N as usize);

    let before = tx.query_count();
    let codes = |cells: Vec<Cell>| -> Vec<String> {
        cells
            .into_iter()
            .map(|c| match c {
                Cell::Str(s) => s.to_string(),
                other => panic!("a code read as {other:?}"),
            })
            .collect()
    };
    let partners = tx
        .env()
        .records("bench.partner")
        .unwrap()
        .browse(partner_ids.iter().copied())
        .unwrap();
    let tags = tx
        .env()
        .records("bench.tag")
        .unwrap()
        .browse(tag_ids.iter().copied())
        .unwrap();
    let cached_partner_codes = codes(partners.cells(&mut tx, "code").await.unwrap());
    let cached_tag_codes = codes(tags.cells(&mut tx, "code").await.unwrap());
    // both answers were cached by the flight: reading them back sends nothing
    assert_eq!(tx.query_count(), before);
    assert_eq!(cached_partner_codes, partner_codes);
    assert_eq!(cached_tag_codes, tag_codes);
    tx.rollback().await.unwrap();
}

/// A decode error in the middle of a search's answer (the database returns an id the ORM cannot
/// hold) is returned with its kind and full message; the rows decoded before it are cached real
/// values, and the next statement on the transaction runs normally (the driver discarded the rest
/// of the answer).
#[tokio::test]
async fn a_decode_error_mid_answer_is_returned_and_the_transaction_goes_on() {
    let h = harness(OrmConfig::default()).await;
    let mut setup = h.begin().await.unwrap();
    setup
        .batch(
            "INSERT INTO bench_tag (code, name) \
               SELECT 'T' || lpad(g::text, 5, '0'), 'Tag ' || g FROM generate_series(1, 3000) g; \
             INSERT INTO bench_tag (id, code, name) VALUES (-5, 'T01500x', 'Broken')",
        )
        .await
        .unwrap();
    setup.commit().await.unwrap();

    let mut tx = h.begin().await.unwrap();
    let tags = tx.env().records("bench.tag").unwrap();
    let first: i64 = tx
        .query("SELECT id FROM bench_tag WHERE code = 'T00001'", &[])
        .await
        .unwrap()[0]
        .get(0);
    let opts = SearchOptions::new()
        .order("code")
        .fetch_fields(["code", "name"]);
    let error = tags
        .search_with(&mut tx, &Domain::TRUE, &opts)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(error.message(), "the database returned the id -5");
    // the rows before the broken one are cached: reading one sends nothing
    let before = tx.query_count();
    let cells = tags
        .browse([first])
        .unwrap()
        .cells(&mut tx, "code")
        .await
        .unwrap();
    assert_eq!(tx.query_count(), before);
    assert_eq!(cells, [Cell::Str("T00001".into())]);
    // the transaction goes on: the rest of the answer was discarded, the next statement answers
    let count: i64 = tx
        .query("SELECT count(*) FROM bench_tag", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(count, 3001);
    assert!(!tx.is_released());
    tx.rollback().await.unwrap();
}

/// A row the second-line range check refuses (the driver lets no malformed row through; the
/// test seam stands in for one) ends the statement as a protocol error, not as a sink refusal:
/// no row is handed over and the connection is released, alone (both execution forms) and in a
/// flight (PERF.STREAM-RAW review, finding on `stream.rs`).
#[tokio::test]
async fn a_row_that_does_not_parse_releases_the_connection() {
    let h = harness(OrmConfig::default()).await;
    let none: &[Param] = &[];
    for cached in [true, false] {
        let mut tx = h.begin().await.unwrap();
        tx.open().await.unwrap();
        tx.refuse_next_row_for_tests();
        let mut seen = 0u32;
        let error = tx
            .query_each_for_tests(
                "SELECT g FROM generate_series(1, 10) g",
                none,
                cached,
                |_| {
                    seen += 1;
                    Ok(())
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Database, "cached = {cached}");
        assert_eq!(error.message(), "error parsing response from server");
        assert_eq!(seen, 0, "cached = {cached}");
        assert!(
            tx.is_released(),
            "cached = {cached}: the connection is released"
        );
    }
    let mut tx = h.begin().await.unwrap();
    tx.open().await.unwrap();
    tx.refuse_next_row_for_tests();
    let error = tx
        .flight_for_tests(
            &[
                ("SELECT g FROM generate_series(1, 10) g", none, true),
                ("SELECT g FROM generate_series(1, 10) g", none, false),
            ],
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Database);
    assert_eq!(error.message(), "error parsing response from server");
    assert!(tx.is_released(), "the flight's connection is released");
}

/// A sink that refuses a row in the middle of a large answer: its error comes back as it is, the
/// rows after it are not handed over, and the next statement on the same connection answers.
/// Both execution forms (prepared and unnamed).
#[tokio::test]
async fn a_sink_error_mid_answer_stops_the_rows_and_the_next_statement_runs() {
    let h = harness(OrmConfig::default()).await;
    let mut tx = h.begin().await.unwrap();
    let pid: i32 = tx.query("SELECT pg_backend_pid()", &[]).await.unwrap()[0].get(0);
    for cached in [true, false] {
        let mut seen = 0u32;
        let error = tx
            .query_each_for_tests(
                "SELECT g, repeat('x', 100) FROM generate_series(1, 50000) g",
                &[],
                cached,
                |row| {
                    let g: i32 = row.try_get(0).expect("an int4 column");
                    seen += 1;
                    if g == 20_000 {
                        return Err(Error::new(
                            ErrorKind::Database,
                            format!("could not decode row {g}"),
                        ));
                    }
                    Ok(())
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Database, "cached = {cached}");
        assert_eq!(error.message(), "could not decode row 20000");
        assert_eq!(seen, 20_000, "no row is handed over after the error");
        let again: i32 = tx.query("SELECT pg_backend_pid()", &[]).await.unwrap()[0].get(0);
        assert_eq!(again, pid, "the same connection answers the next statement");
    }
    assert!(!tx.is_released());
    tx.rollback().await.unwrap();
}

/// `statement_timeout` firing after part of an answer arrived: the typed timeout error naming the
/// limit, and the transaction is aborted as after any server error.
#[tokio::test]
async fn a_statement_timeout_mid_answer_gives_the_typed_error() {
    let h = harness(OrmConfig::default()).await;
    for cached in [true, false] {
        let own = Timeouts::new(ms(100), ms(300), ms(60_000)).unwrap();
        let mut tx = h
            .orm()
            .begin_opts(BeginOptions::new(erp_orm::SUPERUSER_ID).timeouts(own))
            .await
            .unwrap();
        let mut seen = 0u32;
        let error = tx
            .query_each_for_tests(
                "SELECT g, CASE WHEN g = 20000 THEN pg_sleep(5) IS NULL END \
                 FROM generate_series(1, 40000) g",
                &[],
                cached,
                |_| {
                    seen += 1;
                    Ok(())
                },
            )
            .await
            .unwrap_err();
        assert!(seen > 0, "rows arrived before the limit fired");
        assert!(seen < 20_000, "{seen} rows");
        assert_eq!(error.kind(), ErrorKind::Database, "cached = {cached}");
        assert_eq!(error.condition(), Some(Condition::StatementTimeout));
        assert_eq!(error.sqlstate(), Some("57014"));
        assert_eq!(
            error.message(),
            "canceling statement due to statement timeout (statement_timeout was 300 ms when the \
             transaction began)"
        );
        let next = tx.query("SELECT 1", &[]).await.unwrap_err();
        assert_eq!(
            next.message(),
            "current transaction is aborted after an earlier error (canceling statement due to \
             statement timeout); roll back first"
        );
        assert!(!tx.is_released());
        tx.rollback().await.unwrap();
    }
}
