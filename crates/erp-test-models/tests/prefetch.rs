//! Prefetch behavior on searched records (ENV §4.8:…033, 037…039) and
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use support::currency_fixture;

mod support;

use std::collections::BTreeSet;
use std::sync::Arc;

use erp_sdk::{Domain, ErrorKind, RecId, Records, Transaction, Value};
use erp_testkit::assert_query_count;
use support::map;

async fn perf(tx: &mut Transaction, n: usize) -> Records {
    let empty = tx.env().records("test_search.perf").unwrap();
    let values: Vec<_> = (0..n)
        .map(|i| {
            map(&[
                ("name", Value::str(format!("r{i}"))),
                ("value", Value::Int(i as i64)),
                ("rare", Value::str("x")),
            ])
        })
        .collect();
    empty.create_values(tx, &values).await.unwrap();
    tx.invalidate_all().await.unwrap();
    let all = tx.env().records("test_search.perf").unwrap();
    all.search(tx, &Domain::TRUE).await.unwrap()
}

fn set(ids: &[RecId]) -> BTreeSet<RecId> {
    ids.iter().copied().collect()
}

#[tokio::test]
async fn spec_env_028_prefetch_set_propagation() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    let records = perf(&mut tx, 6).await;
    let ids = records.rec_ids().to_vec();
    assert_eq!(
        records.prefetch_ids(),
        &ids[..],
        "a search result prefetches itself"
    );
    assert_eq!(records.with_user(2).prefetch_ids(), &ids[..]);
    assert_eq!(
        records
            .with_context([("lang", Value::str("fr_FR"))])
            .prefetch_ids(),
        &ids[..]
    );
    assert_eq!(records.at(0).unwrap().prefetch_ids(), &ids[..]);
    let sub = records.slice(None, Some(5), 1).unwrap();
    assert_eq!(
        set(sub.prefetch_ids()),
        set(&ids),
        "slices cover the same set"
    );
    let back = sub.with_prefetch(Some(Arc::from(ids.clone())));
    assert_eq!(back.prefetch_ids(), &ids[..]);
    let five = records.browse_ids(ids[..5].iter().copied());
    assert_ne!(set(five.prefetch_ids()), set(&ids));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// keeps the whole set.
#[tokio::test]
async fn spec_env_029_set_operations_combine_prefetch_sets() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    let records = perf(&mut tx, 1000).await;
    let all = set(records.rec_ids());
    let part = records.slice(None, Some(3), 1).unwrap();
    let ners = records.slice(Some(3), None, 1).unwrap();
    for combined in [
        part.concat(&ners).unwrap(),
        part.union(&ners).unwrap(),
        records.intersection(&ners).unwrap(),
        records.difference(&ners).unwrap(),
    ] {
        assert_eq!(set(combined.prefetch_ids()), all);
    }
    let mut acc = records.browse_ids([]);
    for single in records.iter() {
        acc = acc.union(&single.with_prefetch(None)).unwrap();
    }
    assert_eq!(acc.len(), 1000);
    assert_eq!(set(acc.prefetch_ids()), all);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// invalidating that field (budget ≤ 1).
#[tokio::test]
async fn spec_env_030_one_read_loads_the_prefetch_set() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    let records = perf(&mut tx, 1280).await;
    let values = assert_query_count!(tx, >= 1, <= 1, {
        records.cells(&mut tx, "value").await.unwrap()
    });
    assert_eq!(values.len(), 1280);
    tx.invalidate_model("test_search.perf", Some(&["value"]))
        .await
        .unwrap();
    assert_query_count!(tx, >= 1, <= 1, {
        records.cells(&mut tx, "value").await.unwrap();
    });
    drop(tx);
    h.cleanup().await.unwrap();
}

/// with `prefetch=False` loads alone. Named prefetch groups are not declarable yet.
#[tokio::test]
async fn spec_env_031_prefetch_groups() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    let records = perf(&mut tx, 10).await;
    tx.capture_statements(true);
    assert_query_count!(tx, >= 2, <= 2, {
        records.cells(&mut tx, "name").await.unwrap();
        records.cells(&mut tx, "value_pc").await.unwrap();
        records.cells(&mut tx, "rare").await.unwrap();
    });
    let captured = tx.captured().to_vec();
    assert!(captured[0].contains("\"value_pc\"") && !captured[0].contains("\"rare\""));
    assert!(captured[0].contains("\"create_date\"") && captured[0].contains("\"partner_id\""));
    assert_eq!(
        captured[1],
        "SELECT \"id\", \"rare\" FROM \"test_search_perf\" WHERE \"id\" = ANY($1)"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// `fetch` caches the field for every record in bounded batches.
#[tokio::test]
async fn spec_env_032_big_recordsets_are_bounded() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    let records = perf(&mut tx, 10_010).await;
    assert_query_count!(tx, >= 1, <= 1, {
        records.at(0).unwrap().value(&mut tx, "name").await.unwrap();
    });
    let first = records.slice(None, Some(10_000), 1).unwrap();
    assert!(tx.is_cached(&first, "name").unwrap());
    tx.invalidate_model("test_search.perf", Some(&["name"]))
        .await
        .unwrap();
    assert_query_count!(tx, >= 2, <= 2, {
        records.fetch(&mut tx, Some(&["name"])).await.unwrap();
    });
    assert!(tx.is_cached(&records, "name").unwrap());
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_env_033_prefetch_fields_context_key() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    let records = perf(&mut tx, 10).await;
    let narrow = records.with_context([("prefetch_fields", Value::Bool(false))]);
    tx.capture_statements(true);
    narrow.cells(&mut tx, "name").await.unwrap();
    assert_eq!(
        tx.captured().last().unwrap(),
        "SELECT \"id\", \"name\" FROM \"test_search_perf\" WHERE \"id\" = ANY($1)"
    );
    assert!(!tx.is_cached(&records, "value").unwrap());
    drop(tx);
    h.cleanup().await.unwrap();
}

/// it raises `Missing`.
#[tokio::test]
async fn spec_env_037_deleted_records_in_a_prefetch_set() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    let records = perf(&mut tx, 2).await;
    let gone = records.rec_ids()[1].get();
    tx.execute("DELETE FROM test_search_perf WHERE id = $1", &[&gone])
        .await
        .unwrap();
    tx.invalidate_all().await.unwrap();
    let kept = records.at(0).unwrap();
    assert_eq!(kept.value(&mut tx, "name").await.unwrap(), Value::str("r0"));
    let err = records
        .at(1)
        .unwrap()
        .value(&mut tx, "name")
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Missing);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// arrive with slice S4).
#[tokio::test]
async fn spec_env_038_fetch_loads_only_what_is_missing() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    let records = perf(&mut tx, 5).await;
    assert_query_count!(tx, >= 1, <= 1, {
        records.fetch(&mut tx, Some(&["name", "partner_id"])).await.unwrap();
    });
    assert_query_count!(tx, <= 0, {
        records.cells(&mut tx, "name").await.unwrap();
        records.cells(&mut tx, "partner_id").await.unwrap();
    });
    assert_query_count!(tx, >= 1, <= 1, {
        records.cells(&mut tx, "value").await.unwrap();
    });
    assert_query_count!(tx, <= 0, {
        records.fetch(&mut tx, Some(&["name", "value"])).await.unwrap();
        records
            .fetch(&mut tx, Some(&["id", "name", "partner_id"]))
            .await
            .unwrap();
        records.fetch(&mut tx, Some(&["id", "display_name"])).await.unwrap();
    });
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_env_039_fetch_field_validation() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    let records = perf(&mut tx, 2).await;
    let err = assert_query_count!(tx, <= 0, {
        records.fetch(&mut tx, Some(&["nope"])).await.unwrap_err()
    });
    assert_eq!(err.message(), "Invalid field 'nope' on 'test_search.perf'");
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rset_050_prefetch_sets_never_change_results() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    let records = perf(&mut tx, 5).await;
    let sub = records.slice(None, Some(2), 1).unwrap();
    let third = records.at(2).unwrap();
    let alone = assert_query_count!(tx, >= 1, <= 1, {
        sub.with_prefetch(None).cells(&mut tx, "name").await.unwrap()
    });
    assert!(!tx.is_cached(&third, "name").unwrap(), "only 2 rows loaded");
    tx.invalidate_all().await.unwrap();
    let shared = assert_query_count!(tx, >= 1, <= 1, {
        sub.cells(&mut tx, "name").await.unwrap()
    });
    assert!(
        tx.is_cached(&third, "name").unwrap(),
        "the whole set loaded"
    );
    assert_eq!(alone, shared);
    let reset = sub.with_prefetch(None);
    assert_eq!(reset.rec_ids(), sub.rec_ids());
    assert_eq!(reset.env(), sub.env());
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_env_058_read_computes_non_stored_only_for_read_records() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx
        .env()
        .model::<erp_test_models::test_rel::Partner>()
        .unwrap();

    let parent = partners
        .create(
            &mut tx,
            vec![
                partners
                    .vals()
                    .set(erp_test_models::test_rel::Partner::NAME, "Amber & Forge"),
            ],
        )
        .await
        .unwrap();
    let child = partners
        .create(
            &mut tx,
            vec![
                partners
                    .vals()
                    .set(erp_test_models::test_rel::Partner::NAME, "Child")
                    .set(
                        erp_test_models::test_rel::Partner::PARENT_ID,
                        parent.rec_ids()[0],
                    ),
            ],
        )
        .await
        .unwrap();

    tx.commit().await.unwrap();

    let mut tx = h.begin().await.unwrap();
    let parent = parent.with_prefetch(None);

    let res1 = parent.read(&mut tx, &["vat"]).await.unwrap();
    assert_eq!(res1[0].get("vat"), Some(&Value::str("Amber & Forge")));
    assert!(tx.is_cached(parent.as_records(), "vat").unwrap());
    assert!(!tx.is_cached(child.as_records(), "vat").unwrap());

    tx.invalidate_all().await.unwrap();
    let parent = parent.with_prefetch(None);

    let res2 = parent.read(&mut tx, &["child_ids", "vat"]).await.unwrap();
    assert_eq!(res2[0].get("vat"), Some(&Value::str("Amber & Forge")));
    assert_eq!(
        res2[0].get("child_ids"),
        Some(&Value::list([Value::Int(child.ids()[0])]))
    );
    assert!(tx.is_cached(parent.as_records(), "vat").unwrap());
    assert!(!tx.is_cached(child.as_records(), "vat").unwrap());

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_env_036_prefetch_unreadable_fallback() {
    let cond = Domain::condition("name", erp_sdk::Operator::Ne, "hidden").unwrap();
    let policy = erp_testkit::DomainPolicy::builder(erp_testkit::Otherwise::Allow)
        .rule("test_orm.category", erp_orm::Operation::Read, cond)
        .build()
        .unwrap();
    let h = erp_testkit::Harness::builder([
        &erp_test_models::test_orm::ADDON,
        &erp_test_models::base_users::ADDON,
    ])
    .with_static_decls(crate::currency_fixture::decls())
    .policy(policy)
    .build()
    .await
    .expect("harness");
    let mut tx = h.begin().await.unwrap();
    tx.batch("INSERT INTO res_users (id, name, login, active) VALUES (2, 'demo', 'demo', true)")
        .await
        .unwrap();
    let categories = tx
        .env()
        .model::<erp_test_models::test_orm::Category>()
        .unwrap();
    let cat1 = categories
        .create(
            &mut tx,
            vec![
                categories
                    .vals()
                    .set(erp_test_models::test_orm::Category::NAME, "hidden"),
            ],
        )
        .await
        .unwrap();
    let cat2 = categories
        .create(
            &mut tx,
            vec![
                categories
                    .vals()
                    .set(erp_test_models::test_orm::Category::NAME, "visible"),
            ],
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();

    const DEMO: i64 = 2;
    let mut tx = h.begin().await.unwrap();
    let all = cat1.concat(&cat2).unwrap().with_user(DEMO);
    let cat1_demo = all.at(0).unwrap();
    let cat2_demo = all.at(1).unwrap();

    // cat2 is readable and succeeds
    let name2 = cat2_demo
        .get(&mut tx, erp_test_models::test_orm::Category::NAME)
        .await
        .unwrap();
    assert_eq!(name2.as_deref(), Some("visible"));

    // cat1 is unreadable and raises AccessError with exact message
    let err = cat1_demo
        .get(&mut tx, erp_test_models::test_orm::Category::NAME)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        format!(
            "Your access rights do not let you access 'Test ORM Category' (test_orm.category) records.\n\nRecords: test_orm.category({},), User: {DEMO}",
            cat1.ids()[0]
        )
    );

    drop(tx);
    h.cleanup().await.unwrap();
}
