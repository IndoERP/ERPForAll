//! Explicit system-backfill capture boundary, before schema/data mutation.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
use erp_fields::ErrorKind;
use erp_orm::{Orm, SUPERUSER_ID, SchemaSyncOptions, StaticPolicy};
use erp_registry::{FieldDecl, ModelDecl, RegistryBuilder};
use erp_test_models::{
    base_users,
    test_large::{test_large_base, test_large_ext},
};
use erp_testkit::Harness;
use std::sync::Arc;
const MODEL: &str = "test_large.item";
const EXPECTED: &str = "outbox: stored-compute backfill capture for 'test_large.item.name_size' is not implemented; requires the a later release stored-compute-backfill capture slice";
async fn seeded() -> Harness {
    let h = Harness::builder([&base_users::ADDON, &test_large_base::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "INSERT INTO test_large_item (id,name,qty,reference) VALUES (1,'before',1,'r1')",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    h
}
fn extended(h: &Harness, eventful: bool, downstream: bool) -> Orm {
    let mut decls = vec![
        ModelDecl::new(test_large_ext::ADDON.name, MODEL)
            .inherit(&[MODEL])
            .eventful(eventful),
    ];
    if downstream {
        decls.push(
            ModelDecl::new(test_large_ext::ADDON.name, "test.backfill.sink")
                .eventful(true)
                .fields(
                    const {
                        &[
                            FieldDecl::many2one("item_id", MODEL),
                            FieldDecl::integer("copied")
                                .related("item_id.name_size")
                                .store(true),
                        ]
                    },
                ),
        );
    }
    let decls: Vec<_> = decls
        .into_iter()
        .map(|d| &*Box::leak(Box::new(d)))
        .collect();
    let registry = Arc::new(
        RegistryBuilder::new([
            &base_users::ADDON,
            &test_large_base::ADDON,
            &test_large_ext::ADDON,
        ])
        .with_decls(decls)
        .build()
        .unwrap(),
    );
    Orm::builder(registry, h.db().pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap()
}
#[tokio::test]
async fn capture_relevant_backfill_refuses_before_schema_changes() {
    for downstream in [false, true] {
        for threshold in [0, 100] {
            let h = seeded().await;
            let orm = extended(&h, !downstream, downstream);
            let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
            tx.capture_statements(true);
            let error = tx
                .init_models_with(&[], &SchemaSyncOptions::new().large_table_rows(threshold))
                .await
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Programming);
            assert_eq!(error.message(), EXPECTED);
            assert!(
                !tx.captured().iter().any(|s| s.starts_with("ALTER TABLE")
                    || s.starts_with("UPDATE")
                    || s.starts_with("COMMENT ON")),
                "{:?}",
                tx.captured()
            );
            tx.rollback().await.unwrap();
            drop(tx);
            let mut check = h.begin().await.unwrap();
            assert_eq!(
                check
                    .query("SELECT name FROM test_large_item WHERE id=1", &[])
                    .await
                    .unwrap()[0]
                    .get::<_, String>(0),
                "before"
            );
            let count:i64=check.query("SELECT count(*) FROM pg_attribute WHERE attrelid='test_large_item'::regclass AND attname='name_size' AND NOT attisdropped",&[]).await.unwrap()[0].get(0);
            assert_eq!(count, 0);
            check.rollback().await.unwrap();
            drop(check);
            h.cleanup().await.unwrap();
        }
    }
}
#[tokio::test]
async fn resumed_backfill_checks_current_capture_relevance() {
    let h = seeded().await;
    let plain = extended(&h, false, false);
    let options = SchemaSyncOptions::new().large_table_rows(0);
    let mut tx = plain.begin(SUPERUSER_ID).await.unwrap();
    let report = tx.init_models_with(&[], &options).await.unwrap();
    let step = report
        .steps()
        .iter()
        .find(|s| s.key() == "backfill.test_large_item")
        .unwrap()
        .clone();
    tx.commit().await.unwrap();
    drop(tx);
    let orm = extended(&h, true, false);
    let error = orm
        .plan_schema_step(
            "test_large_item",
            "backfill.test_large_item",
            &options,
            &["name_size"],
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(error.message(), EXPECTED);
    let error = orm.run_schema_step(&step, &options).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert!(
        format!("{error:?}").contains("stored-compute backfill capture"),
        "{error:?}"
    );
    let mut check = h.begin().await.unwrap();
    assert_eq!(
        check
            .query("SELECT name_size FROM test_large_item WHERE id=1", &[])
            .await
            .unwrap()[0]
            .get::<_, Option<i32>>(0),
        None
    );
    check.rollback().await.unwrap();
    drop(check);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn ordinary_backfill_and_fresh_eventful_install_remain_supported() {
    let h = seeded().await;
    let orm = extended(&h, false, false);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.init_models(&[]).await.unwrap();
    assert_eq!(
        tx.query("SELECT name_size FROM test_large_item WHERE id=1", &[])
            .await
            .unwrap()[0]
            .get::<_, i32>(0),
        6
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
    let h = Harness::builder([
        &base_users::ADDON,
        &test_large_base::ADDON,
        &test_large_ext::ADDON,
    ])
    .with_decls([ModelDecl::new(test_large_ext::ADDON.name, MODEL)
        .inherit(&[MODEL])
        .eventful(true)])
    .build()
    .await
    .unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn empty_table_stored_compute_backfill_is_admitted_without_events() {
    for downstream in [false, true] {
        // the table exists (test_large_base) and holds no row; an empty table is never "large",
        // so this is the inline path whatever the threshold
        let h = Harness::builder([&base_users::ADDON, &test_large_base::ADDON])
            .build()
            .await
            .unwrap();
        let orm = extended(&h, !downstream, downstream);
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        tx.install_event_schema().await.unwrap();
        tx.init_models(&[]).await.unwrap();
        assert_eq!(
            name_size_columns(&mut tx).await,
            1,
            "downstream={downstream}"
        );
        assert_eq!(outbox_rows(&mut tx).await, 0, "downstream={downstream}");
        tx.rollback().await.unwrap();
        drop(tx);
        h.cleanup().await.unwrap();
    }
}

async fn name_size_columns(tx: &mut erp_orm::Transaction) -> i64 {
    tx.query(
        "SELECT count(*) FROM pg_attribute WHERE attrelid='test_large_item'::regclass AND attname='name_size' AND NOT attisdropped",
        &[],
    )
    .await
    .unwrap()[0]
        .get(0)
}

async fn outbox_rows(tx: &mut erp_orm::Transaction) -> i64 {
    tx.query("SELECT count(*) FROM erp_event.ir_event_outbox", &[])
        .await
        .unwrap()[0]
        .get(0)
}

#[tokio::test]
async fn resumed_backfill_on_a_table_emptied_since_planning_is_admitted() {
    let h = seeded().await;
    let plain = extended(&h, false, false);
    let options = SchemaSyncOptions::new().large_table_rows(0);
    let mut tx = plain.begin(SUPERUSER_ID).await.unwrap();
    let report = tx.init_models_with(&[], &options).await.unwrap();
    let step = report
        .steps()
        .iter()
        .find(|s| s.key() == "backfill.test_large_item")
        .unwrap()
        .clone();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = h.begin().await.unwrap();
    tx.execute("DELETE FROM test_large_item", &[])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let orm = extended(&h, true, false);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.install_event_schema().await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let report = orm.run_schema_step(&step, &options).await.unwrap();
    assert!(report.is_empty(), "{:?}", report.kept_comments());
    let mut check = h.begin().await.unwrap();
    assert_eq!(name_size_columns(&mut check).await, 1);
    assert_eq!(outbox_rows(&mut check).await, 0);
    // the step finished: the column no longer carries the pending-backfill mark
    let comment: Option<String> = check
        .query(
            "SELECT col_description('test_large_item'::regclass, attnum) FROM pg_attribute \
             WHERE attrelid = 'test_large_item'::regclass AND attname = 'name_size'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert!(
        !comment
            .as_deref()
            .is_some_and(|c| c.starts_with(erp_sql::ddl::PENDING_BACKFILL_COMMENT_PREFIX)),
        "{comment:?}"
    );
    check.rollback().await.unwrap();
    drop(check);
    h.cleanup().await.unwrap();
}

/// A row inserted and deleted before planning leaves a dead tuple: the planning probe sees an empty
/// table, the size check after the DDL does not. (Autovacuum does not run for one dead tuple: its
/// threshold is 50 rows.)
#[tokio::test]
async fn inline_backfill_refuses_a_relation_that_still_has_pages_after_planning_passed() {
    let h = seeded().await;
    let mut tx = h.begin().await.unwrap();
    tx.execute("DELETE FROM test_large_item", &[])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let orm = extended(&h, true, false);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.install_event_schema().await.unwrap();
    let error = tx.init_models(&[]).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(error.message(), EXPECTED);
    tx.rollback().await.unwrap();
    drop(tx);
    let mut check = h.begin().await.unwrap();
    assert_eq!(name_size_columns(&mut check).await, 0);
    check.rollback().await.unwrap();
    drop(check);
    h.cleanup().await.unwrap();
}
