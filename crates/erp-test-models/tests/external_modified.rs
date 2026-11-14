// SPDX-License-Identifier: MIT OR Apache-2.0
//! External database changes: author regressions for the native notification contract.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_sdk::prelude::*;
use erp_test_models::base_users;
use erp_testkit::Harness;

erp_sdk::addon!(name = "test_external_modified");

#[model(name = "test_external.node", addon = "test_external_modified")]
pub struct Node {
    #[field(integer)]
    pub source: i32,
    #[field(char)]
    pub note: Option<Arc<str>>,
    #[field(integer, compute = "_double", store)]
    pub doubled: i32,
    #[field(integer, compute = "_preview")]
    pub preview: i32,
    #[field(many2one, comodel = "test_external.node")]
    pub parent_id: Option<RecId>,
    #[field(one2many, comodel = "test_external.node", inverse_name = "parent_id")]
    pub child_ids: Vec<RecId>,
    #[field(integer, compute = "_total", store)]
    pub total: i32,
    #[field(integer, compute = "_total_preview")]
    pub total_preview: i32,
}

#[methods(Node, addon = "test_external_modified")]
impl Node {
    #[compute(depends = ["source"])]
    fn _double(batch: &mut Batch<'_, Node>) -> Result<()> {
        for i in batch.indices() {
            let value = batch.get(i, Node::SOURCE)?;
            batch.set(
                i,
                Node::DOUBLED,
                value
                    .checked_mul(2)
                    .ok_or_else(|| Error::programming("fixture double overflow"))?,
            )?;
        }
        Ok(())
    }

    #[compute(depends = ["source"], depends_context = ["offset"])]
    fn _preview(batch: &mut Batch<'_, Node>) -> Result<()> {
        let offset = match batch.env().context().get("offset") {
            Some(Value::Int(n)) => {
                i32::try_from(n).map_err(|_| Error::programming("fixture offset out of range"))?
            }
            None => 0,
            Some(_) => return Err(Error::programming("fixture offset must be an integer")),
        };
        for i in batch.indices() {
            let value = batch.get(i, Node::SOURCE)?;
            batch.set(
                i,
                Node::PREVIEW,
                value
                    .checked_add(offset)
                    .ok_or_else(|| Error::programming("fixture preview overflow"))?,
            )?;
        }
        Ok(())
    }

    #[compute(depends = ["child_ids.source"])]
    fn _total(batch: &mut Batch<'_, Node>) -> Result<()> {
        for i in batch.indices() {
            let children = batch.get(i, Node::CHILD_IDS)?;
            let total = batch
                .values_of(Node::SOURCE, &children)?
                .into_iter()
                .try_fold(0_i32, |sum, value| {
                    sum.checked_add(value)
                        .ok_or_else(|| Error::programming("fixture total overflow"))
                })?;
            batch.set(i, Node::TOTAL, total)?;
        }
        Ok(())
    }

    #[compute(depends = ["child_ids.source"])]
    fn _total_preview(batch: &mut Batch<'_, Node>) -> Result<()> {
        for i in batch.indices() {
            let children = batch.get(i, Node::CHILD_IDS)?;
            let total = batch
                .values_of(Node::SOURCE, &children)?
                .into_iter()
                .try_fold(0_i32, |sum, value| {
                    sum.checked_add(value)
                        .ok_or_else(|| Error::programming("fixture total overflow"))
                })?;
            batch.set(i, Node::TOTAL_PREVIEW, total)?;
        }
        Ok(())
    }
}

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &ADDON])
        .build()
        .await
        .unwrap()
}

async fn node(tx: &mut Transaction, source: i32, parent: Option<RecId>) -> RecordSet<Node> {
    let model = tx.env().model::<Node>().unwrap();
    model
        .create(
            tx,
            vec![
                model
                    .vals()
                    .set(Node::SOURCE, source)
                    .set(Node::PARENT_ID, parent),
            ],
        )
        .await
        .unwrap()
}

/// The accepted native entry. Baseline commit uses flushing invalidation instead.
async fn notify(records: &RecordSet<Node>, tx: &mut Transaction, fields: &[&str]) -> Result<()> {
    records.modified_external(tx, fields).await
}

async fn raw_source(tx: &mut Transaction, id: RecId, source: i32) {
    assert_eq!(
        tx.execute(
            "UPDATE test_external_node SET source=$1 WHERE id=$2",
            &[&source, &id.get()]
        )
        .await
        .unwrap(),
        1
    );
}

#[tokio::test]
async fn external_scalar_dependents_and_committed_reader() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let rec = node(&mut tx, 3, None).await;
    let id = rec.rec_ids()[0];
    tx.flush().await.unwrap();
    assert_eq!(rec.get(&mut tx, Node::DOUBLED).await.unwrap(), 6);
    assert_eq!(rec.get(&mut tx, Node::PREVIEW).await.unwrap(), 3);
    raw_source(&mut tx, id, 7).await;
    // Plain SQL does not repair already-cached values; prove that setup before notification.
    assert_eq!(rec.get(&mut tx, Node::PREVIEW).await.unwrap(), 3);
    notify(&rec, &mut tx, &["source"]).await.unwrap();
    assert!(
        !tx.records_to_compute(&rec, "doubled").unwrap().is_empty(),
        "stored dependent must be scheduled"
    );
    assert!(
        !tx.is_cached(&rec, "preview").unwrap(),
        "non-stored dependent must be invalidated"
    );
    assert_eq!(rec.get(&mut tx, Node::SOURCE).await.unwrap(), 7);
    assert_eq!(rec.get(&mut tx, Node::DOUBLED).await.unwrap(), 14);
    assert_eq!(rec.get(&mut tx, Node::PREVIEW).await.unwrap(), 7);
    tx.commit().await.unwrap();
    drop(tx);
    let mut reader = h.begin().await.unwrap();
    let fresh = reader
        .env()
        .model::<Node>()
        .unwrap()
        .browse([id.get()])
        .unwrap();
    assert_eq!(fresh.get(&mut reader, Node::DOUBLED).await.unwrap(), 14);
    assert_eq!(fresh.get(&mut reader, Node::PREVIEW).await.unwrap(), 7);
    drop(reader);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn external_child_change_reaches_parent_dependents() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let parent = node(&mut tx, 0, None).await;
    let other = node(&mut tx, 0, None).await;
    let child = node(&mut tx, 2, Some(parent.rec_ids()[0])).await;
    tx.flush().await.unwrap();
    assert_eq!(parent.get(&mut tx, Node::TOTAL).await.unwrap(), 2);
    assert_eq!(parent.get(&mut tx, Node::TOTAL_PREVIEW).await.unwrap(), 2);
    assert_eq!(other.get(&mut tx, Node::TOTAL_PREVIEW).await.unwrap(), 0);
    raw_source(&mut tx, child.rec_ids()[0], 9).await;
    notify(&child, &mut tx, &["source"]).await.unwrap();
    assert!(
        !tx.is_cached(&parent, "total_preview").unwrap(),
        "parent dependent must be invalidated"
    );
    assert!(
        tx.is_cached(&other, "total_preview").unwrap(),
        "unrelated parent remains cached"
    );
    assert_eq!(parent.get(&mut tx, Node::TOTAL).await.unwrap(), 9);
    assert_eq!(parent.get(&mut tx, Node::TOTAL_PREVIEW).await.unwrap(), 9);
    let parent_id = parent.rec_ids()[0];
    tx.commit().await.unwrap();
    drop(tx);
    let mut reader = h.begin().await.unwrap();
    let fresh = reader
        .env()
        .model::<Node>()
        .unwrap()
        .browse([parent_id.get()])
        .unwrap();
    assert_eq!(fresh.get(&mut reader, Node::TOTAL).await.unwrap(), 9);
    assert_eq!(
        fresh.get(&mut reader, Node::TOTAL_PREVIEW).await.unwrap(),
        9
    );
    drop(reader);
    h.cleanup().await.unwrap();
}

fn programming(error: Error, message: &str) {
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Programming);
    assert_eq!(error.message(), message);
}

#[tokio::test]
async fn external_admission_is_atomic_and_names_invalid_sources() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let rec = node(&mut tx, 3, None).await;
    tx.flush().await.unwrap();
    for field in ["missing", "preview", "child_ids", "parent_id", "id"] {
        let error = notify(&rec, &mut tx, &["source", field]).await.unwrap_err();
        let message = match field {
            "missing" => "External modification: unknown field 'test_external.node.missing'",
            "parent_id" => {
                "External modification: relational source 'test_external.node.parent_id' requires a later release"
            }
            "preview" => {
                "External modification: field 'test_external.node.preview' is not a stored column owned by this model"
            }
            "child_ids" => {
                "External modification: field 'test_external.node.child_ids' is not a stored column owned by this model"
            }
            "id" => {
                "External modification: field 'test_external.node.id' is not a stored column owned by this model"
            }
            _ => unreachable!(),
        };
        programming(error, message);
        assert!(tx.is_cached(&rec, "source").unwrap());
        assert!(!tx.is_poisoned());
    }
    let empty = tx.env().model::<Node>().unwrap();
    programming(
        notify(&empty, &mut tx, &["missing"]).await.unwrap_err(),
        "External modification: unknown field 'test_external.node.missing'",
    );
    notify(&empty, &mut tx, &["source"]).await.unwrap();
    notify(&rec, &mut tx, &[]).await.unwrap();
    assert!(tx.is_cached(&rec, "source").unwrap());
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn external_pending_sources_are_not_lost_or_flushed() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let rec = node(&mut tx, 3, None).await;
    tx.flush().await.unwrap();
    rec.set(&mut tx, Node::SOURCE, 8).await.unwrap();
    for field in ["source", "doubled"] {
        let message = format!(
            "External modification: field 'test_external.node.{field}' has pending ORM work; settle it before external SQL"
        );
        programming(notify(&rec, &mut tx, &[field]).await.unwrap_err(), &message);
    }
    assert!(tx.is_dirty(&rec, "source").unwrap());
    let source: i32 = tx
        .query(
            "SELECT source FROM test_external_node WHERE id=$1",
            &[&rec.rec_ids()[0].get()],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(source, 3);
    assert_eq!(rec.get(&mut tx, Node::SOURCE).await.unwrap(), 8);
    assert!(!tx.is_poisoned());
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn external_notification_is_lazy_and_invalidates_all_contexts() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let rec = node(&mut tx, 3, None).await;
    let unrelated = node(&mut tx, 20, None).await;
    tx.flush().await.unwrap();
    let a = rec.with_context([("offset", Value::Int(1))]);
    let b = rec.with_context([("offset", Value::Int(10))]);
    assert_eq!(a.get(&mut tx, Node::PREVIEW).await.unwrap(), 4);
    assert_eq!(b.get(&mut tx, Node::PREVIEW).await.unwrap(), 13);
    assert_eq!(unrelated.get(&mut tx, Node::PREVIEW).await.unwrap(), 20);
    assert_eq!(unrelated.get(&mut tx, Node::NOTE).await.unwrap(), None);
    rec.set(&mut tx, Node::NOTE, "pending").await.unwrap();
    raw_source(&mut tx, rec.rec_ids()[0], 7).await;
    let duplicates = tx
        .env()
        .model::<Node>()
        .unwrap()
        .browse([rec.rec_ids()[0].get(), rec.rec_ids()[0].get()])
        .unwrap();
    tx.capture_statements(true);
    notify(&duplicates, &mut tx, &["source", "source"])
        .await
        .unwrap();
    assert!(
        tx.captured().iter().all(|s| s.starts_with("SELECT")),
        "notification must not write/flush: {:?}",
        tx.captured()
    );
    assert!(tx.is_dirty(&rec, "note").unwrap());
    let row = &tx
        .query(
            "SELECT doubled, note FROM test_external_node WHERE id=$1",
            &[&rec.rec_ids()[0].get()],
        )
        .await
        .unwrap()[0];
    assert_eq!(row.get::<_, i32>(0), 6, "stored dependent remains lazy");
    assert_eq!(
        row.get::<_, Option<String>>(1),
        None,
        "unrelated dirty field remains unflushed"
    );
    assert!(tx.is_cached(&unrelated, "note").unwrap());
    assert_eq!(a.get(&mut tx, Node::PREVIEW).await.unwrap(), 8);
    assert_eq!(b.get(&mut tx, Node::PREVIEW).await.unwrap(), 17);
    notify(&rec, &mut tx, &["source"]).await.unwrap();
    assert_eq!(rec.get(&mut tx, Node::DOUBLED).await.unwrap(), 14);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn external_partial_notification_failure_is_rollback_only() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let rec = node(&mut tx, 3, None).await;
    tx.commit().await.unwrap();
    assert_eq!(rec.get(&mut tx, Node::SOURCE).await.unwrap(), 3);
    tx.invalidate_records(&rec, Some(&["parent_id"]))
        .await
        .unwrap();
    // Force the dependency walk's uncached relation read to fail after source invalidation.
    tx.execute(
        "ALTER TABLE test_external_node RENAME COLUMN parent_id TO unavailable_parent",
        &[],
    )
    .await
    .unwrap();
    let error = notify(&rec, &mut tx, &["source"]).await.unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Database);
    assert_eq!(error.message(), "column \"parent_id\" does not exist");
    assert!(!tx.is_cached(&rec, "source").unwrap());
    assert!(tx.is_poisoned());
    let commit_error = tx.commit().await.unwrap_err();
    assert_eq!(commit_error.kind(), error.kind());
    assert_eq!(
        commit_error.message(),
        format!(
            "transaction rolled back: an earlier statement failed ({})",
            error.message()
        )
    );
    tx.rollback().await.unwrap();
    assert_eq!(rec.get(&mut tx, Node::SOURCE).await.unwrap(), 3);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn external_refuses_drafts_and_other_registries() {
    let h = harness().await;
    let other = harness().await;
    let mut tx = h.begin().await.unwrap();
    let rec = node(&mut tx, 3, None).await;
    tx.flush().await.unwrap();
    let mut foreign = other.begin().await.unwrap();
    let error = notify(&rec, &mut foreign, &["source"]).await.unwrap_err();
    programming(
        error,
        &format!(
            "{} belongs to another registry than this transaction's",
            rec.as_records()
        ),
    );
    let model = tx.env().records("test_external.node").unwrap();
    let draft = tx
        .new_record(&model, erp_sdk::RawVals::new(), None, None)
        .await
        .unwrap();
    let error = draft
        .modified_external(&mut tx, &["source"])
        .await
        .unwrap_err();
    programming(
        error,
        &format!(
            "External modification: draft record {} of 'test_external.node' has no database row",
            draft.rec_ids()[0]
        ),
    );
    drop(tx);
    drop(foreign);
    h.cleanup().await.unwrap();
    other.cleanup().await.unwrap();
}

#[tokio::test]
async fn external_notification_does_not_authorize_reads_or_expose_rpc() {
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .policy(erp_sdk::StaticPolicy::SuperuserOnly)
        .as_user(5)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "INSERT INTO res_users (id, name, login, active) VALUES (5, 'five', 'five', true)",
        &[],
    )
    .await
    .unwrap();
    let model = tx.env().model::<Node>().unwrap().sudo(true);
    let rec = model
        .create(&mut tx, vec![model.vals().set(Node::SOURCE, 3)])
        .await
        .unwrap();
    tx.flush().await.unwrap();
    raw_source(&mut tx, rec.rec_ids()[0], 9).await;
    let restricted = rec.sudo(false);
    notify(&restricted, &mut tx, &["source"]).await.unwrap();
    let error = restricted.get(&mut tx, Node::SOURCE).await.unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Access);
    assert_eq!(
        error.message(),
        format!(
            "Your access rights do not let you read '{}' (test_external.node) records.",
            rec.spec().description
        )
    );
    assert!(
        tx.exposure()
            .get(rec.model_id(), "modified_external")
            .is_none()
    );
    let error = erp_orm::call_method_json(&mut tx, &restricted, "modified_external", "[]")
        .await
        .unwrap_err();
    programming(
        error,
        "'test_external.node' object has no attribute 'modified_external'",
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Script execution has no host binding for the native entry. This runs the actual Luau sandbox,
/// with its injected record table; it does not claim a separate script-addon ORM bridge exists.
#[test]
fn external_script_cannot_call_the_native_entry() {
    use erp_sdk::script::{LuauSandbox, ScriptContext, ScriptError, ScriptValue};
    let mut ctx = ScriptContext::new();
    ctx.set_value("record", ScriptValue::Dict(Default::default()))
        .unwrap();
    let error = LuauSandbox::default()
        .eval("return record:modified_external({'source'})", &ctx)
        .unwrap_err();
    match error {
        ScriptError::Runtime { message, .. } => assert_eq!(
            message,
            "script:1: attempt to call missing method 'modified_external' of table"
        ),
        other => panic!("expected Runtime refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn external_savepoint_rollback_restores_values() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let rec = node(&mut tx, 3, None).await;
    tx.commit().await.unwrap();
    let result: Result<()> = tx
        .savepoint(async |tx| {
            assert_eq!(rec.get(tx, Node::DOUBLED).await?, 6);
            raw_source(tx, rec.rec_ids()[0], 9).await;
            notify(&rec, tx, &["source"]).await?;
            assert_eq!(rec.get(tx, Node::DOUBLED).await?, 18);
            Err(Error::programming("fixture rollback"))
        })
        .await;
    programming(result.unwrap_err(), "fixture rollback");
    assert_eq!(rec.get(&mut tx, Node::SOURCE).await.unwrap(), 3);
    assert_eq!(rec.get(&mut tx, Node::DOUBLED).await.unwrap(), 6);
    assert_eq!(rec.get(&mut tx, Node::PREVIEW).await.unwrap(), 3);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Source notification itself neither writes an audit column nor publishes an event.
#[tokio::test]
async fn external_eventful_source_creates_no_event_or_audit_write() {
    use erp_registry::{FieldDecl, ModelDecl};
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([
            ModelDecl::new("test_external_modified", "test_external.eventful")
                .eventful(true)
                .fields(const { &[FieldDecl::integer("source")] }),
        ])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.install_event_schema().await.unwrap();
    let id: i64 = tx
        .query(
            "INSERT INTO test_external_eventful (source) VALUES (3) RETURNING id",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    let records = tx
        .env()
        .records("test_external.eventful")
        .unwrap()
        .browse([id])
        .unwrap();
    assert_eq!(
        records.cell(&mut tx, "source").await.unwrap(),
        erp_sdk::Cell::Int(3)
    );
    tx.execute(
        "UPDATE test_external_eventful SET source=7 WHERE id=$1",
        &[&id],
    )
    .await
    .unwrap();
    tx.capture_statements(true);
    records
        .modified_external(&mut tx, &["source"])
        .await
        .unwrap();
    assert!(
        tx.captured().is_empty(),
        "no-dependency notification sends no statement: {:?}",
        tx.captured()
    );
    assert_eq!(
        records.cell(&mut tx, "source").await.unwrap(),
        erp_sdk::Cell::Int(7)
    );
    tx.commit().await.unwrap();
    let count: i64 = tx
        .query(
            "SELECT count(*) FROM \"erp_event\".\"ir_event_outbox\"",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(count, 0);
    let writer: Option<i64> = tx
        .query(
            "SELECT write_uid FROM test_external_eventful WHERE id=$1",
            &[&id],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(writer, None);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn external_trigger_notification_advances_stream_generation() {
    use erp_registry::{FieldDecl, ModelDecl};
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([
            ModelDecl::new("test_external_modified", "test_external.clock")
                .fields(const { &[FieldDecl::integer("tick")] }),
        ])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let first = node(&mut tx, 1, None).await;
    let second = node(&mut tx, 2, None).await;
    let clock = tx
        .env()
        .records("test_external.clock")
        .unwrap()
        .create(&mut tx, vec![erp_sdk::RawVals::new()])
        .await
        .unwrap();
    tx.flush().await.unwrap();
    // Constant fixture SQL, inside this disposable test database; DDL precedes the cursor.
    tx.batch("CREATE FUNCTION test_external_tick() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN UPDATE test_external_node SET source=source+10; RETURN NEW; END $$; CREATE TRIGGER test_external_clock_change AFTER UPDATE ON test_external_clock FOR EACH STATEMENT EXECUTE FUNCTION test_external_tick()").await.unwrap();
    let model = tx.env().records("test_external.node").unwrap();
    let mut stream = model
        .stream(
            &mut tx,
            &Domain::TRUE,
            &["source"],
            &erp_orm::StreamOptions::new()
                .order("id")
                .fixed_window_for_tests(1)
                .unwrap(),
        )
        .await
        .unwrap();
    let first_window = stream.next(&mut tx).await.unwrap().unwrap();
    assert_eq!(first_window.get(0, "source").unwrap(), Value::Int(1));
    tx.execute(
        "UPDATE test_external_clock SET tick=1 WHERE id=$1",
        &[&clock.ids()[0]],
    )
    .await
    .unwrap();
    let affected = model
        .browse([first.rec_ids()[0].get(), second.rec_ids()[0].get()])
        .unwrap();
    affected
        .modified_external(&mut tx, &["source"])
        .await
        .unwrap();
    let next = stream.next(&mut tx).await.unwrap().unwrap();
    assert_eq!(next.get(0, "source").unwrap(), Value::Int(12));
    assert!(stream.next(&mut tx).await.unwrap().is_none());
    drop(stream);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn external_refuses_delegated_columns_and_raw_only_scope() {
    use erp_registry::{FieldDecl, ModelDecl};
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([
            ModelDecl::new("test_external_modified", "test_external.delegate")
                .inherits(&[("test_external.node", "node_id")])
                .fields(
                    const {
                        &[FieldDecl::many2one("node_id", "test_external.node")
                            .required(true)
                            .delegate(true)
                            .ondelete(erp_fields::OnDelete::Cascade)]
                    },
                ),
        ])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let child = tx.env().records("test_external.delegate").unwrap();
    programming(
        child
            .modified_external(&mut tx, &["source"])
            .await
            .unwrap_err(),
        "External modification: field 'test_external.delegate.source' is not a stored column owned by this model",
    );
    let rec = node(&mut tx, 3, None).await;
    tx.flush().await.unwrap();
    let mut raw = tx.raw_statements_only();
    programming(
        rec.modified_external(&mut raw, &["source"])
            .await
            .unwrap_err(),
        "the record API is unavailable in a raw-statements-only scope (query, execute, batch)",
    );
    drop(raw);
    assert!(tx.is_cached(&rec, "source").unwrap());
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn external_unadmitted_switch_does_not_flush() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let rec = node(&mut tx, 3, None).await;
    tx.flush().await.unwrap();
    rec.set(&mut tx, Node::NOTE, "pending").await.unwrap();
    // The first switch is from System. The second carries an ordinary user's pending switch.
    let switched = rec.with_user(5).with_user(6);
    tx.capture_statements(true);
    programming(
        notify(&switched, &mut tx, &["source"]).await.unwrap_err(),
        "user 5 may not act as user 6: with_user from a principal that is not the system needs the capability that authorizes impersonation, and this ORM was built without one (OrmBuilder::impersonate_capability)",
    );
    assert!(tx.captured().is_empty());
    assert!(tx.is_dirty(&rec, "note").unwrap());
    assert!(tx.is_cached(&rec, "source").unwrap());
    assert!(!tx.is_poisoned());
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn external_cancelled_notification_is_rollback_only() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let rec = node(&mut tx, 3, None).await;
    tx.commit().await.unwrap();
    assert_eq!(rec.get(&mut tx, Node::SOURCE).await.unwrap(), 3);
    tx.invalidate_records(&rec, Some(&["parent_id"]))
        .await
        .unwrap();
    {
        let mut future = std::pin::pin!(notify(&rec, &mut tx, &["source"]));
        let pending = std::future::poll_fn(|cx| {
            std::task::Poll::Ready(future.as_mut().poll(cx).is_pending())
        })
        .await;
        assert!(
            pending,
            "cold relation lookup must suspend before completion"
        );
    }
    assert!(tx.is_poisoned());
    programming(
        tx.commit().await.unwrap_err(),
        "the transaction can no longer be used: an ORM operation was cancelled (its future was dropped) while it was changing the transaction's state; roll back",
    );
    tx.rollback().await.unwrap();
    assert_eq!(rec.get(&mut tx, Node::SOURCE).await.unwrap(), 3);
    drop(tx);
    h.cleanup().await.unwrap();
}
