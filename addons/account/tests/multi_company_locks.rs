// SPDX-License-Identifier: MIT OR Apache-2.0
//! ACCMC branch semantics through the native policy API; entry hooks remain separate.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
#[path = "support/locking.rs"]
mod support;
use erp_addon_account::locking::{JournalClass, LockCheck, TaxImpact, check_lock_dates};
use erp_sdk::{ErrorKind, prelude::*};
use support::*;

#[tokio::test]
async fn every_lock_uses_ancestor_maxima_without_locking_siblings_or_parents() {
    let h = harness().await;
    let root = company(&h, None).await;
    let branch = company(&h, Some(root)).await;
    let leaf = company(&h, Some(branch)).await;
    let sibling = company(&h, Some(root)).await;
    periods(&h, root).await;
    // Trusted fixture SQL seeds policy only; it is not hard-lock activation evidence.
    for (field, journal, tax) in [
        ("hard_lock_date", JournalClass::Other, TaxImpact::None),
        ("fiscalyear_lock_date", JournalClass::Other, TaxImpact::None),
        ("tax_lock_date", JournalClass::Other, TaxImpact::Report),
        ("sale_lock_date", JournalClass::Sale, TaxImpact::None),
        (
            "purchase_lock_date",
            JournalClass::Purchase,
            TaxImpact::None,
        ),
    ] {
        for parent_later in [false, true] {
            let mut tx = begin(&h).await;
            let (parent_date, branch_date) = if parent_later {
                (d(2024, 2, 15), d(2024, 1, 15))
            } else {
                (d(2024, 1, 15), d(2024, 2, 15))
            };
            tx.execute(
                &format!("UPDATE res_company SET {field}=$2 WHERE id=$1"),
                &[&root, &parent_date],
            )
            .await
            .unwrap();
            tx.execute(
                &format!("UPDATE res_company SET {field}=$2 WHERE id=$1"),
                &[&branch, &branch_date],
            )
            .await
            .unwrap();
            let checks = [
                LockCheck::new(leaf, d(2024, 2, 15), journal, tax).unwrap(),
                LockCheck::new(leaf, d(2024, 2, 16), journal, tax).unwrap(),
                LockCheck::new(root, d(2024, 2, 15), journal, tax).unwrap(),
                LockCheck::new(sibling, d(2024, 2, 15), journal, tax).unwrap(),
            ];
            let result = check_lock_dates(&mut tx, &checks).await.unwrap();
            let error = result[0].clone().into_result().unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Validation);
            assert_eq!(
                error.message(),
                format!("The accounting date 2024-02-15 is locked: {field} through 2024-02-15.")
            );
            result[1].clone().into_result().unwrap();
            for verdict in &result[2..] {
                if parent_later {
                    let error = verdict.clone().into_result().unwrap_err();
                    assert_eq!(error.kind(), ErrorKind::Validation);
                    assert_eq!(
                        error.message(),
                        format!(
                            "The accounting date 2024-02-15 is locked: {field} through 2024-02-15."
                        )
                    );
                } else {
                    verdict.clone().into_result().unwrap();
                }
            }
            tx.rollback().await.unwrap();
            drop(tx);
        }
    }
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn branch_diagnostics_keep_all_applicable_ancestor_locks_and_reread_after_rollback() {
    let h = harness().await;
    let root = company(&h, None).await;
    let branch = company(&h, Some(root)).await;
    periods(&h, root).await;
    let mut tx = begin(&h).await;
    tx.execute(
        "UPDATE res_company SET fiscalyear_lock_date=$2,hard_lock_date=$3 WHERE id=$1",
        &[&root, &d(2024, 1, 15), &d(2024, 1, 31)],
    )
    .await
    .unwrap();
    let checks =
        [LockCheck::new(branch, d(2024, 1, 1), JournalClass::Sale, TaxImpact::Report).unwrap()];
    let expected = "The accounting date 2024-01-01 is locked: fiscalyear_lock_date through 2024-01-15; hard_lock_date through 2024-01-31.";
    let error = check_lock_dates(&mut tx, &checks).await.unwrap()[0]
        .clone()
        .into_result()
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.message(), expected);
    let result: Result<()> = tx.savepoint(async |tx| {
        tx.execute("UPDATE res_company SET tax_lock_date=$2,sale_lock_date=$3 WHERE id=$1",
            &[&branch, &d(2024, 2, 15), &d(2024, 2, 16)]).await?;
        let error = check_lock_dates(tx, &checks).await?[0].clone().into_result().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(error.message(), "The accounting date 2024-01-01 is locked: fiscalyear_lock_date through 2024-01-15; hard_lock_date through 2024-01-31; tax_lock_date through 2024-02-15; sale_lock_date through 2024-02-16.");
        Err(Error::validation("Rollback the branch policy probe."))
    }).await;
    let error = result.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.message(), "Rollback the branch policy probe.");
    let error = check_lock_dates(&mut tx, &checks).await.unwrap()[0]
        .clone()
        .into_result()
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.message(), expected);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}
