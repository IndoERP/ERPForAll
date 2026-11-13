// SPDX-License-Identifier: MIT OR Apache-2.0
//! Durable coordinator contract; deletion and independent-process acceptance remain separate.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
mod attachment_support;
use attachment_support::*;
use erp_addon_base::attachments::ownership::{self, Health, Purpose, Settlement};
use erp_blob::{AttachmentStore, BlobError, Op, When};
use erp_orm::{BeginOptions, Isolation, PrincipalHint};

async fn begin(f: &Fixture) -> erp_sdk::Transaction {
    f.h.orm()
        .begin_opts(BeginOptions::new(PrincipalHint::system()).isolation(Isolation::ReadCommitted))
        .await
        .unwrap()
}
async fn prepared(f: &Fixture, purpose: Purpose) -> ownership::Prepared {
    let mut tx = begin(f).await;
    let p = ownership::prepare(&mut tx, "main", purpose).await.unwrap();
    tx.commit().await.unwrap();
    p
}
async fn settle(f: &Fixture, p: &ownership::Prepared, a: ownership::Attempt) -> Settlement {
    let mut tx = begin(f).await;
    let result = ownership::settle(&mut tx, p, a).await.unwrap();
    tx.commit().await.unwrap();
    result
}
async fn advance(f: &Fixture, purpose: Purpose) -> Settlement {
    let p = prepared(f, purpose).await;
    let a = p.execute().await;
    settle(f, &p, a).await
}

#[tokio::test]
async fn persisted_intent_recovers_a_lost_store_reply() {
    let f = fixture(external_limits()).await;
    assert!(matches!(
        advance(&f, Purpose::Heartbeat).await,
        Settlement::Confirmed { .. }
    ));
    let p = prepared(&f, Purpose::Heartbeat).await;
    assert_eq!(
        scalar(
            &f.h,
            "SELECT count(*) FROM ir_blob_owner_state WHERE pending_marker IS NOT NULL"
        )
        .await,
        1
    );
    let previous = f.store.owner(p.namespace()).await.unwrap().unwrap();
    assert_eq!(previous.marker().generation(), 1);
    f.store.faults().inject(Op::PutOwner, When::After, 0, 1);
    let result = settle(&f, &p, p.execute().await).await;
    assert!(matches!(result, Settlement::Retry(_)));
    assert_eq!(scalar(&f.h, "SELECT count(*) FROM ir_blob_owner_state WHERE pending_marker IS NOT NULL AND last_success_at IS NOT NULL").await, 1);
    let resumed = prepared(&f, Purpose::Heartbeat).await;
    assert_eq!(p.marker(), resumed.marker());
    assert!(matches!(
        settle(&f, &resumed, resumed.execute().await).await,
        Settlement::Confirmed { recovered: true }
    ));
    assert_eq!(
        ownership::health(f.h.orm(), "main").await.unwrap(),
        Health::Healthy
    );
    assert_eq!(scalar(&f.h, "SELECT count(*) FROM ir_blob_owner_state WHERE pending_marker IS NULL AND confirmed_marker IS NOT NULL").await, 1);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn conflicting_write_retains_contest_pending_when_publication_fails() {
    let f = fixture(external_limits()).await;
    assert!(matches!(
        advance(&f, Purpose::Heartbeat).await,
        Settlement::Confirmed { .. }
    ));
    let p = prepared(&f, Purpose::Heartbeat).await;
    let current = f.store.owner(p.namespace()).await.unwrap().unwrap();
    let foreign = erp_blob::OwnerMarker::new(
        current.marker().identity().clone(),
        current.marker().generation() + 1,
        erp_blob::Namespace::random().unwrap(),
        current.marker().observed_at(),
    )
    .unwrap();
    f.store
        .put_owner(p.namespace(), &foreign, Some(current.version()))
        .await
        .unwrap();
    assert!(matches!(
        settle(&f, &p, p.execute().await).await,
        Settlement::ContestPending
    ));
    f.store
        .faults()
        .inject(Op::CreateContested, When::Before, 0, 1);
    let result = p.publish_contest().await;
    let mut tx = begin(&f).await;
    let result = ownership::settle_contest(&mut tx, &p, result)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(matches!(result, Settlement::Retry(_)));
    assert_eq!(
        scalar(
            &f.h,
            "SELECT count(*) FROM ir_blob_owner_state WHERE mode = 'contest_pending'"
        )
        .await,
        1
    );
    assert_eq!(
        ownership::health(f.h.orm(), "main").await.unwrap(),
        Health::Unavailable
    );
    let mut tx = begin(&f).await;
    let result = ownership::settle_contest(&mut tx, &p, p.publish_contest().await)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(matches!(result, Settlement::Contested));
    assert!(f.store.contested(p.namespace()).await.unwrap().is_some());
    assert_eq!(
        scalar(
            &f.h,
            "SELECT count(*) FROM ir_blob_owner_state WHERE mode = 'contested'"
        )
        .await,
        1
    );
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn lost_final_commit_replays_and_old_completion_cannot_replace_new_intent() {
    let f = fixture(external_limits()).await;
    let p = prepared(&f, Purpose::Heartbeat).await;
    let mut tx = begin(&f).await;
    assert!(matches!(
        ownership::settle(&mut tx, &p, p.execute().await)
            .await
            .unwrap(),
        Settlement::Confirmed { .. }
    ));
    tx.rollback().await.unwrap();
    assert!(matches!(
        advance(&f, Purpose::Heartbeat).await,
        Settlement::Confirmed { recovered: true }
    ));
    let next = prepared(&f, Purpose::Heartbeat).await;
    assert_ne!(p.marker().operation(), next.marker().operation());
    assert!(matches!(
        settle(&f, &p, p.execute().await).await,
        Settlement::Superseded
    ));
    assert!(matches!(
        settle(&f, &next, next.execute().await).await,
        Settlement::Confirmed { .. }
    ));
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn stale_heartbeat_is_not_reset_by_reopening_the_runtime() {
    let f = fixture(external_limits()).await;
    assert_eq!(
        ownership::health(f.h.orm(), "main").await.unwrap(),
        Health::Uninitialized
    );
    advance(&f, Purpose::Heartbeat).await;
    let mut tx = begin(&f).await;
    tx.execute("UPDATE ir_blob_owner_state SET last_success_at = clock_timestamp() - interval '600 seconds'", &[]).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        ownership::health(f.h.orm(), "main").await.unwrap(),
        Health::Unavailable
    );
    assert!(matches!(
        advance(&f, Purpose::Heartbeat).await,
        Settlement::Confirmed { .. }
    ));
    assert_eq!(
        ownership::health(f.h.orm(), "main").await.unwrap(),
        Health::Healthy
    );
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn observation_survives_healthy_heartbeats_without_holding_a_lock() {
    let f = fixture(external_limits()).await;
    advance(&f, Purpose::Observe).await;
    assert!(matches!(
        ownership::observation(f.h.orm(), "main").await.unwrap(),
        ownership::Observation::Waiting { .. }
    ));
    let mut tx = begin(&f).await;
    assert!(
        tx.query(
            "SELECT pg_try_advisory_xact_lock(1703, hashtext($1::text))",
            &[&ownership::SINGLETON]
        )
        .await
        .unwrap()[0]
            .get::<_, bool>(0)
    );
    tx.execute("UPDATE ir_blob_owner_state SET observation_started_at = clock_timestamp() - interval '700 seconds', observation_not_before = clock_timestamp() - interval '1 second'", &[]).await.unwrap();
    tx.commit().await.unwrap();
    advance(&f, Purpose::Heartbeat).await;
    assert!(matches!(
        ownership::observation(f.h.orm(), "main").await.unwrap(),
        ownership::Observation::Ready { .. }
    ));
    let p = prepared(&f, Purpose::Heartbeat).await;
    f.store
        .create_contested(p.namespace(), p.marker())
        .await
        .unwrap();
    assert_eq!(
        ownership::health(f.h.orm(), "main").await.unwrap(),
        Health::Unavailable
    );
    assert!(matches!(
        ownership::observation(f.h.orm(), "main").await.unwrap(),
        ownership::Observation::Unavailable
    ));
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn copied_pending_operation_recovers_only_its_exact_marker() {
    let f = fixture(external_limits()).await;
    let a = prepared(&f, Purpose::Heartbeat).await;
    let b = prepared(&f, Purpose::Observe).await;
    assert_eq!(a.marker(), b.marker());
    let (aa, bb) = tokio::join!(a.execute(), b.execute());
    assert!(matches!(
        settle(&f, &a, aa).await,
        Settlement::Confirmed { .. }
    ));
    assert!(matches!(settle(&f, &b, bb).await, Settlement::Superseded));
    assert!(f.store.contested(a.namespace()).await.unwrap().is_none());
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn physical_database_identity_change_refuses_without_rewriting_identity() {
    let f = fixture(external_limits()).await;
    advance(&f, Purpose::Heartbeat).await;
    let mut tx = begin(&f).await;
    tx.execute(
        "UPDATE ir_blob_owner_identity SET database_oid = database_oid + 1",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let mut tx = begin(&f).await;
    let error = ownership::prepare(&mut tx, "main", Purpose::Heartbeat)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Internal);
    assert_eq!(
        error.message(),
        "Attachment ownership database identity changed; neutralize or rebase the clone before maintenance."
    );
    tx.rollback().await.unwrap();
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn retries_keep_typed_storage_failure_and_do_not_report_success() {
    let f = fixture(external_limits()).await;
    let p = prepared(&f, Purpose::Heartbeat).await;
    f.store.faults().inject(Op::PutOwner, When::Before, 0, 1);
    let result = settle(&f, &p, p.execute().await).await;
    let Settlement::Retry(error) = result else {
        panic!("expected typed retry")
    };
    assert!(matches!(
        error.as_ref(),
        BlobError::Unavailable {
            retryable: true,
            ..
        }
    ));
    assert_eq!(
        error.to_string(),
        "the attachment store is unavailable (retryable): injected fault in PutOwner (Before its effect)"
    );
    assert_eq!(
        ownership::health(f.h.orm(), "main").await.unwrap(),
        Health::Unavailable
    );
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn different_databases_sharing_a_namespace_contest_instead_of_adopting() {
    use erp_test_models::base_users;
    use erp_testkit::Harness;
    use std::sync::Arc;
    let a = fixture(external_limits()).await;
    advance(&a, Purpose::Heartbeat).await;
    let current = prepared(&a, Purpose::Heartbeat).await;
    let rt = Arc::new(erp_blob::AttachmentRuntime::single(
        "main",
        a.store.clone(),
        external_limits(),
    ));
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .attach(rt.clone())
        .build()
        .await
        .unwrap();
    let b = Fixture {
        h,
        store: a.store.clone(),
        rt,
    };
    let mut tx = begin(&b).await;
    tx.execute("INSERT INTO ir_blob_store(store_key,namespace,fingerprint) VALUES('main',$1::text::uuid,$2)", &[&current.namespace().as_str(),&a.store.fingerprint().unwrap()]).await.unwrap();
    tx.commit().await.unwrap();
    let p = prepared(&b, Purpose::Heartbeat).await;
    assert_ne!(p.marker().identity(), current.marker().identity());
    assert!(matches!(
        settle(&b, &p, p.execute().await).await,
        Settlement::ContestPending
    ));
    let mut tx = begin(&b).await;
    assert!(matches!(
        ownership::settle_contest(&mut tx, &p, p.publish_contest().await)
            .await
            .unwrap(),
        Settlement::Contested
    ));
    tx.commit().await.unwrap();
    assert_eq!(
        ownership::health(a.h.orm(), "main").await.unwrap(),
        Health::Unavailable
    );
    assert_eq!(
        ownership::health(b.h.orm(), "main").await.unwrap(),
        Health::Unavailable
    );
    b.h.cleanup().await.unwrap();
    a.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn observation_restarts_after_a_health_gap_or_ambiguous_reply() {
    let f = fixture(external_limits()).await;
    advance(&f, Purpose::Observe).await;
    let mut tx = begin(&f).await;
    tx.execute("UPDATE ir_blob_owner_state SET observation_started_at=clock_timestamp()-interval '700 seconds',observation_not_before=clock_timestamp()-interval '40 seconds',last_success_at=clock_timestamp()-interval '600 seconds'",&[]).await.unwrap();
    tx.commit().await.unwrap();
    advance(&f, Purpose::Heartbeat).await;
    assert_eq!(
        ownership::observation(f.h.orm(), "main").await.unwrap(),
        ownership::Observation::Unavailable
    );
    advance(&f, Purpose::Observe).await;
    let p = prepared(&f, Purpose::Heartbeat).await;
    f.store.faults().inject(Op::PutOwner, When::After, 0, 1);
    assert!(matches!(
        settle(&f, &p, p.execute().await).await,
        Settlement::Retry(_)
    ));
    assert!(matches!(
        advance(&f, Purpose::Heartbeat).await,
        Settlement::Confirmed { recovered: true }
    ));
    assert_eq!(
        ownership::observation(f.h.orm(), "main").await.unwrap(),
        ownership::Observation::Unavailable
    );
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn established_missing_marker_and_generation_overflow_are_not_reinitialization() {
    let f = fixture(external_limits()).await;
    advance(&f, Purpose::Heartbeat).await;
    let p = prepared(&f, Purpose::Heartbeat).await;
    let old = f.store.owner(p.namespace()).await.unwrap().unwrap();
    let max = erp_blob::OwnerMarker::new(
        old.marker().identity().clone(),
        u64::MAX,
        erp_blob::Namespace::random().unwrap(),
        old.marker().observed_at(),
    )
    .unwrap();
    let version = f
        .store
        .put_owner(p.namespace(), &max, Some(old.version()))
        .await
        .unwrap();
    let mut tx = begin(&f).await;
    tx.execute("UPDATE ir_blob_owner_state SET mode='healthy',confirmed_marker=$1,confirmed_etag=$2,confirmed_version=$3,pending_marker=NULL,pending_etag=NULL,pending_version=NULL,pending_purpose=NULL", &[&max.encode(),&version.etag(),&version.object_version()]).await.unwrap();
    tx.commit().await.unwrap();
    let mut tx = begin(&f).await;
    let e = ownership::prepare(&mut tx, "main", Purpose::Heartbeat)
        .await
        .unwrap_err();
    assert_eq!(e.kind(), erp_sdk::ErrorKind::Internal);
    assert_eq!(
        e.message(),
        "Attachment owner generation exhausted; maintenance cannot advance it."
    );
    tx.rollback().await.unwrap();
    assert_eq!(
        f.store
            .owner(p.namespace())
            .await
            .unwrap()
            .unwrap()
            .marker(),
        &max
    );
    f.h.cleanup().await.unwrap();
}

mod support;
static OWNER_JOB: ownership::HeartbeatJob = ownership::HeartbeatJob::new();
erp_orm::register_job_kind!(
    OWNER_TEST_KIND,
    erp_orm::jobs::JobKindDecl::new(
        "base",
        "base.owner_test",
        erp_orm::jobs::HandlerRef::Native(&OWNER_JOB)
    )
    .principal(erp_orm::jobs::PrincipalPolicy::System)
    .timeouts(erp_orm::jobs::TimeoutClass::Maintenance)
    .isolation(erp_orm::jobs::KindIsolation::ReadCommitted)
);

#[tokio::test]
async fn native_heartbeat_uses_shared_singleton_and_stale_epoch_writes_nothing() {
    use erp_orm::jobs::{EnqueueRequest, JobKey};
    let f = fixture(external_limits()).await;
    assert_eq!(
        scalar(
            &f.h,
            "SELECT count(*) FROM erp_job WHERE kind='base.owner_test'"
        )
        .await,
        0
    );
    let mut tx = begin(&f).await;
    let env = tx.env();
    tx.enqueue(
        &env,
        EnqueueRequest::new("base.owner_test")
            .singleton(JobKey::new(ownership::SINGLETON).unwrap()),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let a = support::jobs::runner(&f.h);
    let b = support::jobs::runner(&f.h);
    let claim = a.claim(1).await.unwrap().pop().unwrap();
    assert!(b.claim(1).await.unwrap().is_empty());
    let mut tx = begin(&f).await;
    tx.execute("UPDATE erp_job SET lease_epoch=lease_epoch+1", &[])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    a.run_claimed(claim).await.unwrap();
    assert_eq!(
        scalar(&f.h, "SELECT count(*) FROM ir_blob_owner_state").await,
        0
    );
    assert_eq!(a.metrics().lease_lost, 1);
    let mut tx = begin(&f).await;
    tx.execute("UPDATE erp_job SET state='ready',lease_owner=NULL,lease_until=NULL,run_after=clock_timestamp()",&[]).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    b.tick().await.unwrap();
    assert_eq!(
        ownership::health(f.h.orm(), "main").await.unwrap(),
        Health::Healthy
    );
    assert_eq!(scalar(&f.h,"SELECT count(*) FROM erp_job WHERE kind='base.owner_test' AND state='ready' AND run_after>clock_timestamp()").await,1);
    let mut tx = begin(&f).await;
    tx.execute("UPDATE erp_job SET run_after=clock_timestamp()", &[])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    b.tick().await.unwrap();
    let mut tx = begin(&f).await;
    let ns: String = tx
        .query(
            "SELECT namespace::text FROM ir_blob_store WHERE store_key='main'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    tx.rollback().await.unwrap();
    drop(tx);
    let owner = f
        .store
        .owner(&erp_blob::Namespace::parse(&ns).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(owner.marker().generation(), 2);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn successful_old_reply_cannot_clear_a_committed_contest() {
    let f = fixture(external_limits()).await;
    let p = prepared(&f, Purpose::Heartbeat).await;
    let success = p.execute().await;
    let mut tx = begin(&f).await;
    tx.execute(
        "UPDATE ir_blob_owner_state SET mode='contest_pending',failure_code='owner_conflict'",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert!(matches!(
        settle(&f, &p, success).await,
        Settlement::ContestPending
    ));
    assert_eq!(scalar(&f.h,"SELECT count(*) FROM ir_blob_owner_state WHERE mode='contest_pending' AND last_success_at IS NULL").await,1);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn copied_confirmed_identity_diverges_and_contests() {
    use std::sync::Arc;
    let f = fixture(external_limits()).await;
    advance(&f, Purpose::Heartbeat).await;
    let mut tx = begin(&f).await;
    // Two durable coordinator state rows simulate the same-physical-identity snapshot case.
    // This is not a claim to have started two actual VM/database clones.
    tx.execute("INSERT INTO ir_blob_store(store_key,namespace,fingerprint) SELECT 'twin',namespace,fingerprint FROM ir_blob_store WHERE store_key='main'",&[]).await.unwrap();
    tx.execute("INSERT INTO ir_blob_owner_state(store_id,namespace,fingerprint,mode,confirmed_marker,confirmed_etag,confirmed_version,last_success_at,probe_protocol) SELECT s.id,o.namespace,o.fingerprint,o.mode,o.confirmed_marker,o.confirmed_etag,o.confirmed_version,o.last_success_at,o.probe_protocol FROM ir_blob_owner_state o JOIN ir_blob_store a ON a.id=o.store_id JOIN ir_blob_store s ON s.store_key='twin' WHERE a.store_key='main'",&[]).await.unwrap();
    tx.commit().await.unwrap();
    let twin_rt = Arc::new(erp_blob::AttachmentRuntime::single(
        "twin",
        f.store.clone(),
        external_limits(),
    ));
    let twin =
        f.h.orm()
            .rebuild(f.h.registry().clone())
            .generation_attachment(erp_orm::GenerationAttachment::new(
                f.h.registry().clone(),
                twin_rt,
            ))
            .build()
            .unwrap();
    let a = prepared(&f, Purpose::Heartbeat).await;
    let mut tx = twin
        .begin_opts(BeginOptions::new(PrincipalHint::system()).isolation(Isolation::ReadCommitted))
        .await
        .unwrap();
    let b = ownership::prepare(&mut tx, "twin", Purpose::Heartbeat)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(a.marker().identity(), b.marker().identity());
    assert_ne!(a.marker().operation(), b.marker().operation());
    assert!(matches!(
        settle(&f, &a, a.execute().await).await,
        Settlement::Confirmed { .. }
    ));
    assert!(matches!(
        settle(&f, &b, b.execute().await).await,
        Settlement::ContestPending
    ));
    let mut tx = begin(&f).await;
    ownership::settle_contest(&mut tx, &b, b.publish_contest().await)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        ownership::health(f.h.orm(), "main").await.unwrap(),
        Health::Unavailable
    );
    assert_eq!(
        ownership::health(&twin, "twin").await.unwrap(),
        Health::Unavailable
    );
    drop(twin);
    f.h.cleanup().await.unwrap();
}

/// Store-side conditional primitives are explicitly absent despite advertised flags.
struct WithoutFence(erp_blob::MemoryStore);
impl AttachmentStore for WithoutFence {
    fn kind(&self) -> erp_blob::StoreKind {
        self.0.kind()
    }
    fn capabilities(&self) -> erp_blob::Capabilities {
        self.0.capabilities()
    }
    fn fingerprint(&self) -> Option<String> {
        self.0.fingerprint()
    }
    fn verify_fingerprint<'a>(
        &'a self,
        expected: &'a str,
    ) -> erp_sdk::BoxFuture<'a, Result<(), BlobError>> {
        self.0.verify_fingerprint(expected)
    }
    fn stage<'a>(
        &'a self,
        key: &'a erp_blob::StagingKey,
        body: erp_blob::BodyReader<'a>,
        limits: erp_blob::StageLimits,
    ) -> erp_sdk::BoxFuture<'a, Result<erp_blob::StagedBlob, BlobError>> {
        self.0.stage(key, body, limits)
    }
    fn promote<'a>(
        &'a self,
        staged: &'a erp_blob::StagedBlob,
        key: &'a erp_blob::ContentKey,
    ) -> erp_sdk::BoxFuture<'a, Result<(), BlobError>> {
        self.0.promote(staged, key)
    }
    fn get<'a>(
        &'a self,
        key: &'a erp_blob::ContentKey,
        range: Option<erp_blob::ByteRange>,
    ) -> erp_sdk::BoxFuture<'a, Result<erp_blob::BlobRead, BlobError>> {
        self.0.get(key, range)
    }
    fn head<'a>(
        &'a self,
        key: &'a erp_blob::ContentKey,
    ) -> erp_sdk::BoxFuture<'a, Result<erp_blob::BlobMeta, BlobError>> {
        self.0.head(key)
    }
    fn delete_many<'a>(
        &'a self,
        keys: &'a [erp_blob::ContentKey],
    ) -> erp_sdk::BoxFuture<'a, Result<erp_blob::DeleteReport, BlobError>> {
        self.0.delete_many(keys)
    }
    fn delete_staged<'a>(
        &'a self,
        key: &'a erp_blob::StagingKey,
    ) -> erp_sdk::BoxFuture<'a, Result<(), BlobError>> {
        self.0.delete_staged(key)
    }
    fn list<'a>(
        &'a self,
        prefix: &'a str,
        after: Option<&'a str>,
        limit: usize,
    ) -> erp_sdk::BoxFuture<'a, Result<Vec<String>, BlobError>> {
        self.0.list(prefix, after, limit)
    }
}

#[tokio::test]
async fn missing_conditional_capability_remains_explicitly_unsupported() {
    use std::sync::Arc;
    let rt = Arc::new(erp_blob::AttachmentRuntime::single(
        "main",
        Arc::new(WithoutFence(erp_blob::MemoryStore::new())),
        external_limits(),
    ));
    let h = erp_testkit::Harness::builder([
        &erp_test_models::base_users::ADDON,
        &erp_addon_base::ADDON,
    ])
    .attach(rt)
    .build()
    .await
    .unwrap();
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::new(PrincipalHint::system()).isolation(Isolation::ReadCommitted))
        .await
        .unwrap();
    let p = ownership::prepare(&mut tx, "main", Purpose::Heartbeat)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::new(PrincipalHint::system()).isolation(Isolation::ReadCommitted))
        .await
        .unwrap();
    let outcome = ownership::settle(&mut tx, &p, p.execute().await)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let Settlement::Retry(error) = outcome else {
        panic!("expected capability refusal")
    };
    assert!(matches!(error.as_ref(), BlobError::UnsupportedOwnerFence));
    assert_eq!(
        error.to_string(),
        "attachment owner fencing is unsupported by this store; local locking requires a later release"
    );
    assert_eq!(
        ownership::health(h.orm(), "main").await.unwrap(),
        Health::Unsupported
    );
    assert_eq!(
        ownership::observation(h.orm(), "main").await.unwrap(),
        ownership::Observation::Unavailable
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn independent_pools_share_the_coordinator_lock_and_recover_one_intent() {
    use std::{sync::Arc, time::Duration};
    let f = fixture(external_limits()).await;
    let b = erp_orm::Orm::builder(
        f.h.registry().clone(),
        erp_orm::pool::create_pool(f.h.db().config(), 4).unwrap(),
    )
    .policy(erp_orm::StaticPolicy::AllowAll)
    .generation_attachment(erp_orm::GenerationAttachment::new(
        f.h.registry().clone(),
        Arc::clone(&f.rt),
    ))
    .build()
    .unwrap();
    let mut a = begin(&f).await;
    let first = ownership::prepare(&mut a, "main", Purpose::Heartbeat)
        .await
        .unwrap();
    let mut bt = b
        .begin_opts(BeginOptions::new(PrincipalHint::system()).isolation(Isolation::ReadCommitted))
        .await
        .unwrap();
    let pid: i32 = bt.query("SELECT pg_backend_pid()", &[]).await.unwrap()[0].get(0);
    let next = tokio::spawn(async move {
        let p = ownership::prepare(&mut bt, "main", Purpose::Heartbeat)
            .await
            .unwrap();
        bt.commit().await.unwrap();
        p
    });
    erp_testkit::wait_until_blocked(&f.h, pid, Duration::from_secs(30))
        .await
        .unwrap();
    assert!(!next.is_finished());
    a.commit().await.unwrap();
    drop(a);
    let second = next.await.unwrap();
    assert_eq!(first.marker(), second.marker());
    assert!(matches!(
        settle(&f, &second, second.execute().await).await,
        Settlement::Confirmed { .. }
    ));
    drop(b);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn local_store_recovery_preserves_its_opaque_publication_version() {
    use std::sync::Arc;
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(erp_blob::LocalStore::open(directory.path()).await.unwrap());
    let rt = Arc::new(erp_blob::AttachmentRuntime::single(
        "main",
        store.clone(),
        external_limits(),
    ));
    let h = erp_testkit::Harness::builder([
        &erp_test_models::base_users::ADDON,
        &erp_addon_base::ADDON,
    ])
    .attach(rt)
    .build()
    .await
    .unwrap();
    let options = || BeginOptions::new(PrincipalHint::system()).isolation(Isolation::ReadCommitted);
    let mut tx = h.orm().begin_opts(options()).await.unwrap();
    let p = ownership::prepare(&mut tx, "main", Purpose::Heartbeat)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let attempt = p.execute().await;
    let written = store.owner(p.namespace()).await.unwrap().unwrap();
    assert_eq!(written.marker(), p.marker());
    let mut tx = h.orm().begin_opts(options()).await.unwrap();
    assert!(matches!(
        ownership::settle(&mut tx, &p, attempt).await.unwrap(),
        Settlement::Confirmed { recovered: false }
    ));
    tx.rollback().await.unwrap();
    drop(tx);
    let mut tx = h.orm().begin_opts(options()).await.unwrap();
    let recovered = ownership::prepare(&mut tx, "main", Purpose::Heartbeat)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(recovered.marker(), p.marker());
    let attempt = recovered.execute().await;
    let mut tx = h.orm().begin_opts(options()).await.unwrap();
    assert!(matches!(
        ownership::settle(&mut tx, &recovered, attempt)
            .await
            .unwrap(),
        Settlement::Confirmed { recovered: true }
    ));
    let rows = tx
        .query(
            "SELECT confirmed_etag,confirmed_version FROM ir_blob_owner_state",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows[0].get::<_, String>(0), written.version().etag());
    assert_eq!(
        rows[0].get::<_, Option<String>>(1).as_deref(),
        written.version().object_version()
    );
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(
        ownership::health(h.orm(), "main").await.unwrap(),
        Health::Healthy
    );
    h.cleanup().await.unwrap();
}

/// The observation length the coordinator binds is the one the table's CHECK enforces: DDL
/// cannot bind a parameter, so the literal is pinned to [`ownership::OBSERVATION_SECS`] here.
#[test]
fn observation_check_in_the_ddl_matches_the_constant() {
    const DDL: &str = include_str!("../../../addons/base/src/attachments/ownership.sql");
    let check = format!(
        "CHECK (observation_not_before >= observation_started_at + interval '{} seconds')",
        ownership::OBSERVATION_SECS
    );
    assert_eq!(DDL.matches(check.as_str()).count(), 1, "{check}");
    assert_eq!(ownership::FRESH_SECS, 2 * ownership::HEARTBEAT_SECS);
}
