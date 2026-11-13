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
    let expected = 17 + 13 + 19;
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
            tx.execute(
                "UPDATE ir_blob_upload SET reserved_bytes=-1 WHERE state=1",
                &[],
            )
            .await?;
            Ok(())
        })
        .await
        .unwrap_err();
    assert_eq!(bad.kind(), erp_sdk::ErrorKind::Validation);
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

#[tokio::test]
async fn quota_schema_resync_does_not_recreate_capture_triggers() {
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let sql = "SELECT oid::int8 FROM pg_trigger WHERE tgname LIKE 'ir_blob%usage%' ORDER BY tgname";
    let before: Vec<i64> = tx
        .query(sql, &[])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(before.len(), 5);
    erp_orm::core_ddl_run(&mut tx, CorePhase::BeforeModels)
        .await
        .unwrap();
    let after: Vec<i64> = tx
        .query(sql, &[])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(before, after);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn legacy_unknown_size_is_quarantined_without_a_fabricated_reservation() {
    let old = include_str!("fixtures/attachment_pre_quota.sql");
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .before_schema(format!("{old}\nINSERT INTO ir_blob_upload(subject_user,expires_at) VALUES (7,now()+interval '1 day')"))
        .build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let rows = tx
        .query(
            "SELECT state,reserved_bytes,reservation_unknown FROM ir_blob_upload",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows[0].get::<_, i16>(0), 3);
    assert_eq!(rows[0].get::<_, i64>(1), 0);
    assert!(rows[0].get::<_, bool>(2));
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn old_quota_schema_corrects_consumed_and_unknown_reservations_once() {
    let old = include_str!("fixtures/attachment_pre_quota.sql");
    let quota = include_str!("fixtures/attachment_quota_v1.sql");
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .before_schema(format!("{old}\nINSERT INTO ir_blob(sha256,size) VALUES (decode(repeat('33',32),'hex'),17);\nINSERT INTO ir_blob_upload(subject_user,expires_at,state,size) VALUES (7,now(),0,NULL),(7,now(),1,13),(7,now(),2,17);\n{quota}"))
        .build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let usage = "SELECT ((SELECT bytes FROM ir_blob_usage_base WHERE id=1)+COALESCE((SELECT sum(bytes) FROM ir_blob_usage_delta),0))::int8";
    assert_eq!(count(&mut tx, usage).await, 47);
    erp_orm::core_ddl_run(&mut tx, CorePhase::BeforeModels)
        .await
        .unwrap();
    assert_eq!(count(&mut tx, usage).await, 47);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn installed_quota_capture_drift_is_refused() {
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.batch("DROP TRIGGER ir_blob_usage_insert ON ir_blob")
        .await
        .unwrap();
    let error = tx
        .batch(include_str!(
            "../../../addons/base/src/attachments/quota.sql"
        ))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Database);
    assert_eq!(error.sqlstate(), Some("P0001"));
    assert!(
        error
            .message()
            .contains("Attachment quota schema drift: capture triggers"),
        "PostgreSQL context varies: {error}"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn installed_quota_constraint_drift_is_refused() {
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let checks = tx.query("SELECT conname::text, pg_get_expr(conbin,conrelid) FROM pg_constraint WHERE conrelid='ir_blob_upload'::regclass AND contype='c' ORDER BY conname", &[]).await.unwrap();
    for row in checks {
        eprintln!(
            "quota catalog check: {} = {}",
            row.get::<_, String>(0),
            row.get::<_, String>(1)
        );
    }
    tx.batch("ALTER TABLE ir_blob_upload DROP CONSTRAINT ir_blob_upload_reserved_bytes_check; ALTER TABLE ir_blob_upload ADD CONSTRAINT ir_blob_upload_reserved_bytes_check CHECK (true)").await.unwrap();
    let error = tx
        .batch(include_str!(
            "../../../addons/base/src/attachments/quota.sql"
        ))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Database);
    assert_eq!(error.sqlstate(), Some("P0001"));
    assert!(
        error
            .message()
            .contains("Attachment quota schema drift: upload constraints"),
        "PostgreSQL context varies: {error}"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn steady_quota_validation_does_not_block_an_independent_writer() {
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .build()
        .await
        .unwrap();
    let mut validation = h.begin().await.unwrap();
    validation
        .batch(include_str!(
            "../../../addons/base/src/attachments/quota.sql"
        ))
        .await
        .unwrap();
    let mut writer = h.begin().await.unwrap();
    writer.batch("SET LOCAL lock_timeout='1s'").await.unwrap();
    writer
        .execute(
            "INSERT INTO ir_blob(sha256,size) VALUES (decode(repeat('55',32),'hex'),7)",
            &[],
        )
        .await
        .expect("catalog validation must not retain a lock that blocks content writers");
    writer.rollback().await.unwrap();
    validation.rollback().await.unwrap();
    drop(writer);
    drop(validation);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn migration_keeps_consumed_external_occupancy_until_cleanup() {
    let old = include_str!("fixtures/attachment_pre_quota.sql");
    let quota = include_str!("fixtures/attachment_quota_v1.sql");
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .before_schema(format!("{old}\nINSERT INTO ir_blob_store(store_key,fingerprint) VALUES ('old','old');\nINSERT INTO ir_blob_upload(subject_user,expires_at,state,size,store_id,inline_data) VALUES (7,now(),2,17,1,NULL),(7,now(),2,3,NULL,decode('010203','hex'));\n{quota}"))
        .build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    assert_eq!(
        count(
            &mut tx,
            "SELECT reserved_bytes FROM ir_blob_upload WHERE store_id=1"
        )
        .await,
        17
    );
    assert_eq!(
        count(
            &mut tx,
            "SELECT reserved_bytes FROM ir_blob_upload WHERE inline_data IS NOT NULL"
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &mut tx,
            "SELECT bytes::int8 FROM ir_blob_usage_base WHERE id=1"
        )
        .await,
        17
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn concurrent_quota_installers_preserve_accounting() {
    let f = fixture(limits()).await;
    let mut tx = f.h.begin().await.unwrap();
    tx.batch("ALTER TABLE ir_blob_usage_base DROP COLUMN schema_version")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let orm = f.h.orm();
    let install = async || {
        let mut tx = orm
            .begin_opts(
                erp_orm::BeginOptions::new(erp_orm::SUPERUSER_ID)
                    .isolation(erp_orm::Isolation::ReadCommitted),
            )
            .await
            .unwrap();
        tx.batch(include_str!(
            "../../../addons/base/src/attachments/quota.sql"
        ))
        .await
        .unwrap();
        tx.commit().await.unwrap();
    };
    tokio::join!(install(), install());
    let mut tx = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    assert_eq!(
        count(
            &mut tx,
            "SELECT count(*) FROM ir_blob_usage_base WHERE id=1"
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &mut tx,
            "SELECT bytes::int8 FROM ir_blob_usage_base WHERE id=1"
        )
        .await,
        0
    );
    tx.rollback().await.unwrap();
    drop(tx);
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn candidate_version_two_advances_without_losing_external_staging() {
    let old = include_str!("fixtures/attachment_pre_quota.sql");
    let quota = include_str!("fixtures/attachment_quota_v1.sql");
    let h=Harness::builder([&base_users::ADDON,&erp_addon_base::ADDON])
        .before_schema(format!("{old}\nINSERT INTO ir_blob_store(store_key,fingerprint) VALUES ('old','old'); INSERT INTO ir_blob_upload(subject_user,expires_at,state,size,store_id) VALUES (7,now(),2,17,1);\n{quota}\nALTER TABLE ir_blob_usage_base ADD COLUMN schema_version int2 NOT NULL DEFAULT 2 CHECK (schema_version=2); UPDATE ir_blob_upload SET reserved_bytes=0; TRUNCATE ir_blob_usage_delta; UPDATE ir_blob_usage_base SET bytes=0;"))
        .build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    assert_eq!(
        count(
            &mut tx,
            "SELECT schema_version::int8 FROM ir_blob_usage_base"
        )
        .await,
        3
    );
    assert_eq!(
        count(&mut tx, "SELECT reserved_bytes FROM ir_blob_upload").await,
        17
    );
    assert_eq!(
        count(&mut tx, "SELECT bytes::int8 FROM ir_blob_usage_base").await,
        17
    );
    erp_orm::core_ddl_run(&mut tx, CorePhase::BeforeModels)
        .await
        .unwrap();
    assert_eq!(
        count(&mut tx, "SELECT bytes::int8 FROM ir_blob_usage_base").await,
        17
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}
