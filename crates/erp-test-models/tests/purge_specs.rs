//! Uninstall data removal and the external ids of unlinked records: the purge the
//! module system runs before it removes a module's schema and the ORM
//! unlink hook that deletes the external ids of deleted records.
//!
//! The module-level run (hooks, schema, states) is in `erp-modules/tests/data_uninstall_specs.rs`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::sync::Arc;

use erp_data::load::{LoadSession, NoFiles, SessionOptions, purge_module_data};
use erp_data::model_data::{UNLINK_HOOK, lookup};
use erp_data::{XidBinding, XmlId, model_data};
use erp_sdk::{ErrorKind, RawVals, Transaction, Value};
use erp_test_models::{base_users, test_convert};
use erp_testkit::{Harness, assert_query_count};

const RECORD: &str = "test_convert.record";
const USERED: &str = "test_convert.usered";

/// The message of the ORM error a `LoadError` wraps (its own text only says where it happened).
fn source_message(err: &erp_data::load::LoadError) -> String {
    std::error::Error::source(err)
        .expect("an ORM source")
        .to_string()
}

/// A harness with `ir.model.data` and, unless `hook` is false, the unlink hook.
async fn harness(hook: bool) -> Harness {
    let builder = Harness::builder([&base_users::ADDON, &test_convert::ADDON, &model_data::ADDON])
        .with_decls([erp_data::ir_model_data_decl()]);
    let builder = if hook {
        builder.unlink_hook(UNLINK_HOOK)
    } else {
        builder
    };
    builder.build().await.expect("harness")
}

fn xid(text: &str) -> XmlId {
    XmlId::parse(text).unwrap()
}

/// `n` new records of `model`.
async fn make(tx: &mut Transaction, model: &str, n: usize) -> Vec<i64> {
    let records = tx.env().records(model).unwrap();
    let vals: Vec<RawVals> = (0..n)
        .map(|i| {
            records
                .vals_from_values([(&"name".to_owned(), &Value::str(format!("r{i}")))])
                .unwrap()
        })
        .collect();
    records.create(tx, vals).await.unwrap().ids()
}

async fn bind(tx: &mut Transaction, model: &str, pairs: &[(&str, i64)]) {
    let env = tx.env();
    let bindings: Vec<XidBinding> = pairs
        .iter()
        .map(|(x, id)| XidBinding::new(xid(x), model, *id, false))
        .collect();
    model_data::update_xmlids(tx, &env, &bindings, false)
        .await
        .unwrap();
}

/// Insert an external id of a module name the id parser would not take (`__cloc_exclude__`) or of
/// a model that is not in the registry.
async fn raw_xid(tx: &mut Transaction, module: &str, name: &str, model: &str, res_id: i64) {
    tx.execute(
        "INSERT INTO ir_model_data (module, name, model, res_id, noupdate) \
         VALUES ($1, $2, $3, $4, false)",
        &[&module, &name, &model, &res_id],
    )
    .await
    .unwrap();
}

/// Whether the external id exists, straight from the database.
async fn has(tx: &mut Transaction, text: &str) -> bool {
    let env = tx.env();
    let x = xid(text);
    lookup(tx, &env, std::slice::from_ref(&x))
        .await
        .unwrap()
        .contains_key(&x)
}

async fn alive(tx: &mut Transaction, model: &str, ids: &[i64]) -> Vec<i64> {
    let records = tx
        .env()
        .records(model)
        .unwrap()
        .browse(ids.to_vec())
        .unwrap();
    records.exists(tx).await.unwrap().ids()
}

async fn module_xids(tx: &mut Transaction, module: &str) -> i64 {
    let rows = tx
        .query(
            "SELECT count(*) FROM ir_model_data WHERE module = $1",
            &[&module],
        )
        .await
        .unwrap();
    rows[0].get(0)
}

// ---- the unlink hook ---------------------------------------------------------------------

#[tokio::test]
async fn unlink_removes_the_external_ids_of_the_deleted_records_only() {
    let h = harness(true).await;
    let mut tx = h.begin().await.unwrap();
    let ids = make(&mut tx, RECORD, 3).await;
    // the same numeric id in another model must survive
    let other = make(&mut tx, USERED, 1).await;
    assert_eq!(other[0], ids[0], "the fixture ids coincide");
    bind(
        &mut tx,
        RECORD,
        &[("m1.a", ids[0]), ("m2.a", ids[0]), ("m1.b", ids[1])],
    )
    .await;
    bind(&mut tx, USERED, &[("m1.u", other[0])]).await;
    let records = tx
        .env()
        .records(RECORD)
        .unwrap()
        .browse(vec![ids[0]])
        .unwrap();
    records.unlink(&mut tx).await.unwrap();
    assert!(!has(&mut tx, "m1.a").await);
    assert!(!has(&mut tx, "m2.a").await);
    assert!(has(&mut tx, "m1.b").await, "another record's id stays");
    assert!(has(&mut tx, "m1.u").await, "another model's id stays");
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// One statement per `unlink` call, whatever the number of records: the hook costs exactly one
/// statement over the same unlink without it, for 1 and for 50 records.
#[tokio::test]
async fn the_unlink_hook_costs_one_statement_per_call() {
    let with = harness(true).await;
    let without = harness(false).await;
    let mut costs = Vec::new();
    for h in [&with, &without] {
        for n in [1usize, 50] {
            let mut tx = h.begin().await.unwrap();
            let ids = make(&mut tx, RECORD, n).await;
            let pairs: Vec<(String, i64)> =
                ids.iter().map(|id| (format!("m1.r{id}"), *id)).collect();
            let pairs: Vec<(&str, i64)> = pairs.iter().map(|(x, i)| (x.as_str(), *i)).collect();
            bind(&mut tx, RECORD, &pairs).await;
            tx.flush().await.unwrap();
            let records = tx
                .env()
                .records(RECORD)
                .unwrap()
                .browse(ids.clone())
                .unwrap();
            let used = assert_query_count!(tx, >= 1, <= 20, {
                let before = tx.query_count();
                records.unlink(&mut tx).await.unwrap();
                tx.query_count() - before
            });
            costs.push(used);
            tx.rollback().await.unwrap();
        }
    }
    // with: [1 record, 50 records], without: [1 record, 50 records]
    assert_eq!(costs[0], costs[1], "does not grow with the records");
    assert_eq!(costs[2], costs[3]);
    assert_eq!(costs[0], costs[2] + 1, "the hook is one statement");
    with.cleanup().await.unwrap();
    without.cleanup().await.unwrap();
}

/// A registry without `ir.model.data` and the hook registered: unlink works and costs nothing
/// extra.
#[tokio::test]
async fn the_unlink_hook_does_nothing_without_ir_model_data() {
    let build = async |hook: bool| {
        let builder = Harness::builder([&base_users::ADDON, &test_convert::ADDON]);
        let builder = if hook {
            builder.unlink_hook(UNLINK_HOOK)
        } else {
            builder
        };
        builder.build().await.unwrap()
    };
    let mut used = Vec::new();
    for hook in [true, false] {
        let h = build(hook).await;
        let mut tx = h.begin().await.unwrap();
        let ids = make(&mut tx, RECORD, 2).await;
        tx.flush().await.unwrap();
        let records = tx.env().records(RECORD).unwrap().browse(ids).unwrap();
        let before = tx.query_count();
        records.unlink(&mut tx).await.unwrap();
        used.push(tx.query_count() - before);
        tx.rollback().await.unwrap();
        h.cleanup().await.unwrap();
    }
    assert_eq!(used[0], used[1]);
}

// ---- the purge ---------------------------------------------------------------------------

#[tokio::test]
async fn spec_data_030_uninstall_requires_a_system_user() {
    let h = harness(true).await;
    let mut tx = h.begin().await.unwrap();
    let ids = make(&mut tx, RECORD, 1).await;
    bind(&mut tx, RECORD, &[("m1.a", ids[0])]).await;
    tx.commit().await.unwrap();
    let mut user = h.begin_as(2).await.unwrap();
    let err = purge_module_data(&mut user, &["m1"]).await.unwrap_err();
    assert_eq!(err.orm_kind(), Some(ErrorKind::Access));
    assert_eq!(
        source_message(&err),
        "Administrator access is required to uninstall a module"
    );
    // a user who turns elevation on is still not the system principal
    let elevated = user.env().sudo(true);
    assert!(elevated.su());
    user.rollback().await.unwrap();
    let mut check = h.begin().await.unwrap();
    assert!(has(&mut check, "m1.a").await, "nothing was removed");
    assert_eq!(alive(&mut check, RECORD, &ids).await, ids);
    check.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// models that stay: newest external id first, a record another module names is kept with only
/// the removed module's id gone, a `__cloc_exclude__` id goes with its record, ids of vanished
/// records, unavailable models and schema records; copied views are
/// `the_purge_removes_copied_views`, and selection `ondelete` policies are processed)
#[tokio::test]
async fn spec_data_029_removes_owned_records_in_order_and_keeps_shared_ones() {
    let h = harness(true).await;
    let mut tx = h.begin().await.unwrap();
    let recs = make(&mut tx, RECORD, 4).await;
    let users = make(&mut tx, USERED, 2).await;
    // a vanished record: the oldest external id, so it does not decide the model order
    raw_xid(&mut tx, "m1", "gone", RECORD, 987_654).await;
    // the record model's ids are older than the usered model's
    bind(
        &mut tx,
        RECORD,
        &[("m1.a", recs[0]), ("m1.b", recs[1]), ("m1.shared", recs[2])],
    )
    .await;
    bind(
        &mut tx,
        RECORD,
        &[("other.shared", recs[2]), ("other.own", recs[3])],
    )
    .await;
    bind(&mut tx, USERED, &[("m1.u", users[0])]).await;
    raw_xid(&mut tx, "__cloc_exclude__", "x", RECORD, recs[1]).await;
    // an unavailable model, a schema record
    raw_xid(&mut tx, "m1", "ghost", "no.such.model", 1).await;
    raw_xid(&mut tx, "m1", "schema", "ir.model", 5).await;

    let report = purge_module_data(&mut tx, &["m1"]).await.unwrap();

    // newest model first: usered (bound last) before the record model
    assert_eq!(
        report.deleted,
        [(USERED.to_owned(), 1), (RECORD.to_owned(), 2)]
    );
    assert_eq!(report.kept_shared, 1);
    assert_eq!(report.unavailable_models, ["no.such.model"]);
    assert_eq!(alive(&mut tx, RECORD, &recs).await, [recs[2], recs[3]]);
    assert_eq!(alive(&mut tx, USERED, &users).await, [users[1]]);
    // the removed module keeps only what is the module system's (the schema record)
    assert!(!has(&mut tx, "m1.a").await);
    assert!(
        !has(&mut tx, "m1.shared").await,
        "the shared record loses m1's id"
    );
    assert!(!has(&mut tx, "m1.gone").await);
    assert!(!has(&mut tx, "m1.ghost").await);
    assert!(
        has(&mut tx, "m1.schema").await,
        "schema ids are left to the module system"
    );
    assert_eq!(module_xids(&mut tx, "m1").await, 1);
    assert_eq!(module_xids(&mut tx, "__cloc_exclude__").await, 0);
    assert!(has(&mut tx, "other.shared").await);
    assert!(has(&mut tx, "other.own").await);
    assert_eq!(
        report.xids_removed,
        // m1.shared, m1.gone, m1.ghost (the others went with their records)
        3
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// touched, their table is dropped with the module)
#[tokio::test]
async fn records_of_models_that_go_with_the_modules_are_left_to_the_schema_removal() {
    let h = harness(true).await;
    let mut tx = h.begin().await.unwrap();
    let ids = make(&mut tx, RECORD, 2).await;
    bind(
        &mut tx,
        RECORD,
        &[("test_convert.a", ids[0]), ("test_convert.b", ids[1])],
    )
    .await;
    // `test_convert` declares the model: removing it removes the model
    let report = purge_module_data(&mut tx, &["test_convert"]).await.unwrap();
    assert!(report.deleted.is_empty(), "{report:?}");
    assert_eq!(report.xids_removed, 0);
    assert_eq!(alive(&mut tx, RECORD, &ids).await, ids);
    assert!(has(&mut tx, "test_convert.a").await);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// The purge costs the same statements for 10 records and for 1,000: the lookups,
/// per model an `exists`, an `unlink` with the hook's statement, and the final delete.
#[tokio::test]
async fn the_purge_costs_a_fixed_number_of_statements_per_model() {
    let h = harness(true).await;
    let mut costs = Vec::new();
    for n in [10usize, 1000] {
        let mut tx = h.begin().await.unwrap();
        let ids = make(&mut tx, RECORD, n).await;
        let names: Vec<String> = ids.iter().map(|id| format!("m1.r{id}")).collect();
        let pairs: Vec<(&str, i64)> = names
            .iter()
            .zip(&ids)
            .map(|(x, id)| (x.as_str(), *id))
            .collect();
        bind(&mut tx, RECORD, &pairs).await;
        tx.flush().await.unwrap();
        let used = assert_query_count!(tx, >= 1, <= 10, {
            let before = tx.query_count();
            let report = purge_module_data(&mut tx, &["m1"]).await.unwrap();
            assert_eq!(report.deleted, [(RECORD.to_owned(), n)]);
            tx.query_count() - before
        });
        costs.push(used);
        assert_eq!(module_xids(&mut tx, "m1").await, 0);
        tx.rollback().await.unwrap();
    }
    assert_eq!(costs[0], costs[1], "{costs:?}");
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn an_undeletable_record_fails_the_purge_with_the_orm_error() {
    let h = harness(true).await;
    let mut tx = h.begin().await.unwrap();
    let ids = make(&mut tx, RECORD, 1).await;
    bind(&mut tx, RECORD, &[("m1.a", ids[0])]).await;
    let table = tx.registry().model(RECORD).unwrap().table.clone();
    tx.batch(&format!(
        "CREATE TABLE keeper (rid bigint NOT NULL REFERENCES \"{table}\"(id)); \
         INSERT INTO keeper VALUES ({})",
        ids[0]
    ))
    .await
    .unwrap();
    tx.commit().await.unwrap();

    // what the ORM itself says about deleting that record
    let mut probe = h.begin().await.unwrap();
    let records = probe
        .env()
        .records(RECORD)
        .unwrap()
        .with_context([("force_delete", Value::Bool(true))])
        .browse(ids.clone())
        .unwrap();
    let expected = records.unlink(&mut probe).await.unwrap_err();
    drop(probe);

    let mut tx = h.begin().await.unwrap();
    let err = purge_module_data(&mut tx, &["m1"]).await.unwrap_err();
    assert_eq!(err.orm_kind(), Some(expected.kind()));
    assert_eq!(source_message(&err), expected.message());
    drop(tx);
    h.cleanup().await.unwrap();
}

// ---- the end-of-run cleanup and the unlink hook (integration review) ---------------------------

fn end_session() -> LoadSession {
    LoadSession::new(SessionOptions::new("20.0", Arc::new(NoFiles)))
}

/// Make the external ids of `module` older than this transaction, as if a previous run wrote them.
async fn age(tx: &mut Transaction, module: &str) {
    tx.execute(
        "UPDATE ir_model_data SET write_date = now() - interval '1 day' WHERE module = $1",
        &[&module],
    )
    .await
    .unwrap();
}

/// `process_end` deletes with full elevation: only the System principal may ask for it, like the
/// purge, and nothing is deleted for another principal.
#[tokio::test]
async fn process_end_is_refused_to_a_user_principal() {
    let h = harness(true).await;
    let mut tx = h.begin().await.unwrap();
    let ids = make(&mut tx, RECORD, 1).await;
    bind(&mut tx, RECORD, &[("m1.a", ids[0])]).await;
    age(&mut tx, "m1").await;
    tx.commit().await.unwrap();
    let mut user = h.begin_as(2).await.unwrap();
    let err = end_session()
        .process_end(&mut user, &["m1"])
        .await
        .unwrap_err();
    assert_eq!(err.orm_kind(), Some(ErrorKind::Access));
    assert_eq!(
        source_message(&err),
        "Administrator access is required to complete a module update"
    );
    user.rollback().await.unwrap();
    let mut check = h.begin().await.unwrap();
    assert_eq!(alive(&mut check, RECORD, &ids).await, ids);
    check.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// One no code
/// asserted goes.
#[tokio::test]
async fn a_rebound_unchanged_external_id_survives_process_end() {
    let h = harness(true).await;
    let mut tx = h.begin().await.unwrap();
    let ids = make(&mut tx, RECORD, 2).await;
    bind(&mut tx, RECORD, &[("m1.kept", ids[0]), ("m1.lost", ids[1])]).await;
    age(&mut tx, "m1").await;
    // the same binding again: the record and the model are unchanged
    bind(&mut tx, RECORD, &[("m1.kept", ids[0])]).await;
    end_session().process_end(&mut tx, &["m1"]).await.unwrap();
    assert_eq!(alive(&mut tx, RECORD, &ids).await, [ids[0]]);
    assert!(has(&mut tx, "m1.kept").await);
    assert!(!has(&mut tx, "m1.lost").await);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

async fn interleaved(tx: &mut Transaction) -> (Vec<i64>, Vec<i64>) {
    let users = make(tx, USERED, 2).await;
    let recs = make(tx, RECORD, 1).await;
    bind(tx, USERED, &[("m1.u1", users[0])]).await;
    bind(tx, RECORD, &[("m1.r1", recs[0])]).await;
    bind(tx, USERED, &[("m1.u2", users[1])]).await;
    let record_table = tx.registry().model(RECORD).unwrap().table.clone();
    let usered_table = tx.registry().model(USERED).unwrap().table.clone();
    tx.batch(&format!(
        "ALTER TABLE \"{record_table}\" ADD COLUMN uref bigint REFERENCES \"{usered_table}\"(id); \
         UPDATE \"{record_table}\" SET uref = {}",
        users[0]
    ))
    .await
    .unwrap();
    (users, recs)
}

#[tokio::test]
async fn process_end_deletes_interleaved_models_in_references_order() {
    let h = harness(true).await;
    let mut tx = h.begin().await.unwrap();
    let (users, recs) = interleaved(&mut tx).await;
    age(&mut tx, "m1").await;
    end_session().process_end(&mut tx, &["m1"]).await.unwrap();
    assert!(alive(&mut tx, USERED, &users).await.is_empty());
    assert!(alive(&mut tx, RECORD, &recs).await.is_empty());
    assert_eq!(module_xids(&mut tx, "m1").await, 0);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn the_purge_deletes_interleaved_models_in_references_order() {
    let h = harness(true).await;
    let mut tx = h.begin().await.unwrap();
    let (users, recs) = interleaved(&mut tx).await;
    let report = purge_module_data(&mut tx, &["m1"]).await.unwrap();
    assert_eq!(
        report.deleted,
        [
            (USERED.to_owned(), 1),
            (RECORD.to_owned(), 1),
            (USERED.to_owned(), 1)
        ]
    );
    assert!(alive(&mut tx, USERED, &users).await.is_empty());
    assert!(alive(&mut tx, RECORD, &recs).await.is_empty());
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// The hook deletes `ir.model.data` rows behind the ORM's cache and drops what it cached: the
/// external id read through the ORM before the unlink does not exist after it.
#[tokio::test]
async fn the_unlink_hook_drops_the_cached_external_id_rows() {
    let h = harness(true).await;
    let mut tx = h.begin().await.unwrap();
    let ids = make(&mut tx, RECORD, 1).await;
    bind(&mut tx, RECORD, &[("m1.a", ids[0])]).await;
    let env = tx.env();
    let x = xid("m1.a");
    let row_id = lookup(&mut tx, &env, std::slice::from_ref(&x))
        .await
        .unwrap()[&x]
        .id;
    let rows = tx
        .env()
        .records(model_data::MODEL)
        .unwrap()
        .browse([row_id])
        .unwrap();
    assert_eq!(
        rows.values(&mut tx, "name").await.unwrap(),
        [Value::str("a")]
    );
    tx.env()
        .records(RECORD)
        .unwrap()
        .browse(ids)
        .unwrap()
        .unlink(&mut tx)
        .await
        .unwrap();
    assert!(rows.exists(&mut tx).await.unwrap().is_empty());
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

// ---- copied views and what the purge does not do yet (F5) ------------------------

/// A view copied from a view of the removed module (a key `<module>.…` and no external id) is
/// deleted after the module's records (`_remove_copied_views`); the module's own view goes with
/// its records, and a view keyed like the module but named by a module that stays is kept.
#[tokio::test]
async fn the_purge_removes_copied_views() {
    let h = Harness::builder([
        &erp_addon_base::ADDON,
        &test_convert::ADDON,
        &model_data::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .unlink_hook(UNLINK_HOOK)
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    let ids = make(&mut tx, RECORD, 1).await;
    bind(&mut tx, RECORD, &[("m1.a", ids[0])]).await;
    let views = tx.env().records("ir.ui.view").unwrap();
    let view = async |tx: &mut Transaction, name: &str, key: &str| -> i64 {
        let values: erp_sdk::ValueMap = [
            ("name", Value::str(name)),
            ("key", Value::str(key)),
            ("type", Value::str("qweb")),
            ("arch", Value::str("<t/>")),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        let vals = views.vals_from_values(values.iter()).unwrap();
        views.create(tx, vec![vals]).await.unwrap().ids()[0]
    };
    let copy = view(&mut tx, "copy", "m1.copy").await;
    // `_` in a module name is not a wildcard: `m1` does not take `mx1.…`, `m_1` not `mx1.…`
    let other = view(&mut tx, "other", "mx1.copy").await;
    let own = view(&mut tx, "own", "m1.own").await;
    bind(&mut tx, "ir.ui.view", &[("m1.own", own)]).await;
    let shared = view(&mut tx, "shared", "m1.shared").await;
    bind(&mut tx, "ir.ui.view", &[("m2.shared", shared)]).await;
    // an export's external id is no module's: the view is still a copy (review A-S3)
    let exported = view(&mut tx, "exported", "m1.exported").await;
    raw_xid(
        &mut tx,
        "__export__",
        "ir_ui_view_1_abc",
        "ir.ui.view",
        exported,
    )
    .await;
    let report = purge_module_data(&mut tx, &["m1"]).await.unwrap();
    assert_eq!(report.copied_views_removed, 2);
    assert_eq!(report.copied_views_kept, 1);
    assert_eq!(
        alive(&mut tx, "ir.ui.view", &[copy, other, own, shared, exported]).await,
        [other, shared]
    );
    assert!(alive(&mut tx, RECORD, &ids).await.is_empty());
    let report = purge_module_data(&mut tx, &["m_1"]).await.unwrap();
    assert_eq!(report.copied_views_removed, 0);
    assert_eq!(alive(&mut tx, "ir.ui.view", &[other]).await, [other]);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

static SEL_A: erp_registry::AddonDescriptor = erp_registry::AddonDescriptor {
    name: "t_sel_a",
    depends: &["base"],
    exact_only: false,
};
static SEL_B: erp_registry::AddonDescriptor = erp_registry::AddonDescriptor {
    name: "t_sel_b",
    depends: &["t_sel_a"],
    exact_only: false,
};

/// A selection value the removed module added (`selection_add`) that records of a table that
/// stays still hold has its ondelete policy processed (defaulting to set null for optional fields).
#[tokio::test]
async fn the_purge_processes_stored_selection_values_of_the_removed_module() {
    use erp_registry::{FieldDecl, ModelDecl};
    let h = Harness::builder([&base_users::ADDON, &SEL_A, &SEL_B, &model_data::ADDON])
        .with_decls([
            erp_data::ir_model_data_decl(),
            ModelDecl::new("t_sel_a", "t.sel")
                .description("Sel")
                .fields(
                    const {
                        &[
                            FieldDecl::char("name"),
                            FieldDecl::selection("state", &[("a", "A")]),
                        ]
                    },
                ),
            ModelDecl::extension("t_sel_b", "t.sel", &["t.sel"])
                .fields(const { &[FieldDecl::selection_add("state", &[("b", Some("B"))])] }),
        ])
        .unlink_hook(UNLINK_HOOK)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let rows = tx.env().records("t.sel").unwrap();
    let create = |name: &str, state: &str| {
        let values: erp_sdk::ValueMap = [("name", Value::str(name)), ("state", Value::str(state))]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect();
        rows.vals_from_values(values.iter()).unwrap()
    };
    let first = create("x", "a");
    rows.create(&mut tx, vec![first]).await.unwrap();
    // a record of the removed module holding its value is deleted by the purge: no refusal
    // (review r2 DB)
    let owned = create("own", "b");
    let owned_id = rows.create(&mut tx, vec![owned]).await.unwrap().ids()[0];
    bind(&mut tx, "t.sel", &[("t_sel_b.own", owned_id)]).await;
    let report = purge_module_data(&mut tx, &["t_sel_b"]).await.unwrap();
    assert!(report.unavailable_models.is_empty());
    assert!(report.views_named_cascaded.is_empty(), "{report:?}");
    assert!(alive(&mut tx, "t.sel", &[owned_id]).await.is_empty());
    let second = create("y", "b");
    let second_id = rows.create(&mut tx, vec![second]).await.unwrap().ids()[0];
    let report = purge_module_data(&mut tx, &["t_sel_b"]).await.unwrap();
    assert!(report.unavailable_models.is_empty());
    assert_eq!(alive(&mut tx, "t.sel", &[second_id]).await, [second_id]);
    let recs = tx
        .env()
        .records("t.sel")
        .unwrap()
        .browse(vec![second_id])
        .unwrap();
    let vals = recs.read(&mut tx, &["state"]).await.unwrap();
    assert_eq!(vals[0]["state"], Value::False);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
