//! S8: actual root deletions, names-only tombstones and transactional capture.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_fields::{Cell, Error, ErrorKind, OnDelete};
use erp_orm::{BeginOptions, OperationIdentity, RawVals, SUPERUSER_ID, Transaction};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl};
use erp_test_models::base_users;
use erp_testkit::Harness;

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "test_unlink_capture",
    depends: &["base"],
    exact_only: false,
};
const ROOT: &str = "test.unlink.root";
const HOOKED: &str = "test.unlink.hooked";
const NESTED: &str = "test.unlink.nested";
const PLAIN: &str = "test.unlink.plain";
const EVENTS: &str = "\"erp_event\".\"ir_event_outbox\"";

async fn harness() -> Harness {
    harness_with_policy(None).await
}

async fn harness_with_policy(policy: Option<erp_testkit::DomainPolicy>) -> Harness {
    let mut builder = Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([
            ModelDecl::new("test_unlink_capture", ROOT)
                .eventful(true)
                .fields(const { &[FieldDecl::char("name")] }),
            ModelDecl::new("test_unlink_capture", PLAIN)
                .fields(const { &[FieldDecl::char("name")] }),
            ModelDecl::new("test_unlink_capture", "test.unlink.child")
                .eventful(true)
                .fields(
                    const { &[FieldDecl::many2one("root_id", ROOT).ondelete(OnDelete::Cascade)] },
                ),
            ModelDecl::new("test_unlink_capture", "test.unlink.null")
                .eventful(true)
                .fields(
                    const { &[FieldDecl::many2one("root_id", ROOT).ondelete(OnDelete::SetNull)] },
                ),
            ModelDecl::new("test_unlink_capture", "test.unlink.grandchild").fields(
                const {
                    &[FieldDecl::many2one("parent_id", "test.unlink.child")
                        .ondelete(OnDelete::Cascade)]
                },
            ),
            ModelDecl::new("test_unlink_capture", HOOKED)
                .eventful(true)
                .fields(const { &[FieldDecl::char("name")] }),
            ModelDecl::new("test_unlink_capture", NESTED)
                .eventful(true)
                .fields(const { &[FieldDecl::char("name")] }),
        ])
        .unlink_hook(erp_orm::UnlinkHook::new(delete_hook));
    if let Some(policy) = policy {
        builder = builder.policy(policy);
    }
    let h = builder.build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.install_event_schema().await.unwrap();
    tx.execute(
        "INSERT INTO res_users (id, name, login, active) VALUES (7, 'deleter', 'deleter', true)",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    h
}
async fn begin(h: &Harness) -> Transaction {
    h.orm()
        .begin_opts(
            BeginOptions::new(SUPERUSER_ID)
                .operation_identity(OperationIdentity::generate().unwrap()),
        )
        .await
        .unwrap()
}
async fn seed(h: &Harness, n: usize) -> Vec<i64> {
    let mut tx = begin(h).await;
    let rows = (0..n)
        .map(|i| RawVals::new().set_cell("name", Cell::Str(format!("private-value-{i}").into())))
        .collect();
    let ids = tx
        .env()
        .records(ROOT)
        .unwrap()
        .create(&mut tx, rows)
        .await
        .unwrap()
        .ids();
    tx.commit().await.unwrap();
    ids
}
async fn unlink(tx: &mut Transaction, ids: &[i64]) {
    tx.env()
        .records(ROOT)
        .unwrap()
        .browse(ids.iter().copied())
        .unwrap()
        .unlink(tx)
        .await
        .unwrap();
}
async fn count(tx: &mut Transaction, table: &str) -> i64 {
    tx.query(&format!("SELECT count(*) FROM {table}"), &[])
        .await
        .unwrap()[0]
        .get(0)
}
async fn events(tx: &mut Transaction) -> Vec<tokio_postgres::Row> {
    tx.query(&format!("SELECT record_ids, field_names, cascade_models, model, subject, operation_id::text, tx_ordinal FROM {EVENTS} WHERE kind='unlink' ORDER BY id"), &[]).await.unwrap()
}
fn inserts(tx: &Transaction) -> usize {
    tx.captured()
        .iter()
        .filter(|s| s.starts_with("INSERT INTO \"erp_event\".\"ir_event_outbox\""))
        .count()
}

#[tokio::test]
async fn deletes_actual_ids_and_commits_tombstone_atomically() {
    let h = harness().await;
    let ids = seed(&h, 2).await;
    let mut tx = begin(&h).await;
    tx.capture_statements(true);
    unlink(&mut tx, &[ids[0], ids[0], ids[1], i64::MAX]).await;
    assert_eq!(events(&mut tx).await.len(), 0);
    let mut observer = h.begin().await.unwrap();
    assert_eq!(count(&mut observer, "test_unlink_root").await, 2);
    assert_eq!(events(&mut observer).await.len(), 0);
    observer.rollback().await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(inserts(&tx), 1);
    assert_eq!(
        tx.captured()
            .iter()
            .filter(|s| s.starts_with("DELETE FROM \"test_unlink_root\""))
            .count(),
        1
    );
    assert!(
        !tx.captured()
            .iter()
            .any(|s| s.starts_with("SELECT") && s.contains("test_unlink_root"))
    );
    let mut check = h.begin().await.unwrap();
    assert_eq!(count(&mut check, "test_unlink_root").await, 0);
    let rows = events(&mut check).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get::<_, serde_json::Value>(0),
        serde_json::json!(ids)
    );
    assert!(rows[0].get::<_, Vec<String>>(1).is_empty());
    assert_eq!(rows[0].get::<_, String>(3), ROOT);
    check.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn absent_and_plain_deletions_have_no_events() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    tx.capture_statements(true);
    unlink(&mut tx, &[]).await;
    unlink(&mut tx, &[i64::MAX]).await;
    let plain = tx
        .env()
        .records(PLAIN)
        .unwrap()
        .create(&mut tx, vec![RawVals::new()])
        .await
        .unwrap();
    plain.unlink(&mut tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(inserts(&tx), 0);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn returned_ids_split_at_envelope_limit() {
    let h = harness().await;
    let ids = seed(&h, 257).await;
    let mut tx = begin(&h).await;
    unlink(&mut tx, &ids).await;
    unlink(&mut tx, &ids).await;
    tx.commit().await.unwrap();
    let rows = events(&mut tx).await;
    assert_eq!(rows.len(), 2);
    let mut all = Vec::new();
    for row in rows {
        let got: Vec<i64> = serde_json::from_value(row.get(0)).unwrap();
        assert!(got.len() <= 256);
        all.extend(got);
    }
    all.sort_unstable();
    assert_eq!(all, ids);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn cascade_metadata_covers_actual_cold_cache_database_effects() {
    let h = harness().await;
    let ids = seed(&h, 1).await;
    let mut tx = begin(&h).await;
    let child = tx
        .env()
        .records("test.unlink.child")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("root_id", Cell::Id(ids[0]))],
        )
        .await
        .unwrap();
    tx.env()
        .records("test.unlink.grandchild")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("parent_id", Cell::Id(child.ids()[0]))],
        )
        .await
        .unwrap();
    tx.env()
        .records("test.unlink.null")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("root_id", Cell::Id(ids[0]))],
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut tx = begin(&h).await;
    unlink(&mut tx, &ids).await;
    tx.commit().await.unwrap();
    assert_eq!(count(&mut tx, "test_unlink_child").await, 0);
    assert_eq!(count(&mut tx, "test_unlink_grandchild").await, 0);
    let row = tx
        .query("SELECT root_id FROM test_unlink_null", &[])
        .await
        .unwrap();
    assert_eq!(row[0].get::<_, Option<i64>>(0), None);
    let rows = events(&mut tx).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get::<_, Vec<String>>(2),
        [
            "test.unlink.child",
            "test.unlink.derived",
            "test.unlink.grandchild",
            "test.unlink.null"
        ]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn deletion_savepoint_and_root_rollback_restore_rows_and_events() {
    let h = harness().await;
    let ids = seed(&h, 3).await;
    let mut tx = begin(&h).await;
    tx.savepoint(async |tx| {
        unlink(tx, &ids[..1]).await;
        Ok(())
    })
    .await
    .unwrap();
    let error = tx
        .savepoint(async |tx| {
            unlink(tx, &ids[1..2]).await;
            Err::<(), Error>(Error::programming("discard delete"))
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(error.message(), "discard delete");
    tx.commit().await.unwrap();
    let rows = events(&mut tx).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get::<_, serde_json::Value>(0),
        serde_json::json!([ids[0]])
    );
    unlink(&mut tx, &ids[2..]).await;
    tx.rollback().await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(count(&mut tx, "test_unlink_root").await, 2);
    assert_eq!(events(&mut tx).await.len(), 1);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn deleting_actor_survives_different_flusher_and_mixed_operation_order() {
    let h = harness().await;
    let ids = seed(&h, 1).await;
    let mut identity = begin(&h).await;
    let writer = identity.env().with_user(7).sudo(true);
    identity.rollback().await.unwrap();
    let operation = writer.operation_identity().unwrap().operation().to_string();
    let mut tx = begin(&h).await;
    let rec = writer.records(ROOT).unwrap().browse(ids.clone()).unwrap();
    rec.write(
        &mut tx,
        RawVals::new().set_cell("name", Cell::Str("new-private".into())),
    )
    .await
    .unwrap();
    rec.unlink(&mut tx).await.unwrap();
    tx.commit().await.unwrap();
    let rows = events(&mut tx).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, String>(4), "7");
    assert_eq!(rows[0].get::<_, String>(5), operation);
    let kinds: Vec<String> = tx
        .query(
            &format!("SELECT kind FROM {EVENTS} WHERE operation_id::text=$1 ORDER BY id"),
            &[&operation],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(kinds, ["write", "unlink"]);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn deletion_without_identity_is_explicitly_refused() {
    let h = harness().await;
    let ids = seed(&h, 1).await;
    let mut tx = h.begin().await.unwrap();
    tx.capture_statements(true);
    let error = tx
        .env()
        .records(ROOT)
        .unwrap()
        .browse(ids)
        .unwrap()
        .unlink(&mut tx)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "outbox: eventful mutation requires an OperationIdentity"
    );
    assert!(!tx.captured().iter().any(|s| s.starts_with("DELETE")));
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn direct_dispatch_capture_failure_rolls_back_delete() {
    let h = harness().await;
    let ids = seed(&h, 1).await;
    let mut tx = begin(&h).await;
    tx.execute(&format!("DROP TABLE {EVENTS}"), &[])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut tx = begin(&h).await;
    let records = tx.env().records(ROOT).unwrap().browse(ids).unwrap();
    erp_orm::dispatch::call_method::<(), ()>(&mut tx, &records, "unlink", ())
        .await
        .unwrap();
    let error = tx.commit().await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Database);
    assert_eq!(error.sqlstate(), Some("42P01"));
    // PostgreSQL wrapper context is not prescribed; the relation name and SQLSTATE are stable.
    assert!(error.message().contains("ir_event_outbox"), "{error}");
    let mut check = h.begin().await.unwrap();
    assert_eq!(count(&mut check, "test_unlink_root").await, 1);
    check.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn read_only_delete_is_refused_before_sql() {
    let h = harness().await;
    let ids = seed(&h, 1).await;
    let mut tx = h
        .orm()
        .begin_opts(
            BeginOptions::new(SUPERUSER_ID)
                .operation_identity(OperationIdentity::generate().unwrap())
                .read_only("test.unlink.read"),
        )
        .await
        .unwrap();
    tx.capture_statements(true);
    let error = tx
        .env()
        .records(ROOT)
        .unwrap()
        .browse(ids)
        .unwrap()
        .unlink(&mut tx)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "'test.unlink.read' is declared effect = read but tried to unlink 'test.unlink.root'; declare effect = write (or wider)."
    );
    assert!(!tx.captured().iter().any(|s| s.starts_with("DELETE")));
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

fn delete_hook<'a>(
    tx: &'a mut Transaction,
    model: &'a str,
    ids: &'a [erp_fields::RecId],
) -> erp_orm::BoxFuture<'a, erp_fields::Result<()>> {
    Box::pin(async move {
        if model == HOOKED {
            return Err(Error::programming("S8 intentional unlink hook failure"));
        }
        if model == NESTED {
            let error = tx
                .env()
                .records(NESTED)?
                .browse_ids(ids.iter().copied())
                .write(
                    tx,
                    RawVals::new().set_cell("name", Cell::Str("caught side effect".into())),
                )
                .await
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Programming);
            assert_eq!(
                error.message(),
                "a later release S7: nested event-relevant mutations during mutation or compute execution are not implemented"
            );
        }
        Ok(())
    })
}

#[tokio::test]
async fn hook_failure_and_caught_nested_refusal_roll_back_delete() {
    for model in [HOOKED, NESTED] {
        for pipelined in [false, true] {
            let h = harness().await;
            let mut tx = begin(&h).await;
            let rec = tx
                .env()
                .records(model)
                .unwrap()
                .create(&mut tx, vec![RawVals::new()])
                .await
                .unwrap();
            tx.commit().await.unwrap();
            let result = rec.unlink(&mut tx).await;
            let message = if model == HOOKED {
                let error = result.unwrap_err();
                assert_eq!(error.kind(), ErrorKind::Programming);
                assert_eq!(error.message(), "S8 intentional unlink hook failure");
                "S8 intentional unlink hook failure"
            } else {
                result.unwrap();
                "a later release S7: nested event-relevant mutations during mutation or compute execution are not implemented"
            };
            let error = if pipelined {
                match tx.commit_pipelined(None).await.unwrap_err() {
                    erp_orm::CommitError::RolledBack(e) => e,
                    other => panic!("expected rollback, got {other:?}"),
                }
            } else {
                tx.commit().await.unwrap_err()
            };
            assert_eq!(error.kind(), ErrorKind::Programming);
            assert_eq!(error.message(), message);
            let mut check = h.begin().await.unwrap();
            let table = if model == HOOKED {
                "test_unlink_hooked"
            } else {
                "test_unlink_nested"
            };
            assert_eq!(count(&mut check, table).await, 1);
            assert_eq!(events(&mut check).await.len(), 0);
            check.rollback().await.unwrap();
            h.cleanup().await.unwrap();
        }
    }
}

#[tokio::test]
async fn cancelled_delete_cannot_commit_unrecorded_changes() {
    let h = harness().await;
    let ids = seed(&h, 1).await;
    let mut tx = begin(&h).await;
    let pid: i32 = tx.query("SELECT pg_backend_pid()", &[]).await.unwrap()[0].get(0);
    let records = tx.env().records(ROOT).unwrap().browse(ids).unwrap();
    let mut blocker = h.begin().await.unwrap();
    blocker
        .execute("LOCK TABLE test_unlink_root IN SHARE MODE", &[])
        .await
        .unwrap();
    let mut observer = h.begin().await.unwrap();
    let mut deleting = Box::pin(records.unlink(&mut tx));
    // Setup deadline bounds a hung shared-host fixture; this is not a latency assertion.
    let mut waiting = Box::pin(tokio::time::timeout(
        std::time::Duration::from_secs(60),
        async {
            loop {
                let rows=observer.query("SELECT wait_event_type='Lock' AND query LIKE '%DELETE%test_unlink_root%' FROM pg_stat_activity WHERE pid=$1",&[&pid]).await.unwrap();
                if rows[0].get::<_, Option<bool>>(0) == Some(true) {
                    break;
                }
                observer.rollback().await.unwrap();
                tokio::task::yield_now().await;
            }
        },
    ));
    tokio::select! { outcome=&mut deleting=>panic!("delete passed held lock: {outcome:?}"), outcome=&mut waiting=>outcome.expect("DELETE never reached owned lock barrier") }
    drop(waiting);
    drop(deleting);
    blocker.rollback().await.unwrap();
    let error = tx.commit().await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "the transaction can no longer be used: an ORM operation was cancelled (its future was dropped) while it was changing the transaction's state; roll back"
    );
    tx.rollback().await.unwrap();
    observer.rollback().await.unwrap();
    assert_eq!(count(&mut observer, "test_unlink_root").await, 1);
    assert_eq!(events(&mut observer).await.len(), 0);
    observer.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn runtime_role_deletes_and_inserts_only_supplied_event_columns() {
    let h = harness().await;
    let ids = seed(&h, 1).await;
    let role = format!("unlink_{}", h.db().name());
    let quoted = erp_registry::ident::quote(&role);
    let mut tx = begin(&h).await;
    tx.execute(&format!("CREATE ROLE {quoted} NOLOGIN"), &[])
        .await
        .unwrap();
    tx.grant_event_runtime_role(&role).await.unwrap();
    tx.execute(
        &format!("GRANT SELECT, DELETE ON test_unlink_root TO {quoted}"),
        &[],
    )
    .await
    .unwrap();
    // The stored-dependent fixture requires an ordinary dependency read before deletion.
    tx.execute(
        &format!("GRANT SELECT ON test_unlink_derived TO {quoted}"),
        &[],
    )
    .await
    .unwrap();
    // Root deletion cascades into these fixture tables; PostgreSQL enforces FK effects itself.
    tx.commit().await.unwrap();
    tx.execute(&format!("SET LOCAL ROLE {quoted}"), &[])
        .await
        .unwrap();
    assert_eq!(
        tx.query("SELECT current_user::text", &[]).await.unwrap()[0].get::<_, String>(0),
        role
    );
    unlink(&mut tx, &ids).await;
    tx.commit_pipelined(None).await.unwrap();
    let mut check = h.begin().await.unwrap();
    assert_eq!(count(&mut check, "test_unlink_root").await, 0);
    assert_eq!(events(&mut check).await.len(), 1);
    check
        .execute(&format!("DROP OWNED BY {quoted}"), &[])
        .await
        .unwrap();
    check
        .execute(&format!("DROP ROLE {quoted}"), &[])
        .await
        .unwrap();
    check.commit().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn more_than_one_delete_chunk_captures_every_actual_id_once() {
    let h = harness().await;
    let ids = seed(&h, erp_sql::relational::DELETE_CHUNK + 1).await;
    let mut tx = begin(&h).await;
    tx.capture_statements(true);
    unlink(&mut tx, &ids).await;
    tx.commit().await.unwrap();
    assert_eq!(inserts(&tx), 1);
    assert_eq!(
        tx.captured()
            .iter()
            .filter(|s| s.starts_with("DELETE FROM \"test_unlink_root\""))
            .count(),
        2
    );
    let mut all = Vec::new();
    for row in events(&mut tx).await {
        let got: Vec<i64> = serde_json::from_value(row.get(0)).unwrap();
        assert!(got.len() <= 256);
        all.extend(got);
    }
    all.sort_unstable();
    assert_eq!(all, ids);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

mod derived_fixture {
    use erp_sdk::prelude::*;
    #[model(
        name = "test.unlink.derived",
        addon = "test_unlink_capture",
        eventful = true
    )]
    pub struct Derived {
        #[field(many2one, comodel = "test.unlink.root", ondelete = "set null")]
        pub root_id: Option<RecId>,
        #[field(char, related = "root_id.name", store)]
        pub copied: Option<Arc<str>>,
    }
}

#[tokio::test]
async fn set_null_dependent_recompute_keeps_deleting_origin() {
    let h = harness().await;
    let ids = seed(&h, 1).await;
    let mut tx = begin(&h).await;
    let dependent = tx
        .env()
        .records("test.unlink.derived")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("root_id", Cell::Id(ids[0]))],
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let baseline: i64 = tx
        .query(&format!("SELECT coalesce(max(id),0) FROM {EVENTS}"), &[])
        .await
        .unwrap()[0]
        .get(0);
    let mut identity = begin(&h).await;
    let deleter = identity.env().with_user(7).sudo(true);
    identity.rollback().await.unwrap();
    deleter
        .records(ROOT)
        .unwrap()
        .browse(ids)
        .unwrap()
        .unlink(&mut tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let rows = tx
        .query(
            &format!("SELECT kind,model,field_names,subject FROM {EVENTS} WHERE id>$1 ORDER BY id"),
            &[&baseline],
        )
        .await
        .unwrap();
    assert_eq!(rows[0].get::<_, String>(0), "unlink");
    assert_eq!(rows[0].get::<_, String>(1), ROOT);
    let compute: Vec<_> = rows
        .iter()
        .filter(|r| {
            r.get::<_, String>(0) == "write" && r.get::<_, String>(1) == "test.unlink.derived"
        })
        .collect();
    assert!(!compute.is_empty());
    assert!(
        compute
            .iter()
            .any(|r| r.get::<_, Vec<String>>(2).contains(&"copied".to_owned()))
    );
    assert!(compute.iter().all(|r| r.get::<_, String>(3) == "7"));
    let stored = tx
        .query(
            "SELECT root_id,copied,write_uid FROM test_unlink_derived WHERE id=$1",
            &[&dependent.ids()[0]],
        )
        .await
        .unwrap();
    assert_eq!(stored[0].get::<_, Option<i64>>(0), None);
    assert_eq!(stored[0].get::<_, Option<String>>(1), None);
    assert_eq!(stored[0].get::<_, i64>(2), SUPERUSER_ID.get());
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn isolated_delete_restores_rows_and_preserves_prior_pending_write() {
    let h = harness().await;
    let ids = seed(&h, 1).await;
    let mut tx = begin(&h).await;
    let records = tx.env().records(ROOT).unwrap().browse(ids).unwrap();
    records
        .write(
            &mut tx,
            RawVals::new().set_cell("name", Cell::Str("prior write".into())),
        )
        .await
        .unwrap();
    tx.isolated_for_tests(async |tx| {
        records.unlink(tx).await?;
        Ok(())
    })
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(count(&mut tx, "test_unlink_root").await, 1);
    assert!(events(&mut tx).await.is_empty());
    let names = tx
        .query("SELECT name FROM test_unlink_root", &[])
        .await
        .unwrap();
    assert_eq!(names[0].get::<_, String>(0), "prior write");
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn model_and_record_denials_precede_destructive_delete() {
    use erp_domain::{Domain, Operator, Value as DomainValue};
    use erp_testkit::{DomainPolicy, Otherwise};
    for model_denial in [false, true] {
        let predicate = if model_denial {
            Domain::FALSE
        } else {
            Domain::condition("id", Operator::Eq, DomainValue::Int(-1)).unwrap()
        };
        let policy = DomainPolicy::builder(Otherwise::Allow)
            .rule(ROOT, erp_orm::Operation::Unlink, predicate)
            .build()
            .unwrap();
        let h = harness_with_policy(Some(policy)).await;
        let ids = seed(&h, 1).await;
        let mut tx = begin(&h).await;
        tx.capture_statements(true);
        let rec = tx
            .env()
            .with_user(7)
            .records(ROOT)
            .unwrap()
            .browse(ids.clone())
            .unwrap();
        let error = rec.unlink(&mut tx).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access);
        let description = &h.registry().model(ROOT).unwrap().description;
        let expected = if model_denial {
            format!("Your access rights do not let you unlink '{description}' ({ROOT}) records.")
        } else {
            format!(
                "Your access rights do not let you delete '{description}' ({ROOT}) records.\n\nRecords: {ROOT}({},), User: 7",
                ids[0]
            )
        };
        assert_eq!(error.message(), expected);
        assert!(!tx.captured().iter().any(|s| s.starts_with("DELETE")));
        tx.commit().await.unwrap();
        let mut check = h.begin().await.unwrap();
        assert_eq!(count(&mut check, "test_unlink_root").await, 1);
        assert!(events(&mut check).await.is_empty());
        check.rollback().await.unwrap();
        h.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn company_dependent_ancillary_effect_refuses_before_sql() {
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([
            ModelDecl::new("test_unlink_capture", ROOT)
                .eventful(true)
                .fields(const { &[FieldDecl::char("name")] }),
            ModelDecl::new("test_unlink_capture", PLAIN)
                .fields(const { &[FieldDecl::char("name")] }),
            ModelDecl::new("test_unlink_capture", "test.unlink.json.root")
                .eventful(true)
                .fields(const { &[FieldDecl::char("name")] }),
            ModelDecl::new("test_unlink_capture", "test.unlink.json.owner")
                .eventful(true)
                .fields(
                    const {
                        &[FieldDecl::many2one("root_id", "test.unlink.json.root")
                            .company_dependent(true)]
                    },
                ),
        ])
        .build()
        .await
        .unwrap();
    let mut tx = begin(&h).await;
    tx.capture_statements(true);
    let records = tx
        .env()
        .records("test.unlink.json.root")
        .unwrap()
        .browse([1])
        .unwrap();
    let error = records.unlink(&mut tx).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "a later release S8: event-relevant ancillary delete effects on model 'test.unlink.json.owner' are not implemented"
    );
    assert!(tx.captured().is_empty());
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

mod plain_dependency_fixture {
    use erp_sdk::prelude::*;
    #[model(name = "test.unlink.middle", addon = "test_unlink_capture")]
    pub struct Middle {
        #[field(many2one, comodel = "test.unlink.plain", ondelete = "set null")]
        pub root_id: Option<RecId>,
    }
    #[model(
        name = "test.unlink.sink",
        addon = "test_unlink_capture",
        eventful = true
    )]
    pub struct Sink {
        #[field(many2one, comodel = "test.unlink.middle")]
        pub middle_id: Option<RecId>,
        #[field(char, related = "middle_id.root_id.name", store)]
        pub copied: Option<Arc<str>>,
    }
}

#[tokio::test]
async fn plain_root_surviving_compute_keeps_deleting_origin_without_root_event() {
    let h = harness().await;
    let mut tx = begin(&h).await;
    let root = tx
        .env()
        .records(PLAIN)
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("plain-source".into()))],
        )
        .await
        .unwrap();
    let middle = tx
        .env()
        .records("test.unlink.middle")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("root_id", Cell::Id(root.ids()[0]))],
        )
        .await
        .unwrap();
    let sink = tx
        .env()
        .records("test.unlink.sink")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("middle_id", Cell::Id(middle.ids()[0]))],
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let baseline: i64 = tx
        .query(&format!("SELECT coalesce(max(id),0) FROM {EVENTS}"), &[])
        .await
        .unwrap()[0]
        .get(0);
    root.with_env(tx.env().with_user(7).sudo(true))
        .unlink(&mut tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let rows = tx
        .query(
            &format!("SELECT kind,model,subject FROM {EVENTS} WHERE id>$1 ORDER BY id"),
            &[&baseline],
        )
        .await
        .unwrap();
    assert!(!rows.is_empty());
    assert!(rows.iter().all(|r| r.get::<_, String>(0) == "write"
        && r.get::<_, String>(1) == "test.unlink.sink"
        && r.get::<_, String>(2) == "7"));
    let value = tx
        .query(
            "SELECT copied FROM test_unlink_sink WHERE id=$1",
            &[&sink.ids()[0]],
        )
        .await
        .unwrap()[0]
        .get::<_, Option<String>>(0);
    assert_eq!(value, None);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn restrict_fk_failure_keeps_root_and_has_no_event() {
    const RESTRICT_ROOT: &str = "test.unlink.restrict.root";
    const RESTRICT_CHILD: &str = "test.unlink.restrict.child";
    let h =
        Harness::builder([&base_users::ADDON, &ADDON])
            .with_decls([
                ModelDecl::new("test_unlink_capture", ROOT)
                    .eventful(true)
                    .fields(const { &[FieldDecl::char("name")] }),
                ModelDecl::new("test_unlink_capture", PLAIN)
                    .fields(const { &[FieldDecl::char("name")] }),
                ModelDecl::new("test_unlink_capture", RESTRICT_ROOT)
                    .eventful(true)
                    .fields(const { &[FieldDecl::char("name")] }),
                ModelDecl::new("test_unlink_capture", RESTRICT_CHILD).fields(
                    const {
                        &[FieldDecl::many2one("root_id", RESTRICT_ROOT)
                            .ondelete(OnDelete::Restrict)]
                    },
                ),
            ])
            .build()
            .await
            .unwrap();
    let mut tx = begin(&h).await;
    tx.install_event_schema().await.unwrap();
    let root = tx
        .env()
        .records(RESTRICT_ROOT)
        .unwrap()
        .create(&mut tx, vec![RawVals::new()])
        .await
        .unwrap();
    tx.env()
        .records(RESTRICT_CHILD)
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("root_id", Cell::Id(root.ids()[0]))],
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let error = root.unlink(&mut tx).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.sqlstate(), Some("23001"));
    let child = h.registry().model(RESTRICT_CHILD).unwrap();
    let field = child.field("root_id").unwrap();
    assert_eq!(
        error.message(),
        format!(
            "The database refused the change: Other records still point to the record you want to delete.\n\nReferring model: '{}' ({RESTRICT_CHILD})\nConstraint: '{}' ({})\nArchive the record instead of deleting it.",
            child.description, field.string, field.name
        )
    );
    tx.rollback().await.unwrap();
    assert_eq!(count(&mut tx, "test_unlink_restrict_root").await, 1);
    assert!(events(&mut tx).await.is_empty());
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn review_r1_capture_admission_after_pending_drop_is_rollback_only() {
    let h = harness().await;
    let ids = seed(&h, 1).await;
    let mut tx = begin(&h).await;
    let dependent = tx
        .env()
        .records("test.unlink.derived")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("root_id", Cell::Id(ids[0]))],
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let source = tx.env().records(ROOT).unwrap().browse(ids).unwrap();
    source
        .write(
            &mut tx,
            RawVals::new().set_cell(
                "name",
                Cell::Str("must roll back with pending output".into()),
            ),
        )
        .await
        .unwrap();
    let spec = h.registry().model(ROOT).unwrap();
    tx.flush_fields(spec.id, &[spec.field_id("name").unwrap()])
        .await
        .unwrap();
    // The static test policy has no company resolver; core System accepts this typed list.
    let oversized = tx.env().with_context([(
        "allowed_company_ids",
        erp_fields::Value::list((1..=257).map(erp_fields::Value::Int)),
    )]);
    let error = dependent
        .with_env(oversized)
        .unlink(&mut tx)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::ResourceExhausted);
    assert_eq!(
        error.message(),
        "outbox CaptureLimit: event list exceeds 256 entries"
    );
    let error = tx.commit().await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::ResourceExhausted);
    assert_eq!(
        error.message(),
        "outbox CaptureLimit: event list exceeds 256 entries"
    );
    let mut check = h.begin().await.unwrap();
    let rows=check.query("SELECT r.name,d.copied FROM test_unlink_root r JOIN test_unlink_derived d ON d.root_id=r.id",&[]).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, String>(0), "private-value-0");
    assert_eq!(rows[0].get::<_, String>(1), "private-value-0");
    assert!(events(&mut check).await.is_empty());
    check.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
