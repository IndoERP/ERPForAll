//! query budgets that S1 can measure (relational paths and computes come later).
//! Each budget has a lower bound too, and the written values are read back from the database, so a
//! test cannot pass by doing nothing.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use support::currency_fixture;

mod support;

use erp_sdk::{OrmConfig, Vals};
use erp_test_models::test_orm::Partner;
use erp_test_models::{base_users, test_orm};
use erp_testkit::{Harness, assert_query_count};

#[tokio::test]
async fn budget_create_one_record() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().model::<Partner>().unwrap();
    partners
        .create(&mut tx, vec![partners.vals().set(Partner::NAME, "warm")])
        .await
        .unwrap();
    let rec = assert_query_count!(tx, >= 1, <= 1, {
        partners
            .create(&mut tx, vec![partners.vals().set(Partner::NAME, "x")])
            .await
            .unwrap()
    });
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        rec.get(&mut tx, Partner::NAME).await.unwrap().as_deref(),
        Some("x")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Read two fields on 5 records: 1 query (prefetch of the whole set).
#[tokio::test]
async fn budget_read_five_records() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().model::<Partner>().unwrap();
    let vals = (0..5)
        .map(|i| partners.vals().set(Partner::NAME, format!("p{i}").as_str()))
        .collect();
    let rs = partners.create(&mut tx, vals).await.unwrap();
    tx.invalidate_all().await.unwrap();
    let names = assert_query_count!(tx, >= 1, <= 1, {
        let mut names = Vec::new();
        for r in rs.iter() {
            names.push(r.get(&mut tx, Partner::NAME).await.unwrap());
            r.get(&mut tx, Partner::EMAIL).await.unwrap();
        }
        names
    });
    let names: Vec<_> = names.iter().map(|n| n.as_deref().unwrap()).collect();
    assert_eq!(names, ["p0", "p1", "p2", "p3", "p4"]);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Write 5 records (same or different values): budget 1.
#[tokio::test]
async fn budget_write_five_records() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().model::<Partner>().unwrap();
    let vals = (0..5).map(|_| Vals::new()).collect();
    let rs = partners.create(&mut tx, vals).await.unwrap();
    assert_query_count!(tx, >= 1, <= 1, {
        rs.write(&mut tx, rs.vals().set(Partner::NAME, "X")).await.unwrap();
        tx.flush().await.unwrap();
    });
    tx.invalidate_all().await.unwrap();
    let names = rs.mapped(&mut tx, Partner::NAME).await.unwrap();
    assert!(names.iter().all(|n| n.as_deref() == Some("X")), "{names:?}");
    assert_query_count!(tx, >= 1, <= 1, {
        for (i, r) in rs.iter().enumerate() {
            r.set(&mut tx, Partner::NAME, format!("X {i}").as_str()).await.unwrap();
        }
        tx.flush().await.unwrap();
    });
    tx.invalidate_all().await.unwrap();
    let names = rs.mapped(&mut tx, Partner::NAME).await.unwrap();
    let names: Vec<_> = names.iter().map(|n| n.as_deref().unwrap()).collect();
    assert_eq!(names, ["X 0", "X 1", "X 2", "X 3", "X 4"]);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// `mapped('value')` over 1,280 records: budget 1.
#[tokio::test]
async fn budget_mapped_over_1280_records() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().model::<Partner>().unwrap();
    let vals = (0..1280)
        .map(|i| partners.vals().set(Partner::NUMBER, i))
        .collect();
    let rs = partners.create(&mut tx, vals).await.unwrap();
    tx.invalidate_all().await.unwrap();
    let values = assert_query_count!(tx, >= 1, <= 1, {
        rs.mapped(&mut tx, Partner::NUMBER).await.unwrap()
    });
    assert_eq!(values, (0..1280).collect::<Vec<i32>>());
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Prefetch chunks are bounded and each chunk is one query with one array parameter.
#[tokio::test]
async fn prefetch_chunks_are_counted() {
    let h = Harness::builder([&base_users::ADDON, &test_orm::ADDON])
        .with_static_decls(crate::currency_fixture::decls())
        .config(OrmConfig {
            prefetch_chunk: 100,
            ..OrmConfig::default()
        })
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().model::<Partner>().unwrap();
    let vals = (0..250)
        .map(|i| partners.vals().set(Partner::NUMBER, i))
        .collect();
    let rs = partners.create(&mut tx, vals).await.unwrap();
    tx.invalidate_all().await.unwrap();
    tx.capture_statements(true);
    let values = assert_query_count!(tx, >= 3, <= 3, {
        rs.mapped(&mut tx, Partner::NUMBER).await.unwrap()
    });
    assert_eq!(values, (0..250).collect::<Vec<i32>>());
    assert!(
        tx.captured()
            .iter()
            .all(|s| s.ends_with("WHERE \"id\" = ANY($1)"))
    );
    drop(tx);
    h.cleanup().await.unwrap();
}
