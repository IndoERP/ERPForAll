//! A `#[field_access]` override may only narrow, also before it calls `super` (review S1/T1).
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use erp_orm::FieldOp;
use erp_sdk::{ErrorKind, Value};
use erp_test_models::base_users;
use erp_test_models::test_field_access::{self, test_field_access_ext};
use erp_testkit::{FieldPolicy, Harness};
use support::map;

async fn widened(model: &str) -> (ErrorKind, String, ErrorKind) {
    let policy = FieldPolicy::new()
        .with_overrides()
        .deny(model, "name", FieldOp::Write);
    let h = Harness::builder([
        &base_users::ADDON,
        &test_field_access::ADDON,
        &test_field_access_ext::ADDON,
    ])
    .policy(policy.clone())
    .build()
    .await
    .expect("harness");
    policy.attach(h.registry()).unwrap();
    let mut tx = h.begin().await.unwrap();
    let created = tx
        .env()
        .records(model)
        .unwrap()
        .create_values(&mut tx, &[map(&[("name", Value::str("w"))])])
        .await
        .unwrap();
    let uid = tx
        .env()
        .records("res.users")
        .unwrap()
        .create_values(
            &mut tx,
            &[map(&[
                ("name", Value::str("d")),
                ("login", Value::str("d")),
            ])],
        )
        .await
        .unwrap()
        .ids()[0];
    let rec = created.with_user(uid);
    let error = rec
        .write_values(&mut tx, &map(&[("name", Value::str("x"))]))
        .await
        .unwrap_err();
    let read = rec.value(&mut tx, "name").await.unwrap_err();
    drop(tx);
    h.cleanup().await.unwrap();
    (error.kind(), error.to_string(), read.kind())
}

/// An override that passes `FieldMask::full(..)` to `super`.
#[tokio::test]
async fn override_widening_with_full_before_super_is_refused() {
    let (kind, message, read) = widened("test_fa.widen_full").await;
    assert_eq!(kind, ErrorKind::Programming);
    assert_eq!(read, ErrorKind::Programming);
    assert_eq!(
        message,
        "field access override test_fa.widen_full.widen_full passed super a mask wider than its \
         input at field 'name'"
    );
}

/// An override that grants a field before calling `super`.
#[tokio::test]
async fn override_granting_before_super_is_refused() {
    let (kind, message, read) = widened("test_fa.widen_grant").await;
    assert_eq!(kind, ErrorKind::Programming);
    assert_eq!(read, ErrorKind::Programming);
    assert_eq!(
        message,
        "field access override test_fa.widen_grant.widen_grant passed super a mask wider than its \
         input at field 'name'"
    );
}

/// An override that calls `super` with `FieldOp::Write` inside the read run.
#[tokio::test]
async fn override_calling_super_with_another_op_is_refused() {
    let (kind, message, read) = widened("test_fa.wrong_op").await;
    assert_eq!(kind, ErrorKind::Programming);
    assert_eq!(read, ErrorKind::Programming);
    assert_eq!(
        message,
        "field access override test_fa.wrong_op.wrong_op called super with Write in a Read run"
    );
}
