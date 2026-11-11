//! S9 capture admission and rollback boundaries, independent persisted-state readers.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
#[path = "support/outbox_upsert.rs"]
mod fixture;
use erp_fields::{Cell, Error, ErrorKind};
use erp_orm::{RawVals, UpsertCall};
use fixture::*;

#[tokio::test]
async fn missing_identity_refuses_all_entry_paths_before_mutation() {
    let h = harness(true).await;
    let baseline = state(&h).await;
    for surface in 0..3 {
        let mut tx = h.begin().await.unwrap();
        tx.capture_statements(true);
        let r = tx.env().records(MODEL).unwrap();
        let err = match surface {
            0 => r
                .upsert(&mut tx, call(vec![row(Some("a"), 1, None)]))
                .await
                .unwrap_err(),
            1 => r
                .upsert_values(
                    &mut tx,
                    "_business",
                    &[std::collections::BTreeMap::from([(
                        "code".into(),
                        erp_fields::Value::str("a"),
                    )])],
                    &[],
                    &[],
                )
                .await
                .unwrap_err(),
            _ => erp_orm::call_method::<(UpsertCall,), erp_orm::UpsertResult>(
                &mut tx,
                &r,
                "upsert",
                (call(vec![row(Some("a"), 1, None)]),),
            )
            .await
            .unwrap_err(),
        };
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert_eq!(
            err.message(),
            "outbox: eventful mutation requires an OperationIdentity"
        );
        assert!(tx.captured().is_empty());
        tx.rollback().await.unwrap();
        assert_eq!(state(&h).await, baseline);
    }
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn caught_later_chunk_constraint_cannot_commit_partial_rows_or_events() {
    let h = harness(true).await;
    let mut tx = begin(&h).await;
    let op = tx
        .env()
        .operation_identity()
        .unwrap()
        .operation()
        .to_string();
    let mut rows = (0..129)
        .map(|i| row(Some(&format!("k{i:03}")), i, None))
        .collect::<Vec<_>>();
    rows[128] = row(Some("k128"), 128, Some("reject"));
    let err = tx
        .env()
        .records(MODEL)
        .unwrap()
        .upsert(&mut tx, call(rows))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.message(), "upsert fixture rejects note");
    let err = tx.commit().await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.message(), "upsert fixture rejects note");
    tx.rollback().await.unwrap();
    assert!(state(&h).await.is_empty());
    assert!(events(&h, &op).await.is_empty());
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn savepoint_restores_capture_for_a_valid_sibling() {
    let h = harness(true).await;
    let mut tx = begin(&h).await;
    let op = tx
        .env()
        .operation_identity()
        .unwrap()
        .operation()
        .to_string();
    tx.env()
        .records(MODEL)
        .unwrap()
        .upsert(&mut tx, call(vec![row(Some("prefix"), 1, None)]))
        .await
        .unwrap();
    let err = tx
        .savepoint(async |tx| {
            tx.env()
                .records(MODEL)?
                .upsert(tx, call(vec![row(Some("lost"), 2, None)]))
                .await?;
            Err::<(), _>(Error::validation("rollback upsert scope"))
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.message(), "rollback upsert scope");
    tx.env()
        .records(MODEL)
        .unwrap()
        .upsert(&mut tx, call(vec![row(Some("sibling"), 3, None)]))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let data = state(&h).await;
    assert_eq!(
        data.iter()
            .map(|r| r.1.as_deref().unwrap())
            .collect::<Vec<_>>(),
        ["prefix", "sibling"]
    );
    let mut ids = events(&h, &op)
        .await
        .iter()
        .flat_map(|e| e.ids.clone())
        .collect::<Vec<_>>();
    ids.sort_unstable();
    let mut expected = data.iter().map(|r| r.0).collect::<Vec<_>>();
    expected.sort_unstable();
    assert_eq!(ids, expected);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn generated_assignment_and_event_insert_denial_preserve_business_state() {
    let h = harness(true).await;
    let mut tx = begin(&h).await;
    let err = tx
        .env()
        .records(MODEL)
        .unwrap()
        .upsert(
            &mut tx,
            call(vec![
                row(Some("a"), 1, None).set_cell("twice", Cell::Int(2)),
            ]),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "Generated field 'test.outbox.upsert.twice' is read-only"
    );
    tx.rollback().await.unwrap();
    // A required event partition is deliberately removed in this isolated test database.
    let mut setup = h.begin().await.unwrap();
    let tables=setup.query("SELECT inhrelid::regclass::text FROM pg_inherits WHERE inhparent='erp_event.ir_event_outbox'::regclass",&[]).await.unwrap();
    for r in tables {
        let table: String = r.get(0);
        setup
            .execute(&format!("DROP TABLE {table}"), &[])
            .await
            .unwrap();
    }
    setup.commit().await.unwrap();
    let mut tx = begin(&h).await;
    let op = tx
        .env()
        .operation_identity()
        .unwrap()
        .operation()
        .to_string();
    tx.env()
        .records(MODEL)
        .unwrap()
        .upsert(&mut tx, call(vec![row(Some("a"), 1, None)]))
        .await
        .unwrap();
    let err = tx.commit().await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.sqlstate(), Some("23514"));
    assert_eq!(
        err.message(),
        "The operation cannot be completed: no partition of relation \"ir_event_outbox\" found for row"
    );
    tx.rollback().await.unwrap();
    assert!(state(&h).await.is_empty());
    assert!(events(&h, &op).await.is_empty());
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn caught_nested_compute_upsert_prevents_commit() {
    let h = harness(true).await;
    let mut tx = begin(&h).await;
    let op = tx
        .env()
        .operation_identity()
        .unwrap()
        .operation()
        .to_string();
    tx.env()
        .records("test.outbox.upsert.computed")
        .unwrap()
        .upsert(
            &mut tx,
            UpsertCall::new("_business")
                .with_rows([RawVals::new()
                    .set_cell("code", Cell::Str("x".into()))
                    .set_cell("source", Cell::Int(9))])
                .with_update(["source"]),
        )
        .await
        .unwrap();
    let err = tx.commit().await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "a later release S7: nested event-relevant mutations during mutation or compute execution are not implemented"
    );
    tx.rollback().await.unwrap();
    assert!(events(&h, &op).await.is_empty());
    assert!(state(&h).await.is_empty());
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn capture_preserves_existing_authorization_and_exact_errors() {
    for denial in [
        Denial::Model,
        Denial::Field,
        Denial::Write,
        Denial::Read,
        Denial::Create,
    ] {
        let mut expected = None;
        for eventful in [false, true] {
            let h = harness_with(eventful, denial).await;
            seed(&h).await;
            let baseline = state(&h).await;
            let mut tx = user_begin(&h).await;
            let op = tx
                .env()
                .operation_identity()
                .unwrap()
                .operation()
                .to_string();
            let rows = if matches!(denial, Denial::Create) {
                vec![row(Some("new"), 3, None)]
            } else {
                vec![row(Some("a"), 3, None)]
            };
            let err = tx
                .env()
                .records(MODEL)
                .unwrap()
                .upsert(&mut tx, call(rows))
                .await
                .unwrap_err();
            assert!(matches!(
                err.kind(),
                ErrorKind::Access | ErrorKind::Validation
            ));
            if let Some((kind, message)) = &expected {
                assert_eq!(&err.kind(), kind);
                assert_eq!(err.message(), message);
            } else {
                expected = Some((err.kind(), err.message().to_owned()));
            }
            tx.rollback().await.unwrap();
            assert_eq!(state(&h).await, baseline);
            assert!(events(&h, &op).await.is_empty());
            h.cleanup().await.unwrap();
        }
    }
}

#[tokio::test]
async fn cancelled_upsert_cannot_commit_after_its_lock_releases() {
    let h = harness(true).await;
    seed(&h).await;
    let baseline = state(&h).await;
    let mut tx = begin(&h).await;
    let op = tx
        .env()
        .operation_identity()
        .unwrap()
        .operation()
        .to_string();
    let pid: i32 = tx.query("SELECT pg_backend_pid()", &[]).await.unwrap()[0].get(0);
    let mut blocker = h.begin().await.unwrap();
    blocker
        .execute("LOCK TABLE test_outbox_upsert IN SHARE MODE", &[])
        .await
        .unwrap();
    let mut observer = h.begin().await.unwrap();
    let model = tx.env().records(MODEL).unwrap();
    let mut future = Box::pin(model.upsert(&mut tx, call(vec![row(Some("a"), 8, None)])));
    let mut waiting = Box::pin(tokio::time::timeout(
        std::time::Duration::from_secs(60),
        async {
            loop {
                let rows=observer.query("SELECT wait_event_type='Lock' AND query LIKE '%INSERT INTO%test_outbox_upsert%' FROM pg_stat_activity WHERE pid=$1",&[&pid]).await.unwrap();
                if rows[0].get::<_, Option<bool>>(0) == Some(true) {
                    break;
                }
                observer.rollback().await.unwrap();
                tokio::task::yield_now().await;
            }
        },
    ));
    tokio::select! {result=&mut future=>panic!("upsert crossed held lock: {result:?}"),result=&mut waiting=>result.expect("owned upsert lock barrier not reached")}
    drop(waiting);
    drop(future);
    blocker.rollback().await.unwrap();
    let err = tx.commit().await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "the transaction can no longer be used: an ORM operation was cancelled (its future was dropped) while it was changing the transaction's state; roll back"
    );
    tx.rollback().await.unwrap();
    observer.rollback().await.unwrap();
    assert_eq!(state(&h).await, baseline);
    assert!(events(&h, &op).await.is_empty());
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn serialization_retry_keeps_only_successful_upsert_capture() {
    let h = harness(true).await;
    let ids = seed(&h).await;
    let identity = erp_orm::OperationIdentity::generate().unwrap();
    let op = identity.operation().to_string();
    let mut first = h
        .orm()
        .begin_opts(
            erp_orm::BeginOptions::new(erp_orm::SUPERUSER_ID).operation_identity(identity.clone()),
        )
        .await
        .unwrap();
    first
        .query(
            "SELECT number FROM test_outbox_upsert WHERE id=$1",
            &[&ids[0]],
        )
        .await
        .unwrap();
    let mut competitor = begin(&h).await;
    competitor
        .env()
        .records(MODEL)
        .unwrap()
        .upsert(&mut competitor, call(vec![row(Some("a"), 4, None)]))
        .await
        .unwrap();
    competitor.commit().await.unwrap();
    let err = first
        .env()
        .records(MODEL)
        .unwrap()
        .upsert(&mut first, call(vec![row(Some("a"), 5, None)]))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Concurrency);
    assert_eq!(err.sqlstate(), Some("40001"));
    assert_eq!(
        err.message(),
        "could not serialize access due to concurrent update"
    );
    first.rollback().await.unwrap();
    let mut retry = h
        .orm()
        .begin_opts(erp_orm::BeginOptions::new(erp_orm::SUPERUSER_ID).operation_identity(identity))
        .await
        .unwrap();
    retry
        .env()
        .records(MODEL)
        .unwrap()
        .upsert(&mut retry, call(vec![row(Some("a"), 5, None)]))
        .await
        .unwrap();
    retry.commit().await.unwrap();
    let e = events(&h, &op).await;
    assert_eq!(e.len(), 1);
    assert_eq!(e[0].ids, vec![ids[0]]);
    assert_eq!(state(&h).await[0].2, 5);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn changed_name_limit_accepts_boundary_and_refuses_overflow_before_sql() {
    use erp_registry::{FieldDecl, KeyDecl, ModelDecl, TableObjectDecl, TableObjectKind};
    use erp_test_models::base_users;
    let fields = std::iter::once(FieldDecl::char("code"))
        .chain((0..129).map(|i| FieldDecl::integer(Box::leak(format!("f{i:03}").into_boxed_str()))))
        .collect::<Vec<_>>();
    let fields = Box::leak(fields.into_boxed_slice());
    let objects = Box::leak(
        vec![TableObjectDecl {
            name: "_business",
            kind: TableObjectKind::Key(KeyDecl::new(&["code"], "Wide code unique.")),
        }]
        .into_boxed_slice(),
    );
    let h = erp_testkit::Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([ModelDecl::new(ADDON.name, "test.upsert.wide")
            .eventful(true)
            .fields(fields)
            .table_objects(objects)])
        .build()
        .await
        .unwrap();
    let mut setup = h.begin().await.unwrap();
    setup.install_event_schema().await.unwrap();
    setup
        .execute("INSERT INTO test_upsert_wide (code) VALUES ('x')", &[])
        .await
        .unwrap();
    setup.commit().await.unwrap();
    drop(setup);
    let mut tx = begin(&h).await;
    let model = tx.env().records("test.upsert.wide").unwrap();
    let mut vals = RawVals::new().set_cell("code", Cell::Str("x".into()));
    let mut names = Vec::new();
    for i in 0..128 {
        let name = format!("f{i:03}");
        vals = vals.set_cell(name.clone(), Cell::Int(i));
        names.push(name);
    }
    model
        .upsert(
            &mut tx,
            UpsertCall::new("_business")
                .with_rows([vals.clone()])
                .with_update(names.clone()),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = begin(&h).await;
    tx.capture_statements(true);
    names.push("f128".into());
    vals = vals.set_cell("f128", Cell::Int(128));
    let err = model
        .with_env(tx.env())
        .upsert(
            &mut tx,
            UpsertCall::new("_business")
                .with_rows([vals])
                .with_update(names.clone()),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::ResourceExhausted);
    assert_eq!(
        err.message(),
        "outbox CaptureLimit: event exceeds 128 changed field names"
    );
    assert!(
        !tx.captured()
            .iter()
            .any(|s| s.contains("INSERT INTO \"test_upsert_wide\""))
    );
    tx.rollback().await.unwrap();
    drop(tx);
    // A new row with 128 supplied columns fits the SQL projection but its create envelope
    // exceeds the name cap once key/audit columns join it. Prove direct base dispatch poisons
    // after the INSERT, even when the caller catches capture admission failure.
    let mut vals = RawVals::new().set_cell("code", Cell::Str("new".into()));
    for i in 0..128 {
        vals = vals.set_cell(format!("f{i:03}"), Cell::Int(i));
    }
    let mut tx = begin(&h).await;
    tx.capture_statements(true);
    let records = tx.env().records("test.upsert.wide").unwrap();
    let err = erp_orm::call_method::<(UpsertCall,), erp_orm::UpsertResult>(
        &mut tx,
        &records,
        "upsert",
        (UpsertCall::new("_business")
            .with_rows([vals])
            .with_update(names[..128].iter().cloned()),),
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::ResourceExhausted);
    assert_eq!(
        err.message(),
        "outbox CaptureLimit: event exceeds 128 changed field names"
    );
    assert!(
        tx.captured()
            .iter()
            .any(|sql| sql.contains("INSERT INTO \"test_upsert_wide\""))
    );
    let err = tx.commit().await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::ResourceExhausted);
    assert_eq!(
        err.message(),
        "outbox CaptureLimit: event exceeds 128 changed field names"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    let mut check = h.begin().await.unwrap();
    assert_eq!(
        check
            .query("SELECT count(*) FROM test_upsert_wide", &[])
            .await
            .unwrap()[0]
            .get::<_, i64>(0),
        1
    );
    check.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn ordinary_upsert_checks_the_field_write_mask() {
    let h = harness_with(false, Denial::Field).await;
    seed(&h).await;
    let baseline = state(&h).await;
    let mut tx = user_begin(&h).await;
    let model = tx.env().records(MODEL).unwrap();
    let ordinary = model
        .browse([baseline[0].0])
        .unwrap()
        .write(&mut tx, RawVals::new().set_cell("number", Cell::Int(3)))
        .await
        .unwrap_err();
    assert_eq!(ordinary.kind(), ErrorKind::Access);
    let err = model
        .upsert(&mut tx, call(vec![row(Some("a"), 3, None)]))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ordinary.kind());
    assert_eq!(err.message(), ordinary.message());
    tx.rollback().await.unwrap();
    assert_eq!(state(&h).await, baseline);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn failed_write_hook_cannot_commit_its_upsert_or_capture() {
    let h = harness(true).await;
    seed(&h).await;
    let baseline = state(&h).await;
    let mut tx = begin(&h).await;
    let op = tx
        .env()
        .operation_identity()
        .unwrap()
        .operation()
        .to_string();
    let err = tx
        .env()
        .records(MODEL)
        .unwrap()
        .upsert(&mut tx, call(vec![row(Some("a"), -77, None)]))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.message(), "upsert fixture rejects write hook");
    let err = tx.commit().await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.message(), "upsert fixture rejects write hook");
    tx.rollback().await.unwrap();
    assert_eq!(state(&h).await, baseline);
    assert!(events(&h, &op).await.is_empty());
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn runtime_grants_and_revoked_event_insert_are_atomic() {
    let h = harness(true).await;
    seed(&h).await;
    let role = format!("upsert_{}", h.db().name());
    let quoted = erp_registry::ident::quote(&role);
    let mut setup = h.begin().await.unwrap();
    setup
        .execute(&format!("CREATE ROLE {quoted} NOLOGIN"), &[])
        .await
        .unwrap();
    setup.grant_event_runtime_role(&role).await.unwrap();
    setup
        .execute(
            &format!("GRANT SELECT,INSERT,UPDATE ON test_outbox_upsert TO {quoted}"),
            &[],
        )
        .await
        .unwrap();
    setup
        .execute(
            &format!("GRANT USAGE ON ALL SEQUENCES IN SCHEMA public TO {quoted}"),
            &[],
        )
        .await
        .unwrap();
    setup.commit().await.unwrap();
    drop(setup);
    let mut tx = begin(&h).await;
    tx.execute(&format!("SET LOCAL ROLE {quoted}"), &[])
        .await
        .unwrap();
    assert_eq!(
        tx.query("SELECT current_user::text", &[]).await.unwrap()[0].get::<_, String>(0),
        role
    );
    tx.env()
        .records(MODEL)
        .unwrap()
        .upsert(&mut tx, call(vec![row(Some("a"), 3, None)]))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let baseline = state(&h).await;
    let mut setup = h.begin().await.unwrap();
    let columns=setup.query("SELECT attname::text FROM pg_attribute WHERE attrelid='erp_event.ir_event_outbox'::regclass AND attnum>0 AND NOT attisdropped AND has_column_privilege($1,'erp_event.ir_event_outbox',attname,'INSERT')",&[&role]).await.unwrap().iter().map(|r|erp_registry::ident::quote(r.get::<_,&str>(0))).collect::<Vec<_>>();
    assert!(!columns.is_empty());
    setup
        .execute(
            &format!(
                "REVOKE INSERT ({}) ON erp_event.ir_event_outbox FROM {quoted}",
                columns.join(",")
            ),
            &[],
        )
        .await
        .unwrap();
    setup.commit().await.unwrap();
    drop(setup);
    let mut tx = begin(&h).await;
    let op = tx
        .env()
        .operation_identity()
        .unwrap()
        .operation()
        .to_string();
    tx.execute(&format!("SET LOCAL ROLE {quoted}"), &[])
        .await
        .unwrap();
    tx.env()
        .records(MODEL)
        .unwrap()
        .upsert(&mut tx, call(vec![row(Some("a"), 4, None)]))
        .await
        .unwrap();
    let err = tx.commit().await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Database);
    assert_eq!(err.sqlstate(), Some("42501"));
    assert_eq!(err.message(), "permission denied for table ir_event_outbox");
    tx.rollback().await.unwrap();
    assert_eq!(state(&h).await, baseline);
    assert!(events(&h, &op).await.is_empty());
    let mut cleanup = h.begin().await.unwrap();
    cleanup
        .execute(&format!("DROP OWNED BY {quoted}"), &[])
        .await
        .unwrap();
    cleanup
        .execute(&format!("DROP ROLE {quoted}"), &[])
        .await
        .unwrap();
    cleanup.commit().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn denied_upsert_after_dirty_write_keeps_transaction_usable() {
    for denial in [Denial::Model, Denial::Field] {
        let h = harness_with(true, denial).await;
        let ids = seed(&h).await;
        let mut tx = user_begin(&h).await;
        let allowed = tx
            .env()
            .sudo(true)
            .records(MODEL)
            .unwrap()
            .browse([ids[0]])
            .unwrap();
        allowed
            .write(&mut tx, RawVals::new().set_cell("number", Cell::Int(9)))
            .await
            .unwrap();
        tx.capture_statements(true);
        let r = tx.env().records(MODEL).unwrap();
        let error = r
            .upsert(&mut tx, call(vec![row(Some("a"), 10, None)]))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access);
        if matches!(denial, Denial::Model) {
            assert_eq!(error.message(), "upsert fixture denies model write");
        } else {
            assert!(error.message().contains("number"), "{error}");
        }
        assert!(
            !tx.captured().iter().any(|s| s.starts_with("UPDATE")),
            "{:?}",
            tx.captured()
        );
        tx.commit().await.unwrap();
        drop(tx);
        assert_eq!(state(&h).await[0].2, 9);
        h.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn rolled_back_hook_savepoint_keeps_nested_mutation_refusal() {
    let h = harness(true).await;
    seed(&h).await;
    let baseline = state(&h).await;
    let mut tx = begin(&h).await;
    let error = tx
        .env()
        .records(MODEL)
        .unwrap()
        .upsert(&mut tx, call(vec![row(Some("a"), 88, None)]))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "a later release S7: nested event-relevant mutations during mutation or compute execution are not implemented"
    );
    let commit = tx.commit().await.unwrap_err();
    assert_eq!(commit.kind(), error.kind());
    assert_eq!(commit.message(), error.message());
    tx.rollback().await.unwrap();
    drop(tx);
    assert_eq!(state(&h).await, baseline);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn recovered_hook_savepoint_keeps_later_chunk_origin() {
    let h = harness(true).await;
    seed(&h).await;
    let mut tx = begin(&h).await;
    let op = tx
        .env()
        .operation_identity()
        .unwrap()
        .operation()
        .to_string();
    let mut rows = vec![row(Some("a"), 87, None)];
    rows.extend((0..128).map(|n| row(Some(&format!("later{n:03}")), n, None)));
    tx.env()
        .records(MODEL)
        .unwrap()
        .upsert(&mut tx, call(rows))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let events = events(&h, &op).await;
    assert!(!events.is_empty());
    assert!(events.iter().all(|e| e.operation == op));
    assert_eq!(state(&h).await.len(), 130);
    h.cleanup().await.unwrap();
}
