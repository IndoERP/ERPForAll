// SPDX-License-Identifier: MIT OR Apache-2.0
//! Destructive cleanup uses current store ownership, never a cached readiness result.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
mod attachment_support;
use attachment_support::*;
use erp_addon_base::attachments::{
    self, AttachmentContent, NewAttachment, gc,
    ownership::{self, Purpose, Settlement},
};
use erp_blob::{AttachmentStore, Op, When};
use erp_orm::{BeginOptions, Isolation};
use erp_sdk::{ErrorKind, Transaction};

async fn rc(f: &Fixture) -> Transaction {
    f.h.orm()
        .begin_opts(BeginOptions::new(f.h.principal()).isolation(Isolation::ReadCommitted))
        .await
        .unwrap()
}
async fn observe(f: &Fixture) {
    let mut tx = rc(f).await;
    let p = ownership::prepare(&mut tx, "main", Purpose::Observe)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let attempt = p.execute().await;
    let mut tx = rc(f).await;
    assert!(matches!(
        ownership::settle(&mut tx, &p, attempt).await.unwrap(),
        Settlement::Confirmed { .. }
    ));
    tx.commit().await.unwrap();
}
async fn mature(f: &Fixture) {
    let mut tx = rc(f).await;
    tx.execute("UPDATE ir_blob_owner_state SET observation_started_at=clock_timestamp()-interval '700 seconds',observation_not_before=clock_timestamp()-interval '1 second'", &[]).await.unwrap();
    tx.commit().await.unwrap();
}
async fn garbage(f: &Fixture) -> i64 {
    let mut tx = f.h.begin().await.unwrap();
    let env = tx.env();
    tx.rollback().await.unwrap();
    drop(tx);
    let up = stage(f, &env, "fence.txt", b"external cleanup fixture").await;
    let mut tx = f.h.begin().await.unwrap();
    let ids = attachments::create(
        &mut tx,
        vec![NewAttachment::new(
            "fence.txt",
            AttachmentContent::Upload(up.id),
        )],
    )
    .await
    .unwrap();
    let id = ids[0].get();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = f.h.begin().await.unwrap();
    let blob: i64 = tx
        .query("SELECT blob_id FROM ir_attachment WHERE id=$1", &[&id])
        .await
        .unwrap()[0]
        .get(0);
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
    let mut tx = rc(f).await;
    gc::mark_unreferenced(&mut tx, 100).await.unwrap();
    tx.commit().await.unwrap();
    blob
}
fn refused(e: &erp_sdk::Error, reason: &str) {
    assert_eq!(e.kind(), ErrorKind::Internal);
    assert_eq!(
        e.to_string(),
        format!("The attachment store failed: Attachment cleanup refused: {reason}.")
    );
}

#[tokio::test]
async fn both_sweep_transactions_require_owner_admission() {
    let f = fixture(external_limits()).await;
    let blob = garbage(&f).await;
    let mut tx = rc(&f).await;
    // A refused store is skipped by the mark; the strict remove names the reason.
    assert!(
        gc::sweep_mark(&mut tx, 0, 100)
            .await
            .unwrap()
            .ids
            .is_empty()
    );
    let env = tx.env();
    refused(
        &gc::sweep_remove(&mut tx, &env, &[blob], 0)
            .await
            .unwrap_err(),
        "ownership is not admitted",
    );
    tx.rollback().await.unwrap();
    drop(tx);
    let mut tx = rc(&f).await;
    let env = tx.env();
    refused(
        &gc::sweep_remove(&mut tx, &env, &[blob], 0)
            .await
            .unwrap_err(),
        "ownership is not admitted",
    );
    tx.rollback().await.unwrap();
    drop(tx);
    assert_eq!(f.store.content_keys().len(), 1);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn observation_and_current_contest_are_required_in_each_phase() {
    let f = fixture(external_limits()).await;
    let blob = garbage(&f).await;
    observe(&f).await;
    let mut tx = rc(&f).await;
    // A refused store is skipped by the mark; the strict remove names the reason.
    assert!(
        gc::sweep_mark(&mut tx, 0, 100)
            .await
            .unwrap()
            .ids
            .is_empty()
    );
    let env = tx.env();
    refused(
        &gc::sweep_remove(&mut tx, &env, &[blob], 0)
            .await
            .unwrap_err(),
        "observation is incomplete",
    );
    tx.rollback().await.unwrap();
    drop(tx);
    mature(&f).await;
    let mut tx = rc(&f).await;
    let ids = gc::sweep_mark(&mut tx, 0, 100).await.unwrap().ids;
    assert_eq!(ids, vec![blob]);
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = rc(&f).await;
    let p = ownership::prepare(&mut tx, "main", Purpose::Heartbeat)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    drop(tx);
    f.store
        .create_contested(p.namespace(), p.marker())
        .await
        .unwrap();
    let mut tx = rc(&f).await;
    let env = tx.env();
    refused(
        &gc::sweep_remove(&mut tx, &env, &ids, 0).await.unwrap_err(),
        "namespace is contested",
    );
    tx.rollback().await.unwrap();
    drop(tx);
    assert_eq!(f.store.content_keys().len(), 1);
    assert_eq!(
        scalar(
            &f.h,
            "SELECT count(*) FROM ir_blob WHERE sweep_attempted_at IS NOT NULL"
        )
        .await,
        1
    );
    f.h.cleanup().await.unwrap();
}

/// check gives; in-memory store)
#[tokio::test]
async fn cleanup_reports_each_refused_store_with_its_reason() {
    let f = fixture(external_limits()).await;
    let blob = garbage(&f).await;
    let mut tx = f.h.begin().await.unwrap();
    let env = tx.env();
    tx.rollback().await.unwrap();
    drop(tx);
    let _up = stage(&f, &env, "expired.txt", b"expired staging bytes").await;
    let mut tx = rc(&f).await;
    tx.execute("UPDATE ir_blob_upload SET state=3 WHERE state<>2", &[])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let store_id = i16::try_from(
        scalar(
            &f.h,
            "SELECT id::int8 FROM ir_blob_store WHERE store_key='main'",
        )
        .await,
    )
    .unwrap();
    let mut tx = rc(&f).await;
    let mark = gc::sweep_mark(&mut tx, 0, 100).await.unwrap();
    assert!(mark.ids.is_empty());
    let marked: Vec<i16> = mark.refused.iter().map(|r| r.store_id).collect();
    assert_eq!(marked, vec![store_id]);
    // The reported reason is the one the strict check of the same store gives.
    refused(
        &gc::check_cleanup(&mut tx, "main").await.unwrap_err(),
        mark.refused[0].reason,
    );
    let env = tx.env();
    let held = count(
        &mut tx,
        "SELECT count(*) FROM ir_blob_upload WHERE inline_data IS NULL",
    )
    .await;
    let report = gc::clean_staging(&mut tx, &env, 100).await.unwrap();
    assert_eq!(
        (report.consumed, report.expired, report.skipped),
        (0, 0, u64::try_from(held).unwrap())
    );
    let cleaned: Vec<(i16, &str)> = report
        .refused
        .iter()
        .map(|r| (r.store_id, r.reason))
        .collect();
    assert_eq!(cleaned, vec![(store_id, mark.refused[0].reason)]);
    assert!(!mark.held && !report.held);
    tx.rollback().await.unwrap();
    drop(tx);
    // Under an active hold neither call checks a store: they say so instead of an empty list.
    let mut tx = rc(&f).await;
    gc::hold(&mut tx, 60, "refusal test").await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = rc(&f).await;
    let mark = gc::sweep_mark(&mut tx, 0, 100).await.unwrap();
    assert!(mark.held && mark.ids.is_empty() && mark.refused.is_empty());
    let env = tx.env();
    let report = gc::clean_staging(&mut tx, &env, 100).await.unwrap();
    assert!(report.held && report.refused.is_empty());
    tx.rollback().await.unwrap();
    drop(tx);
    assert_eq!(f.store.content_keys().len(), 1);
    assert_eq!(
        scalar(
            &f.h,
            &format!("SELECT count(*) FROM ir_blob WHERE id={blob} AND sweep_attempted_at IS NULL")
        )
        .await,
        1
    );
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn staged_cleanup_is_fenced_and_failed_delete_retains_tracking() {
    let f = fixture(external_limits()).await;
    let mut tx = f.h.begin().await.unwrap();
    let env = tx.env();
    tx.rollback().await.unwrap();
    drop(tx);
    let _up = stage(&f, &env, "staged.txt", b"staging bytes retained").await;
    let mut tx = rc(&f).await;
    tx.execute("UPDATE ir_blob_upload SET state=3", &[])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = rc(&f).await;
    let env = tx.env();
    let report = gc::clean_staging(&mut tx, &env, 100).await.unwrap();
    assert_eq!((report.expired, report.skipped), (0, 1));
    refused(
        &gc::check_cleanup(&mut tx, "main").await.unwrap_err(),
        "ownership is not admitted",
    );
    tx.rollback().await.unwrap();
    drop(tx);
    observe(&f).await;
    mature(&f).await;
    f.store.faults().inject(Op::DeleteStaged, When::After, 0, 1);
    let mut tx = rc(&f).await;
    let env = tx.env();
    let e = gc::clean_staging(&mut tx, &env, 100).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Internal);
    assert_eq!(
        e.message(),
        "The attachment store failed: the attachment store is unavailable (retryable): injected fault in DeleteStaged (After its effect)"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    assert_eq!(
        scalar(
            &f.h,
            "SELECT count(*) FROM ir_blob_upload WHERE reserved_bytes>0"
        )
        .await,
        1
    );
    assert!(f.store.staging_keys().is_empty());
    let mut tx = rc(&f).await;
    let env = tx.env();
    assert_eq!(
        gc::clean_staging(&mut tx, &env, 100).await.unwrap().expired,
        1
    );
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(scalar(&f.h, "SELECT count(*) FROM ir_blob_upload").await, 0);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn stale_health_and_changed_store_version_refuse_removal() {
    let f = fixture(external_limits()).await;
    let blob = garbage(&f).await;
    observe(&f).await;
    mature(&f).await;
    let mut tx = rc(&f).await;
    tx.execute(
        "UPDATE ir_blob_owner_state SET last_success_at=clock_timestamp()-interval '601 seconds'",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = rc(&f).await;
    // A refused store is skipped by the mark; the strict remove names the reason.
    assert!(
        gc::sweep_mark(&mut tx, 0, 100)
            .await
            .unwrap()
            .ids
            .is_empty()
    );
    let env = tx.env();
    refused(
        &gc::sweep_remove(&mut tx, &env, &[blob], 0)
            .await
            .unwrap_err(),
        "ownership is not admitted",
    );
    tx.rollback().await.unwrap();
    drop(tx);
    let mut tx = rc(&f).await;
    tx.execute(
        "UPDATE ir_blob_owner_state SET last_success_at=clock_timestamp()",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = rc(&f).await;
    let ids = gc::sweep_mark(&mut tx, 0, 100).await.unwrap().ids;
    assert_eq!(ids, vec![blob]);
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = rc(&f).await;
    let p = ownership::prepare(&mut tx, "main", Purpose::Heartbeat)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    drop(tx);
    let current = f.store.owner(p.namespace()).await.unwrap().unwrap();
    f.store
        .put_owner(p.namespace(), p.marker(), Some(current.version()))
        .await
        .unwrap();
    let mut tx = rc(&f).await;
    let env = tx.env();
    let e = gc::sweep_remove(&mut tx, &env, &ids, 0).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Internal);
    assert_eq!(
        e.to_string(),
        "The attachment store failed: the attachment owner marker changed or already exists"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    assert_eq!(f.store.content_keys().len(), 1);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn live_upload_is_not_removed_with_a_failed_neighbour() {
    let f = fixture(external_limits()).await;
    let mut tx = f.h.begin().await.unwrap();
    let env = tx.env();
    tx.rollback().await.unwrap();
    drop(tx);
    let failed = stage(&f, &env, "failed.txt", b"failed neighbour bytes").await;
    let _live = stage(&f, &env, "live.txt", b"live neighbour bytes").await;
    let mut tx = rc(&f).await;
    tx.execute(
        "UPDATE ir_blob_upload SET state=3 WHERE id=$1::text::uuid",
        &[&failed.id.as_str()],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    observe(&f).await;
    mature(&f).await;
    let mut tx = rc(&f).await;
    let env = tx.env();
    assert_eq!(
        gc::clean_staging(&mut tx, &env, 100).await.unwrap().expired,
        1
    );
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(f.store.staging_keys().len(), 1);
    assert_eq!(
        scalar(
            &f.h,
            "SELECT count(*) FROM ir_blob_upload WHERE state=1 AND reserved_bytes=size"
        )
        .await,
        1
    );
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn consumed_external_staging_retains_bytes_but_frees_the_pending_slot() {
    let f = fixture(external_limits().with_pending_uploads_for_tests(1)).await;
    let mut tx = f.h.begin().await.unwrap();
    let env = tx.env();
    tx.rollback().await.unwrap();
    drop(tx);
    let up = stage(&f, &env, "used.txt", b"consumed staging occupancy").await;
    let mut tx = f.h.begin().await.unwrap();
    attachments::create(
        &mut tx,
        vec![NewAttachment::new(
            "used.txt",
            AttachmentContent::Upload(up.id),
        )],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(f.store.staging_keys().len(), 1);
    assert_eq!(scalar(&f.h,"SELECT count(*) FROM ir_blob_upload WHERE state=2 AND reserved_bytes=size AND reserved_bytes>0").await,1);
    let _next = stage(&f, &env, "next.txt", b"next staging occupancy").await;
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn inline_consumption_releases_only_its_staging_reservation() {
    let f = fixture(limits()).await;
    let mut tx = f.h.begin().await.unwrap();
    let env = tx.env();
    tx.rollback().await.unwrap();
    drop(tx);
    let up = stage(&f, &env, "small.txt", b"abc").await;
    let mut tx = f.h.begin().await.unwrap();
    attachments::create(
        &mut tx,
        vec![NewAttachment::new(
            "small.txt",
            AttachmentContent::Upload(up.id.clone()),
        )],
    )
    .await
    .unwrap();
    assert_eq!(
        count(
            &mut tx,
            "SELECT reserved_bytes FROM ir_blob_upload WHERE state=2"
        )
        .await,
        0
    );
    tx.rollback().await.unwrap();
    drop(tx);
    assert_eq!(
        scalar(
            &f.h,
            "SELECT reserved_bytes FROM ir_blob_upload WHERE state=1"
        )
        .await,
        3
    );
    let mut tx = f.h.begin().await.unwrap();
    attachments::create(
        &mut tx,
        vec![NewAttachment::new(
            "small.txt",
            AttachmentContent::Upload(up.id),
        )],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(
        scalar(
            &f.h,
            "SELECT reserved_bytes FROM ir_blob_upload WHERE state=2"
        )
        .await,
        0
    );
    assert_eq!(scalar(&f.h,"SELECT ((SELECT bytes FROM ir_blob_usage_base WHERE id=1)+coalesce((SELECT sum(bytes) FROM ir_blob_usage_delta),0))::int8").await,3);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn changed_identity_and_borrowed_metadata_fail_closed() {
    let f = fixture(external_limits()).await;
    let blob = garbage(&f).await;
    observe(&f).await;
    mature(&f).await;
    let mut tx = rc(&f).await;
    gc::check_cleanup(&mut tx, "main").await.unwrap();
    tx.execute("UPDATE ir_blob_store SET role='borrowed'", &[])
        .await
        .unwrap();
    // A refused store is skipped by the mark; the strict remove names the reason.
    assert!(
        gc::sweep_mark(&mut tx, 0, 100)
            .await
            .unwrap()
            .ids
            .is_empty()
    );
    let env = tx.env();
    refused(
        &gc::sweep_remove(&mut tx, &env, &[blob], 0)
            .await
            .unwrap_err(),
        "borrowed namespaces cannot be removed",
    );
    tx.rollback().await.unwrap();
    drop(tx);
    let mut tx = rc(&f).await;
    tx.execute(
        "UPDATE ir_blob_owner_identity SET incarnation=gen_random_uuid()",
        &[],
    )
    .await
    .unwrap();
    // A refused store is skipped by the mark; the strict remove names the reason.
    assert!(
        gc::sweep_mark(&mut tx, 0, 100)
            .await
            .unwrap()
            .ids
            .is_empty()
    );
    let env = tx.env();
    refused(
        &gc::sweep_remove(&mut tx, &env, &[blob], 0)
            .await
            .unwrap_err(),
        "database identity changed",
    );
    tx.rollback().await.unwrap();
    drop(tx);
    assert_eq!(f.store.content_keys().len(), 1);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn cleanup_requires_real_maintenance_sql_rights() {
    let f = fixture(external_limits()).await;
    let blob = garbage(&f).await;
    observe(&f).await;
    mature(&f).await;
    let readonly = f.h.read_only_role().await.unwrap();
    let runtime_role = erp_sql::quote(readonly.get_user().unwrap());
    let mut tx = rc(&f).await;
    tx.batch(&format!("SET LOCAL ROLE {runtime_role}"))
        .await
        .unwrap();
    let e = gc::check_cleanup(&mut tx, "main").await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Database);
    assert_eq!(e.sqlstate(), Some("42501"));
    assert_eq!(e.message(), "permission denied for table ir_blob_store");
    tx.rollback().await.unwrap();
    drop(tx);
    let maintenance = erp_sql::quote(&format!("maint_{}", f.h.db().name()));
    let owner = f.h.db().client().await.unwrap();
    owner.batch_execute(&format!("CREATE ROLE {maintenance} NOLOGIN; GRANT USAGE ON SCHEMA public TO {maintenance}; GRANT SELECT ON ALL TABLES IN SCHEMA public TO {maintenance}; GRANT UPDATE ON ir_blob_store,ir_blob_owner_state,ir_blob TO {maintenance}; GRANT DELETE ON ir_blob,ir_blob_inline TO {maintenance}; GRANT INSERT ON ir_blob_usage_delta TO {maintenance}; GRANT USAGE,SELECT ON ALL SEQUENCES IN SCHEMA public TO {maintenance}; GRANT EXECUTE ON FUNCTION pg_control_system() TO {maintenance}")).await.unwrap();
    let mut tx = rc(&f).await;
    tx.batch(&format!("SET LOCAL ROLE {maintenance}"))
        .await
        .unwrap();
    let ids = gc::sweep_mark(&mut tx, 0, 100).await.unwrap().ids;
    assert_eq!(ids, vec![blob]);
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = rc(&f).await;
    tx.batch(&format!("SET LOCAL ROLE {maintenance}"))
        .await
        .unwrap();
    let env = tx.env();
    assert_eq!(
        gc::sweep_remove(&mut tx, &env, &ids, 0).await.unwrap().keys,
        1
    );
    tx.commit().await.unwrap();
    drop(tx);
    assert!(f.store.content_keys().is_empty());
    owner
        .batch_execute(&format!(
            "DROP OWNED BY {maintenance}; DROP ROLE {maintenance}"
        ))
        .await
        .unwrap();
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn shared_local_cleanup_requires_actual_peer_evidence() {
    use erp_blob::{AttachmentConfig, AttachmentRuntime};
    use erp_test_models::base_users;
    use erp_testkit::Harness;
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let config = format!(
        "[attachments]\nwrite_store='main'\n[attachments.stores.main]\nkind='local'\npath='{}'\nshared_filesystem=true\n",
        dir.path().display()
    );
    let rt = Arc::new(
        AttachmentRuntime::open(AttachmentConfig::from_toml(&config).unwrap())
            .await
            .unwrap(),
    );
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .attach(rt)
        .build()
        .await
        .unwrap();
    admit_cleanup(h.orm()).await;
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::new(h.principal()).isolation(Isolation::ReadCommitted))
        .await
        .unwrap();
    let e = gc::check_cleanup(&mut tx, "main").await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Internal);
    assert_eq!(
        e.message(),
        "The attachment store failed: the attachment store lacks required owner-fence capability: shared-filesystem peer evidence (a later release)"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[derive(Debug)]
struct NoConditional(std::sync::Arc<erp_blob::MemoryStore>);
impl AttachmentStore for NoConditional {
    fn kind(&self) -> erp_blob::StoreKind {
        self.0.kind()
    }
    fn capabilities(&self) -> erp_blob::Capabilities {
        let mut caps = self.0.capabilities();
        caps.if_match_put = false;
        caps
    }
    fn fingerprint(&self) -> Option<String> {
        self.0.fingerprint()
    }
    fn verify_fingerprint<'a>(
        &'a self,
        fp: &'a str,
    ) -> erp_blob::BoxFuture<'a, Result<(), erp_blob::BlobError>> {
        self.0.verify_fingerprint(fp)
    }
    fn stage<'a>(
        &'a self,
        k: &'a erp_blob::StagingKey,
        b: erp_blob::BodyReader<'a>,
        l: erp_blob::StageLimits,
    ) -> erp_blob::BoxFuture<'a, Result<erp_blob::StagedBlob, erp_blob::BlobError>> {
        self.0.stage(k, b, l)
    }
    fn promote<'a>(
        &'a self,
        s: &'a erp_blob::StagedBlob,
        k: &'a erp_blob::ContentKey,
    ) -> erp_blob::BoxFuture<'a, Result<(), erp_blob::BlobError>> {
        self.0.promote(s, k)
    }
    fn get<'a>(
        &'a self,
        k: &'a erp_blob::ContentKey,
        r: Option<erp_blob::ByteRange>,
    ) -> erp_blob::BoxFuture<'a, Result<erp_blob::BlobRead, erp_blob::BlobError>> {
        self.0.get(k, r)
    }
    fn head<'a>(
        &'a self,
        k: &'a erp_blob::ContentKey,
    ) -> erp_blob::BoxFuture<'a, Result<erp_blob::BlobMeta, erp_blob::BlobError>> {
        self.0.head(k)
    }
    fn delete_many<'a>(
        &'a self,
        k: &'a [erp_blob::ContentKey],
    ) -> erp_blob::BoxFuture<'a, Result<erp_blob::DeleteReport, erp_blob::BlobError>> {
        self.0.delete_many(k)
    }
    fn delete_staged<'a>(
        &'a self,
        k: &'a erp_blob::StagingKey,
    ) -> erp_blob::BoxFuture<'a, Result<(), erp_blob::BlobError>> {
        self.0.delete_staged(k)
    }
    fn list<'a>(
        &'a self,
        p: &'a str,
        a: Option<&'a str>,
        l: usize,
    ) -> erp_blob::BoxFuture<'a, Result<Vec<String>, erp_blob::BlobError>> {
        self.0.list(p, a, l)
    }
}

#[tokio::test]
async fn missing_conditional_capability_refuses_actual_cleanup() {
    use std::sync::Arc;
    let f = fixture(external_limits()).await;
    let blob = garbage(&f).await;
    observe(&f).await;
    mature(&f).await;
    let rt = Arc::new(erp_blob::AttachmentRuntime::single(
        "main",
        Arc::new(NoConditional(Arc::clone(&f.store))),
        external_limits(),
    ));
    let orm =
        f.h.orm()
            .rebuild(Arc::clone(f.h.registry()))
            .generation_attachment(erp_orm::GenerationAttachment::new(
                Arc::clone(f.h.registry()),
                rt,
            ))
            .build()
            .unwrap();
    let mut tx = orm
        .begin_opts(BeginOptions::new(f.h.principal()).isolation(Isolation::ReadCommitted))
        .await
        .unwrap();
    let env = tx.env();
    let e = gc::sweep_remove(&mut tx, &env, &[blob], 0)
        .await
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Internal);
    assert_eq!(
        e.message(),
        "The attachment store failed: the attachment store lacks required owner-fence capability: If-Match put"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    drop(orm);
    assert_eq!(f.store.content_keys().len(), 1);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn cleanup_batches_are_bounded_before_any_store_call() {
    let f = fixture(external_limits()).await;
    for limit in [-1, 0, 1001] {
        let mut tx = rc(&f).await;
        let e = gc::sweep_mark(&mut tx, 0, limit).await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Validation);
        assert_eq!(e.message(), "Attachment cleanup batch is 1 to 1000 rows.");
        tx.rollback().await.unwrap();
    }
    let mut tx = rc(&f).await;
    let env = tx.env();
    let e = gc::sweep_remove(&mut tx, &env, &vec![1; 1001], 0)
        .await
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Validation);
    assert_eq!(e.message(), "Attachment cleanup batch is 1 to 1000 rows.");
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn admitted_removal_cannot_skip_the_sweep_attempt_marker() {
    let f = fixture(external_limits()).await;
    let blob = garbage(&f).await;
    observe(&f).await;
    mature(&f).await;
    let mut tx = rc(&f).await;
    let env = tx.env();
    assert_eq!(
        gc::sweep_remove(&mut tx, &env, &[blob], 0)
            .await
            .unwrap()
            .rows,
        0
    );
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(f.store.content_keys().len(), 1);
    assert_eq!(
        scalar(
            &f.h,
            "SELECT count(*) FROM ir_blob WHERE sweep_attempted_at IS NULL"
        )
        .await,
        1
    );
    f.h.cleanup().await.unwrap();
}
