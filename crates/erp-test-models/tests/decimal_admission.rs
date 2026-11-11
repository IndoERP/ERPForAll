//! Generic CRUD admission rejects precision loss before any row mutation.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod support;
use erp_sdk::{ErrorKind, Value};

#[tokio::test]
async fn unrounded_create_and_write_reject_loss_without_mutation() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let all = tx.env().records("test_orm.mixed").unwrap();
    let input = "0.12345678901234567890123456789";
    let good = support::map(&[("numeric", Value::str("1.25"))]);
    let bad = support::map(&[("numeric", Value::str(input))]);
    let error = all
        .create_values(&mut tx, &[good.clone(), bad.clone()])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        format!(
            "Decimal precision exceeded for test_orm.mixed.numeric: '{input}' (maximum 28 digits)"
        )
    );
    assert_eq!(
        support::one(&mut tx, "SELECT count(*)::text FROM test_orm_mixed")
            .await
            .as_deref(),
        Some("0")
    );
    let created = all
        .create_values(&mut tx, &[good.clone(), good])
        .await
        .unwrap();
    let error = created.write_values(&mut tx, &bad).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        format!(
            "Decimal precision exceeded for test_orm.mixed.numeric: '{input}' (maximum 28 digits)"
        )
    );
    assert_eq!(
        support::rows(
            &mut tx,
            "SELECT numeric::text FROM test_orm_mixed ORDER BY id"
        )
        .await,
        vec![vec![Some("1.25".into())], vec![Some("1.25".into())]]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}
