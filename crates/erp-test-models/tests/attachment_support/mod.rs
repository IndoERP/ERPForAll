//! Shared fixtures of the attachment store tests: a database with `base`, the in-memory store
//! with fault injection and a zero garbage-collection grace, and small helpers.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test helpers"
)]

use std::sync::Arc;

use erp_addon_base::attachments::{
    AttachmentContent, NewAttachment, ParentRef, StageRequest, StagedUpload, UNLINK_HOOK, Uploader,
};
use erp_blob::{AttachmentLimits, AttachmentRuntime, MemoryStore};
use erp_sdk::{Cell, Env, RawVals, Transaction};
use erp_test_models::base_users;
use erp_testkit::Harness;

/// The production limits with a zero grace (the sweep sees an unreferenced blob at once) and the
/// default 2 KiB inline threshold.
pub fn limits() -> AttachmentLimits {
    AttachmentLimits::default().with_gc_grace_secs_for_tests(0)
}

/// Like [`limits`] with a tiny inline threshold, so a short text goes through the store.
pub fn external_limits() -> AttachmentLimits {
    limits().with_inline_max_bytes_for_tests(4)
}

pub struct Fixture {
    pub h: Harness,
    pub store: Arc<MemoryStore>,
    pub rt: Arc<AttachmentRuntime>,
}

pub async fn fixture(limits: AttachmentLimits) -> Fixture {
    let store = Arc::new(MemoryStore::new());
    let rt = Arc::new(AttachmentRuntime::single(
        "main",
        Arc::clone(&store) as Arc<dyn erp_blob::AttachmentStore>,
        limits,
    ));
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .attach(Arc::clone(&rt))
        .unlink_hook(UNLINK_HOOK)
        .build()
        .await
        .expect("harness");
    Fixture { h, store, rt }
}

/// A partner to attach files to.
pub async fn partner(tx: &mut Transaction, name: &str) -> i64 {
    tx.env()
        .records("res.partner")
        .unwrap()
        .create(
            tx,
            vec![RawVals::new().set_cell("name", Cell::Str(name.into()))],
        )
        .await
        .unwrap()
        .ids()[0]
}

pub fn on_partner(id: i64) -> ParentRef {
    ParentRef::new("res.partner", id)
}

/// Stage `bytes` as an upload of the transaction's principal.
pub async fn stage(f: &Fixture, env: &Env, name: &str, bytes: &[u8]) -> StagedUpload {
    let mut body = bytes;
    Uploader::new(f.h.orm())
        .stage(env, &mut body, StageRequest::named(name))
        .await
        .unwrap()
}

pub fn bytes_attachment(name: &str, bytes: &[u8], parent: Option<ParentRef>) -> NewAttachment {
    let item = NewAttachment::new(name, AttachmentContent::Bytes(bytes.to_vec()));
    match parent {
        Some(p) => item.on(p),
        None => item,
    }
}

/// One number of a query on a fresh transaction that is ended again (an open transaction keeps
/// its pooled connection).
pub async fn scalar(h: &Harness, sql: &str) -> i64 {
    let mut tx = h.begin().await.unwrap();
    let n = count(&mut tx, sql).await;
    tx.rollback().await.unwrap();
    n
}

pub async fn count(tx: &mut Transaction, sql: &str) -> i64 {
    tx.query(sql, &[]).await.unwrap()[0].get(0)
}

/// `(blob_id, checksum, file_size, mimetype)` of an attachment, read by raw SQL.
pub async fn row(tx: &mut Transaction, id: i64) -> (Option<i64>, String, i32, String) {
    let rows = tx
        .query(
            "SELECT blob_id, checksum, file_size, mimetype FROM ir_attachment WHERE id = $1",
            &[&id],
        )
        .await
        .unwrap();
    let r = &rows[0];
    (r.get(0), r.get(1), r.get(2), r.get(3))
}

/// Explicit setup for tests exercising admitted deletion, separate from ordinary/negative fixtures.
/// Advance through the real coordinator; only this fixture's database timestamps are matured.
pub async fn admit_cleanup(orm: &erp_sdk::Orm) {
    use erp_addon_base::attachments::ownership::{self, Purpose, Settlement};
    use erp_sdk::{BeginOptions, Isolation, SUPERUSER_ID};
    let options = || BeginOptions::new(SUPERUSER_ID).isolation(Isolation::ReadCommitted);
    let mut tx = orm.begin_opts(options()).await.unwrap();
    let p = ownership::prepare(&mut tx, "main", Purpose::Observe)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let result = p.execute().await;
    let mut tx = orm.begin_opts(options()).await.unwrap();
    assert!(matches!(
        ownership::settle(&mut tx, &p, result).await.unwrap(),
        Settlement::Confirmed { .. }
    ));
    tx.execute("UPDATE ir_blob_owner_state SET observation_started_at=clock_timestamp()-interval '700 seconds',observation_not_before=clock_timestamp()-interval '1 second'",&[]).await.unwrap();
    tx.commit().await.unwrap();
}
