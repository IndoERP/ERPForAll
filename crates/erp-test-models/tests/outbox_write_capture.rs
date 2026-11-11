//! S7 database acceptance: actual scalar UPDATEs, no-op filtering and capture provenance.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_fields::{Cell, ErrorKind};
use erp_orm::{BeginOptions, OperationIdentity, RawVals, SUPERUSER_ID, Transaction};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl};
use erp_test_models::base_users;
use erp_testkit::Harness;

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "test_write_capture",
    depends: &["base"],
    exact_only: false,
};
const MODEL: &str = "test.write.capture";
const TABLE: &str = "\"erp_event\".\"ir_event_outbox\"";

async fn harness() -> (Harness, i64) {
    let fields = const { &[FieldDecl::char("name"), FieldDecl::integer("number")] };
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([ModelDecl::new("test_write_capture", MODEL)
            .eventful(true)
            .fields(fields)])
        .build()
        .await
        .unwrap();
    let mut tx = begin(&h).await;
    tx.install_event_schema().await.unwrap();
    tx.execute("INSERT INTO res_users (id, name, login, active) VALUES (7, 'capture writer', 'capture-writer', true)", &[]).await.unwrap();
    let id = tx
        .env()
        .records(MODEL)
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("initial".into()))
                    .set_cell("number", Cell::Int(1)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    tx.commit().await.unwrap();
    (h, id)
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
fn inserts(tx: &Transaction) -> usize {
    tx.captured()
        .iter()
        .filter(|s| s.starts_with("INSERT INTO \"erp_event\".\"ir_event_outbox\""))
        .count()
}
async fn write(tx: &mut Transaction, id: i64, vals: RawVals) {
    tx.env()
        .records(MODEL)
        .unwrap()
        .browse([id])
        .unwrap()
        .write(tx, vals)
        .await
        .unwrap();
}
async fn names(tx: &mut Transaction) -> Vec<Vec<String>> {
    tx.query(
        &format!("SELECT field_names FROM {TABLE} WHERE kind = 'write' ORDER BY id"),
        &[],
    )
    .await
    .unwrap()
    .into_iter()
    .map(|r| r.get(0))
    .collect()
}

#[tokio::test]
async fn actual_updates_capture_only_changed_names_and_no_values() {
    let (h, id) = harness().await;
    let mut tx = begin(&h).await;
    tx.capture_statements(true);
    write(
        &mut tx,
        id,
        RawVals::new().set_cell("name", Cell::Str("initial".into())),
    )
    .await;
    tx.flush().await.unwrap();
    write(
        &mut tx,
        id,
        RawVals::new()
            .set_cell("name", Cell::Str("private replacement".into()))
            .set_cell("number", Cell::Int(1)),
    )
    .await;
    tx.flush().await.unwrap();
    write(&mut tx, id, RawVals::new().set_cell("name", Cell::Null)).await;
    tx.flush().await.unwrap();
    write(&mut tx, id, RawVals::new().set_cell("name", Cell::Null)).await;
    tx.commit().await.unwrap();
    assert_eq!(inserts(&tx), 1);
    assert!(
        !tx.captured()
            .iter()
            .any(|s| s.starts_with("SELECT") && s.contains("test_write_capture")),
        "capture must use UPDATE RETURNING, not a pre-read: {:?}",
        tx.captured()
    );
    let mut check = h.begin().await.unwrap();
    assert_eq!(
        names(&mut check).await,
        vec![vec!["name".to_owned()], vec!["name".to_owned()]]
    );
    let value: Option<String> = check
        .query("SELECT name FROM test_write_capture WHERE id = $1", &[&id])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(value, None);
    check.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn successive_writes_without_explicit_flush_keep_each_operation() {
    let (h, id) = harness().await;
    let mut tx = begin(&h).await;
    for value in [2, 3, 1] {
        write(
            &mut tx,
            id,
            RawVals::new().set_cell("number", Cell::Int(value)),
        )
        .await;
    }
    tx.commit().await.unwrap();
    let mut check = h.begin().await.unwrap();
    assert_eq!(names(&mut check).await, vec![vec!["number".to_owned()]; 3]);
    let rows = check.query(&format!("SELECT tx_ordinal, operation_id::text FROM {TABLE} WHERE kind = 'write' ORDER BY id"), &[]).await.unwrap();
    let ordinals: Vec<i64> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(ordinals, vec![1, 2, 3]);
    assert_eq!(rows[0].get::<_, String>(1), rows[2].get::<_, String>(1));
    check.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn missing_identity_is_refused_before_staging() {
    let (h, id) = harness().await;
    let mut tx = h.begin().await.unwrap();
    let err = tx
        .env()
        .records(MODEL)
        .unwrap()
        .browse([id])
        .unwrap()
        .write(&mut tx, RawVals::new().set_cell("number", Cell::Int(9)))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "outbox: eventful mutation requires an OperationIdentity"
    );
    tx.rollback().await.unwrap();
    let mut check = h.begin().await.unwrap();
    assert!(names(&mut check).await.is_empty());
    let number: i64 = check
        .query(
            "SELECT number::bigint FROM test_write_capture WHERE id = $1",
            &[&id],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(number, 1);
    check.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn writer_identity_survives_the_base_environment_flusher() {
    let (h, id) = harness().await;
    let mut tx = begin(&h).await;
    let writer = tx.env().with_user(7).sudo(true);
    let op = writer.operation_identity().unwrap().operation().to_string();
    writer
        .records(MODEL)
        .unwrap()
        .browse([id])
        .unwrap()
        .write(&mut tx, RawVals::new().set_cell("number", Cell::Int(5)))
        .await
        .unwrap();
    tx.flush().await.unwrap();
    tx.commit().await.unwrap();
    let mut check = h.begin().await.unwrap();
    let rows = check.query(&format!("SELECT subject, actor_id, delegating_user_id, operation_id::text FROM {TABLE} WHERE kind = 'write'"), &[]).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, String>(0), "7");
    assert_eq!(rows[0].get::<_, i64>(1), SUPERUSER_ID.get());
    assert_eq!(rows[0].get::<_, Option<i64>>(2), Some(SUPERUSER_ID.get()));
    assert_eq!(rows[0].get::<_, String>(3), op);
    check.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn scalar_write_savepoints_and_root_reset_keep_capture_with_business_state() {
    let (h, id) = harness().await;
    let mut tx = begin(&h).await;
    tx.savepoint(async |tx| {
        write(tx, id, RawVals::new().set_cell("number", Cell::Int(2))).await;
        Ok(())
    })
    .await
    .unwrap();
    let err = tx
        .savepoint(async |tx| {
            write(tx, id, RawVals::new().set_cell("number", Cell::Int(3))).await;
            tx.flush().await?;
            Err::<(), _>(erp_fields::Error::programming("discard scalar write"))
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(err.message(), "discard scalar write");
    tx.commit().await.unwrap();
    write(&mut tx, id, RawVals::new().set_cell("number", Cell::Int(4))).await;
    tx.rollback().await.unwrap();
    tx.commit().await.unwrap();
    let mut check = h.begin().await.unwrap();
    assert_eq!(names(&mut check).await, vec![vec!["number".to_owned()]]);
    let number: i64 = check
        .query(
            "SELECT number::bigint FROM test_write_capture WHERE id = $1",
            &[&id],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(number, 2);
    check.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn isolated_scope_restores_preexisting_pending_provenance() {
    let (h, id) = harness().await;
    let mut tx = begin(&h).await;
    let writer = tx.env().with_user(7).sudo(true);
    writer
        .records(MODEL)
        .unwrap()
        .browse([id])
        .unwrap()
        .write(&mut tx, RawVals::new().set_cell("number", Cell::Int(6)))
        .await
        .unwrap();
    tx.isolated_for_tests(async |tx| {
        tx.flush().await?;
        write(tx, id, RawVals::new().set_cell("number", Cell::Int(8))).await;
        tx.flush().await
    })
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let mut check = h.begin().await.unwrap();
    assert_eq!(names(&mut check).await, vec![vec!["number".to_owned()]]);
    let subjects = check
        .query(
            &format!("SELECT subject FROM {TABLE} WHERE kind = 'write'"),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(subjects[0].get::<_, String>(0), "7");
    let number: i64 = check
        .query(
            "SELECT number::bigint FROM test_write_capture WHERE id = $1",
            &[&id],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(number, 6);
    check.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

mod computed_fixture {
    use erp_sdk::prelude::*;

    /// Stored late output for capture-attribution acceptance.
    #[model(
        name = "test.write.capture.derived",
        addon = "test_write_capture",
        eventful = true
    )]
    pub struct Derived {
        #[field(integer)]
        pub source: i32,
        #[field(integer, compute = "_double", store, precompute = false, compute_sudo)]
        pub doubled: i32,
    }
    #[methods(Derived, addon = "test_write_capture")]
    impl Derived {
        #[compute(depends = ["source"])]
        fn _double(batch: &mut Batch<'_, Derived>) -> Result<()> {
            for i in batch.indices() {
                let value = batch.get(i, Derived::SOURCE)?;
                batch.set(i, Derived::DOUBLED, value * 2)?;
            }
            Ok(())
        }
    }
}

#[tokio::test]
async fn late_stored_compute_keeps_origin_separate_from_write_uid() {
    use computed_fixture::Derived;
    let (h, _) = harness().await;
    let mut tx = begin(&h).await;
    let model = tx.env().model::<Derived>().unwrap();
    let rec = model
        .create(&mut tx, vec![model.vals().set(Derived::SOURCE, 2_i32)])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let baseline: i64 = tx
        .query(&format!("SELECT coalesce(max(id), 0) FROM {TABLE}"), &[])
        .await
        .unwrap()[0]
        .get(0);
    let writer = rec.with_env(tx.env().with_user(7).sudo(true));
    writer.set(&mut tx, Derived::SOURCE, 4_i32).await.unwrap();
    tx.flush().await.unwrap();
    tx.commit().await.unwrap();
    let mut check = h.begin().await.unwrap();
    let rows = check.query(&format!("SELECT field_names, subject FROM {TABLE} WHERE kind = 'write' AND model = 'test.write.capture.derived' AND id > $1 ORDER BY id"), &[&baseline]).await.unwrap();
    assert!(!rows.is_empty());
    assert!(
        rows.iter()
            .any(|r| r.get::<_, Vec<String>>(0).contains(&"doubled".to_owned()))
    );
    assert!(rows.iter().all(|r| r.get::<_, String>(1) == "7"));
    let data = check
        .query(
            "SELECT doubled::bigint, write_uid FROM test_write_capture_derived WHERE id = $1",
            &[&rec.ids()[0]],
        )
        .await
        .unwrap();
    assert_eq!(data[0].get::<_, i64>(0), 8);
    // Explicit write_uid may accompany the scalar batch. A recompute-only fixture below checks
    // the established base-flusher rule without a direct write on the output model.
    check.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

mod nested_fixture {
    use erp_sdk::prelude::*;
    static NAKED: std::sync::Mutex<Option<Env>> = std::sync::Mutex::new(None);
    // Only the caught-refusal test installs this callback input; other fixtures use values1/9.
    pub(super) fn set_naked(env: Option<Env>) {
        *NAKED.lock().unwrap() = env;
    }
    /// A compute whose explicit side effect exercises the current reentrancy refusal.
    #[model(
        name = "test.write.capture.nested",
        addon = "test_write_capture",
        eventful = true
    )]
    pub struct Nested {
        #[field(integer)]
        pub source: i32,
        #[field(integer, compute = "_result", store, precompute = false)]
        pub result: i32,
    }
    #[methods(Nested, addon = "test_write_capture")]
    impl Nested {
        #[compute(depends = ["source"])]
        async fn _result(
            this: &RecordSet<Nested>,
            tx: &mut Transaction,
            out: &mut Assign<'_, Nested>,
        ) -> Result<()> {
            let values = this.mapped(tx, Nested::SOURCE).await?;
            for (i, value) in values.into_iter().enumerate() {
                if value == 9 || value == 10 || value == 11 {
                    let attempted_env = if value == 11 {
                        let env = NAKED.lock().unwrap().as_ref().unwrap().clone();
                        assert!(Arc::ptr_eq(env.registry(), this.env().registry()));
                        assert!(env.operation_identity().is_none());
                        env
                    } else {
                        this.env().clone()
                    };
                    let attempted = attempted_env
                        .records("test.write.capture")?
                        .create(
                            tx,
                            vec![
                                RawVals::new()
                                    .set_cell("name", Cell::Str("nested side effect".into())),
                            ],
                        )
                        .await;
                    if value == 9 {
                        attempted?;
                    } else {
                        let error = attempted.unwrap_err();
                        assert_eq!(error.kind(), erp_fields::ErrorKind::Programming);
                        assert_eq!(
                            error.message(),
                            "a later release S7: nested event-relevant mutations during mutation or compute execution are not implemented"
                        );
                        // Intentional catching exercises the mandatory outer rollback boundary.
                    }
                }
                out.set(i, Nested::RESULT, value)?;
            }
            Ok(())
        }
    }
}

#[tokio::test]
async fn nested_compute_side_effect_refuses_and_outer_change_cannot_commit() {
    use nested_fixture::Nested;
    let (h, _) = harness().await;
    let mut tx = begin(&h).await;
    let model = tx.env().model::<Nested>().unwrap();
    let rec = model
        .create(&mut tx, vec![model.vals().set(Nested::SOURCE, 1_i32)])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let baseline: i64 = tx
        .query(&format!("SELECT coalesce(max(id), 0) FROM {TABLE}"), &[])
        .await
        .unwrap()[0]
        .get(0);
    rec.set(&mut tx, Nested::SOURCE, 9_i32).await.unwrap();
    let error = tx.flush().await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "a later release S7: nested event-relevant mutations during mutation or compute execution are not implemented"
    );
    let commit_error = tx.commit().await.unwrap_err();
    assert_eq!(commit_error.kind(), ErrorKind::Programming);
    assert_eq!(
        commit_error.message(),
        "a later release S7: nested event-relevant mutations during mutation or compute execution are not implemented"
    );
    tx.rollback().await.unwrap();
    let mut check = h.begin().await.unwrap();
    let count: i64 = check
        .query(
            &format!("SELECT count(*) FROM {TABLE} WHERE id > $1"),
            &[&baseline],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(count, 0);
    let count: i64 = check
        .query(
            "SELECT count(*) FROM test_write_capture WHERE name = 'nested side effect'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(count, 0);
    let value: i32 = check
        .query(
            "SELECT source FROM test_write_capture_nested WHERE id = $1",
            &[&rec.ids()[0]],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(value, 1);
    check.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

mod cross_model_fixture {
    use erp_sdk::prelude::*;
    /// A non-eventful dependency source.
    #[model(name = "test.write.capture.source", addon = "test_write_capture")]
    pub struct Source {
        #[field(integer)]
        pub number: i32,
        #[field(char)]
        pub unrelated: Option<Arc<str>>,
    }
    /// An eventful dependent with an ordinary stored related field.
    #[model(
        name = "test.write.capture.sink",
        addon = "test_write_capture",
        eventful = true
    )]
    pub struct Sink {
        #[field(many2one, comodel = "test.write.capture.source")]
        pub source_id: Option<RecId>,
        #[field(integer, related = "source_id.number", store)]
        pub copied: i32,
    }
}

#[tokio::test]
async fn cross_model_recompute_keeps_writer_but_uses_flusher_write_uid() {
    use cross_model_fixture::{Sink, Source};
    let (h, _) = harness().await;
    let mut tx = begin(&h).await;
    let src = tx.env().model::<Source>().unwrap();
    let src = src
        .create(&mut tx, vec![src.vals().set(Source::NUMBER, 2_i32)])
        .await
        .unwrap();
    let sink = tx
        .env()
        .records("test.write.capture.sink")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("source_id", Cell::Id(src.ids()[0]))],
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    src.with_env(tx.env().with_user(7).sudo(true))
        .set(&mut tx, Source::NUMBER, 9_i32)
        .await
        .unwrap();
    tx.flush().await.unwrap();
    tx.commit().await.unwrap();
    let mut check = h.begin().await.unwrap();
    let rows = check
        .query(
            &format!(
                "SELECT model, field_names, subject FROM {TABLE} WHERE kind = 'write' ORDER BY id"
            ),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, String>(0), "test.write.capture.sink");
    assert_eq!(rows[0].get::<_, Vec<String>>(1), vec!["copied"]);
    assert_eq!(rows[0].get::<_, String>(2), "7");
    let data = check
        .query(
            "SELECT copied::bigint, write_uid FROM test_write_capture_sink WHERE id = $1",
            &[&sink.ids()[0]],
        )
        .await
        .unwrap();
    assert_eq!(data[0].get::<_, i64>(0), 9);
    assert_eq!(data[0].get::<_, i64>(1), SUPERUSER_ID.get());
    check.rollback().await.unwrap();
    // Exercise the actual typed accessor as well as raw persisted values.
    assert_eq!(
        tx.env()
            .model::<Sink>()
            .unwrap()
            .browse(sink.ids())
            .unwrap()
            .get(&mut tx, Sink::COPIED)
            .await
            .unwrap(),
        9
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn cold_noop_write_has_no_outbox_insert() {
    let (h, id) = harness().await;
    let mut tx = begin(&h).await;
    tx.capture_statements(true);
    write(&mut tx, id, RawVals::new().set_cell("number", Cell::Int(1))).await;
    tx.commit().await.unwrap();
    assert_eq!(inserts(&tx), 0);
    let mut check = h.begin().await.unwrap();
    assert!(names(&mut check).await.is_empty());
    check.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn large_scalar_write_captures_every_returned_id_in_bounded_groups() {
    let (h, _) = harness().await;
    let mut tx = begin(&h).await;
    let records = tx
        .env()
        .records(MODEL)
        .unwrap()
        .create(
            &mut tx,
            (0..300)
                .map(|i| RawVals::new().set_cell("number", Cell::Int(i)))
                .collect(),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    tx.capture_statements(true);
    records
        .write(&mut tx, RawVals::new().set_cell("number", Cell::Int(999)))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(inserts(&tx), 1);
    let mut check = h.begin().await.unwrap();
    let rows = check
        .query(
            &format!(
                "SELECT record_ids::text, field_names FROM {TABLE} WHERE kind = 'write' ORDER BY id"
            ),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    let mut actual = Vec::new();
    for row in rows {
        let ids: Vec<i64> = serde_json::from_str(row.get(0)).unwrap();
        assert!(!ids.is_empty() && ids.len() <= 256);
        assert_eq!(row.get::<_, Vec<String>>(1), vec!["number"]);
        actual.extend(ids);
    }
    actual.sort_unstable();
    let mut expected = records.ids();
    expected.sort_unstable();
    assert_eq!(actual, expected);
    check.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn missing_update_target_refuses_and_rolls_back_written_prefix() {
    let (h, id) = harness().await;
    let mut tx = begin(&h).await;
    let records = tx
        .env()
        .records(MODEL)
        .unwrap()
        .browse([id, id + 100_000])
        .unwrap();
    let result = records
        .write(&mut tx, RawVals::new().set_cell("number", Cell::Int(88)))
        .await;
    let error = match result {
        Ok(()) => tx.flush().await.unwrap_err(),
        Err(error) => error,
    };
    assert_eq!(error.kind(), ErrorKind::Missing);
    let expected = format!(
        "Record does not exist or has been deleted.\n(Record: test.write.capture({},), User: {})",
        id + 100_000,
        SUPERUSER_ID.get()
    );
    assert_eq!(error.message(), expected);
    let commit_error = tx.commit().await.unwrap_err();
    assert_eq!(commit_error.kind(), ErrorKind::Missing);
    assert_eq!(commit_error.message(), expected);
    tx.rollback().await.unwrap();
    let mut check = h.begin().await.unwrap();
    assert!(names(&mut check).await.is_empty());
    let number: i64 = check
        .query(
            "SELECT number::bigint FROM test_write_capture WHERE id = $1",
            &[&id],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(number, 1);
    check.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn generated_and_json_fields_report_actual_database_changes() {
    use std::sync::Arc;
    let fields = const {
        &[
            FieldDecl::integer("number"),
            FieldDecl::integer("total")
                .generated("number * 2")
                .generated_storage(erp_registry::generated::GeneratedStorage::Stored),
            FieldDecl::json("payload"),
        ]
    };
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls(
            [ModelDecl::new("test_write_capture", "test.write.generated")
                .eventful(true)
                .fields(fields)],
        )
        .build()
        .await
        .unwrap();
    let mut tx = begin(&h).await;
    tx.install_event_schema().await.unwrap();
    let records = tx
        .env()
        .records("test.write.generated")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("number", Cell::Int(1))],
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    records
        .write(&mut tx, RawVals::new().set_cell("number", Cell::Int(3)))
        .await
        .unwrap();
    tx.flush().await.unwrap();
    records
        .write(
            &mut tx,
            RawVals::new().set_cell(
                "payload",
                Cell::Json(Arc::new(serde_json::json!({"a": 1, "b": 2}))),
            ),
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    records
        .write(
            &mut tx,
            RawVals::new().set_cell(
                "payload",
                Cell::Json(Arc::new(serde_json::json!({"b": 2, "a": 1}))),
            ),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut check = h.begin().await.unwrap();
    let mut actual = names(&mut check).await;
    for fields in &mut actual {
        fields.sort();
    }
    assert_eq!(
        actual,
        vec![
            vec!["number".to_owned(), "total".to_owned()],
            vec!["payload".to_owned()]
        ]
    );
    let total: i64 = check
        .query(
            "SELECT total::bigint FROM test_write_generated WHERE id = $1",
            &[&records.ids()[0]],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(total, 6);
    check.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

mod audit_fixture {
    use erp_sdk::prelude::*;
    /// Its scalar has no direct dependency; only automatic write_date is observed.
    #[model(name = "test.write.audit.source", addon = "test_write_capture")]
    pub struct Source {
        #[field(char)]
        pub label: Option<Arc<str>>,
    }
    /// The eventful sink depends solely on the source's automatic audit field.
    #[model(
        name = "test.write.audit.sink",
        addon = "test_write_capture",
        eventful = true
    )]
    pub struct Sink {
        #[field(many2one, comodel = "test.write.audit.source")]
        pub source_id: Option<RecId>,
        #[field(datetime, related = "source_id.write_date", store)]
        pub stamp: Option<DateTime>,
    }
}

#[tokio::test]
async fn review_r1_audit_only_dependency_keeps_original_writer() {
    use audit_fixture::Source;
    let (h, _) = harness().await;
    let mut tx = begin(&h).await;
    let model = tx.env().model::<Source>().unwrap();
    let src = model
        .create(&mut tx, vec![model.vals().set(Source::LABEL, "initial")])
        .await
        .unwrap();
    let sink = tx
        .env()
        .records("test.write.audit.sink")
        .unwrap()
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("source_id", Cell::Id(src.ids()[0]))],
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let baseline: i64 = tx
        .query(&format!("SELECT coalesce(max(id),0) FROM {TABLE}"), &[])
        .await
        .unwrap()[0]
        .get(0);
    src.with_env(tx.env().with_user(7).sudo(true))
        .set(&mut tx, Source::LABEL, "writer seven")
        .await
        .unwrap();
    // A later operation must settle the first origin before staging its own input.
    src.set(&mut tx, Source::LABEL, "base writer")
        .await
        .unwrap();
    let model = tx
        .env()
        .records("test.write.audit.source")
        .unwrap()
        .model_id();
    tx.flush_model(model).await.unwrap();
    tx.commit().await.unwrap();
    let mut check = h.begin().await.unwrap();
    let rows = check.query(&format!("SELECT model, field_names, subject FROM {TABLE} WHERE id > $1 AND kind = 'write' ORDER BY id"), &[&baseline]).await.unwrap();
    assert!(!rows.is_empty());
    assert_eq!(rows[0].get::<_, String>(0), "test.write.audit.sink");
    assert_eq!(rows[0].get::<_, Vec<String>>(1), vec!["stamp"]);
    assert_eq!(rows[0].get::<_, String>(2), "7");
    assert!(
        rows.iter()
            .all(|r| r.get::<_, String>(0) != "test.write.audit.source")
    );
    let same: bool = check.query("SELECT sink.stamp = source.write_date AND sink.write_uid = 1 FROM test_write_audit_sink sink JOIN test_write_audit_source source ON source.id = sink.source_id WHERE sink.id = $1", &[&sink.ids()[0]]).await.unwrap()[0].get(0);
    assert!(same);
    check.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn review_r2_translation_ownership_refuses_foreign_ids_without_losing_pending_write() {
    let (h, id) = harness().await;
    let fields = const { &[FieldDecl::char("name").translate(true)] };
    let decls = (0..24)
        .map(|i| {
            let name: &'static str = Box::leak(format!("zz.foreign.model{i:02}").into_boxed_str());
            ModelDecl::new("test_write_capture", name).fields(fields)
        })
        .collect::<Vec<_>>();
    let foreign = Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls(decls)
        .without_schema()
        .build()
        .await
        .unwrap();
    let foreign_tx = foreign.begin().await.unwrap();
    let records = foreign_tx
        .env()
        .records("zz.foreign.model23")
        .unwrap()
        .browse([1])
        .unwrap();
    let mut tx = begin(&h).await;
    write(
        &mut tx,
        id,
        RawVals::new().set_cell("number", Cell::Int(23)),
    )
    .await;
    tx.capture_statements(true);
    let expected = format!("{records} belongs to another registry than this transaction's");
    let error = records
        .update_field_translations(&mut tx, "name", Vec::<(String, erp_fields::Value)>::new())
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(error.message(), expected);
    assert!(tx.captured().is_empty());
    tx.commit().await.unwrap();
    let mut check = h.begin().await.unwrap();
    assert_eq!(names(&mut check).await, vec![vec!["number".to_owned()]]);
    check.rollback().await.unwrap();
    drop(foreign_tx);
    foreign.cleanup().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn cancelled_update_is_rollback_only_after_the_database_lock_barrier() {
    let (h, id) = harness().await;
    let mut tx = begin(&h).await;
    let pid: i32 = tx.query("SELECT pg_backend_pid()", &[]).await.unwrap()[0].get(0);
    write(
        &mut tx,
        id,
        RawVals::new().set_cell("number", Cell::Int(99)),
    )
    .await;
    let mut blocker = h.begin().await.unwrap();
    blocker
        .execute("LOCK TABLE test_write_capture IN SHARE MODE", &[])
        .await
        .unwrap();
    let mut observer = h.begin().await.unwrap();
    let mut flushing = Box::pin(tx.flush());
    // The deadline admits a hung shared-host fixture; it is not a latency assertion.
    let mut waiting = Box::pin(tokio::time::timeout(
        std::time::Duration::from_secs(60),
        async {
            loop {
                let rows = observer.query("SELECT wait_event_type = 'Lock' AND query LIKE '%UPDATE%test_write_capture%' FROM pg_stat_activity WHERE pid = $1", &[&pid]).await.unwrap();
                if rows[0].get::<_, Option<bool>>(0) == Some(true) {
                    break;
                }
                observer.rollback().await.unwrap();
                tokio::task::yield_now().await;
            }
        },
    ));
    tokio::select! {
        outcome = &mut flushing => panic!("flush passed the held UPDATE lock: {outcome:?}"),
        outcome = &mut waiting => outcome.expect("UPDATE did not reach its owned lock barrier"),
    }
    drop(waiting);
    drop(flushing);
    blocker.rollback().await.unwrap();
    let error = tx.commit().await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "the transaction can no longer be used: an ORM operation was cancelled (its future was dropped) while it was changing the transaction's state; roll back"
    );
    tx.rollback().await.unwrap();
    observer.rollback().await.unwrap();
    assert!(names(&mut observer).await.is_empty());
    let actual: i32 = observer
        .query(
            "SELECT number FROM test_write_capture WHERE id = $1",
            &[&id],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(actual, 1);
    observer.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn provenance_overflow_is_loud_and_caught_error_cannot_commit() {
    let (h, _) = harness().await;
    let mut tx = begin(&h).await;
    // This is functional over-limit admission, not a throughput or allocation benchmark.
    let records = tx
        .env()
        .records(MODEL)
        .unwrap()
        .create(
            &mut tx,
            (0..16_000)
                .map(|i| RawVals::new().set_cell("number", Cell::Int(i)))
                .collect(),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    tx.capture_statements(true);
    let error = records
        .write(
            &mut tx,
            RawVals::new().set_cell("number", Cell::Int(1_000_000)),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::ResourceExhausted);
    assert_eq!(
        error.message(),
        "outbox CaptureLimit: pending provenance exceeds 8388608 bytes"
    );
    let commit_error = tx.commit().await.unwrap_err();
    assert_eq!(commit_error.kind(), ErrorKind::ResourceExhausted);
    assert_eq!(commit_error.message(), error.message());
    tx.rollback().await.unwrap();
    let mut check = h.begin().await.unwrap();
    assert!(names(&mut check).await.is_empty());
    let count: i64 = check
        .query(
            "SELECT count(*) FROM test_write_capture WHERE number = 1000000",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(count, 0);
    check.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn caught_nested_refusal_still_aborts_the_outer_mutation() {
    use nested_fixture::Nested;
    for value in [10_i32, 11_i32] {
        let (h, _) = harness().await;
        let mut tx = begin(&h).await;
        let model = tx.env().model::<Nested>().unwrap();
        let rec = model
            .create(&mut tx, vec![model.vals().set(Nested::SOURCE, 1_i32)])
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let baseline: i64 = tx
            .query(&format!("SELECT coalesce(max(id),0) FROM {TABLE}"), &[])
            .await
            .unwrap()[0]
            .get(0);
        if value == 11 {
            let naked = h.begin().await.unwrap();
            nested_fixture::set_naked(Some(naked.env()));
            drop(naked);
        }
        rec.set(&mut tx, Nested::SOURCE, value).await.unwrap();
        let error = if value == 10 {
            tx.commit().await.unwrap_err()
        } else {
            match tx.commit_pipelined(None).await.unwrap_err() {
                erp_orm::CommitError::RolledBack(error) => error,
                other => panic!("expected definite rollback, got {other:?}"),
            }
        };
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(
            error.message(),
            "a later release S7: nested event-relevant mutations during mutation or compute execution are not implemented"
        );
        tx.rollback().await.unwrap();
        let mut check = h.begin().await.unwrap();
        let count: i64 = check
            .query(
                &format!("SELECT count(*) FROM {TABLE} WHERE id > $1"),
                &[&baseline],
            )
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(count, 0);
        let value: i32 = check
            .query(
                "SELECT source FROM test_write_capture_nested WHERE id = $1",
                &[&rec.ids()[0]],
            )
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(value, 1);
        check.rollback().await.unwrap();
        nested_fixture::set_naked(None);
        h.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn nontranslated_helper_remains_noop_without_identity() {
    let (h, id) = harness().await;
    let mut tx = h.begin().await.unwrap();
    tx.capture_statements(true);
    let changed = tx
        .env()
        .records(MODEL)
        .unwrap()
        .browse([id])
        .unwrap()
        .update_field_translations(&mut tx, "name", Vec::<(String, erp_fields::Value)>::new())
        .await
        .unwrap();
    assert!(!changed);
    assert!(tx.captured().is_empty());
    tx.commit().await.unwrap();
    h.cleanup().await.unwrap();
}
