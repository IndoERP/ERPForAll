// SPDX-License-Identifier: MIT OR Apache-2.0
//! Pending attachment admission must remain safe after a transaction established its snapshot.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
mod attachment_support;
use attachment_support::*;
use erp_addon_base::attachments::{self, AttachmentContent, NewAttachment};
use erp_orm::{GenerationAttachment, Orm, StaticPolicy};
use erp_sdk::ErrorKind;
use std::sync::Arc;

const PLACEHOLDER: &str = "test.pending";
fn item() -> NewAttachment {
    NewAttachment::new("file.txt", AttachmentContent::Bytes(b"hello".to_vec()))
}
fn other_node(f: &Fixture) -> Orm {
    Orm::builder(
        Arc::clone(f.h.registry()),
        erp_orm::pool::create_pool(f.h.db().config(), 4).unwrap(),
    )
    .policy(StaticPolicy::AllowAll)
    .generation_attachment(GenerationAttachment::new(
        Arc::clone(f.h.registry()),
        Arc::clone(&f.rt),
    ))
    .build()
    .unwrap()
}

#[tokio::test]
async fn pending_guard_rejects_a_stale_snapshot_on_another_node() {
    let f = fixture(limits()).await;
    let other = other_node(&f);
    let mut old = other.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    assert_eq!(
        count(&mut old, "SELECT count(*) FROM ir_attachment").await,
        0
    );
    let mut winner = f.h.begin().await.unwrap();
    attachments::create_pending(&mut winner, PLACEHOLDER, 1, vec![item()])
        .await
        .unwrap();
    winner.commit().await.unwrap();
    drop(winner);
    let error = attachments::create_pending(&mut old, PLACEHOLDER, 1, vec![item()])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Concurrency);
    assert_eq!(error.sqlstate(), Some("40001"));
    assert!(!error.message().is_empty());
    old.rollback().await.unwrap();
    drop(old);
    let mut retry = other.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let error = attachments::create_pending(&mut retry, PLACEHOLDER, 1, vec![item()])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "At most 1 pending attachments are allowed for test.pending."
    );
    retry.rollback().await.unwrap();
    drop(retry);
    assert_eq!(scalar(&f.h, "SELECT count(*) FROM ir_attachment").await, 1);
    drop(other);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn pending_batches_are_atomic_and_rollback_releases_the_guard() {
    let f = fixture(limits()).await;
    let mut tx = f.h.begin().await.unwrap();
    let ids = attachments::create_pending(&mut tx, PLACEHOLDER, 2, vec![item(), item()])
        .await
        .unwrap();
    assert_eq!(ids.len(), 2);
    let error = attachments::create_pending(&mut tx, PLACEHOLDER, 2, vec![item()])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "At most 2 pending attachments are allowed for test.pending."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    assert_eq!(scalar(&f.h, "SELECT count(*) FROM ir_attachment").await, 0);
    let mut tx = f.h.begin().await.unwrap();
    attachments::create_pending(&mut tx, PLACEHOLDER, 2, vec![item()])
        .await
        .unwrap();
    let error = tx
        .savepoint(async |tx| {
            attachments::create_pending(
                tx,
                PLACEHOLDER,
                2,
                vec![
                    item(),
                    NewAttachment::new("bad\nname", AttachmentContent::Empty),
                ],
            )
            .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert!(!error.message().is_empty());
    assert_eq!(
        count(&mut tx, "SELECT count(*) FROM ir_attachment").await,
        1
    );
    tx.commit().await.unwrap();
    drop(tx);
    assert!(f.h.orm().sync_schema().await.unwrap().is_empty());
    assert_eq!(scalar(&f.h, "SELECT count(*) FROM ir_attachment").await, 1);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn pending_first_use_race_commits_only_one_last_slot() {
    let f = fixture(limits()).await;
    let other = other_node(&f);
    let mut a = f.h.begin().await.unwrap();
    let mut b = other.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    count(&mut a, "SELECT count(*) FROM ir_attachment").await;
    count(&mut b, "SELECT count(*) FROM ir_attachment").await;
    let barrier = tokio::sync::Barrier::new(2);
    let run_a = async {
        barrier.wait().await;
        match attachments::create_pending(&mut a, PLACEHOLDER, 1, vec![item()]).await {
            Ok(_) => {
                a.commit().await.unwrap();
                true
            }
            Err(error) => {
                assert_eq!(error.kind(), ErrorKind::Concurrency);
                assert_eq!(error.sqlstate(), Some("40001"));
                a.rollback().await.unwrap();
                false
            }
        }
    };
    let run_b = async {
        barrier.wait().await;
        match attachments::create_pending(&mut b, PLACEHOLDER, 1, vec![item()]).await {
            Ok(_) => {
                b.commit().await.unwrap();
                true
            }
            Err(error) => {
                assert_eq!(error.kind(), ErrorKind::Concurrency);
                assert_eq!(error.sqlstate(), Some("40001"));
                b.rollback().await.unwrap();
                false
            }
        }
    };
    let (a, b) = tokio::join!(run_a, run_b);
    assert_ne!(a, b);
    assert_eq!(scalar(&f.h, "SELECT count(*) FROM ir_attachment").await, 1);
    drop(other);
    f.h.cleanup().await.unwrap();
}
