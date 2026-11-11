//! Bounded S9 acceptance of actual scalar upsert outcomes and capture.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
#[path = "support/outbox_upsert.rs"]
mod fixture;
use erp_fields::Cell;
use erp_orm::{RawVals, UpsertCall, UpsertOutcome};
use fixture::*;

#[tokio::test]
async fn mixed_outcomes_capture_actual_ids_and_names() {
    let h = harness(true).await;
    let ids = seed(&h).await;
    let mut tx = begin(&h).await;
    let op = tx
        .env()
        .operation_identity()
        .unwrap()
        .operation()
        .to_string();
    tx.capture_statements(true);
    let result = tx
        .env()
        .records(MODEL)
        .unwrap()
        .upsert(
            &mut tx,
            call(vec![
                row(Some("c"), 3, Some("秘密")),
                row(Some("a"), 4, Some("first")),
                row(Some("b"), 2, None),
            ]),
        )
        .await
        .unwrap();
    assert_eq!(
        result
            .rows()
            .iter()
            .map(|r| r.outcome())
            .collect::<Vec<_>>(),
        [
            UpsertOutcome::Inserted,
            UpsertOutcome::Updated,
            UpsertOutcome::Unchanged
        ]
    );
    assert_eq!(result.rows()[1].id().get(), ids[0]);
    assert_eq!(result.rows()[2].id().get(), ids[1]);
    tx.commit().await.unwrap();
    assert_eq!(inserts(&tx), 1);
    assert!(
        !tx.captured()
            .iter()
            .any(|s| s.starts_with("SELECT") && s.contains(TABLE))
    );
    let events = events(&h, &op).await;
    let writes: Vec<_> = events.iter().filter(|e| e.kind == "write").collect();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].ids, vec![ids[0]]);
    assert_eq!(writes[0].fields, vec!["number", "twice"]);
    let creates: Vec<_> = events.iter().filter(|e| e.kind == "create").collect();
    assert_eq!(creates.len(), 1);
    assert_eq!(creates[0].ids, vec![result.rows()[0].id().get()]);
    assert!(creates[0].fields.contains(&"code".into()));
    assert_eq!(
        state(&h).await.iter().map(|r| r.2).collect::<Vec<_>>(),
        [4, 2, 3]
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn nulls_and_accumulations_use_database_differences() {
    let h = harness(true).await;
    seed(&h).await;
    let mut tx = begin(&h).await;
    let op = tx
        .env()
        .operation_identity()
        .unwrap()
        .operation()
        .to_string();
    tx.capture_statements(true);
    let model = tx.env().records(MODEL).unwrap();
    model
        .upsert(&mut tx, call(vec![row(Some("a"), 1, Some("first"))]))
        .await
        .unwrap();
    model
        .upsert(
            &mut tx,
            UpsertCall::new("_business").with_rows([row(Some("a"), 99, Some("ignored"))]),
        )
        .await
        .unwrap();
    model
        .upsert(
            &mut tx,
            UpsertCall::new("_business")
                .with_rows([row(Some("a"), 0, None)])
                .with_accumulate("number"),
        )
        .await
        .unwrap();
    model
        .upsert(
            &mut tx,
            UpsertCall::new("_business")
                .with_rows([row(Some("a"), -5, None)])
                .with_accumulate_floor("number", Cell::Int(1)),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(inserts(&tx), 0);
    assert!(events(&h, &op).await.is_empty());
    let mut tx = begin(&h).await;
    let op = tx
        .env()
        .operation_identity()
        .unwrap()
        .operation()
        .to_string();
    model
        .with_env(tx.env())
        .upsert(&mut tx, call(vec![row(Some("a"), 1, None)]))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let e = events(&h, &op).await;
    assert_eq!(e.len(), 1);
    assert_eq!(e[0].fields, vec!["note"]);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn duplicates_capture_physical_rows_once_and_split_entries() {
    let h = harness(true).await;
    let mut tx = begin(&h).await;
    let op = tx
        .env()
        .operation_identity()
        .unwrap()
        .operation()
        .to_string();
    let mut rows = (0..257)
        .map(|i| row(Some(&format!("key-{i:03}")), i, None))
        .collect::<Vec<_>>();
    rows.push(row(None, 7, None));
    rows.push(row(None, 9, None));
    let result = tx
        .env()
        .records(MODEL)
        .unwrap()
        .upsert(&mut tx, call(rows))
        .await
        .unwrap();
    assert_eq!(result.rows()[257].id(), result.rows()[258].id());
    tx.commit().await.unwrap();
    let e = events(&h, &op).await;
    let actual = e
        .iter()
        .flat_map(|e| e.ids.iter().copied())
        .collect::<Vec<_>>();
    let mut expected = result
        .distinct_ids()
        .iter()
        .map(|id| id.get())
        .collect::<Vec<_>>();
    expected.sort_unstable();
    let mut actual = actual;
    actual.sort_unstable();
    assert_eq!(actual, expected);
    assert!(e.iter().all(|e| e.ids.len() <= 256));
    assert_eq!(state(&h).await.last().unwrap().2, 9);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn writer_attribution_survives_base_flusher_and_successive_operations() {
    let h = harness(true).await;
    let ids = seed(&h).await;
    let mut tx = begin(&h).await;
    let writer = tx.env().with_user(7).sudo(true);
    let rec = writer.records(MODEL).unwrap();
    let mut operations = Vec::new();
    for n in [4, 5, 6] {
        let mut donor = begin(&h).await;
        let writer = donor.env().with_user(7).sudo(true);
        operations.push(writer.operation_identity().unwrap().operation().to_string());
        donor.rollback().await.unwrap();
        rec.with_env(writer)
            .upsert(&mut tx, call(vec![row(Some("a"), n, Some("first"))]))
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    for operation in &operations {
        let e = events(&h, operation).await;
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].subject, "7");
        assert_eq!(e[0].ids, vec![ids[0]]);
    }
    let mut check = h.begin().await.unwrap();
    let actual=check.query("SELECT operation_id::text FROM erp_event.ir_event_outbox WHERE operation_id::text=ANY($1) ORDER BY id",&[&operations]).await.unwrap().iter().map(|r|r.get::<_,String>(0)).collect::<Vec<_>>();
    assert_eq!(actual, operations);
    check.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn empty_and_plain_upserts_need_no_capture_identity() {
    let h = harness(false).await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records(MODEL).unwrap();
    tx.capture_statements(true);
    assert!(
        model
            .upsert(&mut tx, call(Vec::<RawVals>::new()))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(tx.captured().is_empty());
    model
        .upsert(&mut tx, call(vec![row(None, 1, None)]))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(inserts(&tx), 0);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn deferred_computes_bind_the_upsert_writer_before_pending() {
    use computed::Computed;
    let h = harness(true).await;
    let mut tx = begin(&h).await;
    let writer = tx.env().with_user(7).sudo(true);
    let op = writer.operation_identity().unwrap().operation().to_string();
    let model = writer.records("test.outbox.upsert.computed").unwrap();
    let vals = |n| {
        RawVals::new()
            .set_cell("code", Cell::Str("x".into()))
            .set_cell("source", Cell::Int(n))
    };
    let inserted = model
        .upsert(
            &mut tx,
            UpsertCall::new("_business")
                .with_rows([vals(2)])
                .with_update(["source"]),
        )
        .await
        .unwrap();
    model
        .upsert(
            &mut tx,
            UpsertCall::new("_business")
                .with_rows([vals(3)])
                .with_update(["source"]),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let e = events(&h, &op).await;
    assert!(e.iter().all(|e| e.subject == "7"));
    assert!(
        e.iter()
            .any(|e| e.kind == "write" && e.fields == vec!["doubled"])
    );
    let mut check = h.begin().await.unwrap();
    let rec = check
        .env()
        .model::<Computed>()
        .unwrap()
        .browse(inserted.ids().iter().map(|id| id.get()))
        .unwrap();
    assert_eq!(
        rec.mapped(&mut check, Computed::DOUBLED).await.unwrap(),
        vec![6]
    );
    check.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn ordinary_user_upsert_captures_subject_and_actor() {
    let h = harness(true).await;
    seed(&h).await;
    let mut tx = user_begin(&h).await;
    let op = tx
        .env()
        .operation_identity()
        .unwrap()
        .operation()
        .to_string();
    tx.env()
        .records(MODEL)
        .unwrap()
        .upsert(&mut tx, call(vec![row(Some("a"), 3, None)]))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut check = h.begin().await.unwrap();
    let rows=check.query("SELECT subject,actor_id,actor_kind,delegating_user_id FROM erp_event.ir_event_outbox WHERE operation_id::text=$1",&[&op]).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, String>(0), "7");
    assert_eq!(rows[0].get::<_, i64>(1), 7);
    assert_eq!(rows[0].get::<_, String>(2), "user");
    assert_eq!(rows[0].get::<_, Option<i64>>(3), None);
    check.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn accumulation_hooks_and_versions_preserve_projection_layout() {
    use std::sync::atomic::Ordering;
    HOOK_HITS.store(0, Ordering::SeqCst);
    let h = harness(true).await;
    let ids = seed(&h).await;
    let mut tx = begin(&h).await;
    let op = tx
        .env()
        .operation_identity()
        .unwrap()
        .operation()
        .to_string();
    let model = tx.env().records(MODEL).unwrap();
    let out = model
        .upsert(
            &mut tx,
            UpsertCall::new("_business")
                .with_rows([row(Some("a"), 76, None)])
                .with_accumulate("number"),
        )
        .await
        .unwrap();
    assert_eq!(out.rows()[0].previous_version(), Some(1));
    assert_eq!(out.rows()[0].version(), Some(2));
    assert_eq!(HOOK_HITS.load(Ordering::SeqCst), 1);
    let unchanged = model
        .upsert(
            &mut tx,
            UpsertCall::new("_business")
                .with_rows([row(Some("a"), 0, None)])
                .with_accumulate("number"),
        )
        .await
        .unwrap();
    assert_eq!(unchanged.rows()[0].outcome(), UpsertOutcome::Unchanged);
    assert_eq!(unchanged.rows()[0].version(), Some(2));
    tx.commit().await.unwrap();
    let e = events(&h, &op).await;
    assert_eq!(e.len(), 1);
    assert_eq!(e[0].ids, vec![ids[0]]);
    assert_eq!(e[0].fields, vec!["number", "twice"]);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn json_equality_and_bookkeeping_only_float_update_emit_nothing() {
    use std::sync::Arc;
    let h = harness(true).await;
    let mut setup = begin(&h).await;
    let vals = |payload| {
        row(Some("float"), 0, None)
            .set_cell("amount", Cell::Float(1e20))
            .set_cell("payload", Cell::Json(Arc::new(payload)))
    };
    setup
        .env()
        .records(MODEL)
        .unwrap()
        .upsert(
            &mut setup,
            UpsertCall::new("_business").with_rows([vals(serde_json::json!({"x":1}))]),
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
    tx.capture_statements(true);
    let model = tx.env().records(MODEL).unwrap();
    let out = model
        .upsert(
            &mut tx,
            UpsertCall::new("_business")
                .with_rows([vals(serde_json::json!({"x":1.0}))])
                .with_update(["payload"]),
        )
        .await
        .unwrap();
    assert_eq!(out.rows()[0].outcome(), UpsertOutcome::Unchanged);
    // Nonzero delta forces the existing physical update predicate, but IEEE rounding leaves
    // the stored amount equal. Only bookkeeping changes, so no business event may be invented.
    let out = model
        .upsert(
            &mut tx,
            UpsertCall::new("_business")
                .with_rows([row(Some("float"), 0, None).set_cell("amount", Cell::Float(1.0))])
                .with_accumulate("amount"),
        )
        .await
        .unwrap();
    assert_eq!(out.rows()[0].outcome(), UpsertOutcome::Updated);
    tx.commit().await.unwrap();
    assert_eq!(inserts(&tx), 0);
    assert!(events(&h, &op).await.is_empty());
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn writer_companies_and_identity_are_bound_before_commit() {
    let h = harness(true).await;
    seed(&h).await;
    let mut donor = user_begin_companies(&h, &[10, 20]).await;
    let writer = donor.env();
    let expected = donor
        .companies(&writer)
        .await
        .unwrap()
        .into_iter()
        .map(|id| id.get())
        .collect::<Vec<_>>();
    let op = writer.operation_identity().unwrap().operation().to_string();
    donor.rollback().await.unwrap();
    drop(donor);
    let mut tx = begin(&h).await;
    assert_ne!(
        tx.env().operation_identity().unwrap().operation().as_str(),
        op
    );
    writer
        .records(MODEL)
        .unwrap()
        .upsert(&mut tx, call(vec![row(Some("a"), 4, None)]))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut check = h.begin().await.unwrap();
    let rows = check
        .query(
            "SELECT company_ids,subject FROM erp_event.ir_event_outbox WHERE operation_id::text=$1",
            &[&op],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, Vec<i64>>(0), expected);
    assert_eq!(rows[0].get::<_, String>(1), "7");
    check.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn typed_and_value_upsert_surfaces_capture_returned_rows() {
    use computed::Computed;
    let h = harness(true).await;
    let mut tx = begin(&h).await;
    let op = tx
        .env()
        .operation_identity()
        .unwrap()
        .operation()
        .to_string();
    let model = tx.env().model::<Computed>().unwrap();
    let out = model
        .upsert(
            &mut tx,
            erp_orm::Upsert::on(Computed::BUSINESS).rows([model
                .vals()
                .set(Computed::CODE, "typed")
                .set(Computed::SOURCE, 2)]),
        )
        .await
        .unwrap();
    let dynamic = tx
        .env()
        .records(MODEL)
        .unwrap()
        .upsert_values(
            &mut tx,
            "_business",
            &[std::collections::BTreeMap::from([
                ("code".into(), erp_fields::Value::str("dynamic")),
                ("number".into(), erp_fields::Value::Int(3)),
            ])],
            &[],
            &[],
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let e = events(&h, &op).await;
    assert!(
        e.iter()
            .any(|e| e.kind == "create" && e.ids == out.records().ids())
    );
    assert!(e.iter().any(|e| e.kind == "create"
        && e.ids == dynamic.ids().iter().map(|id| id.get()).collect::<Vec<_>>()));
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn non_eventful_source_upsert_captures_only_dependent_changes() {
    use cross_model::{Sink, Source};
    let h = harness(true).await;
    let mut setup = begin(&h).await;
    let source = setup.env().model::<Source>().unwrap();
    let source = source
        .create(
            &mut setup,
            vec![
                source
                    .vals()
                    .set(Source::CODE, "source")
                    .set(Source::NUMBER, 1),
            ],
        )
        .await
        .unwrap();
    let sink = setup.env().model::<Sink>().unwrap();
    let sink = sink
        .create(
            &mut setup,
            vec![sink.vals().set(Sink::SOURCE_ID, source.rec_ids()[0])],
        )
        .await
        .unwrap();
    setup.commit().await.unwrap();
    drop(setup);
    let mut tx = user_begin(&h).await;
    let op = tx
        .env()
        .operation_identity()
        .unwrap()
        .operation()
        .to_string();
    tx.env()
        .model::<Source>()
        .unwrap()
        .upsert(
            &mut tx,
            erp_orm::Upsert::on(Source::BUSINESS)
                .rows([source
                    .vals()
                    .set(Source::CODE, "source")
                    .set(Source::NUMBER, 4)])
                .update([Source::NUMBER]),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let e = events(&h, &op).await;
    assert_eq!(e.len(), 1);
    assert_eq!(e[0].ids, sink.ids());
    assert_eq!(e[0].fields, vec!["copied"]);
    assert_eq!(e[0].subject, "7");
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn mixed_create_write_upsert_operations_keep_order() {
    let h = harness(true).await;
    let mut tx = begin(&h).await;
    tx.capture_statements(true);
    let mut envs = Vec::new();
    for _ in 0..5 {
        let mut donor = begin(&h).await;
        envs.push(donor.env().clone());
        donor.rollback().await.unwrap();
    }
    let ops: Vec<_> = envs
        .iter()
        .map(|e| e.operation_identity().unwrap().operation().to_string())
        .collect();
    let made = envs[0]
        .records(MODEL)
        .unwrap()
        .create(&mut tx, vec![row(Some("mixed"), 1, None)])
        .await
        .unwrap();
    made.with_env(envs[1].clone())
        .write(&mut tx, RawVals::new().set_cell("number", Cell::Int(2)))
        .await
        .unwrap();
    envs[2]
        .records(MODEL)
        .unwrap()
        .upsert(&mut tx, call(vec![row(Some("mixed"), 3, None)]))
        .await
        .unwrap();
    made.with_env(envs[3].clone())
        .write(&mut tx, RawVals::new().set_cell("number", Cell::Int(4)))
        .await
        .unwrap();
    envs[4]
        .records(MODEL)
        .unwrap()
        .upsert(&mut tx, call(vec![row(Some("mixed"), 5, None)]))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(inserts(&tx), 1);
    drop(tx);
    let mut check = h.begin().await.unwrap();
    let actual=check.query("SELECT operation_id::text,kind,field_names,record_ids FROM erp_event.ir_event_outbox WHERE operation_id::text=ANY($1) ORDER BY tx_ordinal,id", &[&ops]).await.unwrap();
    assert_eq!(
        actual
            .iter()
            .map(|r| r.get::<_, String>(0))
            .collect::<Vec<_>>(),
        ops
    );
    assert_eq!(
        actual
            .iter()
            .map(|r| r.get::<_, String>(1))
            .collect::<Vec<_>>(),
        ["create", "write", "write", "write", "write"]
    );
    for r in &actual[1..] {
        assert_eq!(r.get::<_, Vec<String>>(2), ["number", "twice"]);
    }
    check.rollback().await.unwrap();
    drop(check);
    assert_eq!(state(&h).await[0].2, 5);
    h.cleanup().await.unwrap();
}
