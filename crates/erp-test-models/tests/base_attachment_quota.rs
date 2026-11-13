// SPDX-License-Identifier: MIT OR Apache-2.0
//! Durable quota reservations and hard staging admission.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
mod attachment_support;
use attachment_support::*;
use erp_addon_base::attachments::{
    self, AttachmentContent, NewAttachment, StageRequest, Uploader, gc,
};
use erp_blob::{AttachmentLimits, AttachmentRuntime, AttachmentStore, Op, When};
use erp_orm::{BeginOptions, Isolation, Orm, PrincipalHint};
use erp_sdk::{Env, ErrorKind};
use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, ReadBuf};

fn budgets(file: u64, staging: u64) -> AttachmentLimits {
    let mut l = limits().with_max_file_bytes_for_tests(file);
    l.staging_max_bytes = staging;
    l
}
async fn env_of(orm: &Orm) -> Env {
    let mut tx = orm.begin(PrincipalHint::system()).await.unwrap();
    let env = tx.env();
    tx.rollback().await.unwrap();
    env
}
fn other_node(f: &Fixture, limits: AttachmentLimits) -> Orm {
    let rt = Arc::new(AttachmentRuntime::single(
        "main",
        Arc::clone(&f.store) as Arc<dyn AttachmentStore>,
        limits,
    ));
    Orm::builder(
        Arc::clone(f.h.registry()),
        erp_orm::pool::create_pool(f.h.db().config(), 4).unwrap(),
    )
    .policy(erp_orm::StaticPolicy::AllowAll)
    .generation_attachment(erp_orm::GenerationAttachment::new(
        Arc::clone(f.h.registry()),
        rt,
    ))
    .build()
    .unwrap()
}
async fn usage(f: &Fixture) -> i64 {
    scalar(&f.h, "SELECT ((SELECT bytes FROM ir_blob_usage_base WHERE id = 1) + COALESCE((SELECT sum(bytes) FROM ir_blob_usage_delta), 0))::int8").await
}
async fn clean(f: &Fixture) {
    admit_cleanup(f.h.orm()).await;
    let mut tx = f
        .h
        .orm()
        .begin_opts(BeginOptions::new(PrincipalHint::system()).isolation(Isolation::ReadCommitted))
        .await
        .unwrap();
    let env = tx.env();
    gc::clean_staging(&mut tx, &env, 100).await.unwrap();
    tx.commit().await.unwrap();
}
struct NeverRead;
impl AsyncRead for NeverRead {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        _: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        panic!("admission must refuse before polling bytes")
    }
}

#[tokio::test]
async fn two_nodes_cannot_race_the_last_pending_slot() {
    let l = budgets(16, 64).with_pending_uploads_for_tests(1);
    let f = fixture(l.clone()).await;
    let b = other_node(&f, l);
    let env = env_of(f.h.orm()).await;
    let other = env_of(&b).await;
    let a = Uploader::new(f.h.orm());
    let b = Uploader::new(&b);
    let mut bytes_a = b"a".as_slice();
    let mut bytes_b = b"b".as_slice();
    let (a, b) = tokio::join!(
        a.stage(&env, &mut bytes_a, StageRequest::named("a.txt")),
        b.stage(&other, &mut bytes_b, StageRequest::named("b.txt"))
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    let err = a.err().or_else(|| b.err()).unwrap();
    assert_eq!(err.kind(), ErrorKind::ResourceExhausted);
    assert_eq!(
        err.message(),
        "The limit of 1 pending uploads is reached. Wait for attachment cleanup, then retry."
    );
    assert_eq!(scalar(&f.h, "SELECT count(*) FROM ir_blob_upload").await, 1);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn retained_and_unknown_length_reservations_enforce_byte_caps_before_reading() {
    let f = fixture(budgets(8, 10)).await;
    let env = env_of(f.h.orm()).await;
    let _up = stage(&f, &env, "a.txt", b"123456").await;
    assert_eq!(usage(&f).await, 6);
    let err = Uploader::new(f.h.orm())
        .stage(&env, &mut NeverRead, StageRequest::named("blocked.txt"))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::ResourceExhausted);
    assert_eq!(
        err.message(),
        "QuotaExceeded: database staging uses 6 bytes; requested 8, limit 10"
    );
    assert_eq!(scalar(&f.h, "SELECT count(*) FROM ir_blob_upload").await, 1);
    let mut tx = f.h.begin().await.unwrap();
    tx.execute(
        "UPDATE ir_blob_upload SET expires_at = now() - interval '1 second'",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let err = Uploader::new(f.h.orm())
        .stage(
            &env,
            &mut NeverRead,
            StageRequest::named("still-blocked.txt"),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::ResourceExhausted);
    assert_eq!(
        err.message(),
        "QuotaExceeded: database staging uses 6 bytes; requested 8, limit 10"
    );
    clean(&f).await;
    assert_eq!(usage(&f).await, 0);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn actual_bytes_cannot_exceed_declared_or_effective_limits() {
    let f = fixture(budgets(4, 100)).await;
    let env = env_of(f.h.orm()).await;
    for (request, limit) in [
        (StageRequest::named("tiny.txt"), 4),
        (StageRequest::named("short.txt").with_declared_length(2), 2),
        (StageRequest::named("effective.txt").with_file_limit(3), 3),
    ] {
        let mut bytes = b"12345".as_slice();
        let err = Uploader::new(f.h.orm())
            .stage(&env, &mut bytes, request)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation);
        assert_eq!(
            err.message(),
            format!(
                "object size in bytes exceeds the limit of {limit} (and fenced cleanup pending; the committed reservation is retained)"
            )
        );
    }
    let err = Uploader::new(f.h.orm())
        .stage(
            &env,
            &mut NeverRead,
            StageRequest::named("oversized.txt").with_declared_length(5),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.message(), "object size in bytes exceeds the limit of 4");
    let mut bytes = b"ok".as_slice();
    let up = Uploader::new(f.h.orm())
        .stage(
            &env,
            &mut bytes,
            StageRequest::named("ok.txt").with_declared_length(4),
        )
        .await
        .unwrap();
    assert_eq!(up.size, 2);
    assert_eq!(
        scalar(
            &f.h,
            "SELECT reserved_bytes FROM ir_blob_upload WHERE state = 1"
        )
        .await,
        2
    );
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn usage_tracks_deduplicated_blobs_and_staging_until_cleanup() {
    let f = fixture(budgets(64, 1000).with_inline_max_bytes_for_tests(4)).await;
    let env = env_of(f.h.orm()).await;
    let payload = b"same external content";
    let up = stage(&f, &env, "a.txt", payload).await;
    let mut tx = f.h.begin().await.unwrap();
    attachments::create(
        &mut tx,
        vec![NewAttachment::new(
            "a.txt",
            AttachmentContent::Upload(up.id.clone()),
        )],
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    drop(tx);
    assert_eq!(usage(&f).await, payload.len() as i64);
    assert_eq!(
        scalar(&f.h, "SELECT count(*) FROM ir_blob_upload WHERE state = 1").await,
        1
    );
    let second = stage(&f, &env, "b.txt", payload).await;
    let mut tx = f.h.begin().await.unwrap();
    attachments::create(
        &mut tx,
        vec![
            NewAttachment::new("a.txt", AttachmentContent::Upload(up.id)),
            NewAttachment::new("b.txt", AttachmentContent::Upload(second.id)),
        ],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(usage(&f).await, 3 * payload.len() as i64);
    clean(&f).await;
    assert_eq!(usage(&f).await, payload.len() as i64);
    clean(&f).await;
    assert_eq!(usage(&f).await, payload.len() as i64);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn failed_deletion_keeps_the_only_marker_and_reservation() {
    let f = fixture(budgets(64, 1000).with_inline_max_bytes_for_tests(4)).await;
    admit_cleanup(f.h.orm()).await;
    let env = env_of(f.h.orm()).await;
    f.store.faults().inject(Op::Stage, When::After, 0, 1);
    f.store
        .faults()
        .inject(Op::DeleteStaged, When::Before, 0, 1);
    let mut body = b"failed external content".as_slice();
    let err = Uploader::new(f.h.orm())
        .stage(&env, &mut body, StageRequest::named("failed.txt"))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Internal);
    assert_eq!(
        err.message(),
        "The attachment store failed: the attachment store is unavailable (retryable): injected fault in Stage (After its effect) (and fenced cleanup pending; the committed reservation is retained)"
    );
    assert!(
        std::error::Error::source(&err)
            .unwrap()
            .downcast_ref::<erp_blob::BlobError>()
            .is_some(),
        "the original typed store source must survive cleanup diagnostics"
    );
    assert_eq!(
        scalar(
            &f.h,
            "SELECT count(*) FROM ir_blob_upload WHERE state = 3 AND reserved_bytes = 64"
        )
        .await,
        1
    );
    assert_eq!(usage(&f).await, 64);
    let mut tx = f
        .h
        .orm()
        .begin_opts(BeginOptions::new(PrincipalHint::system()).isolation(Isolation::ReadCommitted))
        .await
        .unwrap();
    let env = tx.env();
    let err = gc::clean_staging(&mut tx, &env, 100).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Internal);
    assert_eq!(
        err.message(),
        "The attachment store failed: the attachment store is unavailable (retryable): injected fault in DeleteStaged (Before its effect)"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    assert_eq!(usage(&f).await, 64);
    clean(&f).await;
    assert_eq!(usage(&f).await, 0);
    assert_eq!(scalar(&f.h, "SELECT count(*) FROM ir_blob_upload").await, 0);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn inline_writes_charge_new_content_once_and_refuse_the_soft_quota() {
    let mut l = budgets(64, 1000);
    l.quota_bytes = Some(5);
    let f = fixture(l).await;
    let mut tx = f.h.begin().await.unwrap();
    attachments::create(
        &mut tx,
        vec![
            bytes_attachment("a.txt", b"12345", None),
            bytes_attachment("b.txt", b"12345", None),
        ],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(usage(&f).await, 5);
    let mut tx = f.h.begin().await.unwrap();
    let err = attachments::create(&mut tx, vec![bytes_attachment("over.txt", b"x", None)])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::ResourceExhausted);
    assert_eq!(
        err.message(),
        "QuotaExceeded: database quota uses 5 bytes; requested 1, limit 5"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    assert_eq!(usage(&f).await, 5);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn compaction_preserves_concurrent_and_late_committing_deltas() {
    let f = fixture(limits()).await;
    let mut late = f.h.begin().await.unwrap();
    late.execute("INSERT INTO ir_blob_usage_delta(bytes) VALUES (7)", &[])
        .await
        .unwrap();
    let mut tx = f.h.begin().await.unwrap();
    tx.execute("INSERT INTO ir_blob_usage_delta(bytes) VALUES (11)", &[])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut compact = f
        .h
        .orm()
        .begin_opts(BeginOptions::new(PrincipalHint::system()).isolation(Isolation::ReadCommitted))
        .await
        .unwrap();
    gc::compact_usage(&mut compact, 100).await.unwrap();
    compact.commit().await.unwrap();
    drop(compact);
    late.commit().await.unwrap();
    drop(late);
    assert_eq!(usage(&f).await, 18);
    let mut compact = f
        .h
        .orm()
        .begin_opts(BeginOptions::new(PrincipalHint::system()).isolation(Isolation::ReadCommitted))
        .await
        .unwrap();
    gc::compact_usage(&mut compact, 100).await.unwrap();
    compact.rollback().await.unwrap();
    drop(compact);
    assert_eq!(usage(&f).await, 18);
    f.h.cleanup().await.unwrap();
}

struct HoldReader(Option<tokio::sync::oneshot::Sender<()>>);
impl AsyncRead for HoldReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        _: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if let Some(entered) = self.0.take() {
            entered.send(()).unwrap();
        }
        Poll::Pending
    }
}
#[tokio::test]
async fn cancelled_receive_retains_reservation_without_holding_database_locks() {
    let f = fixture(budgets(64, 1000)).await;
    let env = env_of(f.h.orm()).await;
    let orm = f.h.orm().clone();
    let (entered, wait) = tokio::sync::oneshot::channel();
    let receive = tokio::spawn(async move {
        Uploader::new(&orm)
            .stage(
                &env,
                &mut HoldReader(Some(entered)),
                StageRequest::named("cancel.txt"),
            )
            .await
    });
    wait.await.unwrap();
    assert_eq!(usage(&f).await, 64);
    let mut tx = f.h.begin().await.unwrap();
    let free: bool = tx
        .query(
            "SELECT pg_try_advisory_xact_lock(4995143246961136705::int8)",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert!(
        free,
        "an active receiver must not hold the database admission lock"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    receive.abort();
    assert!(receive.await.unwrap_err().is_cancelled());
    assert_eq!(f.rt.available_stream_permits(), 32);
    assert_eq!(
        scalar(
            &f.h,
            "SELECT reserved_bytes FROM ir_blob_upload WHERE state=0"
        )
        .await,
        64
    );
    let mut tx = f.h.begin().await.unwrap();
    tx.execute(
        "UPDATE ir_blob_upload SET expires_at=now()-interval '1 second'",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    clean(&f).await;
    assert_eq!(usage(&f).await, 0);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn two_nodes_cannot_race_the_database_byte_budget() {
    let l = budgets(8, 10);
    let f = fixture(l.clone()).await;
    let b = other_node(&f, l);
    let env = env_of(f.h.orm()).await;
    let mut other_tx = b
        .begin(PrincipalHint::user(erp_orm::UserId::new(7).unwrap()))
        .await
        .unwrap();
    let other = other_tx.env();
    other_tx.rollback().await.unwrap();
    drop(other_tx);
    let a = Uploader::new(f.h.orm());
    let b = Uploader::new(&b);
    let mut first = b"123456".as_slice();
    let mut second = b"abcdef".as_slice();
    let (a, b) = tokio::join!(
        a.stage(
            &env,
            &mut first,
            StageRequest::named("a.txt").with_declared_length(6)
        ),
        b.stage(
            &other,
            &mut second,
            StageRequest::named("b.txt").with_declared_length(6)
        )
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    let err = a.err().or_else(|| b.err()).unwrap();
    assert_eq!(err.kind(), ErrorKind::ResourceExhausted);
    assert_eq!(
        err.message(),
        "QuotaExceeded: database staging uses 6 bytes; requested 6, limit 10"
    );
    assert_eq!(usage(&f).await, 6);
    f.h.cleanup().await.unwrap();
}

async fn key_env(orm: &Orm, user: i64, key: i64) -> Env {
    use erp_orm::{
        ApiKeyId, GroupSet, MethodAllow, ModelAllow, PrincipalBuilder, Resolution, Scope, UserId,
    };
    let scope = Scope::builder(ApiKeyId::new(key).unwrap(), "quota fixture")
        .models(ModelAllow::All)
        .methods(MethodAllow::All)
        .build()
        .unwrap();
    let resolution =
        Resolution::in_id_order(GroupSet::default(), [], std::num::NonZeroU64::MIN).unwrap();
    let principal = PrincipalBuilder::user(UserId::new(user).unwrap())
        .resolution(resolution)
        .scope(scope)
        .build()
        .unwrap();
    let mut tx = orm
        .begin_opts(BeginOptions::for_principal(principal))
        .await
        .unwrap();
    let env = tx.env();
    tx.rollback().await.unwrap();
    env
}
#[tokio::test]
async fn several_keys_share_one_subject_byte_budget() {
    let f = fixture(budgets(8, 1000)).await;
    let first = key_env(f.h.orm(), 7, 11).await;
    let second = key_env(f.h.orm(), 7, 12).await;
    for _ in 0..4 {
        stage(&f, &first, "key.txt", b"12345678").await;
    }
    let err = Uploader::new(f.h.orm())
        .stage(&second, &mut NeverRead, StageRequest::named("refused.txt"))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::ResourceExhausted);
    assert_eq!(
        err.message(),
        "QuotaExceeded: subject staging uses 32 bytes; requested 8, limit 32"
    );
    let other = key_env(f.h.orm(), 8, 13).await;
    stage(&f, &other, "other.txt", b"12345678").await;
    assert_eq!(usage(&f).await, 40);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn inline_accounting_rolls_back_with_its_savepoint() {
    let f = fixture(limits()).await;
    let mut tx = f.h.begin().await.unwrap();
    let result: erp_sdk::Result<()> = tx
        .savepoint(async |tx| {
            attachments::create(
                tx,
                vec![bytes_attachment("rollback.txt", b"savepoint bytes", None)],
            )
            .await?;
            Err(erp_sdk::Error::validation("fixture rollback"))
        })
        .await;
    let err = result.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.message(), "fixture rollback");
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(usage(&f).await, 0);
    assert_eq!(scalar(&f.h, "SELECT count(*) FROM ir_blob").await, 0);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn physical_blob_sweep_releases_its_charge_only_with_commit() {
    let f = fixture(budgets(64, 1000).with_inline_max_bytes_for_tests(4)).await;
    let env = env_of(f.h.orm()).await;
    let up = stage(&f, &env, "sweep.txt", b"swept content").await;
    let mut tx = f.h.begin().await.unwrap();
    attachments::create(
        &mut tx,
        vec![NewAttachment::new(
            "sweep.txt",
            AttachmentContent::Upload(up.id),
        )],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    clean(&f).await;
    let mut tx = f.h.begin().await.unwrap();
    tx.execute("DELETE FROM ir_attachment", &[]).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let rc = || BeginOptions::new(PrincipalHint::system()).isolation(Isolation::ReadCommitted);
    let mut tx = f.h.orm().begin_opts(rc()).await.unwrap();
    gc::mark_unreferenced(&mut tx, 100).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = f.h.orm().begin_opts(rc()).await.unwrap();
    let ids = gc::sweep_mark(&mut tx, 0, 100).await.unwrap().ids;
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = f.h.orm().begin_opts(rc()).await.unwrap();
    let env = tx.env();
    gc::sweep_remove(&mut tx, &env, &ids, 0).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(usage(&f).await, 0);
    assert!(f.store.content_keys().is_empty());
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn deletion_after_effect_failure_keeps_accounting_until_retry() {
    let f = fixture(budgets(64, 1000).with_inline_max_bytes_for_tests(4)).await;
    admit_cleanup(f.h.orm()).await;
    let env = env_of(f.h.orm()).await;
    f.store.faults().inject(Op::Stage, When::After, 0, 1);
    let err = Uploader::new(f.h.orm())
        .stage(
            &env,
            &mut b"abandoned content".as_slice(),
            StageRequest::named("lost.txt"),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Internal);
    assert_eq!(
        err.message(),
        "The attachment store failed: the attachment store is unavailable (retryable): injected fault in Stage (After its effect) (and fenced cleanup pending; the committed reservation is retained)"
    );
    f.store.faults().inject(Op::DeleteStaged, When::After, 0, 1);
    let mut tx = f
        .h
        .orm()
        .begin_opts(BeginOptions::new(PrincipalHint::system()).isolation(Isolation::ReadCommitted))
        .await
        .unwrap();
    let env = tx.env();
    let err = gc::clean_staging(&mut tx, &env, 100).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Internal);
    assert_eq!(
        err.message(),
        "The attachment store failed: the attachment store is unavailable (retryable): injected fault in DeleteStaged (After its effect)"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    assert!(f.store.staging_keys().is_empty());
    assert_eq!(usage(&f).await, 64);
    clean(&f).await;
    assert_eq!(usage(&f).await, 0);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn concurrent_compactors_fold_disjoint_batches_without_losing_usage() {
    let f = fixture(limits()).await;
    let mut tx = f.h.begin().await.unwrap();
    tx.execute("INSERT INTO ir_blob_usage_delta(bytes) SELECT v FROM unnest(ARRAY[3,5,7,11,13,17]) AS t(v)",&[]).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let before = usage(&f).await;
    let compact = async || {
        let mut tx =
            f.h.orm()
                .begin_opts(
                    BeginOptions::new(PrincipalHint::system()).isolation(Isolation::ReadCommitted),
                )
                .await
                .unwrap();
        let n = gc::compact_usage(&mut tx, 3).await.unwrap();
        tx.commit().await.unwrap();
        n
    };
    let (a, b) = tokio::join!(compact(), compact());
    assert_eq!(a + b, 6);
    assert_eq!(usage(&f).await, before);
    assert_eq!(
        scalar(&f.h, "SELECT count(*) FROM ir_blob_usage_delta").await,
        0
    );
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn an_admitted_upload_can_link_at_the_soft_quota_in_both_storage_paths() {
    for inline in [0, 2048] {
        let mut l = budgets(64, 1000).with_inline_max_bytes_for_tests(inline);
        l.quota_bytes = Some(4);
        let f = fixture(l).await;
        let env = env_of(f.h.orm()).await;
        let up = Uploader::new(f.h.orm())
            .stage(
                &env,
                &mut b"four".as_slice(),
                StageRequest::named("four.txt").with_declared_length(4),
            )
            .await
            .unwrap();
        let mut tx = f.h.begin().await.unwrap();
        attachments::create(
            &mut tx,
            vec![NewAttachment::new(
                "four.txt",
                AttachmentContent::Upload(up.id),
            )],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        drop(tx);
        assert_eq!(
            usage(&f).await,
            if inline == 0 { 8 } else { 4 },
            "only an external staging copy remains charged until cleanup"
        );
        clean(&f).await;
        assert_eq!(usage(&f).await, 4);
        f.h.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn a_stalled_receive_cannot_outlive_its_durable_intent() {
    let mut l = budgets(64, 1000);
    l.upload_ttl_secs = 1; // Test-only shortened lease, below the configuration floor.
    let f = fixture(l).await;
    let env = env_of(f.h.orm()).await;
    let (entered, _keep_receiver) = tokio::sync::oneshot::channel();
    let err = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        Uploader::new(f.h.orm()).stage(
            &env,
            &mut HoldReader(Some(entered)),
            StageRequest::named("expired.txt"),
        ),
    )
    .await
    .expect("receive must finish when its durable intent lifetime ends")
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "The upload intent expired before receiving completed. (and fenced cleanup pending; the committed reservation is retained)"
    );
    assert_eq!(
        scalar(
            &f.h,
            "SELECT reserved_bytes FROM ir_blob_upload WHERE state=3"
        )
        .await,
        64
    );
    assert_eq!(f.rt.available_stream_permits(), 32);
    clean(&f).await;
    assert_eq!(usage(&f).await, 0);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn zero_byte_failed_intents_still_count_until_cleanup() {
    let f = fixture(budgets(64, 1000).with_pending_uploads_for_tests(1)).await;
    let env = env_of(f.h.orm()).await;
    let err = Uploader::new(f.h.orm())
        .stage(
            &env,
            &mut b"x".as_slice(),
            StageRequest::named("zero.txt").with_declared_length(0),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "object size in bytes exceeds the limit of 0 (and fenced cleanup pending; the committed reservation is retained)"
    );
    assert_eq!(
        scalar(
            &f.h,
            "SELECT count(*) FROM ir_blob_upload WHERE state=3 AND reserved_bytes=0"
        )
        .await,
        1
    );
    let err = Uploader::new(f.h.orm())
        .stage(&env, &mut NeverRead, StageRequest::named("refused.txt"))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::ResourceExhausted);
    assert_eq!(
        err.message(),
        "The limit of 1 pending uploads is reached. Wait for attachment cleanup, then retry."
    );
    clean(&f).await;
    assert_eq!(scalar(&f.h, "SELECT count(*) FROM ir_blob_upload").await, 0);
    stage(&f, &env, "after.txt", b"after cleanup").await;
    f.h.cleanup().await.unwrap();
}
