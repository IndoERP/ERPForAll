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
async fn batches_are_checkpoints_and_rollback_does_not_advance() {
    let h = initialized().await;
    let c = owner(&h).await;
    let first = insert(&c, 100, "[]").await;
    let second = insert(&c, 100, "[]").await;
    let mut tx = begin(&h).await;
    let one = staged(
        tx.seal_audit_batch(&SealOptions::new(1, 100).unwrap())
            .await
            .unwrap(),
    );
    assert_eq!(one.row_count(), 1);
    assert_eq!(one.lower_cursor(), (0, 0));
    assert_eq!(one.upper_cursor(), first);
    assert_eq!(seals(&c).await, 0);
    tx.rollback().await.unwrap();
    drop(tx);
    assert_eq!(seals(&c).await, 0);
    let mut tx = begin(&h).await;
    let retry = staged(
        tx.seal_audit_batch(&SealOptions::new(1, 100).unwrap())
            .await
            .unwrap(),
    );
    assert_eq!(retry.seal_hash(), one.seal_hash());
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = begin(&h).await;
    let two = staged(tx.seal_audit_batch(&SealOptions::default()).await.unwrap());
    assert_eq!(two.identity().batch_no(), 2);
    assert_eq!(two.lower_cursor(), first);
    assert_eq!(two.upper_cursor(), second);
    assert_eq!(two.row_count(), 1);
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = begin(&h).await;
    assert!(matches!(
        tx.seal_audit_batch(&SealOptions::default()).await.unwrap(),
        SealOutcome::NoWork
    ));
    tx.commit().await.unwrap();
    assert_eq!(seals(&c).await, 2);
}

#[tokio::test]
async fn byte_budget_preserves_a_contiguous_prefix() {
    let h = initialized().await;
    let c = owner(&h).await;
    let a = insert(&c, 60, "[]").await;
    let b = insert(&c, 80, "[]").await;
    let d = insert(&c, 20, "[]").await;
    let mut tx = begin(&h).await;
    let first = staged(
        tx.seal_audit_batch(&SealOptions::new(10, 100).unwrap())
            .await
            .unwrap(),
    );
    assert_eq!(first.row_count(), 1);
    assert_eq!(first.upper_cursor(), a);
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = begin(&h).await;
    let next = staged(
        tx.seal_audit_batch(&SealOptions::new(10, 100).unwrap())
            .await
            .unwrap(),
    );
    assert_eq!(next.lower_cursor(), a);
    assert_eq!(next.upper_cursor(), d);
    assert_eq!(next.row_count(), 2);
    assert!(b < d);
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn invalid_metadata_and_oversize_refuse_without_a_seal() {
    let h = initialized().await;
    let c = owner(&h).await;
    insert(&c, 101, "[]").await;
    for (sql, message) in [
        (
            "UPDATE erp_event.ir_audit_entry SET encoded_bytes=101",
            "a later release: first row exceeds the batch byte limit",
        ),
        (
            "UPDATE erp_event.ir_audit_entry SET encoded_bytes=0",
            "a later release: persisted encoded_bytes must be positive",
        ),
        (
            "UPDATE erp_event.ir_audit_entry SET encoded_bytes=-1",
            "a later release: persisted encoded_bytes must be positive",
        ),
        (
            "UPDATE erp_event.ir_audit_entry SET encoded_bytes=1,schema_version=2",
            "a later release: unsupported persisted audit schema version",
        ),
    ] {
        c.batch_execute(sql).await.unwrap();
        let mut tx = begin(&h).await;
        let e = tx
            .seal_audit_batch(&SealOptions::new(10, 100).unwrap())
            .await
            .unwrap_err();
        refused(&e, message);
        tx.rollback().await.unwrap();
        drop(tx);
        assert_eq!(seals(&c).await, 0);
    }
}

#[tokio::test]
async fn real_seals_continue_across_epoch_changes() {
    let h = initialized().await;
    let c = owner(&h).await;
    let a = insert(&c, 10, "[]").await;
    let mut tx = begin(&h).await;
    let one = staged(tx.seal_audit_batch(&SealOptions::default()).await.unwrap());
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = begin(&h).await;
    assert!(matches!(
        tx.new_audit_chain_epoch(&EpochReason::new("rotate.test").unwrap())
            .await
            .unwrap(),
        EpochChange::Created(_)
    ));
    tx.commit().await.unwrap();
    drop(tx);
    let b = insert(&c, 10, "[]").await;
    let mut tx = begin(&h).await;
    let two = staged(tx.seal_audit_batch(&SealOptions::default()).await.unwrap());
    assert_eq!(two.identity().epoch(), 2);
    assert_eq!(two.identity().batch_no(), 1);
    assert_eq!(two.identity().incarnation(), one.identity().incarnation());
    assert_eq!(two.lower_cursor(), a);
    assert_eq!(two.upper_cursor(), b);
    tx.commit().await.unwrap();
    let prev: Vec<u8> = c
        .query_one(
            "SELECT prev_seal_hash FROM erp_event.ir_audit_seal WHERE chain_epoch=2",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(prev, one.seal_hash());
}

#[tokio::test]
async fn admission_is_explicit_and_fail_closed() {
    for (rows, bytes) in [(0, 1), (65537, 1), (1, 0), (1, 8388609)] {
        refused(
            &SealOptions::new(rows, bytes).unwrap_err(),
            "a later release: limits require 1..=65536 rows and 1..=8388608 stored bytes",
        );
    }
    let h = installed().await;
    let mut tx = begin(&h).await;
    let e = tx
        .seal_audit_batch(&SealOptions::default())
        .await
        .unwrap_err();
    refused(
        &e,
        "a later release: chain is not initialized; initialize it explicitly",
    );
    tx.rollback().await.unwrap();
    drop(tx);
    let mut tx = h.begin_as(2).await.unwrap();
    let before = tx.query_count();
    let e = tx
        .seal_audit_batch(&SealOptions::default())
        .await
        .unwrap_err();
    assert_eq!(e.kind(), erp_fields::ErrorKind::Access);
    assert_eq!(
        e.message(),
        "Transaction::seal_audit_batch: only a transaction of the System principal with full elevation may do this (this one acts as user 2)"
    );
    assert_eq!(tx.query_count(), before);
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn runtime_login_and_stale_isolation_cannot_seal() {
    let h = initialized().await;
    let cfg = h.read_only_role().await.unwrap();
    let mut tx = begin(&h).await;
    tx.grant_event_runtime_role(cfg.get_user().unwrap())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let orm = erp_orm::Orm::builder(
        std::sync::Arc::clone(h.registry()),
        erp_orm::pool::create_pool(cfg, 2).unwrap(),
    )
    .build()
    .unwrap();
    let mut tx = orm
        .begin_opts(
            erp_orm::BeginOptions::new(erp_orm::SUPERUSER_ID)
                .isolation(erp_orm::Isolation::ReadCommitted),
        )
        .await
        .unwrap();
    refused(
        &tx.seal_audit_batch(&SealOptions::default())
            .await
            .unwrap_err(),
        "a later release: erp_event has incompatible owner; owner repair is required",
    );
    tx.rollback().await.unwrap();
    drop(tx);
    let mut tx = begin(&h).await;
    tx.batch("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .await
        .unwrap();
    refused(
        &tx.seal_audit_batch(&SealOptions::default())
            .await
            .unwrap_err(),
        "a later release: epoch mutation requires Read Committed isolation",
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn connection_loss_before_commit_leaves_no_checkpoint() {
    let h = initialized().await;
    let c = owner(&h).await;
    insert(&c, 10, "[]").await;
    let mut tx = begin(&h).await;
    let report = staged(tx.seal_audit_batch(&SealOptions::default()).await.unwrap());
    let pid: i32 = tx.query("SELECT pg_backend_pid()", &[]).await.unwrap()[0].get(0);
    c.query_one("SELECT pg_terminate_backend($1)", &[&pid])
        .await
        .unwrap();
    let e = tx.commit().await.unwrap_err();
    assert_eq!(e.kind(), erp_fields::ErrorKind::Database);
    // The termination notification and client close race; both complete messages are valid.
    assert!(
        [
            "database connection closed: connection closed",
            "terminating connection due to administrator command"
        ]
        .contains(&e.message()),
        "{e:?}"
    );
    drop(tx);
    assert_eq!(seals(&c).await, 0);
    let mut tx = begin(&h).await;
    let retry = staged(tx.seal_audit_batch(&SealOptions::default()).await.unwrap());
    assert_eq!(retry.identity(), report.identity());
    assert_eq!(retry.seal_hash(), report.seal_hash());
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn seal_insert_failure_leaves_no_checkpoint() {
    let h = initialized().await;
    let c = owner(&h).await;
    insert(&c, 10, "[]").await;
    c.query_one(
        "SELECT setval('erp_event.ir_audit_seal_id_seq',9223372036854775807,true)",
        &[],
    )
    .await
    .unwrap();
    let mut tx = begin(&h).await;
    let e = tx
        .seal_audit_batch(&SealOptions::default())
        .await
        .unwrap_err();
    assert_eq!(e.kind(), erp_fields::ErrorKind::Database);
    assert_eq!(e.sqlstate(), Some("2200H"));
    assert_eq!(
        e.message(),
        "nextval: reached maximum value of sequence \"ir_audit_seal_id_seq\" (9223372036854775807)"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    assert_eq!(seals(&c).await, 0);
    c.query_one(
        "SELECT setval('erp_event.ir_audit_seal_id_seq',1,false)",
        &[],
    )
    .await
    .unwrap();
    let mut tx = begin(&h).await;
    assert_eq!(
        staged(tx.seal_audit_batch(&SealOptions::default()).await.unwrap())
            .identity()
            .batch_no(),
        1
    );
    tx.commit().await.unwrap();
}

// Rows get xids 99, 100 and 101: numeric order differs from text order ("100" < "101" < "99"), so
// a sealer that sorted the decimal text would reach Rust out of cursor order.
async fn digit_boundary_rows(
    c: &erp_testkit::tokio_postgres::Client,
    bytes: [i32; 3],
) -> [(u64, i64); 3] {
    let mut out = [(0, 0); 3];
    for (slot, (xid, size)) in out.iter_mut().zip([99u64, 100, 101].into_iter().zip(bytes)) {
        let (_, id) = insert(c, size, "[]").await;
        let updated = c
            .execute(
                "UPDATE erp_event.ir_audit_entry SET xid=$1::text::xid8 WHERE id=$2",
                &[&xid.to_string(), &id],
            )
            .await
            .unwrap();
        assert_eq!(updated, 1);
        *slot = (xid, id);
    }
    out
}

// Rows actually stored in the half-open cursor range (lower, upper].
async fn covered(
    c: &erp_testkit::tokio_postgres::Client,
    lower: (u64, i64),
    upper: (u64, i64),
) -> i64 {
    c.query_one(
        "SELECT count(*) FROM erp_event.ir_audit_entry WHERE (xid,id)>($1::text::xid8,$2) AND (xid,id)<=($3::text::xid8,$4)",
        &[&lower.0.to_string(), &lower.1, &upper.0.to_string(), &upper.1],
    )
    .await
    .unwrap()
    .get(0)
}

#[tokio::test]
async fn batch_order_is_numeric_across_a_digit_boundary() {
    let h = initialized().await;
    let c = owner(&h).await;
    let rows = digit_boundary_rows(&c, [10, 10, 10]).await;
    let mut tx = begin(&h).await;
    let seal = staged(
        tx.seal_audit_batch(&SealOptions::new(10, 100).unwrap())
            .await
            .unwrap(),
    );
    assert_eq!(seal.lower_cursor(), (0, 0));
    assert_eq!(seal.upper_cursor(), rows[2]);
    assert_eq!(seal.row_count(), 3);
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn byte_budget_stop_across_a_digit_boundary_skips_no_row() {
    let h = initialized().await;
    let c = owner(&h).await;
    let rows = digit_boundary_rows(&c, [60, 30, 30]).await;
    let mut tx = begin(&h).await;
    let first = staged(
        tx.seal_audit_batch(&SealOptions::new(10, 100).unwrap())
            .await
            .unwrap(),
    );
    assert_eq!(first.lower_cursor(), (0, 0));
    assert_eq!(first.upper_cursor(), rows[1]);
    assert_eq!(first.row_count(), 2);
    assert_eq!(
        i64::try_from(first.row_count()).unwrap(),
        covered(&c, first.lower_cursor(), first.upper_cursor()).await
    );
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = begin(&h).await;
    let next = staged(
        tx.seal_audit_batch(&SealOptions::new(10, 100).unwrap())
            .await
            .unwrap(),
    );
    assert_eq!(next.lower_cursor(), rows[1]);
    assert_eq!(next.upper_cursor(), rows[2]);
    assert_eq!(next.row_count(), 1);
    tx.commit().await.unwrap();
}
