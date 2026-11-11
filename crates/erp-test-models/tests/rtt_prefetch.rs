//! independent comodels of one `web_read` are fetched in one flight
//! (§6). The statements are the ones the sequential read sends, so
//! the statement count does not change; the round trips drop to one per level. Single-threaded
//! runtime on purpose (the futures of a flight are polled together).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::sync::Arc;

use erp_orm::{OrmConfig, Spec};
use erp_sdk::serde_json::json;
use erp_sdk::{ErrorKind, Records, Transaction, Value};
use erp_test_models::{base_users, test_rel};
use erp_testkit::{Harness, WireCounter, assert_query_count, assert_round_trips};
use support::map;

async fn harness() -> Harness {
    harness_with(OrmConfig::default()).await
}

async fn harness_with(config: OrmConfig) -> Harness {
    Harness::builder([&base_users::ADDON, &test_rel::ADDON])
        .config(config)
        .count_round_trips()
        .build()
        .await
        .expect("harness")
}

fn wire(h: &Harness) -> WireCounter {
    h.wire().expect("built with count_round_trips").clone()
}

fn v(json: erp_sdk::serde_json::Value) -> Value {
    use erp_sdk::serde_json::Value as J;
    match json {
        J::Null => Value::False,
        J::Bool(b) => Value::Bool(b),
        J::Number(n) => Value::Int(n.as_i64().unwrap()),
        J::String(t) => Value::str(t),
        J::Array(items) => Value::list(items.into_iter().map(v)),
        J::Object(m) => Value::Map(Arc::new(m.into_iter().map(|(k, x)| (k, v(x))).collect())),
    }
}

fn spec(json: erp_sdk::serde_json::Value) -> Spec {
    Spec::from_value(&v(json)).unwrap()
}

async fn create(tx: &mut Transaction, model: &str, rows: &[&[(&str, Value)]]) -> Records {
    let records = tx.env().records(model).unwrap();
    let vals: Vec<_> = rows.iter().map(|r| map(r)).collect();
    let created = records.create_values(tx, &vals).await.unwrap();
    tx.flush().await.unwrap();
    created
}

/// Three partners, each with a country, a business and a category.
async fn fixture(h: &Harness) -> Records {
    fixture_with(h, 3, 2).await
}

/// `partners` partners, the i-th with country `i % countries`, one business and a category.
async fn fixture_with(h: &Harness, partners: usize, countries: usize) -> Records {
    let mut tx = h.begin().await.unwrap();
    let country_rows: Vec<Vec<(&str, Value)>> = (0..countries)
        .map(|i| vec![("name", Value::str(format!("country {i}")))])
        .collect();
    let country_refs: Vec<&[(&str, Value)]> = country_rows.iter().map(Vec::as_slice).collect();
    let countries_made = create(&mut tx, "test_rel.country", &country_refs).await;
    let categories = create(
        &mut tx,
        "test_rel.category",
        &[
            &[("name", Value::str("Gold"))],
            &[("name", Value::str("Silver"))],
        ],
    )
    .await;
    let seed = create(
        &mut tx,
        "test_rel.partner",
        &[&[("name", Value::str("seed"))]],
    )
    .await;
    let businesses = create(
        &mut tx,
        "test_rel.business",
        &[&[
            ("name", Value::str("Shop")),
            ("partner_id", Value::Int(seed.ids()[0])),
        ]],
    )
    .await;
    let rows: Vec<Vec<(&str, Value)>> = (0..partners)
        .map(|i| {
            vec![
                ("name", Value::str(format!("partner {i}"))),
                (
                    "country_id",
                    Value::Int(countries_made.ids()[i % countries]),
                ),
                ("business_id", Value::Int(businesses.ids()[0])),
                (
                    "category_id",
                    Value::list([Value::list([
                        Value::Int(6),
                        Value::Int(0),
                        Value::list([Value::Int(categories.ids()[i % 2])]),
                    ])]),
                ),
            ]
        })
        .collect();
    let refs: Vec<&[(&str, Value)]> = rows.iter().map(Vec::as_slice).collect();
    let partners = create(&mut tx, "test_rel.partner", &refs).await;
    tx.commit().await.unwrap();
    partners
}

fn expansion() -> Spec {
    spec(json!({
        "name": {},
        "country_id": {"fields": {"name": {}}},
        "business_id": {"fields": {"name": {}}},
        "category_id": {"fields": {"name": {}}},
    }))
}

/// A `web_read` of N records with k independent comodels, one chunk each: the comodel statements
/// go out in one flight. Statements counted by the transaction are what they were; round trips
/// are the statements minus the ones that share the flight, plus the flight (design note §6).
#[tokio::test]
async fn independent_comodels_share_one_flight() {
    let h = harness().await;
    let partners = fixture(&h).await;
    let ids = partners.ids();
    let wire = wire(&h);
    let k: u64 = 3;

    // warm: prepared statements of the connection, as the budgets assume
    {
        let mut tx = h.begin().await.unwrap();
        let records = tx
            .env()
            .records("test_rel.partner")
            .unwrap()
            .browse(ids.clone())
            .unwrap();
        records.web_read(&mut tx, expansion()).await.unwrap();
        tx.rollback().await.unwrap();
    }
    let mut tx = h.begin().await.unwrap();
    tx.open().await.unwrap();
    let records = tx
        .env()
        .records("test_rel.partner")
        .unwrap()
        .browse(ids.clone())
        .unwrap();
    wire.reset();
    let before = tx.query_count();
    let rows = records.web_read(&mut tx, expansion()).await.unwrap();
    let statements = tx.query_count() - before;
    let trips = wire.count();
    // statement budget: the partners, the many2many links, then one per comodel
    assert_eq!(statements, 2 + k, "statements");
    // design note §6: round trips = statements - (statements in the flight) + 1
    assert_eq!(
        trips,
        statements - k + 1,
        "every statement is its own round trip except the {k} comodel reads, which share one"
    );
    // the values are the database's
    let client = h.db().client().await.unwrap();
    for row in &rows {
        let row = support::deep(row);
        let Value::Map(row) = &row else { panic!("row") };
        let id = match row.get("id") {
            Some(Value::Int(id)) => *id,
            other => panic!("{other:?}"),
        };
        let country: String = client
            .query_one(
                "SELECT c.name FROM test_rel_country c JOIN test_rel_partner p ON p.country_id = c.id WHERE p.id = $1",
                &[&id],
            )
            .await
            .unwrap()
            .get(0);
        let Some(Value::Map(c)) = row.get("country_id") else {
            panic!("country_id: {:?}", row.get("country_id"))
        };
        assert_eq!(c.get("name"), Some(&Value::str(country)));
    }
    tx.rollback().await.unwrap();
}

/// The same read with `assert_query_count!` / `assert_round_trips!` side by side (the budget rows
/// of design note §6).
#[tokio::test]
async fn budget_read_with_k_independent_comodels() {
    let h = harness().await;
    let partners = fixture(&h).await;
    let ids = partners.ids();
    let wire = wire(&h);
    {
        let mut tx = h.begin().await.unwrap();
        let r = tx
            .env()
            .records("test_rel.partner")
            .unwrap()
            .browse(ids.clone())
            .unwrap();
        r.web_read(&mut tx, expansion()).await.unwrap();
        tx.rollback().await.unwrap();
    }
    let mut tx = h.begin().await.unwrap();
    tx.open().await.unwrap();
    let records = tx
        .env()
        .records("test_rel.partner")
        .unwrap()
        .browse(ids)
        .unwrap();
    wire.reset();
    // partners + m2m links + 3 comodels = 5 statements; 3 of them in one flight = 3 round trips
    assert_round_trips!(wire, == 3, {
        assert_query_count!(tx, >= 5, <= 5, {
            records.web_read(&mut tx, expansion()).await.unwrap()
        })
    });
    tx.rollback().await.unwrap();
}

/// Statements of the chunked read: the partners and their category links, one statement per id
/// chunk each, and the comodel chunks (design note §6).
const STATEMENTS_CHUNKED: u64 = 9;
/// The same with 20 partners and 20 countries.
const STATEMENTS_CAPPED: u64 = 44;
/// Round trips of the cold read: the partner statement and the category links each take two (a
/// prepare, then the execute), the flight takes two (the prepares together, then the executes).
const TRIPS_COLD: u64 = 6;

/// Read `ids` of the partners with the full expansion on a warm connection, once to prepare the
/// statements and once measured; returns (statements, round trips).
async fn measured_read(h: &Harness, ids: &[i64], warm: bool) -> (u64, u64) {
    let wire = wire(h);
    let records_in = |tx: &Transaction| {
        tx.env()
            .records("test_rel.partner")
            .unwrap()
            .browse(ids.to_vec())
            .unwrap()
    };
    if warm {
        let mut tx = h.begin().await.unwrap();
        records_in(&tx)
            .web_read(&mut tx, expansion())
            .await
            .unwrap();
        tx.rollback().await.unwrap();
    }
    let mut tx = h.begin().await.unwrap();
    tx.open().await.unwrap();
    let records = records_in(&tx);
    wire.reset();
    let before = tx.query_count();
    records.web_read(&mut tx, expansion()).await.unwrap();
    let measured = (tx.query_count() - before, wire.count());
    tx.rollback().await.unwrap();
    measured
}

/// Comodels read in chunks: with `prefetch_chunk` 1, the countries (2 distinct) and the
/// categories (2 distinct) take two chunk statements each and the business one. All five go in
/// one flight: round trips = statements - sum(chunks) + 1 (design note §6).
#[tokio::test]
async fn chunked_comodels_share_one_flight() {
    let h = harness_with(OrmConfig {
        prefetch_chunk: 1,
        ..OrmConfig::default()
    })
    .await;
    let ids = fixture(&h).await.ids();
    let (statements, trips) = measured_read(&h, &ids, true).await;
    let chunks: u64 = 2 + 1 + 2;
    assert_eq!(statements, STATEMENTS_CHUNKED, "statements");
    assert_eq!(trips, statements - chunks + 1, "round trips");
}

/// A flight carries at most 16 statements: a comodel with more chunks than that reads its
/// statements one by one (cached as they arrive), the others still share a flight.
/// 20 partners with 20 distinct countries and `prefetch_chunk` 1: the countries take 20
/// statements, the business 1 and the 2 distinct categories 2, which share a flight.
#[tokio::test]
async fn a_comodel_over_the_flight_cap_reads_one_by_one() {
    let h = harness_with(OrmConfig {
        prefetch_chunk: 1,
        ..OrmConfig::default()
    })
    .await;
    let ids = fixture_with(&h, 20, 20).await.ids();
    let (statements, trips) = measured_read(&h, &ids, true).await;
    // business and category (1 + 2 statements) share one flight; the 20 country statements and
    // everything else are one round trip each (one flight of all 23 would give statements - 22)
    assert_eq!(statements, STATEMENTS_CAPPED, "statements");
    assert_eq!(trips, statements - 3 + 1, "round trips");
}

/// A connection that has not prepared the comodel statements: the flight prepares them together
/// first (one round trip for all) and then executes them (one round trip).
#[tokio::test]
async fn a_cold_connection_prepares_the_flight_together() {
    let h = harness().await;
    let ids = fixture(&h).await.ids();
    let (statements, trips) = measured_read(&h, &ids, false).await;
    assert_eq!(statements, 5, "statements");
    assert_eq!(trips, TRIPS_COLD, "round trips");
}

/// One comodel failing inside the flight: its error surfaces (not the follow-on "transaction is
/// aborted" of the statements behind it), and the transaction is aborted like after any server
/// error.
#[tokio::test]
async fn a_failing_statement_of_the_flight_surfaces_its_error() {
    let h = harness().await;
    let partners = fixture(&h).await;
    let ids = partners.ids();
    let mut tx = h.begin().await.unwrap();
    let records = tx
        .env()
        .records("test_rel.partner")
        .unwrap()
        .browse(ids)
        .unwrap();
    // the registry still expects test_rel_business.name: the business read fails in the flight
    tx.batch("ALTER TABLE test_rel_business RENAME COLUMN name TO name_gone")
        .await
        .unwrap();
    let error = records.web_read(&mut tx, expansion()).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Database);
    assert_eq!(error.sqlstate(), Some("42703"));
    assert_eq!(
        error.message(),
        "column test_rel_business.name does not exist"
    );
    let after = tx.query("SELECT 1", &[]).await.unwrap_err();
    assert!(
        after
            .message()
            .starts_with("current transaction is aborted after an earlier error"),
        "{}",
        after.message()
    );
    tx.rollback().await.unwrap();
}

/// A single comodel gains nothing from a flight and reads as before.
#[tokio::test]
async fn one_comodel_reads_as_before() {
    let h = harness().await;
    let partners = fixture(&h).await;
    let ids = partners.ids();
    let wire = wire(&h);
    let spec_one = spec(json!({"name": {}, "country_id": {"fields": {"name": {}}}}));
    {
        let mut tx = h.begin().await.unwrap();
        let r = tx
            .env()
            .records("test_rel.partner")
            .unwrap()
            .browse(ids.clone())
            .unwrap();
        r.web_read(&mut tx, spec_one.clone()).await.unwrap();
        tx.rollback().await.unwrap();
    }
    let mut tx = h.begin().await.unwrap();
    tx.open().await.unwrap();
    let records = tx
        .env()
        .records("test_rel.partner")
        .unwrap()
        .browse(ids)
        .unwrap();
    wire.reset();
    let before = tx.query_count();
    records.web_read(&mut tx, spec_one).await.unwrap();
    let statements = tx.query_count() - before;
    assert_eq!(wire.count(), statements, "one round trip per statement");
    tx.rollback().await.unwrap();
}
