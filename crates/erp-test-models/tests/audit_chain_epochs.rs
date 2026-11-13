// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
#[path = "support/audit_chain.rs"]
mod support;
use erp_orm::audit::seal::{EpochChange, EpochReason};
use support::*;

#[tokio::test]
async fn epochs_link_incarnation_hash_and_cursor_without_reset() {
    let h = installed().await;
    let reason = EpochReason::new("operator.initial").unwrap();
    let mut tx = begin(&h).await;
    let EpochChange::Created(first) = tx.initialize_audit_chain(&reason).await.unwrap() else {
        panic!("uncontended")
    };
    assert_eq!(first.epoch(), 1);
    assert_eq!(first.lower_cursor(), (0, 0));
    assert_eq!(first.predecessor_hash(), &[0u8; 32]);
    assert_ne!(first.incarnation(), &[0u8; 16]);
    let e = tx.initialize_audit_chain(&reason).await.unwrap_err();
    refused(
        &e,
        "a later release: chain is already initialized; use a linked epoch transition",
    );
    let e = tx.new_audit_chain_epoch(&reason).await.unwrap_err();
    refused(
        &e,
        "a later release: current epoch has no seal; seal it before starting another epoch",
    );
    let hash = seed_seal(&mut tx).await;
    let EpochChange::Created(second) = tx.new_audit_chain_epoch(&reason).await.unwrap() else {
        panic!("uncontended")
    };
    assert_eq!(second.epoch(), 2);
    assert_eq!(second.incarnation(), first.incarnation());
    assert_eq!(second.lower_cursor(), (42, 7));
    assert_eq!(second.predecessor_hash().as_slice(), hash);
    tx.commit().await.unwrap();
    let c = owner(&h).await;
    let r = c
        .query_one(
            "SELECT previous_epoch,link_hash FROM erp_event.ir_audit_chain WHERE epoch=2",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(r.get::<_, i64>(0), first.epoch());
    assert_eq!(r.get::<_, Vec<u8>>(1), hash);
    let mut tx = begin(&h).await;
    let e = tx.new_audit_chain_epoch(&reason).await.unwrap_err();
    refused(
        &e,
        "a later release: current epoch has no seal; seal it before starting another epoch",
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn competing_epoch_operations_are_busy_and_rollback_releases_lock() {
    let h = installed().await;
    let reason = EpochReason::new("operator.initial").unwrap();
    let mut a = begin(&h).await;
    let mut b = begin(&h).await;
    assert!(matches!(
        a.initialize_audit_chain(&reason).await.unwrap(),
        EpochChange::Created(_)
    ));
    assert!(matches!(
        b.initialize_audit_chain(&reason).await.unwrap(),
        EpochChange::Busy
    ));
    assert!(matches!(
        b.new_audit_chain_epoch(&reason).await.unwrap(),
        EpochChange::Busy
    ));
    a.rollback().await.unwrap();
    assert!(matches!(
        b.initialize_audit_chain(&reason).await.unwrap(),
        EpochChange::Created(_)
    ));
    b.rollback().await.unwrap();
    let mut c = begin(&h).await;
    assert_eq!(count(&mut c).await, 0);
    c.rollback().await.unwrap();
}

#[tokio::test]
async fn epochs_reject_invalid_reasons_and_stale_snapshot_isolation() {
    for value in ["", "bad reason", "bad\nreason", "é", &"x".repeat(129)] {
        let e = EpochReason::new(value).unwrap_err();
        refused(
            &e,
            "a later release: reason must be an ASCII identifier of 1 to 128 bytes (letters, digits, dot, underscore or hyphen)",
        );
    }
    assert!(EpochReason::new(&"x".repeat(128)).is_ok());
    let h = installed().await;
    let mut tx = begin(&h).await;
    tx.batch("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .await
        .unwrap();
    let e = tx
        .initialize_audit_chain(&EpochReason::new("initial").unwrap())
        .await
        .unwrap_err();
    refused(
        &e,
        "a later release: epoch mutation requires Read Committed isolation",
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn epoch_transition_refuses_a_gap_or_wrong_genesis_boundary() {
    let h = installed().await;
    let reason = EpochReason::new("operator.check").unwrap();
    let mut tx = begin(&h).await;
    tx.initialize_audit_chain(&reason).await.unwrap();
    seed_seal(&mut tx).await;
    tx.commit().await.unwrap();
    for sql in [
        "UPDATE erp_event.ir_audit_seal SET batch_no=2",
        "UPDATE erp_event.ir_audit_seal SET lower_xid='1',lower_id=1",
        "UPDATE erp_event.ir_audit_seal SET prev_seal_hash=decode(repeat('01',32),'hex')",
        "UPDATE erp_event.ir_audit_seal SET seal_hash=decode(repeat('00',32),'hex')",
    ] {
        let mut tx = begin(&h).await;
        tx.batch(sql).await.unwrap();
        let e = tx.new_audit_chain_epoch(&reason).await.unwrap_err();
        refused(
            &e,
            "a later release: epoch or predecessor seal is inconsistent; owner investigation is required",
        );
        tx.rollback().await.unwrap();
    }
}

#[tokio::test]
async fn epoch_operations_require_system_before_sql() {
    let h = installed().await;
    let reason = EpochReason::new("operator.initial").unwrap();
    let mut tx = h.begin_as(2).await.unwrap();
    let before = tx.query_count();
    for (name, error) in [
        (
            "initialize_audit_chain",
            tx.initialize_audit_chain(&reason).await.unwrap_err(),
        ),
        (
            "new_audit_chain_epoch",
            tx.new_audit_chain_epoch(&reason).await.unwrap_err(),
        ),
    ] {
        assert_eq!(error.kind(), erp_fields::ErrorKind::Access);
        assert_eq!(
            error.message(),
            format!(
                "Transaction::{name}: only a transaction of the System principal with full elevation may do this (this one acts as user 2)"
            )
        );
    }
    assert_eq!(tx.query_count(), before);
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn two_epoch_transitions_cannot_fork_the_chain() {
    let h = installed().await;
    let reason = EpochReason::new("operator.rotate").unwrap();
    let mut setup = begin(&h).await;
    setup.initialize_audit_chain(&reason).await.unwrap();
    seed_seal(&mut setup).await;
    setup.commit().await.unwrap();
    let mut a = begin(&h).await;
    let mut b = begin(&h).await;
    assert!(matches!(
        a.new_audit_chain_epoch(&reason).await.unwrap(),
        EpochChange::Created(_)
    ));
    assert!(matches!(
        b.new_audit_chain_epoch(&reason).await.unwrap(),
        EpochChange::Busy
    ));
    a.commit().await.unwrap();
    let e = b.new_audit_chain_epoch(&reason).await.unwrap_err();
    refused(
        &e,
        "a later release: current epoch has no seal; seal it before starting another epoch",
    );
    assert_eq!(count(&mut b).await, 2);
    b.rollback().await.unwrap();
}

#[tokio::test]
async fn epoch_number_exhaustion_refuses_without_resetting() {
    let h = installed().await;
    let reason = EpochReason::new("operator.boundary").unwrap();
    let mut tx = begin(&h).await;
    tx.initialize_audit_chain(&reason).await.unwrap();
    seed_seal(&mut tx).await;
    // Synthetic owner-only metadata reaches the terminal bigint without constructing billions
    // of epochs. FK enforcement is restored before invoking the operation under test. This is
    // a checked-integer boundary fixture, not evidence that this synthetic history verifies.
    tx.batch(&format!("SET LOCAL session_replication_role=replica;
        INSERT INTO erp_event.ir_audit_chain(epoch,incarnation,format_version,previous_epoch,link_hash,reason)
          SELECT {},incarnation,1,{},s.seal_hash,'boundary.prior' FROM erp_event.ir_audit_chain c JOIN erp_event.ir_audit_seal s ON s.chain_epoch=c.epoch WHERE c.epoch=1;
        INSERT INTO erp_event.ir_audit_seal(chain_epoch,batch_no,lower_xid,lower_id,upper_xid,upper_id,row_count,batch_digest,prev_seal_hash,seal_hash,format_version)
          SELECT {},1,lower_xid,lower_id,upper_xid,upper_id,row_count,batch_digest,prev_seal_hash,seal_hash,1 FROM erp_event.ir_audit_seal WHERE chain_epoch=1;
        INSERT INTO erp_event.ir_audit_chain(epoch,incarnation,format_version,previous_epoch,link_hash,reason)
          SELECT {},incarnation,1,{},s.seal_hash,'boundary.current' FROM erp_event.ir_audit_chain c JOIN erp_event.ir_audit_seal s ON s.chain_epoch=c.epoch WHERE c.epoch=1;
        INSERT INTO erp_event.ir_audit_seal(chain_epoch,batch_no,lower_xid,lower_id,upper_xid,upper_id,row_count,batch_digest,prev_seal_hash,seal_hash,format_version)
          SELECT {},1,upper_xid,upper_id,'43',8,row_count,batch_digest,seal_hash,decode(repeat('02',32),'hex'),1 FROM erp_event.ir_audit_seal WHERE chain_epoch=1;
        SET LOCAL session_replication_role=origin", i64::MAX-1,i64::MAX-2,i64::MAX-1,i64::MAX,i64::MAX-1,i64::MAX)).await.unwrap();
    let before = count(&mut tx).await;
    let e = tx.new_audit_chain_epoch(&reason).await.unwrap_err();
    refused(
        &e,
        "a later release: epoch number exhausted SQL bigint; owner investigation is required",
    );
    assert_eq!(count(&mut tx).await, before);
    tx.rollback().await.unwrap();
}
