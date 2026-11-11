//! Runtime generated-column contract.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_fields::{Cell, ErrorKind, Value, ValueMap};
use erp_orm::{FieldOp, RawVals, ReadGroup, SearchOptions, UpsertCall};
use erp_registry::{
    AddonDescriptor, FieldDecl, KeyDecl, ModelDecl, TableObjectDecl, TableObjectKind,
};
use erp_sdk::{Domain, Operator};
use erp_test_models::base_users;
use erp_testkit::Harness;

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "sql_runtime",
    depends: &["base"],
    exact_only: false,
};
static FIELDS: &[FieldDecl] = &[
    FieldDecl::integer("a"),
    FieldDecl::integer("b"),
    FieldDecl::char("code"),
    FieldDecl::boolean("flag"),
    FieldDecl::text("label"),
    FieldDecl::integer("choice").generated("a if flag else b"),
    FieldDecl::integer("stored_choice")
        .generated("a if flag else b")
        .generated_storage(erp_fields::GeneratedStorage::Stored),
    FieldDecl::text("lower_label").generated("lower(label)"),
    FieldDecl::text("stored_lower_label")
        .generated("lower(label)")
        .generated_storage(erp_fields::GeneratedStorage::Stored),
    FieldDecl::integer("downstream")
        .related("total")
        .store(true),
    FieldDecl::integer("total").generated("a + b"),
    FieldDecl::integer("stored_total")
        .generated("a + b")
        .generated_storage(erp_fields::GeneratedStorage::Stored),
];
static OBJECTS: &[TableObjectDecl] = &[TableObjectDecl {
    name: "_code_key",
    kind: TableObjectKind::Key(KeyDecl::new(&["code"], "Code must be unique.")),
}];
fn child_decl() -> ModelDecl {
    ModelDecl::new("sql_runtime", "sql_runtime.child")
        .inherits(&[("sql_runtime.sample", "parent_id")])
        .fields(
            const {
                &[FieldDecl::many2one("parent_id", "sql_runtime.sample")
                    .delegate(true)
                    .required(true)
                    .ondelete(erp_fields::OnDelete::Cascade)]
            },
        )
}
async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([
            ModelDecl::new("sql_runtime", "sql_runtime.sample").fields(FIELDS),
            child_decl(),
        ])
        .build()
        .await
        .unwrap()
}
fn inputs(a: i32, b: i32) -> RawVals {
    let mut vals = RawVals::new();
    vals.insert("a".into(), Cell::Int(a));
    vals.insert("b".into(), Cell::Int(b));
    vals
}
fn readonly(error: erp_fields::Error) {
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Generated field 'sql_runtime.sample.total' is read-only"
    );
}

#[tokio::test]
async fn create_reads_database_values() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("sql_runtime.sample").unwrap();
    let records = model
        .create(&mut tx, vec![inputs(0, -4), inputs(7, 3)])
        .await
        .unwrap();
    assert_eq!(
        records.cells(&mut tx, "total").await.unwrap(),
        vec![Cell::Int(-4), Cell::Int(10)]
    );
    assert_eq!(
        records.cells(&mut tx, "stored_total").await.unwrap(),
        vec![Cell::Int(-4), Cell::Int(10)]
    );
}

#[tokio::test]
async fn explicit_null_is_readonly() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("sql_runtime.sample").unwrap();
    let mut vals = inputs(1, 2);
    vals.insert("total".into(), Cell::Null);
    readonly(
        model
            .create(&mut tx, vec![inputs(9, 9), vals])
            .await
            .unwrap_err(),
    );
    tx.rollback().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("sql_runtime.sample").unwrap();
    let records = model.create(&mut tx, vec![inputs(1, 2)]).await.unwrap();
    let mut vals = RawVals::new();
    vals.insert("total".into(), Cell::Null);
    readonly(records.write(&mut tx, vals).await.unwrap_err());
}

#[tokio::test]
async fn dirty_source_invalidates_generated_cache() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("sql_runtime.sample").unwrap();
    let records = model.create(&mut tx, vec![inputs(1, 2)]).await.unwrap();
    assert_eq!(
        records.cells(&mut tx, "total").await.unwrap(),
        vec![Cell::Int(3)]
    );
    records.write(&mut tx, inputs(4, 8)).await.unwrap();
    assert_eq!(
        records.cells(&mut tx, "total").await.unwrap(),
        vec![Cell::Int(12)]
    );
    assert_eq!(
        records.cells(&mut tx, "stored_total").await.unwrap(),
        vec![Cell::Int(12)]
    );
}

#[tokio::test]
async fn unsaved_read_metadata_and_copy() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("sql_runtime.sample").unwrap();
    let new = tx
        .new_record(&model, inputs(1, 2), None, None)
        .await
        .unwrap();
    let error = new.cells(&mut tx, "total").await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "Generated field 'sql_runtime.sample.total' requires a persisted record"
    );
    let metadata = model
        .fields_get(&mut tx, Some(&["total"]), None)
        .await
        .unwrap();
    assert_eq!(metadata["total"]["database_generated"], Value::Bool(true));
    assert_eq!(
        metadata["total"]["generated_storage"],
        Value::str("virtual")
    );
    assert_eq!(metadata["total"]["readonly"], Value::Bool(true));
    let records = model.create(&mut tx, vec![inputs(4, 5)]).await.unwrap();
    let data = records.copy_data(&mut tx, None).await.unwrap();
    assert!(!data[0].as_ref().unwrap().contains_key("total"));
    assert!(!data[0].as_ref().unwrap().contains_key("stored_total"));
    let copied = records.copy(&mut tx, None).await.unwrap();
    assert_eq!(
        copied.cells(&mut tx, "total").await.unwrap(),
        vec![Cell::Int(9)]
    );
    let mut defaults = ValueMap::new();
    defaults.insert("total".into(), Value::False);
    readonly(
        records
            .copy_data(&mut tx, Some(&defaults))
            .await
            .unwrap_err(),
    );
    readonly(records.copy(&mut tx, Some(defaults)).await.unwrap_err());
}

#[tokio::test]
async fn dirty_sources_are_flushed_for_queries() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("sql_runtime.sample").unwrap();
    let first = model.create(&mut tx, vec![inputs(1, 2)]).await.unwrap();
    let second = model.create(&mut tx, vec![inputs(4, 5)]).await.unwrap();
    first.write(&mut tx, inputs(8, 7)).await.unwrap();
    let found = model
        .search(
            &mut tx,
            &Domain::condition("total", Operator::Gt, 10).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(found.ids(), first.ids());
    second.write(&mut tx, inputs(20, 1)).await.unwrap();
    let found = model
        .search_with(
            &mut tx,
            &Domain::TRUE,
            &SearchOptions::new().order("stored_total desc"),
        )
        .await
        .unwrap();
    assert_eq!(found.ids(), vec![second.ids()[0], first.ids()[0]]);
    second.write(&mut tx, inputs(30, 2)).await.unwrap();
    let groups = model
        ._read_group(
            &mut tx,
            ReadGroup::new(Domain::TRUE).aggregates(["total:sum", "stored_total:sum"]),
        )
        .await
        .unwrap();
    assert_eq!(
        groups.rows(),
        vec![vec![
            erp_orm::GroupValue::Int(47),
            erp_orm::GroupValue::Int(47)
        ]]
    );
}

#[tokio::test]
async fn downstream_compute_and_savepoint() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("sql_runtime.sample").unwrap();
    let records = model.create(&mut tx, vec![inputs(1, 2)]).await.unwrap();
    assert_eq!(
        records.cells(&mut tx, "downstream").await.unwrap(),
        vec![Cell::Int(3)]
    );
    records.write(&mut tx, inputs(5, 7)).await.unwrap();
    assert_eq!(
        records.cells(&mut tx, "downstream").await.unwrap(),
        vec![Cell::Int(12)]
    );
    let error = tx
        .savepoint(
            async |tx: &mut erp_orm::Transaction| -> erp_fields::Result<()> {
                records.write(tx, inputs(20, 30)).await?;
                assert_eq!(records.cells(tx, "downstream").await?, vec![Cell::Int(50)]);
                Err(erp_fields::Error::validation("rollback fixture"))
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.message(), "rollback fixture");
    assert_eq!(
        records.cells(&mut tx, "total").await.unwrap(),
        vec![Cell::Int(12)]
    );
    assert_eq!(
        records.cells(&mut tx, "downstream").await.unwrap(),
        vec![Cell::Int(12)]
    );
    tx.flush().await.unwrap();
}

#[tokio::test]
async fn upsert_insert_conflict_and_output_targets() {
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([ModelDecl::new("sql_runtime", "sql_runtime.sample")
            .fields(FIELDS)
            .table_objects(OBJECTS)])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("sql_runtime.sample").unwrap();
    let row = |a, b| inputs(a, b).set_cell("code", Cell::Str("one".into()));
    let inserted = model
        .upsert(
            &mut tx,
            UpsertCall::new("_code_key")
                .with_rows([row(2, 3)])
                .with_update(["a", "b"]),
        )
        .await
        .unwrap();
    let records = model.browse_ids(inserted.distinct_ids());
    assert_eq!(
        records.cells(&mut tx, "total").await.unwrap(),
        vec![Cell::Int(5)]
    );
    model
        .upsert(
            &mut tx,
            UpsertCall::new("_code_key")
                .with_rows([row(7, 8)])
                .with_update(["a", "b"]),
        )
        .await
        .unwrap();
    assert_eq!(
        records.cells(&mut tx, "total").await.unwrap(),
        vec![Cell::Int(15)]
    );
    assert_eq!(
        records.cells(&mut tx, "stored_total").await.unwrap(),
        vec![Cell::Int(15)]
    );
    for call in [
        UpsertCall::new("_code_key").with_rows([row(1, 2).set_cell("total", Cell::Null)]),
        UpsertCall::new("_code_key")
            .with_rows([row(1, 2)])
            .with_update(["total"]),
        UpsertCall::new("_code_key")
            .with_rows([row(1, 2)])
            .with_accumulate("total"),
    ] {
        readonly(model.upsert(&mut tx, call).await.unwrap_err());
    }
}

#[tokio::test]
async fn defaults_and_dynamic_assignments_refuse() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("sql_runtime.sample").unwrap();
    let defaulted = model.with_context([("default_total", Value::False)]);
    readonly(
        defaulted
            .default_get(&mut tx, &["total"])
            .await
            .unwrap_err(),
    );
    readonly(
        defaulted
            .create(&mut tx, vec![inputs(1, 2)])
            .await
            .unwrap_err(),
    );
    let mut map = ValueMap::new();
    map.insert("total".into(), Value::str("wrong type"));
    readonly(model.vals_from_values(&map).unwrap_err());
}

#[tokio::test]
async fn source_acl_is_required_for_output_queries() {
    for denied in ["a", "total"] {
        let h = Harness::builder([&base_users::ADDON, &ADDON])
            .with_decls([ModelDecl::new("sql_runtime", "sql_runtime.sample").fields(FIELDS)])
            .policy(erp_testkit::FieldPolicy::new().deny(
                "sql_runtime.sample",
                denied,
                FieldOp::Read,
            ))
            .build()
            .await
            .unwrap();
        let mut tx = h.begin().await.unwrap();
        let model = tx.env().records("sql_runtime.sample").unwrap();
        let records = model.create(&mut tx, vec![inputs(1, 2)]).await.unwrap();
        let id = records.ids()[0];
        tx.commit().await.unwrap();
        let mut tx = h.begin_as(2).await.unwrap();
        let model = tx.env().records("sql_runtime.sample").unwrap();
        let records = model.browse([id]).unwrap();
        let message = "You do not have enough rights to access the field \"total\" on sql_runtime.sample (sql_runtime.sample). Please contact your system administrator.\n\nOperation: read";
        let errors = [
            records.cells(&mut tx, "total").await.unwrap_err(),
            model
                .search(
                    &mut tx,
                    &Domain::condition("total", Operator::Gt, 0).unwrap(),
                )
                .await
                .unwrap_err(),
            model
                .search_with(&mut tx, &Domain::TRUE, &SearchOptions::new().order("total"))
                .await
                .unwrap_err(),
            model
                ._read_group(&mut tx, ReadGroup::new(Domain::TRUE).groupby(["total"]))
                .await
                .unwrap_err(),
        ];
        for error in errors {
            assert_eq!(error.kind(), ErrorKind::Access);
            assert_eq!(error.message(), message);
        }
        assert!(
            model
                .fields_get(&mut tx, Some(&["total"]), None)
                .await
                .unwrap()
                .is_empty()
        );
    }
}

#[tokio::test]
async fn new_serializers_omit_unavailable_outputs() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("sql_runtime.sample").unwrap();
    let persisted = model.create(&mut tx, vec![inputs(4, 5)]).await.unwrap();
    let spec = erp_orm::web::Spec::new()
        .field("a")
        .field("b")
        .field("total")
        .field("stored_total")
        .field("downstream");
    for origin in [None, Some(persisted.rec_ids()[0])] {
        let new = tx
            .new_record(&model, inputs(20, 30), origin, None)
            .await
            .unwrap();
        let error = new.cells(&mut tx, "total").await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(
            error.message(),
            "Generated field 'sql_runtime.sample.total' requires a persisted record"
        );
        let rows = new.web_read(&mut tx, spec.clone()).await.unwrap();
        let row = rows[0].dict().unwrap();
        assert!(!row.contains("total"));
        assert!(!row.contains("stored_total"));
        assert!(!row.contains("downstream"));
        assert_eq!(row.get("a").unwrap(), Value::Int(20));
    }
    let mut values = ValueMap::new();
    values.insert("a".into(), Value::Int(3));
    values.insert("b".into(), Value::Int(6));
    for records in [model.clone(), persisted] {
        let result = records
            .onchange(&mut tx, values.clone(), Vec::new(), spec.clone())
            .await
            .unwrap();
        let dict = result.dict().unwrap();
        let value = dict.get("value").unwrap();
        let changed = value.dict().unwrap();
        assert!(!changed.contains("total"));
        assert!(!changed.contains("stored_total"));
        assert!(!changed.contains("downstream"));
    }
}

mod native {
    use erp_sdk::prelude::*;

    #[model(name = "sql_runtime.native", addon = "sql_runtime")]
    pub struct Native {
        #[field(integer)]
        pub a: i32,
        #[field(integer)]
        pub b: i32,
        #[field(integer, generated = "a + b")]
        pub total: i32,
        #[field(integer, generated = "a + b", generated_storage = "stored")]
        pub stored_total: i32,
        #[field(integer, compute = "_downstream", store)]
        pub downstream: i32,
        #[field(many2one, comodel = "sql_runtime.native")]
        pub target: Many2one<Native>,
        #[field(
            many2one,
            comodel = "sql_runtime.native",
            compute = "_point",
            compute_sudo = true
        )]
        pub point: Many2one<Native>,
        #[field(many2one, comodel = "sql_runtime.native")]
        pub other: Many2one<Native>,
        #[field(many2many, comodel = "sql_runtime.native", compute = "_lines")]
        pub lines: X2many<Native>,
    }
    #[methods(Native, addon = "sql_runtime")]
    impl Native {
        #[method(override)]
        async fn default_get(
            this: &RecordSet<Native>,
            tx: &mut Transaction,
            names: Vec<String>,
            sup: Super,
        ) -> Result<erp_sdk::ValueMap> {
            if this.env().context().get("inject_output_default").is_some() {
                let mut values = erp_sdk::ValueMap::new();
                values.insert("total".into(), Value::False);
                return Ok(values);
            }
            sup.call(tx, this.as_records(), (names,)).await
        }

        #[compute(depends = ["total"])]
        fn _downstream(batch: &mut Batch<'_, Native>) -> Result<()> {
            for i in batch.indices() {
                batch.set(i, Native::DOWNSTREAM, batch.get(i, Native::TOTAL)?)?;
            }
            Ok(())
        }
        #[compute(depends = ["total", "target"])]
        fn _point(batch: &mut Batch<'_, Native>) -> Result<()> {
            for i in batch.indices() {
                let total = batch.get(i, Native::TOTAL)?;
                let target = batch.get(i, Native::TARGET)?;
                batch.set(
                    i,
                    Native::POINT,
                    if total >= 0 {
                        target
                    } else {
                        Many2one::new(None)
                    },
                )?;
            }
            Ok(())
        }
        #[compute(depends = ["target", "other"])]
        fn _lines(batch: &mut Batch<'_, Native>) -> Result<()> {
            for i in batch.indices() {
                let target = batch.get(i, Native::TARGET)?.id();
                let other = batch.get(i, Native::OTHER)?.id();
                batch.set(
                    i,
                    Native::LINES,
                    X2many::new(target.into_iter().chain(other).collect()),
                )?;
            }
            Ok(())
        }
        #[constrains("a", "b")]
        async fn _generated_hook(this: &RecordSet<Native>, tx: &mut Transaction) -> Result<()> {
            for record in this.iter() {
                let a = record.get(tx, Native::A).await?;
                let b = record.get(tx, Native::B).await?;
                let total = record.get(tx, Native::TOTAL).await?;
                if i64::from(total) != i64::from(a) + i64::from(b) {
                    return Err(Error::validation("generated hook saw a stale value"));
                }
            }
            Ok(())
        }
    }
}

#[tokio::test]
async fn native_constraint_and_compute_read_database_values() {
    use native::Native;
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<Native>().unwrap();
    let records = model
        .create(
            &mut tx,
            vec![model.vals().set(Native::A, 4).set(Native::B, 6)],
        )
        .await
        .unwrap();
    assert_eq!(records.get(&mut tx, Native::DOWNSTREAM).await.unwrap(), 10);
    records.set(&mut tx, Native::A, -20).await.unwrap();
    assert_eq!(records.get(&mut tx, Native::DOWNSTREAM).await.unwrap(), -14);
    records.set(&mut tx, Native::B, 7).await.unwrap();
    assert_eq!(records.get(&mut tx, Native::DOWNSTREAM).await.unwrap(), -13);
    tx.flush().await.unwrap();
    let new = tx
        .new_record(model.as_records(), inputs(1, 2), None, None)
        .await
        .unwrap();
    let rows = new
        .web_read(
            &mut tx,
            erp_orm::Spec::new().field("total").field("downstream"),
        )
        .await
        .unwrap();
    let row = rows[0].dict().unwrap();
    assert!(!row.contains("total"));
    assert!(!row.contains("downstream"));
}

#[tokio::test]
async fn null_and_group_values_match_sql() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("sql_runtime.sample").unwrap();
    let records = model
        .create(
            &mut tx,
            vec![
                inputs(-4, 1),
                inputs(0, 0),
                RawVals::new()
                    .set_cell("a", Cell::Null)
                    .set_cell("b", Cell::Int(2)),
            ],
        )
        .await
        .unwrap();
    let sql = tx
        .query(
            "SELECT total, stored_total FROM sql_runtime_sample ORDER BY id",
            &[],
        )
        .await
        .unwrap();
    let expected: Vec<Cell> = sql
        .iter()
        .map(|row| row.get::<_, Option<i32>>(0).map_or(Cell::Null, Cell::Int))
        .collect();
    assert_eq!(records.cells(&mut tx, "total").await.unwrap(), expected);
    let expected: Vec<Cell> = sql
        .iter()
        .map(|row| row.get::<_, Option<i32>>(1).map_or(Cell::Null, Cell::Int))
        .collect();
    assert_eq!(
        records.cells(&mut tx, "stored_total").await.unwrap(),
        expected
    );
    let actual = model
        ._read_group(
            &mut tx,
            ReadGroup::new(Domain::TRUE).aggregates(["total:sum", "stored_total:sum"]),
        )
        .await
        .unwrap();
    let sql = tx
        .query(
            "SELECT sum(total), sum(stored_total) FROM sql_runtime_sample",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        actual.rows(),
        vec![vec![
            erp_orm::GroupValue::Int(sql[0].get(0)),
            erp_orm::GroupValue::Int(sql[0].get(1))
        ]]
    );
}

#[tokio::test]
async fn user_and_override_defaults_refuse_before_insert() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();
    readonly(
        erp_orm::ir_default::set(
            &mut tx,
            &env,
            "sql_runtime.sample",
            "total",
            &Value::False,
            None,
            None,
            None,
        )
        .await
        .unwrap_err(),
    );
    let model = env
        .records("sql_runtime.native")
        .unwrap()
        .with_context([("inject_output_default", Value::Bool(true))]);
    tx.capture_statements(true);
    let error = model.create(&mut tx, vec![inputs(1, 2)]).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Generated field 'sql_runtime.native.total' is read-only"
    );
    assert!(
        !tx.captured()
            .iter()
            .any(|statement| statement.starts_with("INSERT INTO \"sql_runtime_native\""))
    );
}

#[tokio::test]
async fn import_and_model_data_guard_generated_assignments() {
    use erp_data::model_data::{self, LoadRecord};
    let h = Harness::builder([&base_users::ADDON, &ADDON, &model_data::ADDON])
        .with_decls([
            ModelDecl::new("sql_runtime", "sql_runtime.sample").fields(FIELDS),
            erp_data::ir_model_data_decl(),
        ])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("sql_runtime.sample").unwrap();
    let result = erp_data::impex::load(
        &model,
        &mut tx,
        &["a", "b", "total"],
        &[
            vec!["1".into(), "2".into(), "".into()],
            vec!["3".into(), "4".into(), "7".into()],
        ],
    )
    .await
    .unwrap();
    assert!(result.ids.is_none());
    assert!(!result.messages.is_empty());
    for message in result.messages {
        assert_eq!(message.kind, erp_data::impex::MessageType::Error);
        assert_eq!(
            message.message,
            "Generated field 'sql_runtime.sample.total' is read-only"
        );
    }
    assert!(
        model
            .search(&mut tx, &Domain::TRUE)
            .await
            .unwrap()
            .is_empty()
    );
    let result = erp_data::impex::load(
        &model,
        &mut tx,
        &["a", "b"],
        &[vec!["5".into(), "6".into()]],
    )
    .await
    .unwrap();
    let records = model.browse(result.ids.unwrap()).unwrap();
    assert_eq!(
        records.cells(&mut tx, "total").await.unwrap(),
        vec![Cell::Int(11)]
    );
    let id = records.ids()[0];
    let mut explicit = inputs(10, 20);
    explicit.insert("total".into(), Cell::Null);
    readonly(
        model_data::load_records(
            &mut tx,
            &model,
            vec![
                LoadRecord::new(None, inputs(30, 40), false).with_db_id(id),
                LoadRecord::new(None, explicit, false).with_db_id(id),
            ],
            false,
        )
        .await
        .unwrap_err(),
    );
    assert_eq!(
        records.cells(&mut tx, "total").await.unwrap(),
        vec![Cell::Int(11)]
    );
    let updated = erp_data::impex::load(
        &model,
        &mut tx,
        &[".id", "a", "b"],
        &[vec![id.to_string(), "8".into(), "9".into()]],
    )
    .await
    .unwrap();
    assert!(!updated.has_errors());
    assert_eq!(
        records.cells(&mut tx, "total").await.unwrap(),
        vec![Cell::Int(17)]
    );
}

#[tokio::test]
async fn delegated_outputs_read_owner_and_refuse_unsaved_origin() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("sql_runtime.child").unwrap();
    let child = model.create(&mut tx, vec![inputs(4, 7)]).await.unwrap();
    assert_eq!(
        child.cells(&mut tx, "total").await.unwrap(),
        vec![Cell::Int(11)]
    );
    child.write(&mut tx, inputs(8, 9)).await.unwrap();
    assert_eq!(
        child.cells(&mut tx, "total").await.unwrap(),
        vec![Cell::Int(17)]
    );
    let columns = tx.query("SELECT count(*)::int8 FROM pg_attribute WHERE attrelid = 'sql_runtime_child'::regclass AND attname = 'total' AND NOT attisdropped", &[]).await.unwrap();
    assert_eq!(columns[0].get::<_, i64>(0), 0);
    let new = tx
        .new_record(&model, RawVals::new(), Some(child.rec_ids()[0]), None)
        .await
        .unwrap();
    let error = new.cells(&mut tx, "total").await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "Generated field 'sql_runtime.child.total' requires a persisted record"
    );
    let rows = new
        .web_read(&mut tx, erp_orm::Spec::new().field("total"))
        .await
        .unwrap();
    assert!(!rows[0].dict().unwrap().contains("total"));
}

#[tokio::test]
async fn numeric_conditional_text_queries_match_sql_on_both_kinds() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("sql_runtime.sample").unwrap();
    let records = model
        .create(
            &mut tx,
            vec![
                inputs(-4, 1)
                    .set_cell("flag", Cell::Bool(true))
                    .set_cell("label", Cell::Str("HELLO".into())),
                inputs(0, 0)
                    .set_cell("flag", Cell::Bool(false))
                    .set_cell("label", Cell::Str("".into())),
                RawVals::new()
                    .set_cell("a", Cell::Null)
                    .set_cell("b", Cell::Int(2))
                    .set_cell("flag", Cell::Null)
                    .set_cell("label", Cell::Null),
            ],
        )
        .await
        .unwrap();
    for field in ["total", "stored_total", "choice", "stored_choice"] {
        let sql = match field {
            "total" | "stored_total" => "SELECT a + b FROM sql_runtime_sample ORDER BY id",
            _ => "SELECT CASE WHEN flag THEN a ELSE b END FROM sql_runtime_sample ORDER BY id",
        };
        let expected = tx
            .query(sql, &[])
            .await
            .unwrap()
            .iter()
            .map(|row| row.get::<_, Option<i32>>(0).map_or(Cell::Null, Cell::Int))
            .collect::<Vec<_>>();
        assert_eq!(records.cells(&mut tx, field).await.unwrap(), expected);
        let found = model
            .search(
                &mut tx,
                &Domain::condition(field, Operator::Gt, -1).unwrap(),
            )
            .await
            .unwrap();
        let sql = if field.ends_with("total") {
            "SELECT id FROM sql_runtime_sample WHERE COALESCE(a + b, 0) > -1 ORDER BY id"
        } else {
            "SELECT id FROM sql_runtime_sample WHERE COALESCE(CASE WHEN flag THEN a ELSE b END, 0) > -1 ORDER BY id"
        };
        let expected = tx
            .query(sql, &[])
            .await
            .unwrap()
            .iter()
            .map(|row| row.get::<_, i64>(0))
            .collect::<Vec<_>>();
        assert_eq!(found.ids(), expected);
        let found = model
            .search_with(
                &mut tx,
                &Domain::TRUE,
                &SearchOptions::new().order(format!("{field} asc nulls last, id asc")),
            )
            .await
            .unwrap();
        let sql = if field.ends_with("total") {
            "SELECT id FROM sql_runtime_sample ORDER BY a + b ASC NULLS LAST, id"
        } else {
            "SELECT id FROM sql_runtime_sample ORDER BY (CASE WHEN flag THEN a ELSE b END) ASC NULLS LAST, id"
        };
        let expected = tx
            .query(sql, &[])
            .await
            .unwrap()
            .iter()
            .map(|row| row.get::<_, i64>(0))
            .collect::<Vec<_>>();
        assert_eq!(found.ids(), expected);
        let grouped = model
            ._read_group(
                &mut tx,
                ReadGroup::new(Domain::TRUE)
                    .groupby([field])
                    .aggregates(["a:sum"]),
            )
            .await
            .unwrap();
        let mut actual = grouped
            .rows()
            .iter()
            .map(|row| {
                let integer = |value: &erp_orm::GroupValue| match value {
                    erp_orm::GroupValue::Int(n) => Some(*n),
                    erp_orm::GroupValue::False => None,
                    other => panic!("unexpected group value {other:?}"),
                };
                (integer(&row[0]), integer(&row[1]))
            })
            .collect::<Vec<_>>();
        let sql = if field.ends_with("total") {
            "SELECT (a + b)::int8, sum(a) FROM sql_runtime_sample GROUP BY a + b"
        } else {
            "SELECT (CASE WHEN flag THEN a ELSE b END)::int8, sum(a) FROM sql_runtime_sample GROUP BY CASE WHEN flag THEN a ELSE b END"
        };
        let mut expected = tx
            .query(sql, &[])
            .await
            .unwrap()
            .iter()
            .map(|row| (row.get::<_, Option<i64>>(0), row.get::<_, Option<i64>>(1)))
            .collect::<Vec<_>>();
        actual.sort();
        expected.sort();
        assert_eq!(actual, expected);
    }
    for field in ["lower_label", "stored_lower_label"] {
        let expected = tx
            .query(
                "SELECT lower(label) FROM sql_runtime_sample ORDER BY id",
                &[],
            )
            .await
            .unwrap()
            .iter()
            .map(|row| {
                row.get::<_, Option<String>>(0)
                    .map_or(Cell::Null, |s| Cell::Str(s.into()))
            })
            .collect::<Vec<_>>();
        assert_eq!(records.cells(&mut tx, field).await.unwrap(), expected);
        let found = model
            .search(
                &mut tx,
                &Domain::condition(field, Operator::Eq, "hello").unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(found.ids(), vec![records.ids()[0]]);
    }
}

#[tokio::test]
async fn related_alias_cannot_declassify_generated_sources() {
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([ModelDecl::new("sql_runtime", "sql_runtime.sample").fields(FIELDS)])
        .policy(erp_testkit::FieldPolicy::new().deny("sql_runtime.sample", "a", FieldOp::Read))
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("sql_runtime.sample").unwrap();
    let records = model.create(&mut tx, vec![inputs(1, 2)]).await.unwrap();
    assert_eq!(
        records.cells(&mut tx, "downstream").await.unwrap(),
        vec![Cell::Int(3)]
    );
    let id = records.ids()[0];
    tx.commit().await.unwrap();
    let mut tx = h.begin_as(2).await.unwrap();
    let records = tx
        .env()
        .records("sql_runtime.sample")
        .unwrap()
        .browse([id])
        .unwrap();
    let error = records.cells(&mut tx, "downstream").await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "You do not have enough rights to access the field \"downstream\" on sql_runtime.sample (sql_runtime.sample). Please contact your system administrator.\n\nOperation: read"
    );
    assert!(
        records
            .fields_get(&mut tx, Some(&["downstream"]), None)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn cached_values_do_not_survive_source_permission_revocation() {
    let policy = erp_testkit::FieldPolicy::new()
        .enforce_groups()
        .member(2, ["sql_runtime.source"]);
    let mut fields = FIELDS.to_vec();
    fields[0] = FieldDecl::integer("a").groups("sql_runtime.source");
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([ModelDecl::new("sql_runtime", "sql_runtime.sample")
            .fields(Box::leak(fields.into_boxed_slice()))])
        .policy(policy.clone())
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("sql_runtime.sample").unwrap();
    let created = model.create(&mut tx, vec![inputs(1, 2)]).await.unwrap();
    let user = created.with_user(2);
    assert_eq!(
        user.cells(&mut tx, "total").await.unwrap(),
        vec![Cell::Int(3)]
    );
    assert_eq!(
        user.cells(&mut tx, "downstream").await.unwrap(),
        vec![Cell::Int(3)]
    );
    let _revoked = policy.clone().member(2, std::iter::empty::<&str>());
    for field in ["total", "downstream"] {
        let error = user.cells(&mut tx, field).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access);
        assert_eq!(
            error.message(),
            format!(
                "You do not have enough rights to access the field \"{field}\" on sql_runtime.sample (sql_runtime.sample). Please contact your system administrator.\n\nOperation: read"
            )
        );
        assert_eq!(
            user.sudo(true).cells(&mut tx, field).await.unwrap(),
            vec![Cell::Int(3)]
        );
    }
    assert!(
        user.fields_get(&mut tx, Some(&["total", "downstream"]), None)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn generated_values_preserve_owner_row_rules() {
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([ModelDecl::new("sql_runtime", "sql_runtime.sample").fields(FIELDS)])
        .policy(
            erp_testkit::DomainPolicy::builder(erp_testkit::Otherwise::Allow)
                .rule(
                    "sql_runtime.sample",
                    erp_orm::Operation::Read,
                    Domain::condition("a", Operator::Gt, 0).unwrap(),
                )
                .build()
                .unwrap(),
        )
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let user_id = tx
        .env()
        .records("res.users")
        .unwrap()
        .create_values(
            &mut tx,
            &[std::collections::BTreeMap::from([
                ("name".to_owned(), Value::str("Generated reader")),
                ("login".to_owned(), Value::str("generated-reader")),
            ])],
        )
        .await
        .unwrap()
        .ids()[0];
    let model = tx.env().records("sql_runtime.sample").unwrap();
    let hidden = model.create(&mut tx, vec![inputs(-4, 8)]).await.unwrap();
    let visible = model.create(&mut tx, vec![inputs(5, 6)]).await.unwrap();
    assert_eq!(
        hidden.cells(&mut tx, "total").await.unwrap(),
        vec![Cell::Int(4)]
    );
    let user = model.with_user(user_id);
    let found = user
        .search_with(
            &mut tx,
            &Domain::condition("total", Operator::Gt, 0).unwrap(),
            &SearchOptions::new().order("total desc"),
        )
        .await
        .unwrap();
    assert_eq!(found.ids(), visible.ids());
    let groups = user
        ._read_group(
            &mut tx,
            ReadGroup::new(Domain::TRUE).aggregates(["total:sum", "stored_total:sum"]),
        )
        .await
        .unwrap();
    assert_eq!(
        groups.rows(),
        vec![vec![
            erp_orm::GroupValue::Int(11),
            erp_orm::GroupValue::Int(11)
        ]]
    );
    let error = hidden
        .with_user(user_id)
        .cells(&mut tx, "total")
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access, "{error:?}");
    assert_eq!(
        error.message(),
        format!(
            "You are not allowed to access 'sql_runtime.sample' (sql_runtime.sample) records.\n\nRecords: sql_runtime.sample({},), User: {user_id}",
            hidden.ids()[0]
        )
    );
}

async fn editable_alias_harness() -> Harness {
    let mut fields = FIELDS.to_vec();
    fields.push(
        FieldDecl::integer("editable_total")
            .related("total")
            .store(true)
            .readonly(false),
    );
    Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([ModelDecl::new("sql_runtime", "sql_runtime.sample")
            .fields(Box::leak(fields.into_boxed_slice()))])
        .build()
        .await
        .unwrap()
}

#[tokio::test]
async fn editable_generated_alias_create_is_readonly() {
    let h = editable_alias_harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("sql_runtime.sample").unwrap();
    readonly(
        model
            .create(
                &mut tx,
                vec![inputs(1, 2).set_cell("editable_total", Cell::Null)],
            )
            .await
            .unwrap_err(),
    );
}

#[tokio::test]
async fn editable_generated_alias_write_is_readonly() {
    let h = editable_alias_harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("sql_runtime.sample").unwrap();
    let records = model.create(&mut tx, vec![inputs(1, 2)]).await.unwrap();
    readonly(
        records
            .write(
                &mut tx,
                RawVals::new().set_cell("editable_total", Cell::Int(9)),
            )
            .await
            .unwrap_err(),
    );
}

#[tokio::test]
async fn empty_generated_read_does_not_flush_sources() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("sql_runtime.sample").unwrap();
    let records = model.create(&mut tx, vec![inputs(1, 2)]).await.unwrap();
    assert_eq!(
        records.cells(&mut tx, "total").await.unwrap(),
        vec![Cell::Int(3)]
    );
    records.write(&mut tx, inputs(4, 8)).await.unwrap();
    tx.capture_statements(true);
    assert!(model.cells(&mut tx, "total").await.unwrap().is_empty());
    // This is the existing empty-read no-op contract, not a performance threshold.
    assert!(tx.captured().is_empty(), "{:?}", tx.captured());
    assert_eq!(
        records.cells(&mut tx, "total").await.unwrap(),
        vec![Cell::Int(12)]
    );
}

#[tokio::test]
async fn ordinary_stored_relations_remain_searchable() {
    let h = Harness::builder([&ADDON])
        .with_decls([ModelDecl::new("sql_runtime", "sql_runtime.sample").fields(
            const {
                &[
                    FieldDecl::many2many("links", "sql_runtime.sample").relation(
                        "sql_runtime_sample_links_rel",
                        "source_id",
                        "target_id",
                    ),
                ]
            },
        )])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("sql_runtime.sample").unwrap();
    let fields = model.fields_get(&mut tx, None, None).await.unwrap();
    assert_eq!(fields["links"]["store"], Value::Bool(true));
    assert_eq!(fields["links"]["searchable"], Value::Bool(true));
    assert_eq!(fields["links"]["database_generated"], Value::Bool(false));
    drop(tx);
    h.cleanup().await.unwrap();
}

async fn cross_record_alias_query(query: u8, delegated: bool) {
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([
            ModelDecl::new("sql_runtime", "sql_runtime.sample").fields(FIELDS),
            ModelDecl::new("sql_runtime", "sql_runtime.link").fields(
                const {
                    &[
                        FieldDecl::many2one("target_id", "sql_runtime.sample"),
                        FieldDecl::integer("total")
                            .related("target_id.total")
                            .store(true),
                    ]
                },
            ),
            child_decl(),
        ])
        .policy(
            erp_testkit::DomainPolicy::builder(erp_testkit::Otherwise::Allow)
                .rule(
                    "sql_runtime.sample",
                    erp_orm::Operation::Read,
                    Domain::condition("a", Operator::Gt, 0).unwrap(),
                )
                .build()
                .unwrap(),
        )
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let reader = tx
        .env()
        .records("res.users")
        .unwrap()
        .create_values(
            &mut tx,
            &[std::collections::BTreeMap::from([
                ("name".to_owned(), Value::str("Alias reader")),
                ("login".to_owned(), Value::str("alias-reader")),
            ])],
        )
        .await
        .unwrap();
    let target = tx
        .env()
        .records("sql_runtime.sample")
        .unwrap()
        .create(&mut tx, vec![inputs(-5, 17)])
        .await
        .unwrap();
    let model_name = if delegated {
        "sql_runtime.child"
    } else {
        "sql_runtime.link"
    };
    let model = tx.env().records(model_name).unwrap();
    let linked = model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell(
                if delegated { "parent_id" } else { "target_id" },
                Cell::Id(target.ids()[0]),
            )],
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let user = model.with_user(reader.ids()[0]);
    assert_eq!(
        linked.cells(&mut tx, "total").await.unwrap(),
        vec![Cell::Int(12)]
    );
    let denied = linked
        .with_user(reader.ids()[0])
        .cells(&mut tx, "total")
        .await
        .unwrap_err();
    assert_eq!(denied.kind(), ErrorKind::Access);
    assert_eq!(
        denied.message(),
        format!(
            "You are not allowed to access 'sql_runtime.sample' (sql_runtime.sample) records.\n\nRecords: sql_runtime.sample({},), User: {}",
            target.ids()[0],
            reader.ids()[0]
        )
    );
    let error = if query == 1 {
        user._read_group(
            &mut tx,
            ReadGroup::new(Domain::TRUE)
                .groupby(["total"])
                .aggregates(["__count"]),
        )
        .await
        .unwrap_err()
    } else if query == 2 {
        user.search_with(
            &mut tx,
            &Domain::TRUE,
            &SearchOptions::new().order("total asc"),
        )
        .await
        .unwrap_err()
    } else if query == 3 {
        user._read_group(
            &mut tx,
            ReadGroup::new(Domain::TRUE).aggregates(["total:sum"]),
        )
        .await
        .unwrap_err()
    } else {
        user.search(
            &mut tx,
            &Domain::condition("total", Operator::Eq, 12).unwrap(),
        )
        .await
        .unwrap_err()
    };
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        format!(
            "Querying generated alias '{model_name}.total' requires target-record authorization (a later release)"
        )
    );
    assert_eq!(
        model
            .search(
                &mut tx,
                &Domain::condition("total", Operator::Eq, 12).unwrap()
            )
            .await
            .unwrap()
            .ids(),
        linked.ids()
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn stored_cross_record_alias_domain_fails_closed() {
    cross_record_alias_query(0, false).await;
}

#[tokio::test]
async fn stored_cross_record_alias_group_fails_closed() {
    cross_record_alias_query(1, false).await;
}

#[tokio::test]
async fn stored_cross_record_alias_order_fails_closed() {
    cross_record_alias_query(2, false).await;
}

#[tokio::test]
async fn stored_cross_record_alias_aggregate_fails_closed() {
    cross_record_alias_query(3, false).await;
}

#[tokio::test]
async fn delegated_generated_queries_fail_closed() {
    for query in 0..4 {
        cross_record_alias_query(query, true).await;
    }
}

#[tokio::test]
async fn delegated_native_derivatives_are_omitted_on_new_records() {
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([ModelDecl::new("sql_runtime", "sql_runtime.native_child")
            .inherits(&[("sql_runtime.native", "parent_id")])
            .fields(
                const {
                    &[FieldDecl::many2one("parent_id", "sql_runtime.native")
                        .delegate(true)
                        .required(true)
                        .ondelete(erp_fields::OnDelete::Cascade)]
                },
            )])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let parent = tx
        .env()
        .records("sql_runtime.native")
        .unwrap()
        .create(&mut tx, vec![inputs(4, 5)])
        .await
        .unwrap();
    let model = tx.env().records("sql_runtime.native_child").unwrap();
    let persisted = model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("parent_id", Cell::Id(parent.ids()[0]))],
        )
        .await
        .unwrap();
    let spec = erp_orm::Spec::new().field("total").field("downstream");
    for origin in [None, Some(persisted.rec_ids()[0])] {
        let new = tx
            .new_record(&model, RawVals::new(), origin, None)
            .await
            .unwrap();
        let rows = new.web_read(&mut tx, spec.clone()).await.unwrap();
        let row = rows[0].dict().unwrap();
        assert!(!row.contains("total"));
        assert!(
            !row.contains("downstream"),
            "unexpected derived output: {:?}",
            row.get("downstream")
        );
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn mixed_new_and_persisted_serializers_preserve_available_outputs() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("sql_runtime.sample").unwrap();
    let persisted = model.create(&mut tx, vec![inputs(4, 5)]).await.unwrap();
    let spec = erp_orm::Spec::new()
        .field("a")
        .field("total")
        .field("stored_total")
        .field("downstream");
    let expected = web_json(persisted.web_read(&mut tx, spec.clone()).await.unwrap());
    for origin in [None, Some(persisted.rec_ids()[0])] {
        let new = tx
            .new_record(&model, inputs(20, 30), origin, None)
            .await
            .unwrap();
        for new_first in [false, true] {
            let mixed = if new_first {
                new.concat(&persisted).unwrap()
            } else {
                persisted.concat(&new).unwrap()
            };
            let direct = web_json(mixed.web_read(&mut tx, spec.clone()).await.unwrap());
            assert_eq!(
                direct[usize::from(new_first)],
                expected[0],
                "persisted sibling lost values before nested serialization"
            );
            {
                let rows = &direct;
                assert_eq!(rows.as_array().unwrap().len(), 2);
                let (old_pos, new_pos) = if new_first { (1, 0) } else { (0, 1) };
                assert_eq!(rows[old_pos], expected[0], "persisted sibling lost values");
                assert_eq!(rows[new_pos]["a"], 20);
                for field in ["total", "stored_total", "downstream"] {
                    assert!(
                        !rows[new_pos].as_object().unwrap().contains_key(field),
                        "new row unexpectedly exposes {field}"
                    );
                }
            }
        }
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn new_relational_derivatives_are_omitted_before_prefetch() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("sql_runtime.native").unwrap();
    let target = model.create(&mut tx, vec![inputs(4, 5)]).await.unwrap();
    let values = || inputs(20, 30).set_cell("target", Cell::Id(target.ids()[0]));
    let persisted = model.create(&mut tx, vec![values()]).await.unwrap();
    let spec = erp_orm::Spec::new().field("a").with(
        "point",
        erp_orm::web::SpecItem::fields(erp_orm::Spec::new().field("a")),
    );
    let expected = web_json(persisted.web_read(&mut tx, spec.clone()).await.unwrap());
    assert_eq!(expected[0]["point"]["a"], 4);
    for origin in [None, Some(persisted.rec_ids()[0])] {
        let new = tx.new_record(&model, values(), origin, None).await.unwrap();
        let rows = web_json(new.web_read(&mut tx, spec.clone()).await.unwrap());
        assert_eq!(rows[0]["a"], 20);
        assert!(!rows[0].as_object().unwrap().contains_key("point"));
        for mixed in [
            new.concat(&persisted).unwrap(),
            persisted.concat(&new).unwrap(),
        ] {
            let rows = web_json(mixed.web_read(&mut tx, spec.clone()).await.unwrap());
            let objects = rows.as_array().unwrap();
            assert_eq!(
                objects
                    .iter()
                    .filter(|row| row.as_object().unwrap().contains_key("point"))
                    .count(),
                1
            );
        }
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

fn web_json(rows: Vec<Value>) -> serde_json::Value {
    let bytes = erp_rpc::__output::serialize(&Value::list(rows), 1_048_576).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn mixed_serializers_preserve_output_and_target_access_errors() {
    for (model_name, denied, nested) in [
        ("sql_runtime.sample", "total", false),
        ("sql_runtime.native", "b", true),
    ] {
        let h = Harness::builder([&base_users::ADDON, &ADDON])
            .with_decls([ModelDecl::new("sql_runtime", "sql_runtime.sample").fields(FIELDS)])
            .policy(erp_testkit::FieldPolicy::new().deny(model_name, denied, FieldOp::Read))
            .build()
            .await
            .unwrap();
        let mut tx = h.begin().await.unwrap();
        let model = tx.env().records(model_name).unwrap();
        let target = model.create(&mut tx, vec![inputs(4, 5)]).await.unwrap();
        let values = if nested {
            inputs(20, 30).set_cell("target", Cell::Id(target.ids()[0]))
        } else {
            inputs(20, 30)
        };
        let persisted = model.create(&mut tx, vec![values.clone()]).await.unwrap();
        let id = persisted.ids()[0];
        let spec = if nested {
            erp_orm::Spec::new().field("a").with(
                "point",
                erp_orm::web::SpecItem::fields(erp_orm::Spec::new().field("b")),
            )
        } else {
            erp_orm::Spec::new().field("b").field("total")
        };
        // Populate the relation/value caches under sudo before checking caller authorization.
        persisted.web_read(&mut tx, spec.clone()).await.unwrap();
        tx.commit().await.unwrap();
        let mut tx = h.begin_as(2).await.unwrap();
        let model = tx.env().records(model_name).unwrap();
        let persisted = model.browse([id]).unwrap();
        let new = tx.new_record(&model, values, None, None).await.unwrap();
        let preview = new.web_read(&mut tx, spec.clone()).await.unwrap();
        assert!(
            !preview[0]
                .dict()
                .unwrap()
                .contains(if nested { "point" } else { "total" })
        );
        for mixed in [
            new.concat(&persisted).unwrap(),
            persisted.concat(&new).unwrap(),
        ] {
            let error = mixed.web_read(&mut tx, spec.clone()).await.unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Access);
            assert_eq!(
                error.message(),
                format!(
                    "You do not have enough rights to access the field \"{denied}\" on {model_name} ({model_name}). Please contact your system administrator.\n\nOperation: read"
                )
            );
        }
        drop(tx);
        h.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn nested_mixed_serializers_preserve_available_outputs() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("sql_runtime.native").unwrap();
    let persisted = model.create(&mut tx, vec![inputs(4, 5)]).await.unwrap();
    let sub = erp_orm::Spec::new()
        .field("a")
        .field("total")
        .field("stored_total")
        .field("downstream");
    let expected = web_json(persisted.web_read(&mut tx, sub.clone()).await.unwrap());
    for origin in [None, Some(persisted.rec_ids()[0])] {
        let new = tx
            .new_record(&model, inputs(20, 30), origin, None)
            .await
            .unwrap();
        for new_first in [false, true] {
            let ids = if new_first {
                [new.rec_ids()[0], persisted.rec_ids()[0]]
            } else {
                [persisted.rec_ids()[0], new.rec_ids()[0]]
            };
            // A computed x2many retains the actual real/new identities. Ordinary SET on a
            // new parent's editable relation intentionally aligns every line to a new origin.
            let parent = tx
                .new_record(
                    &model,
                    inputs(1, 2)
                        .set_cell("target", Cell::Id(ids[0].get()))
                        .set_cell("other", Cell::Id(ids[1].get())),
                    None,
                    None,
                )
                .await
                .unwrap();
            let spec = erp_orm::Spec::new()
                .field("a")
                .with("lines", erp_orm::web::SpecItem::fields(sub.clone()));
            let rows = web_json(parent.web_read(&mut tx, spec).await.unwrap());
            let lines = &rows[0]["lines"];
            if origin.is_none() {
                assert_eq!(lines, &expected, "originless cleanup lost persisted output");
            } else {
                assert_eq!(lines.as_array().unwrap().len(), 2);
                let (old_pos, new_pos) = if new_first { (1, 0) } else { (0, 1) };
                assert_eq!(lines[old_pos], expected[0]);
                assert_eq!(lines[new_pos]["a"], 20);
                for field in ["total", "stored_total", "downstream"] {
                    assert!(!lines[new_pos].as_object().unwrap().contains_key(field));
                }
            }
        }
    }
    drop(tx);
    h.cleanup().await.unwrap();
}
