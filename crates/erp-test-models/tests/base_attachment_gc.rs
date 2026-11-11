//! garbage collection, staging cleanup, reconcile and holds on the job queue
//! (deterministic cases and 102).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod attachment_support;
mod support;

use attachment_support::*;
use erp_addon_base::attachments::gc::{
    clean_staging, hold, mark_unreferenced, reconcile_chunk, release_hold, sweep_mark, sweep_remove,
};
use erp_addon_base::attachments::{self, AttachmentContent, NewAttachment};
use erp_blob::{Op, When};
use erp_orm::{BeginOptions, Isolation};
use erp_sdk::ErrorKind;

const HELD: &str = "x";

/// Attach `bytes` (through the store) to nothing and commit; returns the attachment id.
async fn external(f: &Fixture, name: &str, bytes: &[u8]) -> i64 {
    let mut tx = f.h.begin().await.unwrap();
    let env = tx.env();
    tx.commit().await.unwrap();
    drop(tx);
    let up = stage(f, &env, name, bytes).await;
    let mut tx = f.h.begin().await.unwrap();
    let ids = attachments::create(
        &mut tx,
        vec![NewAttachment::new(name, AttachmentContent::Upload(up.id))],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    ids[0].get()
}

async fn unlink(f: &Fixture, id: i64) {
    let mut tx = f.h.begin().await.unwrap();
    tx.env()
        .records("ir.attachment")
        .unwrap()
        .browse([id])
        .unwrap()
        .unlink(&mut tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
}

async fn blobs(f: &Fixture) -> i64 {
    scalar(&f.h, "SELECT count(*) FROM ir_blob").await
}

async fn phase_a(f: &Fixture) -> (u64, u64) {
    let mut tx = f.h.begin().await.unwrap();
    let r = mark_unreferenced(&mut tx, 100).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    (r.claimed, r.marked)
}

/// A transaction as the sweep's job kinds open one: READ COMMITTED (the sweep refuses others).
async fn rc(f: &Fixture) -> erp_sdk::Transaction {
    f.h.orm()
        .begin_opts(BeginOptions::new(f.h.principal()).isolation(Isolation::ReadCommitted))
        .await
        .unwrap()
}

async fn mark(f: &Fixture) -> Vec<i64> {
    let mut tx = rc(f).await;
    let ids = sweep_mark(&mut tx, 0, 100).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    ids
}

async fn remove(
    f: &Fixture,
    ids: &[i64],
) -> erp_sdk::Result<erp_addon_base::attachments::gc::SweepReport> {
    let mut tx = rc(f).await;
    let env = tx.env();
    let report = sweep_remove(&mut tx, &env, ids, 0).await;
    match report {
        Ok(r) => {
            tx.commit().await.unwrap();
            drop(tx);
            Ok(r)
        }
        Err(e) => {
            let _ = tx.rollback().await;
            Err(e)
        }
    }
}

#[tokio::test]
async fn an_unreferenced_blob_is_marked_then_swept_with_its_key() {
    let f = fixture(external_limits()).await;
    let id = external(&f, "a.txt", b"collect me please").await;
    assert_eq!(f.store.content_keys().len(), 1);
    unlink(&f, id).await;
    assert_eq!(phase_a(&f).await, (1, 1));
    let ids = mark(&f).await;
    assert_eq!(ids.len(), 1);
    let report = remove(&f, &ids).await.unwrap();
    assert_eq!((report.rows, report.keys, report.already_absent), (1, 1, 0));
    assert_eq!(blobs(&f).await, 0);
    assert!(f.store.content_keys().is_empty());
}

#[tokio::test]
async fn the_grace_and_a_hold_keep_an_unreferenced_blob() {
    let f = fixture(external_limits().with_gc_grace_secs_for_tests(3600)).await;
    let id = external(&f, "g.txt", b"kept for a while").await;
    unlink(&f, id).await;
    phase_a(&f).await;
    let mut tx = rc(&f).await;
    assert!(
        sweep_mark(&mut tx, 3600, 100).await.unwrap().is_empty(),
        "inside the grace"
    );
    tx.commit().await.unwrap();
    drop(tx);
    // with a zero grace a hold still keeps it
    let mut tx = f.h.begin().await.unwrap();
    let hold_id = hold(&mut tx, 600, "backup running").await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    assert!(mark(&f).await.is_empty(), "held");
    let mut tx = f.h.begin().await.unwrap();
    release_hold(&mut tx, hold_id).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(mark(&f).await.len(), 1, "released");
    assert_eq!(f.store.content_keys().len(), 1);
    let _ = HELD;
}

/// The sweep refuses a REPEATABLE READ transaction: its snapshot is taken before the hold lock
/// wait and could miss the hold it waited for (the two-node serialization test is in
/// base_attachment_nodes).
#[tokio::test]
async fn the_sweep_refuses_a_repeatable_read_transaction() {
    let f = fixture(external_limits()).await;
    let mut tx = f.h.begin().await.unwrap();
    let err = sweep_mark(&mut tx, 0, 100).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "The garbage collection sweep runs in a READ COMMITTED transaction (its job kinds do): a \
         REPEATABLE READ snapshot predates the hold lock and could miss a hold."
    );
    let _ = tx.rollback().await;
    let mut tx = f.h.begin().await.unwrap();
    let env = tx.env();
    let err = sweep_remove(&mut tx, &env, &[1], 0).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    let _ = tx.rollback().await;
}

#[tokio::test]
async fn a_writer_revives_a_deleting_blob_and_the_sweep_then_leaves_it() {
    let f = fixture(external_limits()).await;
    let id = external(&f, "r.txt", b"revive this content").await;
    unlink(&f, id).await;
    phase_a(&f).await;
    // the same content arrives again before the sweep
    let again = external(&f, "r2.txt", b"revive this content").await;
    let mut tx = f.h.begin().await.unwrap();
    assert_eq!(
        count(
            &mut tx,
            "SELECT count(*) FROM ir_blob WHERE state = 0 AND deleting_since IS NULL"
        )
        .await,
        1
    );
    tx.commit().await.unwrap();
    drop(tx);
    assert!(mark(&f).await.is_empty(), "revived rows are not swept");
    let mut tx = f.h.begin().await.unwrap();
    let handle = attachments::open(&mut tx, again).await.unwrap();
    assert_eq!(
        handle.read_to_vec(100).await.unwrap(),
        b"revive this content"
    );
}

#[tokio::test]
async fn a_lost_reply_after_delete_many_is_finished_by_the_next_sweep() {
    let f = fixture(external_limits()).await;
    let id = external(&f, "c.txt", b"crash after delete").await;
    unlink(&f, id).await;
    phase_a(&f).await;
    let ids = mark(&f).await;
    f.store.faults().inject(Op::DeleteMany, When::After, 0, 1);
    let err = remove(&f, &ids).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Internal);
    assert!(f.store.content_keys().is_empty(), "the bytes are gone");
    assert_eq!(
        blobs(&f).await,
        1,
        "the row stays: the transaction rolled back"
    );
    let report = remove(&f, &ids).await.unwrap();
    assert_eq!((report.rows, report.keys, report.already_absent), (1, 0, 1));
    assert_eq!(blobs(&f).await, 0);
}

/// brings the bytes again)
#[tokio::test]
async fn a_swept_row_is_revived_only_with_proven_or_resupplied_bytes() {
    let f = fixture(external_limits()).await;
    let id = external(&f, "s.txt", b"swept then written").await;
    unlink(&f, id).await;
    phase_a(&f).await;
    let ids = mark(&f).await;
    assert_eq!(ids.len(), 1);
    f.store.faults().inject(Op::DeleteMany, When::After, 0, 1);
    let _ = remove(&f, &ids).await.unwrap_err();
    assert!(f.store.content_keys().is_empty());
    // the same bytes uploaded again: the staged object is promoted, so the row is revived
    let again = external(&f, "s2.txt", b"swept then written").await;
    assert_eq!(f.store.content_keys().len(), 1);
    let mut tx = f.h.begin().await.unwrap();
    let handle = attachments::open(&mut tx, again).await.unwrap();
    assert_eq!(
        handle.read_to_vec(100).await.unwrap(),
        b"swept then written"
    );
}

/// checklist entry when it writes the file (`_mark_for_gc`,,230-243`) and
/// its autovacuum removes the unreferenced file; here the upload row is the marker: after it
/// expires the cleaner adopts its digest and the sweep removes the key after the grace. Nothing
/// lists the store.
#[tokio::test]
async fn a_key_promoted_by_a_rolled_back_link_is_adopted_and_swept() {
    let f = fixture(external_limits()).await;
    let mut tx = f.h.begin().await.unwrap();
    let env = tx.env();
    tx.commit().await.unwrap();
    drop(tx);
    let up = stage(&f, &env, "o.txt", b"orphaned by rollback").await;
    let mut tx = f.h.begin().await.unwrap();
    attachments::create(
        &mut tx,
        vec![NewAttachment::new(
            "o.txt",
            AttachmentContent::Upload(up.id.clone()),
        )],
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    drop(tx);
    assert_eq!(
        f.store.content_keys().len(),
        1,
        "promoted, never referenced"
    );
    assert_eq!(blobs(&f).await, 0);
    // the upload expires, the cleaner adopts its digest, the sweep removes the key
    let mut tx = f.h.begin().await.unwrap();
    tx.execute(
        "UPDATE ir_blob_upload SET expires_at = now() - interval '1 minute'",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = f.h.begin().await.unwrap();
    let env = tx.env();
    let report = clean_staging(&mut tx, &env, 100).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!((report.expired, report.adopted), (1, 1));
    let ids = mark(&f).await;
    assert_eq!(ids.len(), 1);
    remove(&f, &ids).await.unwrap();
    assert!(f.store.content_keys().is_empty());
    assert!(f.store.staging_keys().is_empty());
}

/// Consumed uploads leave with their staging objects.
#[tokio::test]
async fn the_cleaner_removes_consumed_uploads_and_their_staged_objects() {
    let f = fixture(external_limits()).await;
    let first = external(&f, "a.txt", b"duplicate bytes here").await;
    let second = external(&f, "b.txt", b"duplicate bytes here").await;
    assert_ne!(first, second);
    // the second upload's object was not promoted (the content existed): it is still staged
    assert_eq!(f.store.staging_keys().len(), 1);
    let mut tx = f.h.begin().await.unwrap();
    let env = tx.env();
    let report = clean_staging(&mut tx, &env, 100).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(report.consumed, 2);
    assert!(f.store.staging_keys().is_empty());
    let mut tx = f.h.begin().await.unwrap();
    assert_eq!(
        count(&mut tx, "SELECT count(*) FROM ir_blob_upload").await,
        0
    );
}

/// Reconcile finds an unreferenced blob a path without triggers left behind.
#[tokio::test]
async fn reconcile_queues_unreferenced_blobs_that_the_triggers_missed() {
    let f = fixture(external_limits()).await;
    let id = external(&f, "t.txt", b"deleted without triggers").await;
    let mut tx = f.h.begin().await.unwrap();
    tx.batch("ALTER TABLE ir_attachment DISABLE TRIGGER ir_attachment_gc_delete")
        .await
        .unwrap();
    tx.execute("DELETE FROM ir_attachment WHERE id = $1", &[&id])
        .await
        .unwrap();
    tx.batch("ALTER TABLE ir_attachment ENABLE TRIGGER ir_attachment_gc_delete")
        .await
        .unwrap();
    assert_eq!(
        count(&mut tx, "SELECT count(*) FROM ir_blob_gc_candidate").await,
        0
    );
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = f.h.begin().await.unwrap();
    let env = tx.env();
    let report = reconcile_chunk(&mut tx, &env, 0, 100).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!((report.looked_at, report.queued), (1, 1));
    assert_eq!(phase_a(&f).await, (1, 1));
    let ids = mark(&f).await;
    remove(&f, &ids).await.unwrap();
    assert!(f.store.content_keys().is_empty());
}

/// The kinds run through the job runner: collection, cleanup and reconcile as leased jobs.
#[tokio::test]
async fn the_collection_job_removes_the_blob_through_the_runner() {
    let f = fixture(external_limits()).await;
    let id = external(&f, "j.txt", b"collected by the job").await;
    unlink(&f, id).await;
    erp_addon_base::attachments::gc::ensure_schedules(f.h.orm())
        .await
        .unwrap();
    let mut tx = f.h.begin().await.unwrap();
    tx.batch("UPDATE erp_job SET run_after = now() WHERE kind LIKE 'base.attachment_%'")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let runner = support::jobs::runner(&f.h);
    // the collection runs Phase A, the sweep needs a second pass to be past the (zero) grace
    for _ in 0..6 {
        runner.tick().await.unwrap();
        let mut tx = f.h.begin().await.unwrap();
        tx.batch("UPDATE erp_job SET run_after = now() WHERE kind LIKE 'base.attachment_%' AND state = 'ready'")
            .await
            .unwrap();
        tx.commit().await.unwrap();
        drop(tx);
        if blobs(&f).await == 0 {
            break;
        }
    }
    assert_eq!(blobs(&f).await, 0);
    assert!(f.store.content_keys().is_empty());
    let mut tx = f.h.begin().await.unwrap();
    assert_eq!(
        count(
            &mut tx,
            "SELECT count(*) FROM erp_job WHERE kind LIKE 'base.attachment_%' AND state = 'dead'"
        )
        .await,
        0
    );
}

/// A busy node is not an integrity alarm: reconcile passes the stream admission's `Lock` through
/// instead of reporting the referenced `deleting` blob as unprovable; once a permit is free it
/// proves and revives the row.
#[tokio::test]
async fn reconcile_reports_a_busy_node_as_busy_not_as_missing_content() {
    let f = fixture(external_limits().with_stream_permits_for_tests(1)).await;
    let id = external(&f, "busy.txt", b"referenced but marked deleting").await;
    let mut tx = f.h.begin().await.unwrap();
    tx.execute(
        "UPDATE ir_blob SET state = 1, deleting_since = now(), sweep_attempted_at = now() \
         WHERE id = (SELECT blob_id FROM ir_attachment WHERE id = $1)",
        &[&id],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let held = f.rt.try_stream_permit().unwrap();
    let mut tx = f.h.begin().await.unwrap();
    let env = tx.env();
    let err = reconcile_chunk(&mut tx, &env, 0, 100).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Lock, "{err:?}");
    let _ = tx.rollback().await;
    drop(tx);
    drop(held);
    let mut tx = f.h.begin().await.unwrap();
    let env = tx.env();
    let report = reconcile_chunk(&mut tx, &env, 0, 100).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(report.revived, 1);
}
