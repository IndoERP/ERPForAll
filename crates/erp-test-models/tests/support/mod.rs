//! Helpers shared by the parity tests.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers"
)]

pub mod currency_fixture;
pub mod jobs;
pub mod rpc;

use erp_sdk::{Transaction, Value};
use erp_test_models::{base_users, test_orm};
use erp_testkit::Harness;

/// `base_users` + `test_orm` on a fresh database.
pub async fn orm() -> Harness {
    Harness::builder([&base_users::ADDON, &test_orm::ADDON])
        .with_static_decls(currency_fixture::decls())
        .build()
        .await
        .expect("harness")
}

/// `base_users` + `test_orm` + `test_search` on a fresh database.
pub async fn search_orm() -> Harness {
    Harness::builder([
        &base_users::ADDON,
        &test_orm::ADDON,
        &erp_test_models::test_search::ADDON,
    ])
    .with_static_decls(currency_fixture::decls())
    .build()
    .await
    .expect("harness")
}

/// `base_users` + `test_rel` (relational fixtures) on a fresh database.
pub async fn rel_orm() -> Harness {
    Harness::builder([&base_users::ADDON, &erp_test_models::test_rel::ADDON])
        .build()
        .await
        .expect("harness")
}

/// `base_users` + `test_depends` (computes through relations) on a fresh database.
pub async fn dep_orm() -> Harness {
    Harness::builder([&base_users::ADDON, &erp_test_models::test_depends::ADDON])
        .build()
        .await
        .expect("harness")
}

/// A domain from JSON list syntax (external input).
pub fn dom(json: erp_sdk::serde_json::Value) -> erp_sdk::Domain {
    erp_sdk::Domain::from_json(&json).expect("domain")
}

/// SQL compared structurally (sql-shape cases): whitespace runs collapsed and no
/// space just inside parentheses, so line wrapping of an expectation never matters.
pub fn norm_sql(sql: &str) -> String {
    sql.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace("( ", "(")
        .replace(" )", ")")
}

/// Assert two SQL texts are equal after [`norm_sql`].
#[track_caller]
pub fn assert_sql(actual: &str, expected: &str) {
    assert_eq!(norm_sql(actual), norm_sql(expected));
}

/// Raw ids of records.
pub fn ids(records: &erp_sdk::Records) -> Vec<i64> {
    records.ids()
}

/// Rows of a raw query as text cells (`NULL` → `None`).
pub async fn rows(tx: &mut Transaction, sql: &str) -> Vec<Vec<Option<String>>> {
    let rows = tx.query(sql, &[]).await.expect(sql);
    rows.iter()
        .map(|r| {
            (0..r.len())
                .map(|i| r.get::<_, Option<String>>(i))
                .collect()
        })
        .collect()
}

/// A single text cell.
pub async fn one(tx: &mut Transaction, sql: &str) -> Option<String> {
    rows(tx, sql)
        .await
        .into_iter()
        .next()
        .and_then(|r| r.into_iter().next().flatten())
}

/// A `{name: value}` dictionary of dynamic values, in the given order.
pub fn vmap(pairs: &[(&str, Value)]) -> erp_sdk::ValueMap {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

/// A `{name: value}` map.
pub fn map(pairs: &[(&str, Value)]) -> std::collections::BTreeMap<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

/// A read result as dictionaries all the way down: `Value::Row` handles become `Value::Map`,
/// `Value::Rows` become lists of maps (`web_read` returns batch handles since PERF.ROWS-READ;
/// tests that destructure the dictionaries read them through this until PERF.ROWS-API moves
/// them to `Value::dict`). Equal to the input by `==`.
pub fn deep(value: &Value) -> Value {
    match value {
        Value::Row(row) => Value::Map(std::sync::Arc::new(
            row.iter()
                .map(|entry| {
                    let (key, value) = entry.expect("a batch entry");
                    (key.to_owned(), deep(&value))
                })
                .collect(),
        )),
        Value::Rows(rows) => Value::list(
            rows.iter()
                .expect("batch rows")
                .map(|row| deep(&Value::Row(row))),
        ),
        Value::List(items) => Value::list(items.iter().map(deep)),
        other => other.clone(),
    }
}
