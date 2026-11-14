//! More ENV cases from the S1 review: pending writes across commit/rollback,
//! fetch never overwriting pending values end to end, when access is checked, and
//! malformed company lists.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use support::currency_fixture;

mod support;

use erp_sdk::{ErrorKind, StaticPolicy, Value};
use erp_test_models::test_orm::Partner;
use erp_test_models::{base_users, test_orm};
use erp_testkit::{Harness, assert_query_count};
use support::one;

#[tokio::test]
async fn spec_env_013_unflushed_write_is_committed() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().model::<Partner>().unwrap();
    let rec = partners
        .create(&mut tx, vec![partners.vals().set(Partner::NAME, "before")])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    rec.set(&mut tx, Partner::NAME, "after").await.unwrap();
    assert!(tx.is_dirty(&rec, "name").unwrap());
    tx.commit().await.unwrap();
    assert!(!tx.is_cached(&rec, "name").unwrap());
    assert_eq!(
        one(&mut tx, "SELECT name FROM test_orm_partner")
            .await
            .as_deref(),
        Some("after")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_env_013_unflushed_write_is_rolled_back() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().model::<Partner>().unwrap();
    let rec = partners
        .create(&mut tx, vec![partners.vals().set(Partner::NAME, "before")])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    rec.set(&mut tx, Partner::NAME, "after").await.unwrap();
    tx.rollback().await.unwrap();
    assert!(!tx.is_cached(&rec, "name").unwrap());
    assert!(!tx.is_dirty(&rec, "name").unwrap());
    assert_eq!(
        rec.get(&mut tx, Partner::NAME).await.unwrap().as_deref(),
        Some("before")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// SELECT, sends no UPDATE first, and keeps the pending value.
#[tokio::test]
async fn spec_env_017_fetch_never_overwrites_pending_values() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().model::<Partner>().unwrap();
    let vals = (1..=3)
        .map(|i| {
            partners
                .vals()
                .set(Partner::NAME, format!("p{i}").as_str())
                .set(Partner::NUMBER, i)
        })
        .collect();
    let records = partners.create(&mut tx, vals).await.unwrap();
    tx.invalidate_all().await.unwrap();
    records
        .at(1)
        .unwrap()
        .set(&mut tx, Partner::NUMBER, 42)
        .await
        .unwrap();
    tx.capture_statements(true);
    let names = assert_query_count!(tx, >= 1, <= 1, {
        let mut names = Vec::new();
        for r in records.iter() {
            names.push(r.get(&mut tx, Partner::NAME).await.unwrap());
        }
        names
    });
    assert!(
        tx.captured()[0].starts_with("SELECT"),
        "{:?}",
        tx.captured()
    );
    assert_eq!(names.len(), 3);
    let numbers = assert_query_count!(tx, <= 0, {
        records.mapped(&mut tx, Partner::NUMBER).await.unwrap()
    });
    assert_eq!(numbers, [1, 42, 3]);
    // still pending: the database has the old value
    assert_eq!(
        one(
            &mut tx,
            &format!(
                "SELECT number::text FROM test_orm_partner WHERE id = {}",
                records.ids()[1]
            )
        )
        .await
        .as_deref(),
        Some("2")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn access_is_checked_on_both_cached_and_uncached_reads() {
    let h = Harness::builder([&base_users::ADDON, &test_orm::ADDON])
        .with_static_decls(crate::currency_fixture::decls())
        .policy(StaticPolicy::SuperuserOnly)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().model::<Partner>().unwrap();
    let rec = partners
        .create(&mut tx, vec![partners.vals().set(Partner::NAME, "cached")])
        .await
        .unwrap();
    let as_user = rec.with_user(7);
    for invalidate in [false, true] {
        if invalidate {
            tx.invalidate_all().await.unwrap();
        }
        let err = as_user.get(&mut tx, Partner::NAME).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Access);
        assert_eq!(
            err.message(),
            "Your access rights do not let you read 'Test ORM Partner' (test_orm.partner) records."
        );
        assert_eq!(
            rec.get(&mut tx, Partner::NAME).await.unwrap().as_deref(),
            Some("cached")
        );
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

/// a malformed `allowed_company_ids` is an error, never "no restriction".
#[tokio::test]
async fn malformed_company_list_is_an_error() {
    let h = support::orm().await;
    let tx = h.begin().await.unwrap();
    let env = tx
        .env()
        .with_context([("allowed_company_ids", Value::str("1,2"))]);
    let context_error = env.context().allowed_company_ids().unwrap_err();
    assert_eq!(context_error.kind(), ErrorKind::Validation);
    // the environment reports the same error when its companies are used
    let err = env.with_company(1).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.message(), context_error.message());
    assert_eq!(
        err.message(),
        "Invalid allowed_company_ids in context: '1,2' (expected a list of company ids)"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}
