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
    clean_staging, ensure_missing_schedules, hold, mark_unreferenced, reconcile_chunk,
    release_hold, sweep_mark, sweep_remove,
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
    let mut tx = rc(f).await;
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
    let ids = sweep_mark(&mut tx, 0, 100).await.unwrap().ids;
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
    admit_cleanup(f.h.orm()).await;
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
    admit_cleanup(f.h.orm()).await;
    let id = external(&f, "g.txt", b"kept for a while").await;
    unlink(&f, id).await;
    phase_a(&f).await;
    let mut tx = rc(&f).await;
    assert!(
        sweep_mark(&mut tx, 3600, 100).await.unwrap().ids.is_empty(),
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
    admit_cleanup(f.h.orm()).await;
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
    admit_cleanup(f.h.orm()).await;
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
    admit_cleanup(f.h.orm()).await;
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
    admit_cleanup(f.h.orm()).await;
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
    admit_cleanup(f.h.orm()).await;
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
    let mut tx = rc(&f).await;
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
    admit_cleanup(f.h.orm()).await;
    let first = external(&f, "a.txt", b"duplicate bytes here").await;
    let second = external(&f, "b.txt", b"duplicate bytes here").await;
    assert_ne!(first, second);
    // Both promotion and dedup retain their staging source until admitted cleanup.
    assert_eq!(f.store.staging_keys().len(), 2);
    let mut tx = rc(&f).await;
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
    admit_cleanup(f.h.orm()).await;
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
    let mut tx = rc(&f).await;
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
    admit_cleanup(f.h.orm()).await;
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
    admit_cleanup(f.h.orm()).await;
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
    let mut tx = rc(&f).await;
    let env = tx.env();
    let err = reconcile_chunk(&mut tx, &env, 0, 100).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Lock, "{err:?}");
    let _ = tx.rollback().await;
    drop(tx);
    drop(held);
    let mut tx = rc(&f).await;
    let env = tx.env();
    let report = reconcile_chunk(&mut tx, &env, 0, 100).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(report.revived, 1);
}

/// The attachment kinds every database keeps one recurring row of.
const SCHEDULED_KINDS: [&str; 3] = [
    "base.attachment_gc",
    "base.attachment_clean_staging",
    "base.attachment_reconcile",
];

/// `(state, run_after as text)` of every row of `kind`, oldest first.
async fn schedule_rows(f: &Fixture, kind: &str) -> Vec<(String, String)> {
    let mut tx = f.h.begin().await.unwrap();
    let rows = tx
        .query(
            "SELECT state, run_after::text FROM erp_job WHERE kind = $1 ORDER BY id",
            &[&kind],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();
    tx.rollback().await.unwrap();
    rows
}

async fn ensure_missing(f: &Fixture) -> erp_sdk::Result<()> {
    let mut tx = f.h.begin().await.unwrap();
    let env = tx.env();
    let outcome = ensure_missing_schedules(&mut tx, &env).await;
    match outcome {
        Ok(()) => tx.commit().await,
        Err(error) => {
            tx.rollback().await.unwrap();
            Err(error)
        }
    }
}

/// the due time of an existing row)
#[tokio::test]
async fn ensuring_the_schedules_again_keeps_each_row_and_its_due_time() {
    let f = fixture(limits()).await;
    erp_addon_base::attachments::gc::ensure_schedules(f.h.orm())
        .await
        .unwrap();
    let first: Vec<_> = {
        let mut all = Vec::new();
        for kind in SCHEDULED_KINDS {
            let rows = schedule_rows(&f, kind).await;
            assert_eq!(rows.len(), 1, "{kind}: {rows:?}");
            assert_eq!(rows[0].0, "ready", "{kind}");
            all.push(rows);
        }
        all
    };
    erp_addon_base::attachments::gc::ensure_schedules(f.h.orm())
        .await
        .unwrap();
    ensure_missing(&f).await.unwrap();
    for (kind, before) in SCHEDULED_KINDS.into_iter().zip(first) {
        assert_eq!(schedule_rows(&f, kind).await, before, "{kind}");
    }
}

/// letter gets a fresh live row next to it)
#[tokio::test]
async fn ensuring_missing_schedules_keeps_a_running_row_and_replaces_a_dead_one() {
    let f = fixture(limits()).await;
    ensure_missing(&f).await.unwrap();
    let mut tx = f.h.begin().await.unwrap();
    tx.batch(
        "UPDATE erp_job SET state = 'running', lease_owner = uuidv7(), \
         lease_until = clock_timestamp() + interval '5 minutes', started_at = clock_timestamp() \
         WHERE kind = 'base.attachment_gc'; \
         UPDATE erp_job SET state = 'dead', dead_at = clock_timestamp() \
         WHERE kind = 'base.attachment_clean_staging'; \
         UPDATE erp_job SET run_after = '2001-02-03 04:05:06+00' \
         WHERE kind = 'base.attachment_reconcile'",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let running = schedule_rows(&f, "base.attachment_gc").await;
    let reconcile = schedule_rows(&f, "base.attachment_reconcile").await;
    ensure_missing(&f).await.unwrap();
    assert_eq!(schedule_rows(&f, "base.attachment_gc").await, running);
    assert_eq!(running[0].0, "running");
    assert_eq!(
        schedule_rows(&f, "base.attachment_reconcile").await,
        reconcile
    );
    let clean = schedule_rows(&f, "base.attachment_clean_staging").await;
    let states: Vec<&str> = clean.iter().map(|(state, _)| state.as_str()).collect();
    assert_eq!(states, ["dead", "ready"], "{clean:?}");
}

/// Only the system principal keeps the attachment schedules.
#[tokio::test]
async fn ensuring_missing_schedules_refuses_a_user_principal() {
    let f = fixture(limits()).await;
    let mut tx = f.h.begin_as(2).await.unwrap();
    let env = tx.env();
    let err = ensure_missing_schedules(&mut tx, &env).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err:?}");
    assert_eq!(
        err.message(),
        "The attachment schedules are kept by the system principal only."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    assert_eq!(
        scalar(
            &f.h,
            "SELECT count(*) FROM erp_job WHERE kind LIKE 'base.attachment_%'"
        )
        .await,
        0
    );
}

/// `(state, attempts, started)` of the one row of `kind`.
async fn run_state(f: &Fixture, kind: &str) -> (String, i32, bool) {
    let mut tx = f.h.begin().await.unwrap();
    let rows = tx
        .query(
            "SELECT state, attempts, started_at IS NOT NULL FROM erp_job WHERE kind = $1",
            &[&kind],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "{kind}");
    let state = (rows[0].get(0), rows[0].get(1), rows[0].get(2));
    tx.rollback().await.unwrap();
    state
}

/// a runner built with `exclude_kinds` never claims a due job of an excluded kind
/// (it stays `ready`, never started, attempts 0), while it claims and runs a due job of another
/// kind in the same tick.
#[tokio::test]
async fn a_runner_never_claims_an_excluded_kind() {
    let f = fixture(limits()).await;
    ensure_missing(&f).await.unwrap();
    let mut tx = f.h.begin().await.unwrap();
    tx.batch("UPDATE erp_job SET run_after = now() WHERE kind LIKE 'base.attachment\\_%'")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let runner = erp_orm::jobs::JobRunner::builder(f.h.orm().clone(), support::jobs::PassResolver)
        .exclude_kinds(["base.attachment_gc", "base.attachment_clean_staging"])
        .build()
        .unwrap();
    for _ in 0..3 {
        runner.tick().await.unwrap();
    }
    // the reconcile walk was claimed and ran (an empty blob table: it reschedules itself)
    let (state, _, started) = run_state(&f, "base.attachment_reconcile").await;
    assert_eq!((state.as_str(), started), ("ready", true));
    for kind in ["base.attachment_gc", "base.attachment_clean_staging"] {
        assert_eq!(
            run_state(&f, kind).await,
            ("ready".to_owned(), 0, false),
            "{kind}"
        );
    }
}

/// A kind that is not elevated, so a runner built with `exclude_elevated` still claims it.
struct NotElevated;
impl erp_orm::jobs::JobHandler for NotElevated {
    fn run<'a>(
        &'a self,
        _ctx: &'a mut erp_orm::jobs::JobContext<'_>,
    ) -> erp_orm::BoxFuture<'a, Result<erp_orm::jobs::RunOutcome, erp_orm::jobs::JobError>> {
        Box::pin(async { Ok(erp_orm::jobs::RunOutcome::Done) })
    }
}
static NOT_ELEVATED: NotElevated = NotElevated;
erp_orm::register_job_kind!(
    NOT_ELEVATED_KIND,
    erp_orm::jobs::JobKindDecl::new(
        "base",
        "base.test_not_elevated",
        erp_orm::jobs::HandlerRef::Native(&NOT_ELEVATED)
    )
    .principal(erp_orm::jobs::PrincipalPolicy::System)
);

/// a runner built with `exclude_elevated` never claims a due job of a kind whose
/// declaration is elevated (the three scheduled attachment kinds and the owner heartbeat), while
/// it claims and runs a due job of a kind that is not elevated in the same ticks.
#[tokio::test]
async fn a_runner_built_with_exclude_elevated_never_claims_an_elevated_kind() {
    let f = fixture(limits()).await;
    ensure_missing(&f).await.unwrap();
    let mut tx = f.h.begin().await.unwrap();
    tx.batch("UPDATE erp_job SET run_after = now() WHERE kind LIKE 'base.attachment\\_%'")
        .await
        .unwrap();
    let env = tx.env();
    tx.enqueue(
        &env,
        erp_orm::jobs::EnqueueRequest::new("base.test_not_elevated"),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let runner = erp_orm::jobs::JobRunner::builder(f.h.orm().clone(), support::jobs::PassResolver)
        .exclude_elevated()
        .build()
        .unwrap();
    for _ in 0..3 {
        runner.tick().await.unwrap();
    }
    // a finished job leaves the queue: the kind that is not elevated was claimed and ran
    let mut tx = f.h.begin().await.unwrap();
    let left = tx
        .query(
            "SELECT state FROM erp_job WHERE kind = 'base.test_not_elevated'",
            &[],
        )
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert!(left.is_empty(), "the runner claimed nothing at all");
    for kind in SCHEDULED_KINDS
        .into_iter()
        .chain(["base.attachment_owner_heartbeat"])
    {
        assert_eq!(
            run_state(&f, kind).await,
            ("ready".to_owned(), 0, false),
            "{kind}"
        );
    }
}

/// S3: a base upgrade's REPEATABLE READ snapshot predates the
/// rows a node start commits. When the upgrade then keeps the schedules it gets a serialization
/// failure (`Concurrency`, SQLSTATE `40001`), which the module system reports as
/// `ConcurrentChange` ("retry the module operation").
#[tokio::test]
async fn an_upgrade_snapshot_older_than_a_start_gets_a_serialization_failure() {
    let f = fixture(limits()).await;
    let mut upgrade =
        f.h.orm()
            .begin_opts(BeginOptions::new(f.h.principal()).isolation(Isolation::RepeatableRead))
            .await
            .unwrap();
    // the first statement takes the upgrade's snapshot, before any schedule row exists
    upgrade.query("SELECT 1", &[]).await.unwrap();
    let mut start = rc(&f).await;
    let start_env = start.env();
    ensure_missing_schedules(&mut start, &start_env)
        .await
        .unwrap();
    start.commit().await.unwrap();
    drop(start);
    let upgrade_env = upgrade.env();
    let err = ensure_missing_schedules(&mut upgrade, &upgrade_env)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Concurrency, "{err:?}");
    assert_eq!(err.sqlstate(), Some("40001"), "{err:?}");
    upgrade.rollback().await.unwrap();
}

/// An excluded kind that this binary does not register refuses the runner.
#[tokio::test]
async fn excluding_an_unregistered_kind_refuses_the_runner() {
    let f = fixture(limits()).await;
    let err = erp_orm::jobs::JobRunner::builder(f.h.orm().clone(), support::jobs::PassResolver)
        .exclude_kinds(["base.attachment_gc", "base.attachment_nonexistent"])
        .build()
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming, "{err:?}");
    assert_eq!(
        err.message(),
        "JobRunner::exclude_kinds names the job kind 'base.attachment_nonexistent', which is not \
         registered in this binary"
    );
}

/// Attachment kinds that are deliberately not elevated: none today. A new
/// `base.attachment_*` kind (the owner heartbeat, for one) must be classified here or in
/// `ELEVATED_KINDS` before this test passes.
const NOT_ELEVATED_ATTACHMENT_KINDS: [&str; 0] = [];

/// Every registered `base.attachment_*` kind is classified, and every elevated kind exists.
#[test]
fn every_attachment_kind_is_classified_for_the_ordinary_runner() {
    use erp_addon_base::attachments::gc::ELEVATED_KINDS;
    let registered: Vec<&str> = erp_orm::jobs::JOB_KINDS
        .iter()
        .map(|decl| decl.name)
        .filter(|name| name.starts_with("base.attachment_"))
        .collect();
    for name in &registered {
        assert!(
            ELEVATED_KINDS.contains(name) || NOT_ELEVATED_ATTACHMENT_KINDS.contains(name),
            "job kind {name} is neither in ELEVATED_KINDS nor in NOT_ELEVATED_ATTACHMENT_KINDS"
        );
    }
    for name in ELEVATED_KINDS {
        assert!(registered.contains(&name), "{name} is not registered");
    }
}

const GC: &str = "base.attachment_gc";

async fn exec(f: &Fixture, sql: &str) {
    let mut tx = f.h.begin().await.unwrap();
    tx.batch(sql).await.unwrap();
    tx.commit().await.unwrap();
}

/// Make only the collection job due and let one runner tick.
async fn run_gc(f: &Fixture) {
    exec(
        f,
        &format!("UPDATE erp_job SET run_after = now() WHERE kind = '{GC}' AND state = 'ready'"),
    )
    .await;
    support::jobs::runner(&f.h).tick().await.unwrap();
}

async fn text(f: &Fixture, sql: &str) -> String {
    let mut tx = f.h.begin().await.unwrap();
    let v: String = tx.query(sql, &[]).await.unwrap()[0].get(0);
    tx.rollback().await.unwrap();
    v
}

/// `state`, error code and error of the collection job, as one text.
async fn gc_outcome(f: &Fixture) -> String {
    text(
        f,
        &format!(
            "SELECT state || coalesce(' ' || last_error_code, '') || coalesce(' ' || last_error, '') \
             FROM erp_job WHERE kind = '{GC}'"
        ),
    )
    .await
}

/// The store's admission is lost between the sweep's two transactions (here: a test trigger on
/// the marking makes the heartbeat stale, as a concurrent heartbeat preparation would make it
/// `recovering`). The strict removal refuses; the job retries instead of dead-lettering, the
/// rows stay marked, and a later run removes the blob.
#[tokio::test]
async fn the_collection_job_retries_when_admission_is_lost_between_mark_and_remove() {
    let f = fixture(external_limits()).await;
    admit_cleanup(f.h.orm()).await;
    let id = external(&f, "race.txt", b"lost admission between phases").await;
    unlink(&f, id).await;
    erp_addon_base::attachments::gc::ensure_schedules(f.h.orm())
        .await
        .unwrap();
    exec(
        &f,
        "CREATE FUNCTION test_lose_admission() RETURNS trigger LANGUAGE plpgsql AS $fn$ \
         BEGIN UPDATE ir_blob_owner_state SET last_success_at = clock_timestamp() - interval '1 hour'; \
         RETURN NULL; END $fn$; \
         CREATE TRIGGER test_lose_admission AFTER UPDATE OF sweep_attempted_at ON ir_blob \
         FOR EACH ROW EXECUTE FUNCTION test_lose_admission();",
    )
    .await;
    for _ in 0..4 {
        run_gc(&f).await;
        if gc_outcome(&f).await != "ready" {
            break;
        }
    }
    // The refusal the strict check gives now is the one the job stored.
    let mut tx = rc(&f).await;
    let refusal = erp_addon_base::attachments::gc::check_cleanup(&mut tx, "main")
        .await
        .unwrap_err();
    tx.rollback().await.unwrap();
    drop(tx);
    assert_eq!(
        gc_outcome(&f).await,
        format!("ready cleanup_refused {}", refusal.message())
    );
    assert_eq!(
        scalar(
            &f.h,
            "SELECT count(*) FROM ir_blob WHERE sweep_attempted_at IS NOT NULL"
        )
        .await,
        1
    );
    assert_eq!(f.store.content_keys().len(), 1);

    exec(
        &f,
        "DROP TRIGGER test_lose_admission ON ir_blob; DROP FUNCTION test_lose_admission(); \
         UPDATE ir_blob_owner_state SET last_success_at = clock_timestamp();",
    )
    .await;
    for _ in 0..4 {
        run_gc(&f).await;
        if blobs(&f).await == 0 {
            break;
        }
    }
    assert_eq!(blobs(&f).await, 0);
    assert!(f.store.content_keys().is_empty());
    assert_eq!(gc_outcome(&f).await, "ready");
}

/// a store stays refused is set back in the stored progress, never slept)
#[tokio::test]
async fn the_collection_job_fails_its_run_once_a_store_stays_refused_past_the_observation() {
    let f = fixture(external_limits()).await;
    let id = external(&f, "refused.txt", b"garbage of a refused store").await;
    unlink(&f, id).await;
    erp_addon_base::attachments::gc::ensure_schedules(f.h.orm())
        .await
        .unwrap();
    run_gc(&f).await;
    assert_eq!(gc_outcome(&f).await, "ready");
    let store_id = scalar(
        &f.h,
        "SELECT id::int8 FROM ir_blob_store WHERE store_key='main'",
    )
    .await;
    let reason = text(
        &f,
        &format!(
            "SELECT progress->'refused'->'{store_id}'->>'reason' FROM erp_job WHERE kind = '{GC}'"
        ),
    )
    .await;
    let since_query = format!(
        "SELECT (progress->'refused'->'{store_id}'->>'since')::int8 FROM erp_job WHERE kind = '{GC}'"
    );
    let since = scalar(&f.h, &since_query).await;
    let earlier = since
        - i64::try_from(erp_addon_base::attachments::ownership::OBSERVATION_SECS).unwrap()
        - 1;
    exec(
        &f,
        &format!(
            "UPDATE erp_job SET progress = jsonb_set(progress, '{{refused,{store_id},since}}', to_jsonb({earlier}::int8)) \
             WHERE kind = '{GC}'"
        ),
    )
    .await;
    run_gc(&f).await;
    assert_eq!(
        gc_outcome(&f).await,
        format!(
            "ready cleanup_refused Attachment cleanup has refused these stores for longer than {} \
             seconds; their rows and reservations are kept: store {store_id} ({reason})",
            erp_addon_base::attachments::ownership::OBSERVATION_SECS
        )
    );
    assert_eq!(scalar(&f.h, &since_query).await, earlier);
    assert_eq!(blobs(&f).await, 1);
    assert_eq!(
        scalar(&f.h, "SELECT count(*) FROM erp_job WHERE state = 'dead'").await,
        0
    );
}

/// The declarations flagged elevated are exactly `ELEVATED_KINDS`: a dropped `.elevated()` or a new
/// elevated kind missing from the pinned list fails here.
#[test]
fn the_elevated_declarations_are_exactly_the_elevated_kinds() {
    use erp_addon_base::attachments::gc::ELEVATED_KINDS;
    let mut flagged: Vec<&str> = erp_orm::jobs::JOB_KINDS
        .iter()
        .filter(|decl| decl.elevated)
        .map(|decl| decl.name)
        .collect();
    flagged.sort_unstable();
    let mut listed = ELEVATED_KINDS.to_vec();
    listed.sort_unstable();
    assert_eq!(flagged, listed);
}

#[tokio::test]
async fn usage_compactor_has_one_schedule_and_is_excluded_from_ordinary_execution() {
    let f = fixture(limits()).await;
    attachments::gc::ensure_schedules(f.h.orm()).await.unwrap();
    attachments::gc::ensure_schedules(f.h.orm()).await.unwrap();
    assert_eq!(scalar(&f.h,"SELECT count(*) FROM erp_job WHERE kind='base.attachment_usage_compact' AND state<>'dead'").await,1);
    assert!(attachments::gc::ELEVATED_KINDS.contains(&"base.attachment_usage_compact"));
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn metadata_maintenance_refuses_repeatable_read_before_mutation() {
    let f = fixture(limits()).await;
    for reconcile in [false, true] {
        let mut tx = f.h.begin().await.unwrap();
        let env = tx.env();
        let err = if reconcile {
            reconcile_chunk(&mut tx, &env, 0, 10)
                .await
                .map(|_| ())
                .unwrap_err()
        } else {
            mark_unreferenced(&mut tx, 10)
                .await
                .map(|_| ())
                .unwrap_err()
        };
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert_eq!(
            err.message(),
            "The garbage collection sweep runs in a READ COMMITTED transaction (its job kinds do): a REPEATABLE READ snapshot predates the hold lock and could miss a hold."
        );
        tx.rollback().await.unwrap();
        drop(tx);
    }
    f.h.cleanup().await.unwrap();
}
