// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
#[path = "support/locking.rs"]
mod support;
use erp_addon_account::locking::{
    JournalClass, LockCheck, LockVerdict, TaxImpact, check_lock_dates,
};
use erp_sdk::{ErrorKind, prelude::*};
use support::*;

fn request(company: i64, date: Date, journal: JournalClass, tax: TaxImpact) -> LockCheck {
    LockCheck::new(company, date, journal, tax).unwrap()
}
#[tokio::test]
async fn inclusive_locks_select_by_journal_and_tax_impact() {
    let h = harness().await;
    let c = company(&h, None).await;
    periods(&h, c).await;
    h.db()
        .client()
        .await
        .unwrap()
        .execute(
            "UPDATE res_company SET fiscalyear_lock_date=$2, \
        sale_lock_date=$3,purchase_lock_date=$3,tax_lock_date=$3 WHERE id=$1",
            &[&c, &d(2024, 1, 15), &d(2024, 2, 15)],
        )
        .await
        .unwrap();
    let mut tx = begin(&h).await;
    let checks = [
        request(c, d(2024, 1, 15), JournalClass::Other, TaxImpact::None),
        request(c, d(2024, 1, 16), JournalClass::Other, TaxImpact::None),
        request(c, d(2024, 2, 15), JournalClass::Sale, TaxImpact::Report),
        request(c, d(2024, 2, 16), JournalClass::Sale, TaxImpact::Report),
        request(c, d(2024, 2, 15), JournalClass::Purchase, TaxImpact::None),
    ];
    let result = check_lock_dates(&mut tx, &checks).await.unwrap();
    assert_eq!(result.len(), 5);
    let error = result[0].clone().into_result().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "The accounting date 2024-01-15 is locked: fiscalyear_lock_date through 2024-01-15."
    );
    assert!(matches!(result[1], LockVerdict::Allowed));
    let error = result[2].clone().into_result().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "The accounting date 2024-02-15 is locked: sale_lock_date through 2024-02-15; tax_lock_date through 2024-02-15."
    );
    assert!(matches!(result[3], LockVerdict::Allowed));
    let error = result[4].clone().into_result().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "The accounting date 2024-02-15 is locked: purchase_lock_date through 2024-02-15."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}
#[tokio::test]
async fn ancestors_contribute_but_branches_do_not_lock_parents() {
    let h = harness().await;
    let root = company(&h, None).await;
    let branch = company(&h, Some(root)).await;
    periods(&h, root).await;
    h.db()
        .client()
        .await
        .unwrap()
        .execute(
            "UPDATE res_company SET sale_lock_date=$2 WHERE id=$1",
            &[&branch, &d(2024, 2, 15)],
        )
        .await
        .unwrap();
    let mut tx = begin(&h).await;
    let result = check_lock_dates(
        &mut tx,
        &[
            request(root, d(2024, 2, 15), JournalClass::Sale, TaxImpact::None),
            request(branch, d(2024, 2, 15), JournalClass::Sale, TaxImpact::None),
        ],
    )
    .await
    .unwrap();
    assert!(matches!(result[0], LockVerdict::Allowed));
    let error = result[1].clone().into_result().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "The accounting date 2024-02-15 is locked: sale_lock_date through 2024-02-15."
    );
    tx.execute(
        "UPDATE res_company SET purchase_lock_date=$2 WHERE id=$1",
        &[&root, &d(2024, 3, 1)],
    )
    .await
    .unwrap();
    let result = check_lock_dates(
        &mut tx,
        &[request(
            branch,
            d(2024, 3, 1),
            JournalClass::Purchase,
            TaxImpact::None,
        )],
    )
    .await
    .unwrap();
    let error = result[0].clone().into_result().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "The accounting date 2024-03-01 is locked: purchase_lock_date through 2024-03-01."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}
#[tokio::test]
async fn savepoint_rollback_cannot_leave_a_stale_policy_cache() {
    let h = harness().await;
    let c = company(&h, None).await;
    periods(&h, c).await;
    let mut tx = begin(&h).await;
    let checks = [request(
        c,
        d(2024, 1, 1),
        JournalClass::Sale,
        TaxImpact::None,
    )];
    assert!(matches!(
        check_lock_dates(&mut tx, &checks).await.unwrap()[0],
        LockVerdict::Allowed
    ));
    let result: Result<()> = tx
        .savepoint(async |tx| {
            tx.execute(
                "UPDATE res_company SET sale_lock_date=$2 WHERE id=$1",
                &[&c, &d(2024, 1, 31)],
            )
            .await?;
            let error = check_lock_dates(tx, &checks).await?[0]
                .clone()
                .into_result()
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Validation);
            assert_eq!(
                error.message(),
                "The accounting date 2024-01-01 is locked: sale_lock_date through 2024-01-31."
            );
            Err(Error::validation("rollback probe"))
        })
        .await;
    let error = result.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.message(), "rollback probe");
    assert!(matches!(
        check_lock_dates(&mut tx, &checks).await.unwrap()[0],
        LockVerdict::Allowed
    ));
    assert!(check_lock_dates(&mut tx, &[]).await.unwrap().is_empty());
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}
#[tokio::test]
async fn hard_locking_is_not_hidden_by_an_unset_company_date() {
    let h = harness().await;
    let c = company(&h, None).await;
    periods(&h, c).await;
    h.db()
        .client()
        .await
        .unwrap()
        .execute(
            "UPDATE account_fiscal_period SET status='hard_locking', \
        closed_at=now(),hard_lock_started_at=now(),hard_lock_started_by=1 \
        WHERE company_id=$1 AND number=1",
            &[&c],
        )
        .await
        .unwrap();
    let mut tx = begin(&h).await;
    let result = check_lock_dates(
        &mut tx,
        &[request(
            c,
            d(2024, 1, 2),
            JournalClass::Other,
            TaxImpact::None,
        )],
    )
    .await
    .unwrap();
    let error = result[0].clone().into_result().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "The fiscal period 2024/01 is hard_locking by the hard lock; nothing can be written in it."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn foreign_and_missing_companies_have_the_same_access_refusal() {
    use erp_sdk::{BeginOptions, Context, UserId};
    let h = harness().await;
    let mine = company(&h, None).await;
    let theirs = company(&h, None).await;
    let mut tx = h
        .orm()
        .begin_opts(
            BeginOptions::new(UserId::new(2).unwrap()).context(Context::from_pairs([(
                "allowed_company_ids",
                Value::list([Value::Int(mine)]),
            )])),
        )
        .await
        .unwrap();
    for company in [theirs, 999999] {
        let error = check_lock_dates(
            &mut tx,
            &[request(
                company,
                d(2024, 1, 1),
                JournalClass::Other,
                TaxImpact::None,
            )],
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access);
        assert_eq!(
            error.message(),
            "One of the selected companies does not exist or is not one of yours."
        );
    }
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[test]
fn check_input_rejects_invalid_company_and_extreme_date() {
    let error = LockCheck::new(0, d(2024, 1, 1), JournalClass::Other, TaxImpact::None).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "A lock check requires a positive company id."
    );
    let error = LockCheck::new(
        1,
        Date::from_days(i32::MIN),
        JournalClass::Other,
        TaxImpact::None,
    )
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "The accounting date is outside PostgreSQL's date range."
    );
}

#[tokio::test]
async fn a_truncated_branch_path_never_hides_its_parent_lock() {
    let h = harness().await;
    let root = company(&h, None).await;
    let branch = company(&h, Some(root)).await;
    periods(&h, root).await;
    let client = h.db().client().await.unwrap();
    client
        .execute(
            "UPDATE res_company SET sale_lock_date=$2 WHERE id=$1",
            &[&root, &d(2024, 1, 31)],
        )
        .await
        .unwrap();
    client
        .execute(
            "UPDATE res_company SET parent_path=id::text || '/' WHERE id=$1",
            &[&branch],
        )
        .await
        .unwrap();
    drop(client);
    let mut tx = begin(&h).await;
    let error = check_lock_dates(
        &mut tx,
        &[request(
            branch,
            d(2024, 1, 1),
            JournalClass::Sale,
            TaxImpact::None,
        )],
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Internal);
    assert_eq!(
        error.message(),
        format!("Company {branch} has an invalid lock-policy hierarchy.")
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}
