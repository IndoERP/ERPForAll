// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
#[path = "support/audit_sealer.rs"]
mod support;
use erp_orm::audit::seal::{EpochChange, EpochReason, SealOptions, SealOutcome};
use support::*;

#[tokio::test]
async fn held_low_xid_writer_cannot_be_skipped() {
    let h = initialized().await;
    let low = owner(&h).await;
    let high = owner(&h).await;
    low.batch_execute("BEGIN").await.unwrap();
    let a = insert(&low, 10, "[]").await;
    let b = insert(&high, 10, "[]").await;
    assert!(a < b);
    let mut tx = begin_unsettled(&h).await;
    assert!(matches!(
        tx.seal_audit_batch(&SealOptions::default()).await.unwrap(),
        SealOutcome::NoWork
    ));
    tx.rollback().await.unwrap();
    drop(tx);
    low.batch_execute("COMMIT").await.unwrap();
    let mut tx = begin(&h).await;
    let seal = staged(tx.seal_audit_batch(&SealOptions::default()).await.unwrap());
    assert_eq!(seal.row_count(), 2);
    assert_eq!(seal.upper_cursor(), b);
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn rolled_back_frontier_writer_does_not_leave_a_gap() {
    let h = initialized().await;
    let low = owner(&h).await;
    let high = owner(&h).await;
    low.batch_execute("BEGIN").await.unwrap();
    insert(&low, 10, "[]").await;
    let b = insert(&high, 10, "[]").await;
    low.batch_execute("ROLLBACK").await.unwrap();
    let mut tx = begin(&h).await;
    let seal = staged(tx.seal_audit_batch(&SealOptions::default()).await.unwrap());
    assert_eq!(seal.row_count(), 1);
    assert_eq!(seal.upper_cursor(), b);
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn sealers_and_epoch_transitions_share_the_transaction_lock() {
    let h = initialized().await;
    let c = owner(&h).await;
    insert(&c, 10, "[]").await;
    let mut a = begin(&h).await;
    let mut b = begin(&h).await;
    staged(a.seal_audit_batch(&SealOptions::default()).await.unwrap());
    assert!(matches!(
        b.seal_audit_batch(&SealOptions::default()).await.unwrap(),
        SealOutcome::Busy
    ));
    assert!(matches!(
        b.new_audit_chain_epoch(&EpochReason::new("rotate.test").unwrap())
            .await
            .unwrap(),
        EpochChange::Busy
    ));
    a.commit().await.unwrap();
    drop(a);
    assert!(matches!(
        b.new_audit_chain_epoch(&EpochReason::new("rotate.test").unwrap())
            .await
            .unwrap(),
        EpochChange::Created(_)
    ));
    let mut a = begin(&h).await;
    assert!(matches!(
        a.seal_audit_batch(&SealOptions::default()).await.unwrap(),
        SealOutcome::Busy
    ));
    b.rollback().await.unwrap();
    drop(b);
    assert!(matches!(
        a.seal_audit_batch(&SealOptions::default()).await.unwrap(),
        SealOutcome::NoWork
    ));
    a.rollback().await.unwrap();
}

#[tokio::test]
async fn rows_sharing_xid_resume_by_id() {
    let h = initialized().await;
    let c = owner(&h).await;
    c.batch_execute("BEGIN").await.unwrap();
    let a = insert(&c, 10, "[]").await;
    c.batch_execute("SELECT nextval('erp_event.ir_audit_entry_id_seq')")
        .await
        .unwrap();
    let b = insert(&c, 10, "[]").await;
    c.batch_execute("COMMIT").await.unwrap();
    assert_eq!(a.0, b.0);
    assert!(b.1 > a.1 + 1);
    for (lower, upper) in [((0, 0), a), (a, b)] {
        let mut tx = begin(&h).await;
        let seal = staged(
            tx.seal_audit_batch(&SealOptions::new(1, 100).unwrap())
                .await
                .unwrap(),
        );
        assert_eq!(seal.lower_cursor(), lower);
        assert_eq!(seal.upper_cursor(), upper);
        tx.commit().await.unwrap();
    }
    assert_eq!(seals(&c).await, 2);
}

#[tokio::test]
async fn duplicate_cursor_at_count_boundary_is_refused() {
    let h = initialized().await;
    let c = owner(&h).await;
    c.batch_execute("BEGIN").await.unwrap();
    let a = insert(&c, 10, "[]").await;
    let b = insert(&c, 10, "[]").await;
    c.execute("UPDATE erp_event.ir_audit_entry SET id=$1,create_date=create_date+interval '1 second' WHERE id=$2",&[&a.1,&b.1]).await.unwrap();
    c.batch_execute("COMMIT").await.unwrap();
    let mut tx = begin(&h).await;
    refused(
        &tx.seal_audit_batch(&SealOptions::new(1, 100).unwrap())
            .await
            .unwrap_err(),
        "a later release: persisted audit cursors are invalid or not strictly increasing",
    );
    tx.rollback().await.unwrap();
    assert_eq!(seals(&c).await, 0);
}
