//! Search semantics of scalar fields with NULL and falsy values (the search parts of
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use erp_sdk::serde_json::{Value as Json, json};
use erp_sdk::{SearchOptions, Transaction, Value};
use erp_testkit::assert_query_count;
use support::{dom, map, one};

async fn create(tx: &mut Transaction, rows: &[&[(&str, Value)]]) -> Vec<i64> {
    let empty = tx.env().records("test_search.mixed").unwrap();
    let values: Vec<_> = rows.iter().map(|r| map(r)).collect();
    let created = empty.create_values(tx, &values).await.unwrap();
    tx.flush().await.unwrap();
    created.ids()
}

async fn find(tx: &mut Transaction, domain: Json) -> Vec<i64> {
    let m = tx.env().records("test_search.mixed").unwrap();
    m.search_with(tx, &dom(domain), &SearchOptions::new().order("id"))
        .await
        .unwrap()
        .ids()
}

/// Every subset of `values` (as JSON lists).
fn subsets(values: &[Json]) -> Vec<Vec<Json>> {
    (0..1u32 << values.len())
        .map(|mask| {
            values
                .iter()
                .enumerate()
                .filter(|(i, _)| mask & (1 << i) != 0)
                .map(|(_, v)| v.clone())
                .collect()
        })
        .collect()
}

#[tokio::test]
async fn spec_field_016_integer_null_semantics() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    let ids = create(
        &mut tx,
        &[
            &[("number", Value::Int(42))],
            &[("number", Value::Int(0))],
            &[("number", Value::False)],
            &[],
        ],
    )
    .await;
    let stored = support::rows(
        &mut tx,
        "SELECT number::text FROM test_search_mixed ORDER BY id",
    )
    .await;
    let stored: Vec<Option<&str>> = stored.iter().map(|r| r[0].as_deref()).collect();
    assert_eq!(stored, [Some("42"), Some("0"), Some("0"), None]);
    let last3 = ids[1..].to_vec();
    for d in [
        json!([["number", "=", 0]]),
        json!([["number", "=", false]]),
        json!([["number", "<", 1]]),
        json!([["number", "<=", 0]]),
    ] {
        assert_eq!(find(&mut tx, d.clone()).await, last3, "{d}");
    }
    for d in [
        json!([["number", "!=", 0]]),
        json!([["number", "!=", false]]),
        json!([["number", ">", 1]]),
    ] {
        assert_eq!(find(&mut tx, d.clone()).await, [ids[0]], "{d}");
    }
    for d in [json!([["number", ">", -1]]), json!([["number", ">=", 0]])] {
        assert_eq!(find(&mut tx, d.clone()).await, ids, "{d}");
    }
    assert!(find(&mut tx, json!([["number", "<", -1]])).await.is_empty());
    // in-memory model: NULL reads as 0, and False means 0
    let values = [Some(42), Some(0), Some(0), None::<i64>];
    let as_int = |v: &Json| v.as_i64().unwrap_or(0);
    for set in subsets(&[json!(42), json!(0), json!(false)]) {
        let members: Vec<i64> = set.iter().map(as_int).collect();
        let inside: Vec<i64> = ids
            .iter()
            .zip(values)
            .filter(|(_, v)| members.contains(&v.unwrap_or(0)))
            .map(|(id, _)| *id)
            .collect();
        let outside: Vec<i64> = ids
            .iter()
            .copied()
            .filter(|id| !inside.contains(id))
            .collect();
        assert_eq!(
            find(&mut tx, json!([["number", "in", set]])).await,
            inside,
            "in {set:?}"
        );
        assert_eq!(
            find(&mut tx, json!([["number", "not in", set]])).await,
            outside,
            "not in {set:?}"
        );
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_017_char_empty_string_vs_null() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    let ids = create(
        &mut tx,
        &[
            &[("name", Value::str("name"))],
            &[("name", Value::str(""))],
            &[("name", Value::False)],
            &[],
        ],
    )
    .await;
    assert_eq!(
        one(
            &mut tx,
            "SELECT count(*)::text FROM test_search_mixed WHERE name IS NULL"
        )
        .await
        .as_deref(),
        Some("2")
    );
    let last3 = ids[1..].to_vec();
    for d in [json!([["name", "=", ""]]), json!([["name", "=", false]])] {
        assert_eq!(find(&mut tx, d.clone()).await, last3, "{d}");
    }
    for d in [json!([["name", "!=", ""]]), json!([["name", "!=", false]])] {
        assert_eq!(find(&mut tx, d.clone()).await, [ids[0]], "{d}");
    }
    assert_eq!(
        find(&mut tx, json!([["name", "=like", "na%"]])).await,
        [ids[0]]
    );
    assert_eq!(
        find(&mut tx, json!([["name", "not =like", "na%"]])).await,
        last3
    );
    let values = ["name", "", "", ""];
    for set in subsets(&[json!("name"), json!(""), json!(false)]) {
        let members: Vec<&str> = set.iter().map(|v| v.as_str().unwrap_or("")).collect();
        let inside: Vec<i64> = ids
            .iter()
            .zip(values)
            .filter(|(_, v)| members.contains(v))
            .map(|(id, _)| *id)
            .collect();
        let outside: Vec<i64> = ids
            .iter()
            .copied()
            .filter(|id| !inside.contains(id))
            .collect();
        assert_eq!(
            find(&mut tx, json!([["name", "in", set]])).await,
            inside,
            "in {set:?}"
        );
        assert_eq!(
            find(&mut tx, json!([["name", "not in", set]])).await,
            outside,
            "not in {set:?}"
        );
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_018_boolean_null_semantics() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    let ids = create(
        &mut tx,
        &[
            &[("flag", Value::Bool(true))],
            &[("flag", Value::Bool(false))],
            &[],
        ],
    )
    .await;
    tx.execute(
        "UPDATE test_search_mixed SET flag = NULL WHERE id = $1",
        &[&ids[2]],
    )
    .await
    .unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(find(&mut tx, json!([["flag", "=", true]])).await, [ids[0]]);
    let falsy = find(&mut tx, json!([["flag", "=", false]])).await;
    assert_eq!(falsy, [ids[1], ids[2]]);
    assert_eq!(find(&mut tx, json!([["flag", "!=", true]])).await, falsy);
    let m = tx.env().records("test_search.mixed").unwrap();
    tx.capture_statements(true);
    m.search(&mut tx, &dom(json!([["flag", "=", true]])))
        .await
        .unwrap();
    m.search(&mut tx, &dom(json!([["flag", "=", false]])))
        .await
        .unwrap();
    m.search(&mut tx, &dom(json!([["flag", "in", [true, false]]])))
        .await
        .unwrap();
    let t = "\"test_search_mixed\"";
    let captured = tx.captured().to_vec();
    assert!(
        captured[0].contains(&format!("WHERE {t}.\"flag\" IS TRUE ")),
        "{captured:?}"
    );
    assert!(
        captured[1].contains(&format!("WHERE {t}.\"flag\" IS NOT TRUE ")),
        "{captured:?}"
    );
    assert!(!captured[2].contains("WHERE"), "{captured:?}");
    assert_query_count!(tx, <= 0, {
        let none = m
            .search(&mut tx, &dom(json!([["flag", "not in", [true, false]]])))
            .await
            .unwrap();
        assert!(none.is_empty());
    });
    drop(tx);
    h.cleanup().await.unwrap();
}
