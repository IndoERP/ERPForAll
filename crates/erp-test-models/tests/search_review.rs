//! Fixes from the S2 review round: audit-field freshness, error provenance, stable order, the typed
//! search API, `search_count(limit=0)`, many2one rec names.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use std::sync::Arc;

use erp_sdk::serde_json::json;
use erp_sdk::{Domain, ErrorKind, Operator, SearchOptions, Transaction, Value};
use erp_test_models::test_search::Partner;
use support::{dom, map, one};

async fn create(tx: &mut Transaction, model: &str, rows: &[&[(&str, Value)]]) -> Vec<i64> {
    let empty = tx.env().records(model).unwrap();
    let values: Vec<_> = rows.iter().map(|r| map(r)).collect();
    empty.create_values(tx, &values).await.unwrap().ids()
}

#[tokio::test]
async fn write_date_is_fresh_for_searches() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    let id = create(
        &mut tx,
        "test_search.partner",
        &[&[("name", Value::str("a"))]],
    )
    .await[0];
    tx.execute(
        "UPDATE test_search_partner SET write_date = '2000-01-01' WHERE id = $1",
        &[&id],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let t0 = one(
        &mut tx,
        "SELECT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS')",
    )
    .await
    .unwrap();
    let partners = tx.env().records("test_search.partner").unwrap();
    partners
        .browse([id])
        .unwrap()
        .set_value(&mut tx, "name", Value::str("b"))
        .await
        .unwrap();
    let found = partners
        .search(&mut tx, &dom(json!([["write_date", ">=", t0]])))
        .await
        .unwrap();
    assert_eq!(found.ids(), [id]);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review item 6: a condition a later slice supports is the client's error for an
/// external domain (`Validation`, 422) and the code's for a domain built in Rust (`Programming`).
#[tokio::test]
async fn not_supported_errors_follow_provenance() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().records("test_search.partner").unwrap();
    let external = dom(json!([["name", "child_of", 1]]));
    let err = partners.search(&mut tx, &external).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation, "{err}");
    assert!(
        err.message().contains("works only for relational fields"),
        "{err}"
    );
    let typed = Domain::condition("name", Operator::ChildOf, 1).unwrap();
    let err = partners.search(&mut tx, &typed).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming, "{err}");
    // the `access` operator refuses a bad operation the same way
    let err = partners
        .search(&mut tx, &dom(json!([["country_id", "access", "blabla"]])))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation, "{err}");
    assert_eq!(
        err.message(),
        "Invalid value for 'access' operator in condition ('country_id', 'access', 'blabla')"
    );
    let typed = Domain::condition("country_id", Operator::Access, "blabla").unwrap();
    let err = partners.search(&mut tx, &typed).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming, "{err}");
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review item 9: `id` breaks ties, so equal keys come back in a stable order on every page.
#[tokio::test]
async fn ties_are_ordered_by_id() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    let ids = create(
        &mut tx,
        "test_search.partner",
        &[
            &[("name", Value::str("X"))],
            &[("name", Value::str("X"))],
            &[("name", Value::str("A"))],
            &[("name", Value::str("X"))],
        ],
    )
    .await;
    let partners = tx.env().records("test_search.partner").unwrap();
    for (order, expected) in [
        ("name", [ids[2], ids[0], ids[1], ids[3]]),
        ("name desc", [ids[0], ids[1], ids[3], ids[2]]),
    ] {
        let found = partners
            .search_with(&mut tx, &Domain::TRUE, &SearchOptions::new().order(order))
            .await
            .unwrap();
        assert_eq!(found.ids(), expected, "{order}");
        for page in 0..4 {
            let one = partners
                .search_with(
                    &mut tx,
                    &Domain::TRUE,
                    &SearchOptions::new().order(order).offset(page).limit(1),
                )
                .await
                .unwrap();
            assert_eq!(one.ids(), [expected[page as usize]], "{order} page {page}");
        }
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn typed_search_api() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().model::<Partner>().unwrap();
    let created = partners
        .create(
            &mut tx,
            (0..3)
                .map(|i| {
                    partners
                        .vals()
                        .set(Partner::NAME, format!("acme {i}").as_str())
                        .set(Partner::NUMBER, i)
                })
                .collect(),
        )
        .await
        .unwrap();
    let acme = Domain::field("name").unwrap().ilike("acme");
    assert_eq!(
        partners.search_count(&mut tx, &acme, None).await.unwrap(),
        3
    );
    assert_eq!(
        partners
            .search_count(&mut tx, &acme, Some(0))
            .await
            .unwrap(),
        3
    );
    assert_eq!(
        partners
            .search_count(&mut tx, &acme, Some(2))
            .await
            .unwrap(),
        2
    );
    let found = partners.search(&mut tx, &acme).await.unwrap();
    assert_eq!(found, created);
    let names = partners.name_search(&mut tx, "acme 1").await.unwrap();
    assert_eq!(names, [(created.ids()[1], Some("acme 1".into()))]);
    let rows = partners
        .search_read(
            &mut tx,
            &Domain::field("number").unwrap().ge(1),
            &["name", "number"],
            &SearchOptions::new().order("number desc"),
        )
        .await
        .unwrap();
    let numbers: Vec<&Value> = rows.iter().map(|r| &r["number"]).collect();
    assert_eq!(numbers, [&Value::Int(2), &Value::Int(1)]);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rset_051_many2one_rec_name_is_the_target_display_name() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    let pid = create(
        &mut tx,
        "test_search.partner",
        &[&[("name", Value::str("p"))]],
    )
    .await[0];
    let id = create(
        &mut tx,
        "test_search.by_partner",
        &[&[("partner_id", Value::Int(pid))]],
    )
    .await[0];
    let records = tx.env().records("test_search.by_partner").unwrap();
    let name = records
        .browse([id])
        .unwrap()
        .value(&mut tx, "display_name")
        .await
        .unwrap();
    assert_eq!(name, Value::str("p"));
    // the name search goes through the comodel's display name
    let found = records.name_search(&mut tx, "p").await.unwrap();
    assert_eq!(found, [(id, Some(Arc::from("p")))]);
    assert!(records.name_search(&mut tx, "zz").await.unwrap().is_empty());
    drop(tx);
    h.cleanup().await.unwrap();
}
