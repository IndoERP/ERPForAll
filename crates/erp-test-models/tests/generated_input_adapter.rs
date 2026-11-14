// SPDX-License-Identifier: MIT OR Apache-2.0
//! Generated input adapters: public and nested caller boundaries.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "acceptance tests"
)]
use erp_sdk::{
    Cell, Command, Error, ErrorKind, FieldOp, GeneratedInput, Many2one, RawVals, RecordSet, Result,
    Transaction, Value, ValueMap, X2many, methods, model,
};
use erp_test_models::base_users;
use erp_testkit::Harness;

static ADAPTER_PENDING: tokio::sync::Notify = tokio::sync::Notify::const_new();

static ADDON: erp_sdk::AddonDescriptor = erp_sdk::AddonDescriptor {
    name: "input_fixture",
    depends: &["base"],
    exact_only: false,
};
#[model(name = "input_fixture.line", addon = "input_fixture")]
struct Line {
    #[field(integer)]
    source: i32,
    #[field(integer)]
    other: i32,
    #[field(integer)]
    unrelated: i32,
    #[field(integer)]
    aux: i32,
    #[field(integer, compute = "_compute_aux_double", store)]
    aux_double: i32,
    #[field(integer, generated = "aux + 1", input_adapted = true)]
    aux_total: i32,
    #[field(many2many, comodel = "input_fixture.parent")]
    peers: X2many<Parent>,
    #[field(integer, generated = "source + other", input_adapted = true)]
    total: i32,
    #[field(integer, generated = "other + 1")]
    unadapted: i32,
    #[field(many2one, comodel = "input_fixture.parent")]
    parent_id: Many2one<Parent>,
}
#[model(name = "input_fixture.parent", addon = "input_fixture")]
struct Parent {
    #[field(one2many, comodel = "input_fixture.line", inverse_name = "parent_id")]
    lines: X2many<Line>,
}
#[derive(Default)]
struct WriterObservations {
    creates: Vec<Vec<RawVals>>,
    writes: Vec<(Vec<erp_sdk::RecId>, RawVals)>,
}

#[methods(Line, addon = "input_fixture")]
impl Line {
    #[compute(depends = ["aux"])]
    fn _compute_aux_double(batch: &mut erp_sdk::Batch<'_, Line>) -> Result<()> {
        for i in batch.indices() {
            batch.set(i, Line::AUX_DOUBLE, batch.get(i, Line::AUX)?)?;
        }
        Ok(())
    }
    #[method(override)]
    async fn create(
        this: &RecordSet<Line>,
        tx: &mut Transaction,
        rows: Vec<RawVals>,
        sup: erp_sdk::Super,
    ) -> Result<erp_sdk::Records> {
        tx.extension::<WriterObservations>()
            .creates
            .push(rows.clone());
        sup.call(tx, this.as_records(), (rows,)).await
    }
    #[method(override)]
    async fn write(
        this: &RecordSet<Line>,
        tx: &mut Transaction,
        values: RawVals,
        sup: erp_sdk::Super,
    ) -> Result<()> {
        tx.extension::<WriterObservations>()
            .writes
            .push((this.rec_ids().to_vec(), values.clone()));
        if this.env().context().get("fail_second_write") == Some(Value::Bool(true))
            && tx.extension::<WriterObservations>().writes.len() == 2
        {
            return Err(Error::validation("second singleton refused"));
        }
        sup.call(tx, this.as_records(), (values,)).await
    }

    #[method]
    async fn _adapt_generated_input(
        this: &RecordSet<Line>,
        tx: &mut Transaction,
        input: GeneratedInput,
    ) -> Result<Vec<RawVals>> {
        let create = input.is_create();
        let mut rows = input.into_rows();
        if let Some(Value::Str(mode)) = this.env().context().get("adapter_mode") {
            match mode.as_ref() {
                "fail" => return Err(Error::programming("adapter was invoked")),
                "caught_read" => {
                    let _caught = this.as_records().cell(tx, "other").await;
                    for row in &mut rows {
                        row.remove("total");
                        row.insert("source".into(), Cell::Int(1));
                    }
                    return Ok(rows);
                }
                "read_generated" => {
                    this.as_records().cell(tx, "total").await?;
                }
                "read_pending" => {
                    this.as_records().cell(tx, "aux_double").await?;
                }
                "pure_sql" => {
                    let mut pure = tx.pure_methods("input_fixture.line", &["_metadata"]);
                    let _caught = pure.query("SELECT 1", &[]).await;
                }
                "panic" => panic!("adapter unwind fixture"),
                "pending" => {
                    ADAPTER_PENDING.notify_one();
                    return std::future::pending().await;
                }
                "extra_row" => {
                    rows.push(RawVals::new());
                    return Ok(rows);
                }
                "missing_row" => {
                    rows.pop();
                    return Ok(rows);
                }
                "retain" => return Ok(rows),
                "inject" => {
                    rows[0].insert("unrelated".into(), Cell::Int(1));
                }
                "add_unflagged" => {
                    rows[0].insert("unadapted".into(), Cell::Int(9));
                }
                "change_plain_row" => {
                    rows[1].insert("source".into(), Cell::Int(17));
                }
                "sequence_sql" => {
                    tx.query("SELECT nextval('input_fixture_line_id_seq')", &[])
                        .await?;
                }
                "drop_unknown_input" => {
                    rows[0].remove("misspelled");
                }
                "unknown" => {
                    rows[0].insert("missing_field".into(), Cell::Int(1));
                }
                "commands" => {
                    rows[0].insert_commands("peers".into(), Vec::new());
                }
                "cross_row" => {
                    rows[0].remove("total");
                    rows[0].insert("aux".into(), Cell::Int(3));
                    rows[1].remove("aux_total");
                    rows[1].insert("aux".into(), Cell::Int(3));
                    return Ok(rows);
                }
                "commit" => {
                    tx.commit().await?;
                }
                "rollback" => {
                    tx.rollback().await?;
                }
                "extension" => {
                    *tx.extension::<u32>() = 1;
                }
                "after_commit" => {
                    tx.after_commit(|| panic!("adapter must not schedule hooks"));
                }
                "create" => {
                    this.create(tx, Vec::new()).await?;
                }
                "write" => {
                    this.as_records().write(tx, RawVals::new()).await?;
                }
                "dynamic_empty_write" => {
                    this.as_records().write_values(tx, &ValueMap::new()).await?;
                }
                "dispatch_empty_write" => {
                    erp_orm::dispatch::call_method::<(RawVals,), ()>(
                        tx,
                        this.as_records(),
                        "write",
                        (RawVals::new(),),
                    )
                    .await?;
                }
                "flush" => {
                    tx.flush().await?;
                }
                "caught_flush" => {
                    let _caught = tx.flush().await;
                }
                "sql" => {
                    tx.execute("UPDATE input_fixture_line SET source = 99", &[])
                        .await?;
                }
                _ => return Err(Error::programming("unknown adapter test mode")),
            }
        }
        for (i, row) in rows.iter_mut().enumerate() {
            let Some(total) = row.remove("total") else {
                continue;
            };
            let Cell::Int(total) = total else {
                return Err(Error::validation("Adapted total must be an integer"));
            };
            let other = match row.get("other").cloned() {
                Some(Cell::Int(value)) => value,
                None if create => 0,
                None => {
                    this.iter()
                        .nth(i)
                        .expect("core preserves write positions")
                        .get(tx, Line::OTHER)
                        .await?
                }
                _ => return Err(Error::validation("Adapted other must be an integer")),
            };
            row.insert("source".into(), Cell::Int(total - other));
        }
        Ok(rows)
    }
}
async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &ADDON])
        .build()
        .await
        .unwrap()
}
fn total(value: i32) -> RawVals {
    RawVals::new().set_cell("total", Cell::Int(value))
}
fn error(error: Error, kind: ErrorKind, message: &str) {
    assert_eq!(error.kind(), kind);
    assert_eq!(error.message(), message);
}

#[tokio::test]
async fn typed_create_and_row_specific_write() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let lines = tx.env().records("input_fixture.line").unwrap();
    let rows = lines
        .create(
            &mut tx,
            vec![
                total(10).set_cell("other", Cell::Int(2)),
                total(20).set_cell("other", Cell::Int(7)),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        rows.cells(&mut tx, "source").await.unwrap(),
        vec![Cell::Int(8), Cell::Int(13)]
    );
    rows.write(&mut tx, total(30)).await.unwrap();
    assert_eq!(
        rows.cells(&mut tx, "source").await.unwrap(),
        vec![Cell::Int(28), Cell::Int(23)]
    );
    assert_eq!(
        rows.cells(&mut tx, "total").await.unwrap(),
        vec![Cell::Int(30), Cell::Int(30)]
    );
}
#[tokio::test]
async fn dynamic_and_nested_commands_reach_adapter() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let lines = tx.env().records("input_fixture.line").unwrap();
    let values = std::collections::BTreeMap::from([("total".into(), Value::Int(12))]);
    let line = lines.create_values(&mut tx, &[values]).await.unwrap();
    line.write_values(&mut tx, &ValueMap::from([("total".into(), Value::Int(14))]))
        .await
        .unwrap();
    assert_eq!(line.cell(&mut tx, "source").await.unwrap(), Cell::Int(14));
    let parents = tx.env().records("input_fixture.parent").unwrap();
    let parent = parents
        .create(
            &mut tx,
            vec![RawVals::new().set_commands("lines", [Command::Create(total(5))])],
        )
        .await
        .unwrap();
    let children = parent.related(&mut tx, "lines").await.unwrap();
    assert_eq!(
        children.cell(&mut tx, "source").await.unwrap(),
        Cell::Int(5)
    );
    parent
        .write(
            &mut tx,
            RawVals::new()
                .set_commands("lines", [Command::Update(children.rec_ids()[0], total(9))]),
        )
        .await
        .unwrap();
    assert_eq!(
        children.cell(&mut tx, "source").await.unwrap(),
        Cell::Int(9)
    );
}
#[tokio::test]
async fn import_insert_and_update_reach_adapter() {
    let h = Harness::builder([&base_users::ADDON, &ADDON, &erp_data::model_data::ADDON])
        .with_decls([erp_data::ir_model_data_decl()])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let lines = tx.env().records("input_fixture.line").unwrap();
    let loaded = erp_data::impex::load(
        &lines,
        &mut tx,
        &["total", "other"],
        &[vec!["18".into(), "3".into()]],
    )
    .await
    .unwrap();
    assert!(!loaded.has_errors());
    let records = lines.browse(loaded.ids.unwrap()).unwrap();
    assert_eq!(
        records.cell(&mut tx, "source").await.unwrap(),
        Cell::Int(15)
    );
    let updated = erp_data::impex::load(
        &lines,
        &mut tx,
        &[".id", "total"],
        &[vec![records.ids()[0].to_string(), "21".into()]],
    )
    .await
    .unwrap();
    assert!(!updated.has_errors());
    assert_eq!(
        records.cell(&mut tx, "source").await.unwrap(),
        Cell::Int(18)
    );
}
#[tokio::test]
async fn source_only_calls_do_not_invoke_adapter() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let lines = tx
        .env()
        .records("input_fixture.line")
        .unwrap()
        .with_context([("adapter_mode", Value::str("fail"))]);
    let records = lines
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("source", Cell::Int(3))],
        )
        .await
        .unwrap();
    records
        .write(&mut tx, RawVals::new().set_cell("source", Cell::Int(4)))
        .await
        .unwrap();
    assert_eq!(records.cell(&mut tx, "source").await.unwrap(), Cell::Int(4));
}
#[tokio::test]
async fn flagged_input_access_is_checked_before_callback() {
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .policy(erp_testkit::FieldPolicy::new().deny("input_fixture.line", "total", FieldOp::Write))
        .build()
        .await
        .unwrap();
    let mut tx = h.begin_as(2).await.unwrap();
    let lines = tx
        .env()
        .records("input_fixture.line")
        .unwrap()
        .with_context([("adapter_mode", Value::str("fail"))]);
    error(
        lines.create(&mut tx, vec![total(8)]).await.unwrap_err(),
        ErrorKind::Access,
        "You do not have enough rights to access the field \"total\" on input_fixture.line (input_fixture.line). Please contact your system administrator.\n\nOperation: write",
    );
}
#[tokio::test]
async fn generated_defaults_and_unflagged_input_still_fail() {
    for (name, context) in [("unadapted", false), ("total", true)] {
        let h = harness().await;
        let mut tx = h.begin().await.unwrap();
        let lines = tx.env().records("input_fixture.line").unwrap();
        let result = if context {
            lines
                .with_context([("default_total", Value::Int(3))])
                .create(&mut tx, vec![RawVals::new()])
                .await
        } else {
            lines
                .create(&mut tx, vec![RawVals::new().set_cell(name, Cell::Int(3))])
                .await
        };
        error(
            result.unwrap_err(),
            ErrorKind::Validation,
            &format!("Generated field 'input_fixture.line.{name}' is read-only"),
        );
    }
}

#[tokio::test]
async fn invalid_adapter_outputs_are_refused_before_writes() {
    for (mode, reason) in [
        ("extra_row", "output row count changed"),
        ("missing_row", "output row count changed"),
        ("retain", "output retains generated field 'total'"),
        ("inject", "output adds unrelated field 'unrelated'"),
        ("unknown", "output contains unknown field 'missing_field'"),
        ("commands", "output adds unrelated field 'peers'"),
        (
            "add_unflagged",
            "output retains generated field 'unadapted'",
        ),
        ("change_plain_row", "a row without adapted input changed"),
        ("cross_row", "output adds unrelated field 'aux'"),
    ] {
        let h = harness().await;
        let mut tx = h.begin().await.unwrap();
        let lines = tx.env().records("input_fixture.line").unwrap();
        let adapted = lines.with_context([("adapter_mode", Value::str(mode))]);
        let rows = if mode == "cross_row" {
            vec![total(3), RawVals::new().set_cell("aux_total", Cell::Int(4))]
        } else if mode == "change_plain_row" {
            vec![total(3), RawVals::new().set_cell("source", Cell::Int(2))]
        } else {
            vec![total(3)]
        };
        error(
            adapted.create(&mut tx, rows).await.unwrap_err(),
            ErrorKind::Programming,
            &format!("Generated input adapter on 'input_fixture.line': {reason}"),
        );
        assert!(
            lines
                .search(&mut tx, &erp_sdk::Domain::TRUE)
                .await
                .unwrap()
                .is_empty()
        );
    }
}

#[tokio::test]
async fn adapter_effect_attempts_are_sticky_and_scope_restores() {
    for mode in [
        "create",
        "write",
        "flush",
        "caught_flush",
        "sql",
        "sequence_sql",
        "commit",
        "rollback",
        "extension",
        "after_commit",
    ] {
        let h = harness().await;
        let mut tx = h.begin().await.unwrap();
        let lines = tx.env().records("input_fixture.line").unwrap();
        let adapted = lines.with_context([("adapter_mode", Value::str(mode))]);
        error(
            adapted.create(&mut tx, vec![total(3)]).await.unwrap_err(),
            ErrorKind::Programming,
            "Generated input adapter on 'input_fixture.line' may read but cannot mutate, flush or change execution context",
        );
        let created = lines
            .create(
                &mut tx,
                vec![RawVals::new().set_cell("source", Cell::Int(8))],
            )
            .await
            .unwrap();
        assert_eq!(created.cell(&mut tx, "source").await.unwrap(), Cell::Int(8));
        tx.commit().await.unwrap();
    }
}

#[tokio::test]
async fn adapter_reads_latest_dirty_source_without_flushing() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let lines = tx.env().records("input_fixture.line").unwrap();
    let record = lines
        .create(&mut tx, vec![total(10).set_cell("other", Cell::Int(2))])
        .await
        .unwrap();
    record
        .write(&mut tx, RawVals::new().set_cell("other", Cell::Int(7)))
        .await
        .unwrap();
    record.write(&mut tx, total(20)).await.unwrap();
    assert_eq!(record.cell(&mut tx, "source").await.unwrap(), Cell::Int(13));
}

#[tokio::test]
async fn denied_source_write_is_not_granted_by_input_authorization() {
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .policy(erp_testkit::FieldPolicy::new().deny(
            "input_fixture.line",
            "source",
            FieldOp::Write,
        ))
        .build()
        .await
        .unwrap();
    let mut tx = h.begin_as(2).await.unwrap();
    let lines = tx.env().records("input_fixture.line").unwrap();
    error(
        lines.create(&mut tx, vec![total(8)]).await.unwrap_err(),
        ErrorKind::Access,
        "You do not have enough rights to access the field \"source\" on input_fixture.line (input_fixture.line). Please contact your system administrator.\n\nOperation: write",
    );
}

mod wrapper {
    use super::*;
    static ADDON: erp_sdk::AddonDescriptor = erp_sdk::AddonDescriptor {
        name: "input_wrapper",
        depends: &["input_fixture"],
        exact_only: false,
    };
    #[erp_sdk::extend(Line, addon = "input_wrapper")]
    struct Wrapper {}
    #[methods(Line, addon = "input_wrapper")]
    impl Wrapper {
        #[method(override)]
        async fn _adapt_generated_input(
            this: &RecordSet<Line>,
            tx: &mut Transaction,
            input: GeneratedInput,
            sup: erp_sdk::Super,
        ) -> Result<Vec<RawVals>> {
            let mut rows: Vec<RawVals> = sup.call(tx, this.as_records(), (input,)).await?;
            for row in &mut rows {
                if let Some(Cell::Int(value)) = row.get("source").cloned() {
                    row.insert("source".into(), Cell::Int(value + 1));
                }
            }
            Ok(rows)
        }
    }
    #[tokio::test]
    async fn adapter_supports_ordinary_super_wrapping() {
        let h = Harness::builder([&base_users::ADDON, &super::ADDON, &ADDON])
            .build()
            .await
            .unwrap();
        let mut tx = h.begin().await.unwrap();
        let lines = tx.env().records("input_fixture.line").unwrap();
        let records = lines.create(&mut tx, vec![total(7)]).await.unwrap();
        assert_eq!(records.cell(&mut tx, "source").await.unwrap(), Cell::Int(8));
    }
}

mod incompatible {
    use super::*;
    static ADDON: erp_sdk::AddonDescriptor = erp_sdk::AddonDescriptor {
        name: "input_wrong",
        depends: &["base"],
        exact_only: false,
    };
    #[model(name = "input_wrong.line", addon = "input_wrong")]
    struct Wrong {
        #[field(integer)]
        source: i32,
        #[field(integer, generated = "source + 1", input_adapted = true)]
        total: i32,
    }
    #[methods(Wrong, addon = "input_wrong")]
    impl Wrong {
        #[method]
        async fn _adapt_generated_input(
            _this: &RecordSet<Wrong>,
            _tx: &mut Transaction,
            _input: GeneratedInput,
        ) -> Result<bool> {
            Ok(true)
        }
    }
    #[test]
    fn executable_adapter_signature_is_checked_at_build() {
        match Harness::builder([&base_users::ADDON, &ADDON])
            .check()
            .unwrap_err()
        {
            erp_testkit::HarnessError::Orm(err) => error(
                err,
                ErrorKind::Programming,
                "Model 'input_wrong.line' requires native _adapt_generated_input(GeneratedInput) -> Vec<RawVals>",
            ),
            other => panic!("unexpected fixture error: {other}"),
        }
    }
}

#[tokio::test]
async fn model_data_load_uses_the_adapter() {
    use erp_data::model_data::{self, LoadRecord};
    let h = Harness::builder([&base_users::ADDON, &ADDON, &model_data::ADDON])
        .with_decls([erp_data::ir_model_data_decl()])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let lines = tx.env().records("input_fixture.line").unwrap();
    model_data::load_records(
        &mut tx,
        &lines,
        vec![LoadRecord::new(None, total(9), false)],
        false,
    )
    .await
    .unwrap();
    // Read the model rather than infer a load result's ID layout.
    let records = lines.search(&mut tx, &erp_sdk::Domain::TRUE).await.unwrap();
    assert_eq!(records.cell(&mut tx, "source").await.unwrap(), Cell::Int(9));
    model_data::load_records(
        &mut tx,
        &lines,
        vec![LoadRecord::new(None, total(12), false).with_db_id(records.ids()[0])],
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        records.cell(&mut tx, "source").await.unwrap(),
        Cell::Int(12)
    );
}

#[tokio::test]
async fn direct_new_empty_and_base_assignments_remain_guarded() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let lines = tx.env().records("input_fixture.line").unwrap();
    let records = lines
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("source", Cell::Int(1))],
        )
        .await
        .unwrap();
    error(
        records.write_base(&mut tx, total(3)).await.unwrap_err(),
        ErrorKind::Validation,
        "Generated field 'input_fixture.line.total' is read-only",
    );
    tx.rollback().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let lines = tx.env().records("input_fixture.line").unwrap();
    error(
        tx.new_record(&lines, total(3), None, None)
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        "Generated field 'input_fixture.line.total' is read-only",
    );
    error(
        lines.write(&mut tx, total(3)).await.unwrap_err(),
        ErrorKind::Validation,
        "Generated field 'input_fixture.line.total' is read-only",
    );
}

#[tokio::test]
async fn cancelled_adapter_scope_restores_the_transaction() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let lines = tx.env().records("input_fixture.line").unwrap();
    let pending = lines.with_context([("adapter_mode", Value::str("pending"))]);
    {
        let mut operation = Box::pin(pending.create(&mut tx, vec![total(3)]));
        tokio::select! {
            _ = ADAPTER_PENDING.notified() => {},
            result = &mut operation => panic!("adapter should remain pending: {result:?}"),
        }
        // Dropping the future drops the borrowing adapter scope before the next operation.
    }
    let created = lines
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("source", Cell::Int(8))],
        )
        .await
        .unwrap();
    assert_eq!(created.cell(&mut tx, "source").await.unwrap(), Cell::Int(8));
}

#[tokio::test]
async fn non_superuser_cold_source_read_remains_authorized() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let lines = tx.env().records("input_fixture.line").unwrap();
    let created = lines
        .create(&mut tx, vec![total(10).set_cell("other", Cell::Int(2))])
        .await
        .unwrap();
    let ids = created.ids();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = h.begin_as(2).await.unwrap();
    let records = tx
        .env()
        .records("input_fixture.line")
        .unwrap()
        .browse(ids)
        .unwrap();
    records.write(&mut tx, total(15)).await.unwrap();
    assert_eq!(
        records.cell(&mut tx, "source").await.unwrap(),
        Cell::Int(13)
    );
}

#[tokio::test]
async fn empty_dynamic_and_direct_dispatch_writes_are_forbidden() {
    for mode in ["dispatch_empty_write", "dynamic_empty_write"] {
        let h = harness().await;
        let mut tx = h.begin().await.unwrap();
        let lines = tx
            .env()
            .records("input_fixture.line")
            .unwrap()
            .with_context([("adapter_mode", Value::str(mode))]);
        error(
            lines.create(&mut tx, vec![total(3)]).await.unwrap_err(),
            ErrorKind::Programming,
            "Generated input adapter on 'input_fixture.line' may read but cannot mutate, flush or change execution context",
        );
    }
}

mod missing_executable {
    use super::*;
    static ADDON: erp_sdk::AddonDescriptor = erp_sdk::AddonDescriptor {
        name: "input_missing",
        depends: &["base"],
        exact_only: false,
    };
    #[model(name = "input_missing.line", addon = "input_missing")]
    struct Missing {
        #[field(integer)]
        source: i32,
        #[field(integer, generated = "source + 1", input_adapted = true)]
        total: i32,
    }
    #[erp_sdk::__private::linkme::distributed_slice(erp_registry::GENERATED_INPUT_ADAPTER_DECLS)]
    #[linkme(crate = erp_sdk::__private::linkme)]
    static EXPECTATION: erp_registry::GeneratedInputAdapterDecl =
        erp_registry::GeneratedInputAdapterDecl::new("input_missing", "input_missing.line");
    #[test]
    fn metadata_without_executable_method_refuses_build() {
        match Harness::builder([&base_users::ADDON, &ADDON])
            .check()
            .unwrap_err()
        {
            erp_testkit::HarnessError::Orm(err) => error(
                err,
                ErrorKind::Programming,
                "Model 'input_missing.line' requires native _adapt_generated_input(GeneratedInput) -> Vec<RawVals>",
            ),
            other => panic!("unexpected fixture error: {other}"),
        }
    }
}

#[tokio::test]
async fn adapter_source_read_cannot_bypass_field_policy() {
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .policy(erp_testkit::FieldPolicy::new().deny("input_fixture.line", "other", FieldOp::Read))
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let lines = tx.env().records("input_fixture.line").unwrap();
    let records = lines
        .create(&mut tx, vec![total(10).set_cell("other", Cell::Int(2))])
        .await
        .unwrap();
    let ids = records.ids();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = h.begin_as(2).await.unwrap();
    let records = tx
        .env()
        .records("input_fixture.line")
        .unwrap()
        .browse(ids)
        .unwrap();
    error(
        records.write(&mut tx, total(15)).await.unwrap_err(),
        ErrorKind::Access,
        "You do not have enough rights to access the field \"other\" on input_fixture.line (input_fixture.line). Please contact your system administrator.\n\nOperation: read",
    );
}

#[tokio::test]
async fn dynamic_nested_values_reach_child_adapter() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let parents = tx.env().records("input_fixture.parent").unwrap();
    let command = Value::list([
        Value::Int(0),
        Value::Int(0),
        Value::Map(std::sync::Arc::new(ValueMap::from([(
            "total".into(),
            Value::Int(11),
        )]))),
    ]);
    let values = std::collections::BTreeMap::from([("lines".into(), Value::list([command]))]);
    let parent = parents.create_values(&mut tx, &[values]).await.unwrap();
    let children = parent.related(&mut tx, "lines").await.unwrap();
    assert_eq!(
        children.cell(&mut tx, "source").await.unwrap(),
        Cell::Int(11)
    );
}

#[tokio::test]
async fn adapter_cannot_hide_unknown_original_input_names() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let lines = tx
        .env()
        .records("input_fixture.line")
        .unwrap()
        .with_context([("adapter_mode", Value::str("drop_unknown_input"))]);
    error(
        lines
            .create(&mut tx, vec![total(3).set_cell("misspelled", Cell::Int(8))])
            .await
            .unwrap_err(),
        ErrorKind::Programming,
        "Invalid field 'misspelled' in 'input_fixture.line'",
    );
}

#[tokio::test]
async fn ordinary_overrides_receive_normalized_values_in_target_order() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let lines = tx.env().records("input_fixture.line").unwrap();
    let records = lines
        .create(
            &mut tx,
            vec![
                total(4).set_cell("other", Cell::Int(1)),
                total(7).set_cell("other", Cell::Int(2)),
            ],
        )
        .await
        .unwrap();
    let creates = &tx.extension::<WriterObservations>().creates;
    assert_eq!(creates.len(), 1);
    assert_eq!(creates[0][0].get("source"), Some(&Cell::Int(3)));
    assert_eq!(creates[0][1].get("source"), Some(&Cell::Int(5)));
    assert!(creates[0].iter().all(|row| !row.contains("total")));
    records.write(&mut tx, total(10)).await.unwrap();
    let writes = &tx.extension::<WriterObservations>().writes;
    assert_eq!(writes.len(), 2);
    assert_eq!(
        writes[0],
        (
            vec![records.rec_ids()[0]],
            RawVals::new().set_cell("source", Cell::Int(9))
        )
    );
    assert_eq!(
        writes[1],
        (
            vec![records.rec_ids()[1]],
            RawVals::new().set_cell("source", Cell::Int(8))
        )
    );
    tx.extension::<WriterObservations>().writes.clear();
    records
        .write(&mut tx, total(10).set_cell("other", Cell::Int(3)))
        .await
        .unwrap();
    let writes = &tx.extension::<WriterObservations>().writes;
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].0, records.rec_ids());
    assert_eq!(writes[0].1.get("source"), Some(&Cell::Int(7)));
    assert!(!writes[0].1.contains("total"));
}

#[tokio::test]
async fn duplicate_write_targets_preserve_their_positions() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let lines = tx.env().records("input_fixture.line").unwrap();
    let record = lines
        .create(&mut tx, vec![total(4).set_cell("other", Cell::Int(1))])
        .await
        .unwrap();
    let id = record.ids()[0];
    let repeated = lines.browse([id, id]).unwrap();
    assert_eq!(repeated.len(), 2);
    repeated.write(&mut tx, total(9)).await.unwrap();
    assert_eq!(
        repeated.cells(&mut tx, "source").await.unwrap(),
        vec![Cell::Int(8), Cell::Int(8)]
    );
    let writes = &tx.extension::<WriterObservations>().writes;
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].0, repeated.rec_ids());
}

/// any override, since that route has no adapter)
#[tokio::test]
async fn wire_create_and_write_refuse_flagged_input_before_overrides() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let lines = tx.env().records("input_fixture.line").unwrap();
    error(
        erp_orm::call_method_json(&mut tx, &lines, "create", r#"[[{"total": 3}]]"#)
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        "Generated field 'input_fixture.line.total' is read-only",
    );
    let seen = &tx.extension::<WriterObservations>().creates;
    assert!(seen.is_empty(), "create override saw {seen:?}");
    let record = lines
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("source", Cell::Int(1))],
        )
        .await
        .unwrap();
    tx.extension::<WriterObservations>().creates.clear();
    error(
        erp_orm::call_method_json(&mut tx, &record, "write", r#"[{"total": 3}]"#)
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        "Generated field 'input_fixture.line.total' is read-only",
    );
    let seen = &tx.extension::<WriterObservations>().writes;
    assert!(seen.is_empty(), "write override saw {seen:?}");
}

#[tokio::test]
async fn upsert_and_copy_defaults_refuse_flagged_input() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let lines = tx.env().records("input_fixture.line").unwrap();
    error(
        lines
            .upsert(
                &mut tx,
                erp_orm::UpsertCall::new("unused").with_rows([total(3)]),
            )
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        "Generated field 'input_fixture.line.total' is read-only",
    );
    let record = lines
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("source", Cell::Int(1))],
        )
        .await
        .unwrap();
    tx.extension::<WriterObservations>().creates.clear();
    error(
        record
            .copy(
                &mut tx,
                Some(ValueMap::from([("total".into(), Value::Int(3))])),
            )
            .await
            .unwrap_err(),
        ErrorKind::Validation,
        "Generated field 'input_fixture.line.total' is read-only",
    );
    assert!(tx.extension::<WriterObservations>().creates.is_empty());
}

async fn rule_harness(field: &str) -> Harness {
    let policy = erp_testkit::DomainPolicy::builder(erp_testkit::Otherwise::Allow)
        .rule(
            "input_fixture.line",
            erp_orm::Operation::Read,
            erp_sdk::Domain::condition(field, erp_sdk::Operator::Ge, 0).unwrap(),
        )
        .build()
        .unwrap();
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .policy(policy)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.batch("INSERT INTO res_users (id, name, login, active) VALUES (2, 'Adapter reader', 'adapter-reader', true)").await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    h
}

#[tokio::test]
async fn three_row_write_uses_each_stored_counterpart() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let lines = tx.env().records("input_fixture.line").unwrap();
    let records = lines
        .create(
            &mut tx,
            vec![
                total(10).set_cell("other", Cell::Int(1)),
                total(10).set_cell("other", Cell::Int(3)),
                total(10).set_cell("other", Cell::Int(7)),
            ],
        )
        .await
        .unwrap();
    records.write(&mut tx, total(20)).await.unwrap();
    let writes = &tx.extension::<WriterObservations>().writes;
    assert_eq!(writes.len(), 3);
    for (i, source) in [19, 17, 13].into_iter().enumerate() {
        assert_eq!(writes[i].0, vec![records.rec_ids()[i]]);
        assert_eq!(
            writes[i].1,
            RawVals::new().set_cell("source", Cell::Int(source))
        );
    }
    assert_eq!(
        records.cells(&mut tx, "source").await.unwrap(),
        vec![Cell::Int(19), Cell::Int(17), Cell::Int(13)]
    );
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = h.begin().await.unwrap();
    let stored = tx
        .env()
        .records("input_fixture.line")
        .unwrap()
        .browse(records.ids())
        .unwrap();
    assert_eq!(
        stored.cells(&mut tx, "total").await.unwrap(),
        vec![Cell::Int(20); 3]
    );
}

#[tokio::test]
async fn second_singleton_failure_leaves_no_committed_partial_write() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let lines = tx.env().records("input_fixture.line").unwrap();
    let records = lines
        .create(
            &mut tx,
            vec![
                total(10).set_cell("other", Cell::Int(1)),
                total(10).set_cell("other", Cell::Int(3)),
                total(10).set_cell("other", Cell::Int(7)),
            ],
        )
        .await
        .unwrap();
    let ids = records.ids();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = h.begin().await.unwrap();
    let records = tx
        .env()
        .records("input_fixture.line")
        .unwrap()
        .browse(ids.clone())
        .unwrap()
        .with_context([("fail_second_write", Value::Bool(true))]);
    error(
        records.write(&mut tx, total(20)).await.unwrap_err(),
        ErrorKind::Validation,
        "second singleton refused",
    );
    assert_eq!(tx.extension::<WriterObservations>().writes.len(), 2);
    assert!(tx.is_poisoned());
    let refused = tx.commit().await.unwrap_err();
    error(refused, ErrorKind::Validation, "second singleton refused");
    tx.rollback().await.unwrap();
    drop(tx);
    let mut tx = h.begin().await.unwrap();
    let stored = tx
        .env()
        .records("input_fixture.line")
        .unwrap()
        .browse(ids)
        .unwrap();
    assert_eq!(
        stored.cells(&mut tx, "source").await.unwrap(),
        vec![Cell::Int(9), Cell::Int(7), Cell::Int(3)]
    );
    assert_eq!(
        stored.cells(&mut tx, "total").await.unwrap(),
        vec![Cell::Int(10); 3]
    );
}

#[tokio::test]
async fn parent_two_cold_adapted_updates_under_read_rule() {
    let h = rule_harness("unrelated").await;
    let mut tx = h.begin().await.unwrap();
    let parent = tx
        .env()
        .records("input_fixture.parent")
        .unwrap()
        .create(&mut tx, vec![RawVals::new()])
        .await
        .unwrap();
    let lines = tx
        .env()
        .records("input_fixture.line")
        .unwrap()
        .create(
            &mut tx,
            vec![
                total(10)
                    .set_cell("other", Cell::Int(2))
                    .set_cell("unrelated", Cell::Int(0))
                    .set_cell("parent_id", Cell::Id(parent.ids()[0])),
                total(10)
                    .set_cell("other", Cell::Int(5))
                    .set_cell("unrelated", Cell::Int(0))
                    .set_cell("parent_id", Cell::Id(parent.ids()[0])),
            ],
        )
        .await
        .unwrap();
    let parent_ids = parent.ids();
    let line_ids = lines.ids();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = h.begin_as(2).await.unwrap();
    let parent = tx
        .env()
        .records("input_fixture.parent")
        .unwrap()
        .browse(parent_ids)
        .unwrap()
        .with_context([("prefetch_fields", Value::Bool(false))]);
    parent
        .write(
            &mut tx,
            RawVals::new().set_commands(
                "lines",
                [
                    Command::Update(lines.rec_ids()[0], total(20)),
                    Command::Update(lines.rec_ids()[1], total(30)),
                ],
            ),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = h.begin_as(2).await.unwrap();
    let stored = tx
        .env()
        .records("input_fixture.line")
        .unwrap()
        .browse(line_ids)
        .unwrap();
    assert_eq!(
        stored.cells(&mut tx, "source").await.unwrap(),
        vec![Cell::Int(18), Cell::Int(25)]
    );
    assert_eq!(
        stored.cells(&mut tx, "total").await.unwrap(),
        vec![Cell::Int(20), Cell::Int(30)]
    );
}

#[tokio::test]
async fn dirty_rule_input_refusal_names_field() {
    let h = rule_harness("unrelated").await;
    let mut tx = h.begin().await.unwrap();
    let records = tx
        .env()
        .records("input_fixture.line")
        .unwrap()
        .create(
            &mut tx,
            vec![
                total(10)
                    .set_cell("other", Cell::Int(2))
                    .set_cell("unrelated", Cell::Int(0)),
                total(10)
                    .set_cell("other", Cell::Int(5))
                    .set_cell("unrelated", Cell::Int(0)),
            ],
        )
        .await
        .unwrap();
    let ids = records.ids();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = h.begin_as(2).await.unwrap();
    let lines = tx
        .env()
        .records("input_fixture.line")
        .unwrap()
        .with_context([("prefetch_fields", Value::Bool(false))]);
    lines
        .browse([ids[0]])
        .unwrap()
        .write(&mut tx, RawVals::new().set_cell("unrelated", Cell::Int(1)))
        .await
        .unwrap();
    error(
        lines
            .browse([ids[1]])
            .unwrap()
            .write(&mut tx, total(20))
            .await
            .unwrap_err(),
        ErrorKind::Programming,
        "Generated input adapter on 'input_fixture.line': read of 'input_fixture.line.unrelated' requires a flush or recompute",
    );
}

#[tokio::test]
async fn caught_rule_read_refusal_keeps_field_diagnostic() {
    let h = rule_harness("unrelated").await;
    let mut tx = h.begin().await.unwrap();
    let records = tx
        .env()
        .records("input_fixture.line")
        .unwrap()
        .create(
            &mut tx,
            vec![
                total(10).set_cell("unrelated", Cell::Int(0)),
                total(10).set_cell("unrelated", Cell::Int(0)),
            ],
        )
        .await
        .unwrap();
    let ids = records.ids();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = h.begin_as(2).await.unwrap();
    let lines = tx
        .env()
        .records("input_fixture.line")
        .unwrap()
        .with_context([("prefetch_fields", Value::Bool(false))]);
    lines
        .browse([ids[0]])
        .unwrap()
        .write(&mut tx, RawVals::new().set_cell("unrelated", Cell::Int(1)))
        .await
        .unwrap();
    error(
        lines
            .browse([ids[1]])
            .unwrap()
            .with_context([("adapter_mode", Value::str("caught_read"))])
            .write(&mut tx, total(20))
            .await
            .unwrap_err(),
        ErrorKind::Programming,
        "Generated input adapter on 'input_fixture.line': read of 'input_fixture.line.unrelated' requires a flush or recompute",
    );
}

#[tokio::test]
async fn generated_rule_dependency_refuses_dirty_source() {
    let h = rule_harness("unadapted").await;
    let mut tx = h.begin().await.unwrap();
    let records = tx
        .env()
        .records("input_fixture.line")
        .unwrap()
        .create(
            &mut tx,
            vec![
                total(10).set_cell("other", Cell::Int(2)),
                total(10).set_cell("other", Cell::Int(5)),
            ],
        )
        .await
        .unwrap();
    let ids = records.ids();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = h.begin_as(2).await.unwrap();
    let lines = tx
        .env()
        .records("input_fixture.line")
        .unwrap()
        .with_context([("prefetch_fields", Value::Bool(false))]);
    lines
        .browse([ids[0]])
        .unwrap()
        .write(&mut tx, RawVals::new().set_cell("other", Cell::Int(3)))
        .await
        .unwrap();
    error(
        lines
            .browse([ids[1]])
            .unwrap()
            .write(&mut tx, total(20))
            .await
            .unwrap_err(),
        ErrorKind::Programming,
        "Generated input adapter on 'input_fixture.line': read of 'input_fixture.line.unadapted' requires a flush or recompute",
    );
}

#[tokio::test]
async fn unrelated_pending_compute_does_not_block_cold_read() {
    let h = rule_harness("unrelated").await;
    let mut tx = h.begin().await.unwrap();
    let records = tx
        .env()
        .records("input_fixture.line")
        .unwrap()
        .create(
            &mut tx,
            vec![
                total(10).set_cell("unrelated", Cell::Int(0)),
                total(10).set_cell("unrelated", Cell::Int(0)),
            ],
        )
        .await
        .unwrap();
    let ids = records.ids();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = h.begin_as(2).await.unwrap();
    let lines = tx
        .env()
        .records("input_fixture.line")
        .unwrap()
        .with_context([("prefetch_fields", Value::Bool(false))]);
    lines
        .browse([ids[0]])
        .unwrap()
        .write(&mut tx, RawVals::new().set_cell("aux", Cell::Int(4)))
        .await
        .unwrap();
    assert!(
        tx.fields_to_compute()
            .contains(&"input_fixture.line.aux_double".to_owned())
    );
    lines
        .browse([ids[1]])
        .unwrap()
        .write(&mut tx, total(20))
        .await
        .unwrap();
    assert!(
        tx.fields_to_compute()
            .contains(&"input_fixture.line.aux_double".to_owned())
    );
    assert!(
        tx.is_dirty(&lines.browse([ids[0]]).unwrap(), "aux")
            .unwrap()
    );
}

#[tokio::test]
async fn pending_rule_compute_refuses_by_field() {
    let h = rule_harness("aux_double").await;
    let mut tx = h.begin().await.unwrap();
    let records = tx
        .env()
        .records("input_fixture.line")
        .unwrap()
        .create(&mut tx, vec![total(10), total(10)])
        .await
        .unwrap();
    let ids = records.ids();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = h.begin_as(2).await.unwrap();
    let lines = tx
        .env()
        .records("input_fixture.line")
        .unwrap()
        .with_context([("prefetch_fields", Value::Bool(false))]);
    lines
        .browse([ids[0]])
        .unwrap()
        .write(&mut tx, RawVals::new().set_cell("aux", Cell::Int(4)))
        .await
        .unwrap();
    error(
        lines
            .browse([ids[1]])
            .unwrap()
            .write(&mut tx, total(20))
            .await
            .unwrap_err(),
        ErrorKind::Programming,
        "Generated input adapter on 'input_fixture.line': read of 'input_fixture.line.aux_double' requires a flush or recompute",
    );
}

#[tokio::test]
async fn nested_pure_refusal_stays_fatal_to_adapter() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let lines = tx.env().records("input_fixture.line").unwrap();
    error(
        lines
            .with_context([("adapter_mode", Value::str("pure_sql"))])
            .create(&mut tx, vec![total(10)])
            .await
            .unwrap_err(),
        ErrorKind::Programming,
        "ForbiddenEffect: pure method scope refuses database, record, transaction or environment effects",
    );
    lines.create(&mut tx, vec![total(10)]).await.unwrap();
    tx.query("SELECT 1", &[]).await.unwrap();
}

#[tokio::test]
async fn unwound_adapter_restores_effect_scope() {
    use futures_util::FutureExt;
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let lines = tx.env().records("input_fixture.line").unwrap();
    let adapted = lines.with_context([("adapter_mode", Value::str("panic"))]);
    assert!(
        std::panic::AssertUnwindSafe(adapted.create(&mut tx, vec![total(10)]))
            .catch_unwind()
            .await
            .is_err()
    );
    lines.create(&mut tx, vec![total(10)]).await.unwrap();
    tx.query("SELECT 1", &[]).await.unwrap();
}

#[tokio::test]
async fn direct_adapter_reads_name_generated_or_pending_field() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let lines = tx.env().records("input_fixture.line").unwrap();
    let records = lines.create(&mut tx, vec![total(10)]).await.unwrap();
    for (mode, field) in [("read_generated", "total"), ("read_pending", "aux_double")] {
        records
            .write(&mut tx, RawVals::new().set_cell("aux", Cell::Int(4)))
            .await
            .unwrap();
        error(
            records
                .with_context([("adapter_mode", Value::str(mode))])
                .write(&mut tx, total(20))
                .await
                .unwrap_err(),
            ErrorKind::Programming,
            &format!(
                "Generated input adapter on 'input_fixture.line': read of 'input_fixture.line.{field}' requires a flush or recompute"
            ),
        );
    }
}

#[tokio::test]
async fn audit_rule_refuses_dirty_model() {
    let h = rule_harness("write_uid").await;
    let mut tx = h.begin().await.unwrap();
    let records = tx
        .env()
        .records("input_fixture.line")
        .unwrap()
        .create(&mut tx, vec![total(10), total(10)])
        .await
        .unwrap();
    let ids = records.ids();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = h.begin_as(2).await.unwrap();
    let lines = tx
        .env()
        .records("input_fixture.line")
        .unwrap()
        .with_context([("prefetch_fields", Value::Bool(false))]);
    lines
        .browse([ids[0]])
        .unwrap()
        .write(&mut tx, RawVals::new().set_cell("source", Cell::Int(3)))
        .await
        .unwrap();
    error(
        lines
            .browse([ids[1]])
            .unwrap()
            .write(&mut tx, total(20))
            .await
            .unwrap_err(),
        ErrorKind::Programming,
        "Generated input adapter on 'input_fixture.line': read of 'input_fixture.line.write_uid' requires a flush or recompute",
    );
}
