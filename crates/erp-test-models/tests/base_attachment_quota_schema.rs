// SPDX-License-Identifier: MIT OR Apache-2.0
//! Upgrade real pre-quota tables before repeating core schema synchronization.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
use erp_orm::CorePhase;
use erp_test_models::base_users;
use erp_testkit::Harness;

#[tokio::test]
async fn existing_schema_is_backfilled_once_and_later_deltas_survive_resync() {
    let old = include_str!("fixtures/attachment_pre_quota.sql");
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .before_schema(format!("{old}\nINSERT INTO ir_blob(sha256,size) VALUES (decode(repeat('11',32),'hex'),17);\nINSERT INTO ir_blob_upload(subject_user, expires_at, state, size) VALUES (7,now()+interval '1 day',0,NULL),(7,now()+interval '1 day',1,13),(7,now()-interval '1 day',2,19);"))
        .build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let usage = |row: &erp_sdk::Row| row.get::<_, i64>(0);
    let sql = "SELECT ((SELECT bytes FROM ir_blob_usage_base WHERE id=1)+COALESCE((SELECT sum(bytes) FROM ir_blob_usage_delta),0))::int8";
    let expected = 17 + 2_147_483_647i64 + 13 + 19;
    assert_eq!(usage(&tx.query(sql, &[]).await.unwrap()[0]), expected);
    tx.execute(
        "INSERT INTO ir_blob(sha256,size) VALUES (decode(repeat('22',32),'hex'),23)",
        &[],
    )
    .await
    .unwrap();
    erp_orm::core_ddl_run(&mut tx, CorePhase::BeforeModels)
        .await
        .unwrap();
    erp_orm::core_ddl_run(&mut tx, CorePhase::BeforeModels)
        .await
        .unwrap();
    assert_eq!(usage(&tx.query(sql, &[]).await.unwrap()[0]), expected + 23);
    let bad = tx
        .savepoint(async |tx| {
            tx.execute("UPDATE ir_blob_upload SET reserved_bytes=-1", &[])
                .await?;
            Ok(())
        })
        .await
        .unwrap_err();
    assert_eq!(bad.sqlstate(), Some("23514"));
    assert!(bad.message().contains("reserved"), "{bad}");
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

mod attachment_support;
use attachment_support::*;

#[tokio::test]
async fn referenced_bytes_survive_failed_and_successful_quota_upgrade() {
    let f = fixture(limits()).await;
    let payload = b"pre-quota referenced bytes";
    let mut tx = f.h.begin().await.unwrap();
    let ids = erp_addon_base::attachments::create(
        &mut tx,
        vec![bytes_attachment("legacy.txt", payload, None)],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = f.h.begin().await.unwrap();
    tx.batch("DROP TRIGGER ir_blob_usage_insert ON ir_blob; DROP TRIGGER ir_blob_usage_delete ON ir_blob; DROP TRIGGER ir_blob_upload_usage_insert ON ir_blob_upload; DROP TRIGGER ir_blob_upload_usage_update ON ir_blob_upload; DROP TRIGGER ir_blob_upload_usage_delete ON ir_blob_upload; DROP TABLE ir_blob_usage_delta,ir_blob_usage_base; ALTER TABLE ir_blob_upload DROP COLUMN reserved_bytes, DROP COLUMN cleanup_error").await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = f.h.begin().await.unwrap();
    erp_orm::core_ddl_run(&mut tx, CorePhase::BeforeModels)
        .await
        .unwrap();
    assert_eq!(
        count(
            &mut tx,
            "SELECT bytes::int8 FROM ir_blob_usage_base WHERE id=1"
        )
        .await,
        payload.len() as i64
    );
    tx.rollback().await.unwrap();
    drop(tx);
    let mut tx = f.h.begin().await.unwrap();
    let absent: bool = tx
        .query(
            "SELECT to_regclass('public.ir_blob_usage_base') IS NULL",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert!(
        absent,
        "an aborted upgrade must not publish its accounting schema"
    );
    assert_eq!(
        erp_addon_base::attachments::open(&mut tx, ids[0].get())
            .await
            .unwrap()
            .read_to_vec(100)
            .await
            .unwrap(),
        payload
    );
    tx.rollback().await.unwrap();
    drop(tx);
    let mut tx = f.h.begin().await.unwrap();
    erp_orm::core_ddl_run(&mut tx, CorePhase::BeforeModels)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = f.h.begin().await.unwrap();
    assert_eq!(
        erp_addon_base::attachments::open(&mut tx, ids[0].get())
            .await
            .unwrap()
            .read_to_vec(100)
            .await
            .unwrap(),
        payload
    );
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}
